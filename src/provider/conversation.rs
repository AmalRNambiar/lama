//! A session's conversation, read for the console: what you asked, what the
//! agent said, and the tools it ran in between, in order.
//!
//! The rest of the crate sees transcripts only as facts; the console is the
//! one place that wants the words themselves, so this reads each format
//! directly, inside the provider boundary. Reading is incremental: a
//! [`Conversation`] remembers how far into the file it got and only parses
//! what was appended since.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::Provider;

/// One step of a conversation.
#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    /// A prompt typed by the user.
    User(String),
    /// Text the agent wrote back.
    Agent(String),
    /// A tool call; `ok` is `None` until its result arrives.
    Tool {
        name: String,
        summary: String,
        ok: Option<bool>,
    },
}

/// A session's conversation, kept up to date by [`refresh`](Self::refresh).
#[derive(Debug)]
pub struct Conversation {
    pub provider: Provider,
    pub path: PathBuf,
    pub turns: Vec<Turn>,
    offset: u64,
    /// A line still being written when last read.
    carry: String,
    /// Tool call id → index of its [`Turn::Tool`], for marking results.
    calls: HashMap<String, usize>,
}

impl Conversation {
    pub fn new(provider: Provider, path: PathBuf) -> Self {
        Conversation {
            provider,
            path,
            turns: Vec::new(),
            offset: 0,
            carry: String::new(),
            calls: HashMap::new(),
        }
    }

    /// Read whatever was appended since the last call. Returns whether any
    /// turn was added or changed.
    pub fn refresh(&mut self) -> bool {
        let Some(text) = read_from(&self.path, &mut self.offset) else {
            return false;
        };
        let before = (self.turns.len(), self.tool_results());
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
        (self.turns.len(), self.tool_results()) != before
    }

    fn tool_results(&self) -> usize {
        self.turns
            .iter()
            .filter(|t| matches!(t, Turn::Tool { ok: Some(_), .. }))
            .count()
    }

    /// Distinctive lines to look for on a terminal's screen: the latest few
    /// prompts and replies, newest first.
    pub fn snippets(&self) -> Vec<String> {
        let mut out = Vec::new();
        let (mut you, mut agent) = (0, 0);
        for t in self.turns.iter().rev() {
            match t {
                Turn::User(s) if you < 3 => {
                    out.push(s.clone());
                    you += 1;
                }
                Turn::Agent(s) if agent < 3 => {
                    out.push(s.clone());
                    agent += 1;
                }
                _ => {}
            }
            if you == 3 && agent == 3 {
                break;
            }
        }
        out
    }

    /// The agent's most recent words, if it has said anything.
    pub fn last_agent_text(&self) -> Option<&str> {
        self.turns.iter().rev().find_map(|t| match t {
            Turn::Agent(s) => Some(s.as_str()),
            _ => None,
        })
    }

    fn push_tool(&mut self, id: Option<&str>, name: &str, summary: String) {
        if let Some(id) = id {
            self.calls.insert(id.to_string(), self.turns.len());
        }
        self.turns.push(Turn::Tool {
            name: name.to_string(),
            summary,
            ok: None,
        });
    }

    fn finish_tool(&mut self, id: &str, ok: bool) {
        if let Some(&i) = self.calls.get(id)
            && let Some(Turn::Tool { ok: slot, .. }) = self.turns.get_mut(i)
        {
            *slot = Some(ok);
        }
    }

    fn claude(&mut self, v: &Value) {
        // Sidechain and meta entries are not the conversation you had.
        if v.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || v.get("isMeta").and_then(Value::as_bool) == Some(true)
        {
            return;
        }
        let Some(content) = v.pointer("/message/content") else {
            return;
        };
        match v.get("type").and_then(Value::as_str) {
            Some("user") => match content {
                Value::String(s) => self.user(s),
                Value::Array(items) => {
                    for item in items {
                        match item.get("type").and_then(Value::as_str) {
                            Some("text") => {
                                if let Some(s) = item.get("text").and_then(Value::as_str) {
                                    self.user(s);
                                }
                            }
                            Some("tool_result") => {
                                if let Some(id) = item.get("tool_use_id").and_then(Value::as_str) {
                                    let err =
                                        item.get("is_error").and_then(Value::as_bool) == Some(true);
                                    self.finish_tool(id, !err);
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            },
            Some("assistant") => {
                for item in content.as_array().into_iter().flatten() {
                    match item.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(s) = item.get("text").and_then(Value::as_str) {
                                self.agent(s);
                            }
                        }
                        Some("tool_use") => {
                            let name = item.get("name").and_then(Value::as_str).unwrap_or("tool");
                            let summary = item.get("input").map(summarize).unwrap_or_default();
                            let id = item.get("id").and_then(Value::as_str);
                            self.push_tool(id, name, summary);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn codex(&mut self, v: &Value) {
        if v.get("type").and_then(Value::as_str) != Some("response_item") {
            return;
        }
        let Some(p) = v.get("payload") else { return };
        match p.get("type").and_then(Value::as_str) {
            Some("message") => {
                let role = p.get("role").and_then(Value::as_str);
                for item in p
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(s) = item.get("text").and_then(Value::as_str) else {
                        continue;
                    };
                    match role {
                        Some("user") => self.user(s),
                        Some("assistant") => self.agent(s),
                        _ => {}
                    }
                }
            }
            Some("function_call") | Some("custom_tool_call") => {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
                // Arguments arrive as a JSON string (function calls) or raw
                // input (custom tools such as apply_patch).
                let summary = match p.get("arguments").and_then(Value::as_str) {
                    Some(args) => serde_json::from_str::<Value>(args)
                        .map(|a| summarize(&a))
                        .unwrap_or_else(|_| patch_target(args)),
                    None => p
                        .get("input")
                        .and_then(Value::as_str)
                        .map(patch_target)
                        .unwrap_or_default(),
                };
                let id = p.get("call_id").and_then(Value::as_str);
                self.push_tool(id, name, summary);
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                if let Some(id) = p.get("call_id").and_then(Value::as_str) {
                    let out = p.get("output").map(|o| o.to_string()).unwrap_or_default();
                    let failed = out.contains("\"exit_code\":1")
                        || out.contains("Exit code: 1")
                        || out.contains("\"success\":false");
                    self.finish_tool(id, !failed);
                }
            }
            _ => {}
        }
    }

    /// A user turn — unless it is harness-injected context (system
    /// reminders, command echoes, environment blocks), which arrive as user
    /// messages but were never typed.
    fn user(&mut self, s: &str) {
        let s = s.trim();
        let injected = s.starts_with('<')
            || s.starts_with("[Request interrupted")
            // Codex hands the project's AGENTS.md over as a user message.
            || s.starts_with("# AGENTS.md")
            || s.contains("</INSTRUCTIONS>");
        if s.is_empty() || injected {
            return;
        }
        self.turns.push(Turn::User(s.to_string()));
    }

    fn agent(&mut self, s: &str) {
        let s = s.trim();
        if !s.is_empty() {
            self.turns.push(Turn::Agent(s.to_string()));
        }
    }
}

/// Where a session's latest turn stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnState {
    /// A prompt is being worked on.
    Working,
    /// The agent finished its turn and is waiting for the next prompt.
    Done,
}

/// The state of a session's latest turn, from the tail of its transcript:
/// Claude Code closes a turn with a `turn_duration` record (or an
/// `end_turn` reply) and opens one with a prompt; Codex writes
/// `task_started` and `task_complete`. `None` when the tail shows neither.
pub fn turn_state(provider: Provider, path: &Path) -> Option<TurnState> {
    const TAIL: u64 = 256 * 1024;
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.saturating_sub(TAIL))).ok()?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut state = None;
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let kind = v.get("type").and_then(Value::as_str);
        let step = match provider {
            Provider::Claude => match kind {
                Some("system")
                    if v.get("subtype").and_then(Value::as_str) == Some("turn_duration") =>
                {
                    Some(TurnState::Done)
                }
                Some("assistant")
                    if v.pointer("/message/stop_reason").and_then(Value::as_str)
                        == Some("end_turn") =>
                {
                    Some(TurnState::Done)
                }
                Some("user") if is_prompt(&v) => Some(TurnState::Working),
                _ => None,
            },
            Provider::Codex => match (kind, v.pointer("/payload/type").and_then(Value::as_str)) {
                (Some("event_msg"), Some("task_started" | "user_message")) => {
                    Some(TurnState::Working)
                }
                (Some("event_msg"), Some("task_complete")) => Some(TurnState::Done),
                _ => None,
            },
        };
        if step.is_some() {
            state = step;
        }
    }
    state
}

/// A Claude user entry that is a typed prompt — not a tool result, not
/// harness context.
fn is_prompt(v: &Value) -> bool {
    if v.get("isMeta").and_then(Value::as_bool) == Some(true)
        || v.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return false;
    }
    let typed = |s: &str| {
        let s = s.trim();
        !s.is_empty() && !s.starts_with('<')
    };
    match v.pointer("/message/content") {
        Some(Value::String(s)) => typed(s),
        Some(Value::Array(items)) => items.iter().any(|i| {
            i.get("type").and_then(Value::as_str) == Some("text")
                && i.get("text").and_then(Value::as_str).is_some_and(typed)
        }),
        _ => false,
    }
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

/// A one-line gist of a tool call's input: the command, path or pattern it
/// acts on, whichever it has.
fn summarize(input: &Value) -> String {
    for key in [
        "command",
        "cmd",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ] {
        match input.get(key) {
            Some(Value::String(s)) => return first_line(s),
            Some(Value::Array(parts)) => {
                let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
                // `["bash", "-lc", "<script>"]`: the script is the gist.
                let line = match words.as_slice() {
                    [_, flag, script] if flag.starts_with('-') => script.to_string(),
                    _ => words.join(" "),
                };
                return first_line(&line);
            }
            _ => {}
        }
    }
    String::new()
}

/// The gist of a raw tool input: the shell commands a Codex code-mode
/// script runs (`exec_command({cmd:"git fetch"})`), the file an
/// `apply_patch` touches, else its first line.
fn patch_target(input: &str) -> String {
    let cmds: Vec<&str> = input
        .split("cmd:\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|c| !c.is_empty())
        .collect();
    if !cmds.is_empty() {
        return cmds.join(" · ");
    }
    input
        .lines()
        .find_map(|l| {
            l.strip_prefix("*** ")
                .and_then(|r| r.split_once("File: ").map(|(_, f)| f.trim().to_string()))
        })
        .unwrap_or_else(|| first_line(input))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(provider: Provider, lines: &[&str]) -> Conversation {
        let dir =
            std::env::temp_dir().join(format!("lama-conv-{}-{}", std::process::id(), lines.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{:?}.jsonl", provider));
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut c = Conversation::new(provider, path);
        c.refresh();
        c
    }

    #[test]
    fn claude_turns_in_order() {
        let c = feed(
            Provider::Claude,
            &[
                r#"{"type":"user","message":{"content":"make it lazy"}}"#,
                r#"{"type":"user","message":{"content":"<system-reminder>x</system-reminder>"}}"#,
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"On it."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test\nmore"}}]}}"#,
                r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true}]}}"#,
            ],
        );
        assert_eq!(
            c.turns,
            vec![
                Turn::User("make it lazy".into()),
                Turn::Agent("On it.".into()),
                Turn::Tool {
                    name: "Bash".into(),
                    summary: "cargo test".into(),
                    ok: Some(false)
                },
            ]
        );
    }

    #[test]
    fn codex_turns_skip_injected_context() {
        let c = feed(
            Provider::Codex,
            &[
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>cwd</environment_context>"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"review the MR"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1","arguments":"{\"command\":[\"bash\",\"-lc\",\"rg -n foo\"]}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{\"exit_code\":0}"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"Looks good."}]}}"#,
            ],
        );
        assert_eq!(c.turns.len(), 3);
        assert_eq!(c.turns[0], Turn::User("review the MR".into()));
        assert_eq!(
            c.turns[1],
            Turn::Tool {
                name: "shell".into(),
                summary: "rg -n foo".into(),
                ok: Some(true)
            }
        );
        assert_eq!(c.last_agent_text(), Some("Looks good."));
    }

    #[test]
    fn code_mode_scripts_show_their_commands() {
        let js = r#"const r = await Promise.allSettled([tools.exec_command({cmd:"git fetch origin",max_output_tokens:400}), tools.exec_command({cmd:"git status"})]);"#;
        assert_eq!(patch_target(js), "git fetch origin · git status");
        assert_eq!(
            patch_target("*** Begin Patch\n*** Update File: src/a.rs\n"),
            "src/a.rs"
        );
    }

    #[test]
    fn turn_state_follows_the_latest_turn() {
        let dir = std::env::temp_dir().join(format!("lama-turn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.jsonl");
        let write = |lines: &[&str]| std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        write(&[
            r#"{"type":"user","message":{"content":"fix it"}}"#,
            r#"{"type":"assistant","message":{"stop_reason":"tool_use","content":[]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t"}]}}"#,
        ]);
        assert_eq!(
            turn_state(Provider::Claude, &path),
            Some(TurnState::Working),
            "a tool result is not a prompt"
        );
        write(&[
            r#"{"type":"user","message":{"content":"fix it"}}"#,
            r#"{"type":"system","subtype":"turn_duration"}"#,
        ]);
        assert_eq!(turn_state(Provider::Claude, &path), Some(TurnState::Done));
        let cx = dir.join("x.jsonl");
        std::fs::write(&cx, "{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_started\"}}\n{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\"}}\n").unwrap();
        assert_eq!(turn_state(Provider::Codex, &cx), Some(TurnState::Done));
    }

    #[test]
    fn partial_lines_wait_for_their_end() {
        let dir = std::env::temp_dir().join(format!("lama-conv-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        std::fs::write(&path, r#"{"type":"user","message":{"content":"hel"#).unwrap();
        let mut c = Conversation::new(Provider::Claude, path.clone());
        assert!(!c.refresh());
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, r#"lo"}}}}"#).unwrap();
        assert!(c.refresh());
        assert_eq!(c.turns, vec![Turn::User("hello".into())]);
    }
}
