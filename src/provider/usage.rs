//! A session's token usage, read for the context and cost gauges.
//!
//! Like [`Conversation`](super::conversation::Conversation) this reads the
//! transcript itself, inside the provider boundary, and incrementally: a
//! [`UsageMeter`] remembers how far into the file it got and only parses what
//! was appended since. What leaves is a format-free
//! [`UsageSummary`].
//!
//! Prices are USD per million tokens. Claude's come from Anthropic's pricing
//! page (platform.claude.com/docs/en/about-claude/pricing, as of 2026-10);
//! nothing else is built in. A price file, `lama/prices.json` under
//! `$XDG_CONFIG_HOME` (else `~/.config`), adds or overrides entries:
//!
//! ```json
//! { "gpt-6.1": { "input": 1.25, "output": 10.0, "cache_read": 0.125 } }
//! ```
//!
//! Keys are model-id substrings; the longest match wins. `cache_read`,
//! `cache_write` and `cache_write_1h` default to `input`, which is what a
//! cache token costs where no discount applies. A model nothing matches gets
//! no price, never a guess: its session shows tokens and no cost.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde_json::Value;

use super::Provider;
use crate::state::usage::UsageSummary;

/// One model's rates, USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rate {
    input: f64,
    output: f64,
    cache_read: f64,
    /// Five-minute cache writes.
    cache_write: f64,
    /// One-hour cache writes (Claude Code's default TTL), at twice input.
    cache_write_1h: f64,
}

/// A model's rates and what it knows about its window.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pricing {
    rate: Rate,
    /// Rates once a prompt passes this many tokens (Haiku 5.5's long tier).
    long: Option<(u64, Rate)>,
    /// The context window when the transcript does not record one.
    window: Option<u64>,
}

const fn rate(input: f64, output: f64, cache_read: f64) -> Rate {
    Rate {
        input,
        output,
        cache_read,
        cache_write: input * 1.25,
        cache_write_1h: input * 2.0,
    }
}

const K200: u64 = 200_000;
const M1: u64 = 1_000_000;

/// Anthropic's published prices (5m writes 1.25× input, 1h writes 2×).
/// Windows: 1M from Opus 4.7 / Sonnet 5 on, 200k before; a `[1m]` model id
/// or a context seen past 200k lifts an older model to 1M.
const CLAUDE: &[(&str, Rate, u64)] = &[
    ("claude-fable-5-1", rate(10.0, 50.0, 0.25), M1),
    ("claude-mythos-5-1", rate(10.0, 50.0, 0.25), M1),
    ("claude-fable-5", rate(10.0, 50.0, 1.0), M1),
    ("claude-mythos-5", rate(10.0, 50.0, 1.0), M1),
    ("claude-opus-5-5", rate(4.0, 20.0, 0.20), M1),
    ("claude-opus-5", rate(5.0, 25.0, 0.50), M1),
    ("claude-opus-4-8", rate(5.0, 25.0, 0.50), M1),
    ("claude-opus-4-7", rate(5.0, 25.0, 0.50), M1),
    ("claude-opus-4-6", rate(5.0, 25.0, 0.50), K200),
    ("claude-opus-4-5", rate(5.0, 25.0, 0.50), K200),
    ("claude-opus-4-1", rate(15.0, 75.0, 1.50), K200),
    ("claude-opus-4", rate(15.0, 75.0, 1.50), K200),
    ("claude-sonnet-5-5", rate(2.0, 10.0, 0.10), M1),
    ("claude-sonnet-5", rate(2.0, 10.0, 0.20), M1),
    ("claude-sonnet-4-6", rate(3.0, 15.0, 0.30), K200),
    ("claude-sonnet-4-5", rate(3.0, 15.0, 0.30), K200),
    ("claude-sonnet-4", rate(3.0, 15.0, 0.30), K200),
    ("claude-haiku-5-5", rate(0.10, 0.50, 0.01), M1),
    ("claude-haiku-4-5", rate(1.0, 5.0, 0.10), K200),
    ("claude-3-5-haiku", rate(0.80, 4.0, 0.08), K200),
];

/// Every priced model, by model-id substring.
#[derive(Debug, Clone, Default)]
struct Prices(Vec<(String, Pricing)>);

impl Prices {
    fn builtin() -> Self {
        Prices(
            CLAUDE
                .iter()
                .map(|&(key, rate, window)| {
                    let long = (key == "claude-haiku-5-5")
                        .then_some((100_000, self::rate(0.50, 2.50, 0.05)));
                    let p = Pricing {
                        rate,
                        long,
                        window: Some(window),
                    };
                    (key.to_string(), p)
                })
                .collect(),
        )
    }

    /// The built-in table with the user's price file laid over it, read
    /// once per process.
    fn shared() -> &'static Prices {
        static PRICES: OnceLock<Prices> = OnceLock::new();
        PRICES.get_or_init(|| {
            let mut p = Prices::builtin();
            if let Some(text) = price_file().and_then(|f| std::fs::read_to_string(f).ok()) {
                p.overlay(&text);
            }
            p
        })
    }

    /// Add or replace entries from a price file's JSON. Malformed entries
    /// are skipped; a price file can only add prices, never break the table.
    fn overlay(&mut self, json: &str) {
        let Ok(Value::Object(entries)) = serde_json::from_str::<Value>(json) else {
            return;
        };
        for (key, v) in entries {
            let num = |k: &str| v.get(k).and_then(Value::as_f64);
            let (Some(input), Some(output)) = (num("input"), num("output")) else {
                continue;
            };
            let write = num("cache_write").unwrap_or(input);
            let rate = Rate {
                input,
                output,
                cache_read: num("cache_read").unwrap_or(input),
                cache_write: write,
                cache_write_1h: num("cache_write_1h").unwrap_or(write),
            };
            // An override keeps what the table knew about the window.
            let window = self.exact(&key).and_then(|p| p.window);
            self.0.retain(|(k, _)| *k != key);
            self.0.push((
                key,
                Pricing {
                    rate,
                    long: None,
                    window,
                },
            ));
        }
    }

    fn exact(&self, key: &str) -> Option<&Pricing> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, p)| p)
    }

    /// The longest key contained in `model`, so `claude-opus-5-5` beats
    /// `claude-opus-5`.
    fn lookup(&self, model: &str) -> Option<&Pricing> {
        self.0
            .iter()
            .filter(|(k, _)| model.contains(k.as_str()))
            .max_by_key(|(k, _)| k.len())
            .map(|(_, p)| p)
    }
}

/// Where the user's price file lives: `$XDG_CONFIG_HOME/lama/prices.json`,
/// else `~/.config/lama/prices.json`.
pub fn price_file() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("lama").join("prices.json"))
}

/// Token counts of one model call, split the way they are billed.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Tokens {
    /// Input read fresh, neither from nor into the cache.
    input: u64,
    cache_read: u64,
    cache_write: u64,
    cache_write_1h: u64,
    output: u64,
}

impl Tokens {
    fn prompt(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.cache_write_1h
    }

    /// USD, or `None` when the model has no price.
    fn cost(&self, prices: &Prices, model: &str) -> Option<f64> {
        let p = prices.lookup(model)?;
        let r = match p.long {
            Some((over, long)) if self.prompt() > over => long,
            _ => p.rate,
        };
        let usd = self.input as f64 * r.input
            + self.cache_read as f64 * r.cache_read
            + self.cache_write as f64 * r.cache_write
            + self.cache_write_1h as f64 * r.cache_write_1h
            + self.output as f64 * r.output;
        Some(usd / 1e6)
    }
}

/// One local day's spend.
#[derive(Debug, Clone, Copy, Default)]
struct Day {
    usd: f64,
    /// Calls on a model with no price: while any remain, the day has no
    /// total.
    unpriced: u32,
}

impl Day {
    fn add(&mut self, cost: Option<f64>, sign: f64) {
        match cost {
            Some(c) => self.usd += sign * c,
            None if sign > 0.0 => self.unpriced += 1,
            None => self.unpriced = self.unpriced.saturating_sub(1),
        }
    }

    fn total(&self) -> Option<f64> {
        (self.unpriced == 0).then_some(self.usd.max(0.0))
    }
}

/// A session's usage, kept up to date by [`refresh`](Self::refresh).
#[derive(Debug)]
pub struct UsageMeter {
    pub provider: Provider,
    pub path: PathBuf,
    prices: &'static Prices,
    offset: u64,
    /// A line still being written when last read.
    carry: String,
    days: BTreeMap<NaiveDate, Day>,
    context_used: u64,
    /// The window the transcript recorded (Codex).
    recorded_window: Option<u64>,
    /// The latest model seen.
    model: Option<String>,
    /// Some call's model id asked for the 1M window, or context passed 200k.
    long_context: bool,
    /// Claude: each `requestId`'s day and cost so far. One reply spans
    /// several lines repeating the same usage; a repeat replaces what was
    /// counted rather than adding to it.
    requests: HashMap<String, (NaiveDate, Option<f64>)>,
    /// Codex: the last cumulative `total_token_usage`, to bill the delta.
    codex_total: Tokens,
}

impl UsageMeter {
    pub fn new(provider: Provider, path: PathBuf) -> Self {
        Self::with_prices(provider, path, Prices::shared())
    }

    fn with_prices(provider: Provider, path: PathBuf, prices: &'static Prices) -> Self {
        UsageMeter {
            provider,
            path,
            prices,
            offset: 0,
            carry: String::new(),
            days: BTreeMap::new(),
            context_used: 0,
            recorded_window: None,
            model: None,
            long_context: false,
            requests: HashMap::new(),
            codex_total: Tokens::default(),
        }
    }

    /// Read whatever was appended since the last call and summarize.
    pub fn refresh(&mut self) -> UsageSummary {
        if let Some(text) = read_from(&self.path, &mut self.offset) {
            let mut buf = std::mem::take(&mut self.carry);
            buf.push_str(&text);
            let complete = buf.ends_with('\n');
            let mut lines: Vec<&str> = buf.split('\n').collect();
            let tail = if complete {
                ""
            } else {
                lines.pop().unwrap_or("")
            };
            for line in lines.into_iter().filter(|l| !l.trim().is_empty()) {
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    match self.provider {
                        Provider::Claude => self.claude(&v),
                        Provider::Codex => self.codex(&v),
                    }
                }
            }
            self.carry = tail.to_string();
        }
        self.summary(Local::now().date_naive())
    }

    fn summary(&self, today: NaiveDate) -> UsageSummary {
        let mut total = Day::default();
        for d in self.days.values() {
            total.usd += d.usd;
            total.unpriced += d.unpriced;
        }
        UsageSummary {
            context_used: self.context_used,
            context_window: self.window(),
            cost_usd: total.total(),
            cost_today_usd: self.days.get(&today).copied().unwrap_or_default().total(),
        }
    }

    fn window(&self) -> Option<u64> {
        if let Some(w) = self.recorded_window {
            return Some(w);
        }
        let known = self
            .model
            .as_deref()
            .and_then(|m| self.prices.lookup(m))
            .and_then(|p| p.window);
        let lifted = (self.long_context || self.context_used > K200).then_some(M1);
        match (known, lifted) {
            (Some(k), Some(l)) => Some(k.max(l)),
            (k, l) => k.or(l),
        }
    }

    fn claude(&mut self, v: &Value) {
        if v.get("type").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        let Some(usage) = v.pointer("/message/usage") else {
            return;
        };
        let model = v
            .pointer("/message/model")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Claude Code's own placeholder replies made no model call.
        if model.starts_with('<') {
            return;
        }
        let n = |k: &str| usage.get(k).and_then(Value::as_u64).unwrap_or(0);
        let written = n("cache_creation_input_tokens");
        let hour = usage
            .pointer("/cache_creation/ephemeral_1h_input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(written);
        let tokens = Tokens {
            input: n("input_tokens"),
            cache_read: n("cache_read_input_tokens"),
            cache_write: written - hour,
            cache_write_1h: hour,
            output: n("output_tokens"),
        };
        // A subagent's turns share the file but not the main context.
        let sidechain = v.get("isSidechain").and_then(Value::as_bool) == Some(true);
        if !sidechain {
            self.context_used = tokens.prompt();
            self.model = Some(model.to_string());
            self.long_context |= model.contains("[1m]") || self.context_used > K200;
        }
        // Nothing billed, nothing to price: an empty usage on an unknown
        // model must not hide the session's cost.
        if tokens == Tokens::default() {
            return;
        }
        let day = local_day(v);
        let cost = tokens.cost(self.prices, model);
        match v.get("requestId").and_then(Value::as_str) {
            Some(id) => {
                if let Some((old_day, old)) = self.requests.insert(id.to_string(), (day, cost)) {
                    self.days.entry(old_day).or_default().add(old, -1.0);
                }
                self.days.entry(day).or_default().add(cost, 1.0);
            }
            None => self.days.entry(day).or_default().add(cost, 1.0),
        }
    }

    fn codex(&mut self, v: &Value) {
        let Some(p) = v.get("payload") else { return };
        match v.get("type").and_then(Value::as_str) {
            Some("turn_context") => {
                if let Some(m) = p.get("model").and_then(Value::as_str) {
                    self.model = Some(m.to_string());
                }
            }
            Some("event_msg") if p.get("type").and_then(Value::as_str) == Some("token_count") => {
                // The first count of a session comes before any call, with
                // no `info`.
                let Some(info) = p.get("info").filter(|i| i.is_object()) else {
                    return;
                };
                if let Some(w) = info.get("model_context_window").and_then(Value::as_u64) {
                    self.recorded_window = Some(w);
                }
                if let Some(last) = info.get("last_token_usage") {
                    let n = |k: &str| last.get(k).and_then(Value::as_u64).unwrap_or(0);
                    self.context_used = n("input_tokens") + n("output_tokens");
                }
                let Some(total) = info.get("total_token_usage").map(codex_tokens) else {
                    return;
                };
                // Counts repeat when only rate limits changed, so bill the
                // growth of the running total; a total that shrank restarted.
                let prev = if total.prompt() < self.codex_total.prompt() {
                    Tokens::default()
                } else {
                    self.codex_total
                };
                let delta = Tokens {
                    input: total.input.saturating_sub(prev.input),
                    cache_read: total.cache_read.saturating_sub(prev.cache_read),
                    cache_write: total.cache_write.saturating_sub(prev.cache_write),
                    cache_write_1h: 0,
                    output: total.output.saturating_sub(prev.output),
                };
                self.codex_total = total;
                if delta == Tokens::default() {
                    return;
                }
                let model = self.model.as_deref().unwrap_or("");
                let cost = delta.cost(self.prices, model);
                self.days.entry(local_day(v)).or_default().add(cost, 1.0);
            }
            _ => {}
        }
    }
}

/// Codex counts cached and cache-written tokens inside `input_tokens`, and
/// reasoning inside `output_tokens`; split them the way they are billed.
fn codex_tokens(u: &Value) -> Tokens {
    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = n("cached_input_tokens");
    let cache_write = n("cache_write_input_tokens");
    Tokens {
        input: n("input_tokens").saturating_sub(cache_read + cache_write),
        cache_read,
        cache_write,
        cache_write_1h: 0,
        output: n("output_tokens"),
    }
}

/// The local calendar day of a record's UTC `timestamp` (today if absent).
fn local_day(v: &Value) -> NaiveDate {
    v.get("timestamp")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<DateTime<Utc>>().ok())
        .map(|t| t.with_timezone(&Local).date_naive())
        .unwrap_or_else(|| Local::now().date_naive())
}

/// Read a file from `offset` to its end, advancing `offset`. A file that
/// shrank (rewritten) is read again from the start.
fn read_from(path: &Path, offset: &mut u64) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len < *offset {
        *offset = 0;
    }
    if len == *offset {
        return None;
    }
    f.seek(SeekFrom::Start(*offset)).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    *offset += bytes.len() as u64;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> &'static Prices {
        static P: OnceLock<Prices> = OnceLock::new();
        P.get_or_init(Prices::builtin)
    }

    fn write(name: &str, lines: &[String]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lama-usage-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    /// A UTC timestamp at local noon of `day`, so the local date is `day`
    /// in every timezone.
    fn at(day: NaiveDate) -> String {
        let local = day
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        local.with_timezone(&Utc).to_rfc3339()
    }

    fn claude_line(ts: &str, req: &str, model: &str, input: u64, read: u64, out: u64) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"{req}","message":{{"model":"{model}","usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{read},"output_tokens":{out}}}}}}}"#
        )
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn claude_counts_each_request_once() {
        let today = Local::now().date_naive();
        let ts = at(today);
        // One reply split over three lines, then a second reply.
        let path = write(
            "dedup",
            &[
                claude_line(&ts, "r1", "claude-opus-5-5", 1_000, 0, 500),
                claude_line(&ts, "r1", "claude-opus-5-5", 1_000, 0, 500),
                claude_line(&ts, "r1", "claude-opus-5-5", 1_000, 0, 500),
                claude_line(&ts, "r2", "claude-opus-5-5", 2_000, 100_000, 1_000),
            ],
        );
        let mut m = UsageMeter::with_prices(Provider::Claude, path, builtin());
        let s = m.refresh();
        // r1: 1000×4 + 500×20; r2: 2000×4 + 100k×0.20 + 1000×20 (per M).
        let want = (4_000.0 + 10_000.0 + 8_000.0 + 20_000.0 + 20_000.0) / 1e6;
        assert!(close(s.cost_usd, want), "{s:?}");
        assert!(close(s.cost_today_usd, want));
        assert_eq!(s.context_used, 102_000);
        assert_eq!(s.context_window, Some(1_000_000));
    }

    #[test]
    fn earlier_days_count_in_total_not_today() {
        let today = Local::now().date_naive();
        let before = today.pred_opt().unwrap();
        let path = write(
            "days",
            &[
                claude_line(&at(before), "a", "claude-sonnet-4-5", 100_000, 0, 0),
                claude_line(&at(today), "b", "claude-sonnet-4-5", 0, 0, 100_000),
            ],
        );
        let mut m = UsageMeter::with_prices(Provider::Claude, path, builtin());
        let s = m.refresh();
        assert!(close(s.cost_usd, 0.3 + 1.5), "{s:?}");
        assert!(close(s.cost_today_usd, 1.5));
        assert_eq!(s.context_window, Some(200_000), "an older model's window");
    }

    #[test]
    fn past_200k_lifts_the_window() {
        let ts = at(Local::now().date_naive());
        let path = write(
            "long",
            &[claude_line(&ts, "a", "claude-sonnet-4-5", 10, 250_000, 1)],
        );
        let s = UsageMeter::with_prices(Provider::Claude, path, builtin()).refresh();
        assert_eq!(s.context_window, Some(1_000_000));
    }

    #[test]
    fn one_hour_cache_writes_bill_at_twice_input() {
        let ts = at(Local::now().date_naive());
        let line = format!(
            r#"{{"type":"assistant","timestamp":"{ts}","requestId":"a","message":{{"model":"claude-opus-5-5","usage":{{"input_tokens":0,"cache_creation_input_tokens":1000000,"cache_read_input_tokens":0,"output_tokens":0,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":1000000}}}}}}}}"#
        );
        let s =
            UsageMeter::with_prices(Provider::Claude, write("1h", &[line]), builtin()).refresh();
        assert!(close(s.cost_usd, 8.0), "{s:?}");
    }

    #[test]
    fn unpriced_model_has_tokens_but_no_cost() {
        let ts = at(Local::now().date_naive());
        let path = write(
            "unpriced",
            &[claude_line(&ts, "a", "some-future-model", 5_000, 0, 10)],
        );
        let s = UsageMeter::with_prices(Provider::Claude, path, builtin()).refresh();
        assert_eq!(s.context_used, 5_000);
        assert_eq!(s.cost_usd, None);
        assert_eq!(s.cost_today_usd, None);
    }

    fn codex_count(ts: &str, input: u64, cached: u64, out: u64, last_in: u64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"cache_write_input_tokens":0,"output_tokens":{out},"reasoning_output_tokens":0,"total_tokens":{t}}},"last_token_usage":{{"input_tokens":{last_in},"cached_input_tokens":0,"output_tokens":100}},"model_context_window":400000}},"rate_limits":null}}}}"#,
            t = input + out
        )
    }

    #[test]
    fn codex_bills_the_running_total_with_a_price_file() {
        let ts = at(Local::now().date_naive());
        let path = write(
            "codex",
            &[
                format!(
                    r#"{{"timestamp":"{ts}","type":"turn_context","payload":{{"model":"gpt-test-1"}}}}"#
                ),
                format!(
                    r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":null}}}}"#
                ),
                codex_count(&ts, 1_000_000, 0, 0, 50_000),
                // Repeated count (a rate-limit update): nothing new to bill.
                codex_count(&ts, 1_000_000, 0, 0, 50_000),
                codex_count(&ts, 2_000_000, 1_000_000, 1_000_000, 80_000),
            ],
        );
        let unpriced = UsageMeter::with_prices(Provider::Codex, path.clone(), builtin()).refresh();
        assert_eq!(unpriced.cost_usd, None, "no OpenAI price is built in");
        assert_eq!(unpriced.context_used, 80_100);
        assert_eq!(unpriced.context_window, Some(400_000));

        let mut prices = Prices::builtin();
        prices.overlay(r#"{"gpt-test": {"input": 1.0, "output": 10.0, "cache_read": 0.1}}"#);
        let prices: &'static Prices = Box::leak(Box::new(prices));
        let s = UsageMeter::with_prices(Provider::Codex, path, prices).refresh();
        // 1M fresh input, then 1M cached + 1M output: 1 + 0.1 + 10.
        assert!(close(s.cost_usd, 11.1), "{s:?}");
        assert!(close(s.cost_today_usd, 11.1));
    }

    #[test]
    fn longest_key_wins_and_files_override() {
        let mut p = Prices::builtin();
        assert_eq!(p.lookup("claude-opus-5-5").unwrap().rate.input, 4.0);
        assert_eq!(p.lookup("claude-opus-5").unwrap().rate.input, 5.0);
        assert_eq!(
            p.lookup("claude-opus-4-1-20250805").unwrap().rate.input,
            15.0
        );
        p.overlay(r#"{"claude-opus-5-5": {"input": 3.0, "output": 15.0}, "bad": {"input": 1}}"#);
        let o = p.lookup("claude-opus-5-5").unwrap();
        assert_eq!((o.rate.input, o.rate.cache_read), (3.0, 3.0));
        assert_eq!(o.window, Some(M1), "an override keeps the window");
        assert!(p.lookup("bad").is_none());
    }

    #[test]
    fn appended_lines_are_read_incrementally() {
        let ts = at(Local::now().date_naive());
        let path = write("incr", &[]);
        std::fs::write(&path, "").unwrap();
        let mut m = UsageMeter::with_prices(Provider::Claude, path.clone(), builtin());
        let line = claude_line(&ts, "a", "claude-haiku-4-5", 1_000_000, 0, 0);
        let (head, tail) = line.split_at(40);
        std::fs::write(&path, head).unwrap();
        assert!(
            close(m.refresh().cost_usd, 0.0),
            "half a line is not a call"
        );
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "{tail}").unwrap();
        assert!(close(m.refresh().cost_usd, 1.0));
    }
}
