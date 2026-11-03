//! lama's own record of every agent session it has seen running, whether
//! it started in a lama tab or a terminal of your own: which process runs
//! it, in which kitty window, started how, and how it ended. One file per
//! session under `~/.local/share/lama/sessions/`.
//!
//! The record is what makes a session findable after the fact. Claude names
//! the session each process runs; Codex doesn't, so a fresh `codex` is tied
//! to its session once, when that session's file appears while the match is
//! unambiguous, and the tie is kept from then on however many windows open
//! after. A session closed while it waited on you, or mid-turn, is marked
//! so; one lama started that closed before its agent wrote anything keeps
//! its first prompt, so it can be started again rather than lost.
//!
//! A background observer ([`spawn_observer`]) keeps the record while lama
//! runs; everything else reads the snapshot it leaves behind.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::provider::Provider;

/// How often the observer looks.
const EVERY: std::time::Duration = std::time::Duration::from_secs(3);

/// How long a closed session is kept.
const KEEP_DAYS: i64 = 30;

/// How the session a process runs was found out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bound {
    /// Named by the agent itself (Claude's process registry) or by the
    /// command line (`codex resume <id>`).
    Exact,
    /// A fresh `codex`'s: the only window that could have started it.
    Inferred,
}

/// How a session's terminal went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Closed {
    /// It was waiting on a permission question.
    Asking,
    /// Its turn was still running.
    Working,
    /// Its turn was done.
    Idle,
    /// The agent never wrote anything: only its first prompt is left.
    Unstarted,
}

/// One session (or, before its agent has named one, one launch or process).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub provider: Provider,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub bound: Option<Bound>,
    /// When the session began (its first record).
    #[serde(default)]
    pub born: Option<DateTime<Utc>>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub transcript: Option<PathBuf>,
    /// The first prompt, when lama started it.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The kitty tab title lama gave it.
    #[serde(default)]
    pub title: Option<String>,
    /// Started from lama (the composer, a relaunch).
    #[serde(default)]
    pub from_lama: bool,
    /// The command line of the process last seen running it.
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    /// When that process started, so a reused pid isn't taken for it.
    #[serde(default)]
    pub started: Option<DateTime<Utc>>,
    #[serde(default)]
    pub socket: Option<String>,
    #[serde(default)]
    pub window: Option<u32>,
    #[serde(default)]
    pub tty: Option<String>,
    pub first_seen: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    #[serde(default)]
    pub closed: Option<DateTime<Utc>>,
    #[serde(default)]
    pub closed_as: Option<Closed>,
    /// Waiting on a permission question since, as lama last saw it.
    #[serde(default)]
    pub asking: Option<DateTime<Utc>>,
    /// What it is about: its first prompt, read off the transcript.
    #[serde(default)]
    pub about: Option<String>,
    /// The last prompt and reply when it closed.
    #[serde(default)]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub last_reply: Option<String>,
}

impl Entry {
    fn blank(provider: Provider, cwd: PathBuf, now: DateTime<Utc>) -> Entry {
        Entry {
            provider,
            session: None,
            bound: None,
            born: None,
            cwd,
            transcript: None,
            prompt: None,
            title: None,
            from_lama: false,
            argv: Vec::new(),
            pid: None,
            started: None,
            socket: None,
            window: None,
            tty: None,
            first_seen: now,
            last_seen: now,
            closed: None,
            closed_as: None,
            asking: None,
            about: None,
            last_prompt: None,
            last_reply: None,
        }
    }

    /// One line saying what it is: what you asked it.
    pub fn label(&self) -> String {
        self.prompt
            .as_deref()
            .or(self.about.as_deref())
            .or(self.last_prompt.as_deref())
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_else(|| match &self.session {
                Some(s) => format!("session {}", s.chars().take(8).collect::<String>()),
                None => "(no session yet)".into(),
            })
    }

    pub fn is_open(&self) -> bool {
        self.closed.is_none()
    }

    /// The file it is kept in, by session once there is one.
    fn key(&self) -> String {
        let p = self.provider.name();
        match (&self.session, self.pid) {
            (Some(s), _) => format!("{p}-{}", safe(s)),
            (None, Some(pid)) if !self.from_lama => format!(
                "{p}-proc-{pid}-{}",
                self.started.map_or(0, |t| t.timestamp())
            ),
            _ => format!("{p}-launch-{}", self.first_seen.timestamp_millis()),
        }
    }

    /// The project's name, for lists.
    pub fn project(&self) -> String {
        self.cwd
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.cwd.display().to_string())
    }
}

fn safe(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Where the record lives: `$XDG_DATA_HOME/lama/sessions`, else
/// `~/.local/share/lama/sessions`.
pub fn root() -> Option<PathBuf> {
    crate::task::root().and_then(|t| t.parent().map(|p| p.join("sessions")))
}

/// Serialises this process's reads and writes of the record.
static LOCK: Mutex<()> = Mutex::new(());
/// The record as the observer last left it.
static SNAPSHOT: Mutex<Option<Vec<Entry>>> = Mutex::new(None);

fn load(dir: &Path) -> HashMap<String, Entry> {
    let mut out = HashMap::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for f in rd.flatten() {
        let path = f.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        if let Some(e) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Entry>(&t).ok())
        {
            out.insert(stem, e);
        }
    }
    out
}

/// Write `after` over what was `before`: changed entries rewritten (whole,
/// through a rename), gone ones removed.
fn save(dir: &Path, before: &HashMap<String, Entry>, after: &[Entry]) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut kept = std::collections::HashSet::new();
    for e in after {
        let key = e.key();
        if before.get(&key) != Some(e)
            && let Ok(text) = serde_json::to_string_pretty(e)
        {
            let tmp = dir.join(format!(".{key}.tmp"));
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, dir.join(format!("{key}.json")));
            }
        }
        kept.insert(key);
    }
    for key in before.keys().filter(|k| !kept.contains(*k)) {
        let _ = std::fs::remove_file(dir.join(format!("{key}.json")));
    }
}

/// Change the record under the lock and refresh the snapshot.
fn edit(f: impl FnOnce(&mut Vec<Entry>)) {
    let Some(dir) = root() else { return };
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let before = load(&dir);
    let mut entries: Vec<Entry> = before.values().cloned().collect();
    f(&mut entries);
    save(&dir, &before, &entries);
    *SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some(entries);
}

/// Every entry: the open ones, newest first, then the closed ones, most
/// recently closed first. The order holds still while sessions run.
pub fn all() -> Vec<Entry> {
    let cached = SNAPSHOT.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut v = match cached {
        Some(v) => v,
        None => root()
            .map(|d| load(&d).into_values().collect())
            .unwrap_or_default(),
    };
    v.sort_by_cached_key(|e| {
        (
            e.closed.is_some(),
            std::cmp::Reverse(e.closed.unwrap_or(e.first_seen)),
            std::cmp::Reverse(e.first_seen),
            e.key(),
        )
    });
    v
}

/// The entry for session `session`.
pub fn find(provider: Provider, session: &str) -> Option<Entry> {
    all()
        .into_iter()
        .find(|e| e.provider == provider && e.session.as_deref() == Some(session))
}

/// The process running `session` right now, if the record ties one to it
/// and that process is still the one it saw.
pub fn pid_of(provider: Provider, session: &str) -> Option<u32> {
    let e = find(provider, session).filter(Entry::is_open)?;
    let pid = e.pid?;
    still_running(pid, e.started).then_some(pid)
}

/// The processes the record ties to some other session than `session`.
pub fn claimed_elsewhere(provider: Provider, session: &str) -> Vec<u32> {
    all()
        .into_iter()
        .filter(|e| e.provider == provider && e.is_open() && e.bound.is_some())
        .filter(|e| e.session.as_deref() != Some(session))
        .filter_map(|e| e.pid)
        .collect()
}

fn still_running(pid: u32, started: Option<DateTime<Utc>>) -> bool {
    let now = crate::procs::start_time(pid).map(DateTime::<Utc>::from);
    match (now, started) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() <= 2,
        (Some(_), None) => true,
        _ => false,
    }
}

/// lama just opened `window` to start a `provider` session with `prompt`.
pub fn record_launch(
    provider: Provider,
    cwd: &Path,
    prompt: &str,
    title: &str,
    window: &crate::kitty::Window,
) {
    let e = Entry {
        prompt: Some(prompt.to_string()),
        title: Some(title.to_string()),
        from_lama: true,
        socket: Some(window.socket.clone()),
        window: Some(window.id),
        ..Entry::blank(provider, cwd.to_path_buf(), Utc::now())
    };
    edit(|v| v.push(e));
}

/// Which sessions are waiting on a permission question now: each one's
/// `asking` is set (kept from when it began) or cleared.
pub fn note_asking(asking: &std::collections::HashSet<String>) {
    let changed = all().iter().any(|e| {
        e.is_open()
            && e.session
                .as_ref()
                .is_some_and(|s| asking.contains(s) != e.asking.is_some())
    });
    if !changed {
        return;
    }
    let now = Utc::now();
    edit(|v| {
        for e in v.iter_mut().filter(|e| e.is_open()) {
            let ask = e.session.as_ref().is_some_and(|s| asking.contains(s));
            match (ask, e.asking) {
                (true, None) => e.asking = Some(now),
                (false, Some(_)) => e.asking = None,
                _ => {}
            }
        }
    });
}

/// Drop a closed entry from the record.
pub fn forget(e: &Entry) {
    let key = e.key();
    edit(|v| v.retain(|x| x.key() != key || x.is_open()));
}

/// Pick a closed session back up in a new tab: a task's agent with
/// its bus, a session with a transcript by resuming it (with the model it
/// ran on), and one that never got going by starting it again with its
/// first prompt. Returns the new window.
pub fn resume(e: &Entry) -> Result<crate::kitty::Window, String> {
    if let Some(s) = &e.session
        && let Some(done) = crate::task::launch::resume_agent(s)
    {
        return done;
    }
    let title = e.title.clone().unwrap_or_else(|| {
        let short: String = e.session.as_deref().unwrap_or("").chars().take(8).collect();
        format!("{} {short}", e.project())
    });
    if let Some(s) = &e.session
        && has_transcript(e)
    {
        return crate::kitty::resume_with(&e.cwd, e.provider, s, &title, &model_args(&e.argv));
    }
    let Some(prompt) = &e.prompt else {
        return Err("nothing to resume — the agent never wrote anything".into());
    };
    let w = crate::kitty::launch_window(&e.cwd, e.provider, prompt, &title)?;
    let gone = e.key();
    edit(|v| {
        v.retain(|x| x.key() != gone);
        let now = Utc::now();
        v.push(Entry {
            first_seen: now,
            last_seen: now,
            socket: Some(w.socket.clone()),
            window: Some(w.id),
            ..fresh_launch(e)
        });
    });
    Ok(w)
}

/// `e` as a launch not yet seen running.
fn fresh_launch(e: &Entry) -> Entry {
    Entry {
        prompt: e.prompt.clone(),
        title: e.title.clone(),
        from_lama: true,
        ..Entry::blank(e.provider, e.cwd.clone(), Utc::now())
    }
}

fn has_transcript(e: &Entry) -> bool {
    e.transcript.as_ref().is_some_and(|p| p.exists())
        || e.session.as_ref().is_some_and(|s| {
            crate::provider::open(&crate::provider::Target::Id(s.clone()), Some(e.provider)).is_ok()
        })
}

/// The model flags a command line ran with, to resume on the same model.
fn model_args(argv: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "--model" | "-m") {
            if let Some(v) = it.next() {
                out.extend([a.clone(), v.clone()]);
            }
        } else if let Some(v) = a.strip_prefix("--model=") {
            out.extend(["--model".to_string(), v.to_string()]);
        }
    }
    out
}

/// Start the observer: a thread that brings the record up to date every
/// few seconds while lama runs.
pub fn spawn_observer() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("lama-registry".into())
            .spawn(|| {
                loop {
                    observe();
                    std::thread::sleep(EVERY);
                }
            });
    });
}

/// A running agent process, as one look at the machine found it.
#[derive(Debug, Clone)]
struct Seen {
    pid: u32,
    started: Option<DateTime<Utc>>,
    provider: Provider,
    cwd: PathBuf,
    argv: Vec<String>,
    /// The session it names itself.
    session: Option<String>,
    window: Option<crate::kitty::Window>,
}

/// A Codex session file that began recently, in a project with a fresh
/// `codex` open.
#[derive(Debug, Clone)]
struct Born {
    cwd: PathBuf,
    session: String,
    at: DateTime<Utc>,
    path: PathBuf,
}

/// One look: what runs now, then the record brought in line with it.
fn observe() {
    let seen = look();
    let open_codex: Vec<&Seen> = seen
        .iter()
        .filter(|s| s.provider == Provider::Codex && s.session.is_none())
        .collect();
    let births = codex_births(&open_codex);
    let now = Utc::now();
    edit(|v| {
        apply(v, &seen, &births, now, &|e, b| screen_score(e, b));
        close_out(v, &seen, now);
        resolve_transcripts(v);
    });
}

fn look() -> Vec<Seen> {
    crate::procs::agents()
        .into_iter()
        .map(|a| {
            let argv = crate::procs::argv(a.pid);
            let session = match a.provider {
                Provider::Claude => crate::procs::claude_session(a.pid),
                Provider::Codex => a.session.clone(),
            };
            Seen {
                pid: a.pid,
                started: crate::procs::start_time(a.pid).map(DateTime::<Utc>::from),
                provider: a.provider,
                cwd: a.cwd,
                argv,
                session,
                window: crate::kitty::window_of_pid(a.pid),
            }
        })
        .collect()
}

/// The Codex sessions that began since the oldest fresh `codex` in each
/// project started.
fn codex_births(fresh: &[&Seen]) -> Vec<Born> {
    let mut since: HashMap<&Path, DateTime<Utc>> = HashMap::new();
    for s in fresh {
        let t = s.started.unwrap_or_else(Utc::now);
        since
            .entry(s.cwd.as_path())
            .and_modify(|x| *x = (*x).min(t))
            .or_insert(t);
    }
    let mut out = Vec::new();
    for (cwd, from) in since {
        let from = from - chrono::Duration::seconds(10);
        let scope = crate::provider::Scope {
            project: Some(cwd.to_path_buf()),
            since: Some(from.into()),
            id_prefix: None,
        };
        for s in crate::provider::sweep(&scope, Some(Provider::Codex)) {
            if let Some(at) = first_timestamp(&s.root.path).filter(|t| *t >= from) {
                out.push(Born {
                    cwd: cwd.to_path_buf(),
                    session: s.id,
                    at,
                    path: s.root.path,
                });
            }
        }
    }
    out.sort_by_key(|b| b.at);
    out
}

fn first_timestamp(path: &Path) -> Option<DateTime<Utc>> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(f).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.get("timestamp")?.as_str()?.parse().ok()
}

/// How strongly window of `e` shows session `b`'s conversation.
fn screen_score(e: &Entry, b: &Born) -> usize {
    let (Some(socket), Some(id)) = (&e.socket, e.window) else {
        return 0;
    };
    let mut conv =
        crate::provider::conversation::Conversation::new(Provider::Codex, b.path.clone());
    conv.refresh();
    let w = crate::kitty::Window {
        socket: socket.clone(),
        id,
        tty: None,
    };
    crate::kitty::score(&w, &conv.snippets())
}

/// Bring the record in line with what runs: every process gets its entry,
/// and every entry the session it runs.
fn apply(
    v: &mut Vec<Entry>,
    seen: &[Seen],
    births: &[Born],
    now: DateTime<Utc>,
    score: &dyn Fn(&Entry, &Born) -> usize,
) {
    for s in seen {
        let same_proc = |e: &Entry| {
            e.pid == Some(s.pid)
                && e.provider == s.provider
                && match (e.started, s.started) {
                    (Some(a), Some(b)) => (a - b).num_seconds().abs() <= 2,
                    _ => true,
                }
        };
        let k = v
            .iter()
            .position(|e| e.is_open() && same_proc(e))
            .or_else(|| {
                // A window lama opened, the first time its agent is seen.
                let w = s.window.as_ref()?;
                v.iter().position(|e| {
                    e.from_lama
                        && e.is_open()
                        && e.pid.is_none()
                        && e.provider == s.provider
                        && e.socket.as_deref() == Some(w.socket.as_str())
                        && e.window == Some(w.id)
                })
            });
        let k = match k {
            Some(k) => k,
            None => {
                v.push(Entry::blank(s.provider, s.cwd.clone(), now));
                v.len() - 1
            }
        };
        let e = &mut v[k];
        e.pid = Some(s.pid);
        e.started = s.started;
        e.cwd = s.cwd.clone();
        e.argv = s.argv.clone();
        if let Some(w) = &s.window {
            e.socket = Some(w.socket.clone());
            e.window = Some(w.id);
            e.tty = w.tty.clone();
        }
        e.last_seen = now;
        if let Some(id) = &s.session {
            bind(v, k, id, Bound::Exact, None, None, now);
        }
    }
    // Fresh `codex` windows: each new session goes to the one window that
    // could have started it — or, with several, to the one whose screen
    // shows it. Nothing is guessed.
    for b in births {
        if v.iter().any(|e| {
            e.provider == Provider::Codex && e.is_open() && e.session.as_deref() == Some(&b.session)
        }) {
            continue;
        }
        let could = |e: &Entry| {
            e.provider == Provider::Codex
                && e.is_open()
                && e.pid.is_some()
                && e.cwd == b.cwd
                && e.bound != Some(Bound::Exact)
                && e.started
                    .is_none_or(|t| t <= b.at + chrono::Duration::seconds(5))
        };
        // A window that already runs an older session may have moved on to
        // this one (`/new`).
        let fresh: Vec<usize> = (0..v.len())
            .filter(|&k| could(&v[k]) && v[k].session.is_none())
            .collect();
        let moved: Vec<usize> = (0..v.len())
            .filter(|&k| could(&v[k]) && v[k].born.is_some_and(|t| t < b.at))
            .collect();
        let pick = match (fresh.as_slice(), moved.as_slice()) {
            ([only], []) => Some(*only),
            // One new window, and windows that could have moved on: the
            // new one, unless another plainly shows the session instead.
            ([only], moved) => {
                let mine = score(&v[*only], b);
                let theirs: Vec<(usize, usize)> =
                    moved.iter().map(|&k| (k, score(&v[k], b))).collect();
                let best = theirs.iter().map(|(_, x)| *x).max().unwrap_or(0);
                let top: Vec<usize> = theirs
                    .iter()
                    .filter(|(_, x)| *x == best && best > mine)
                    .map(|(k, _)| *k)
                    .collect();
                match top.as_slice() {
                    [k] => Some(*k),
                    _ => Some(*only),
                }
            }
            _ => {
                let all: Vec<usize> = fresh.iter().chain(&moved).copied().collect();
                let scores: Vec<usize> = all.iter().map(|&k| score(&v[k], b)).collect();
                let best = scores.iter().copied().max().unwrap_or(0);
                (best > 0 && scores.iter().filter(|&&x| x == best).count() == 1)
                    .then(|| all[scores.iter().position(|&x| x == best).unwrap_or(0)])
            }
        };
        if let Some(k) = pick {
            bind(
                v,
                k,
                &b.session,
                Bound::Inferred,
                Some(b.at),
                Some(b.path.clone()),
                now,
            );
        }
    }
}

/// Entry `k`'s process runs `session`. An entry already kept for that
/// session takes over the process (a resume); one for another session
/// stays behind, closed (the process moved on).
fn bind(
    v: &mut Vec<Entry>,
    k: usize,
    session: &str,
    how: Bound,
    born: Option<DateTime<Utc>>,
    transcript: Option<PathBuf>,
    now: DateTime<Utc>,
) {
    if v[k].session.as_deref() == Some(session) {
        v[k].bound = Some(how);
        if v[k].born.is_none() {
            v[k].born = born;
        }
        return;
    }
    let provider = v[k].provider;
    let live = v[k].clone();
    let other = v
        .iter()
        .position(|e| e.provider == provider && e.session.as_deref() == Some(session));
    let target = match other {
        Some(j) => {
            let e = &mut v[j];
            e.pid = live.pid;
            e.started = live.started;
            e.cwd = live.cwd.clone();
            e.argv = live.argv.clone();
            e.socket = live.socket.clone();
            e.window = live.window;
            e.tty = live.tty.clone();
            e.last_seen = now;
            e.closed = None;
            e.closed_as = None;
            e.asking = None;
            if e.prompt.is_none() {
                e.prompt = live.prompt.clone();
            }
            if e.title.is_none() {
                e.title = live.title.clone();
            }
            e.from_lama |= live.from_lama;
            j
        }
        None if live.session.is_none() => k,
        None => {
            let mut e = live.clone();
            e.session = None;
            e.born = None;
            e.transcript = None;
            e.prompt = None;
            e.first_seen = now;
            v.push(e);
            v.len() - 1
        }
    };
    let e = &mut v[target];
    e.session = Some(session.to_string());
    e.bound = Some(how);
    if born.is_some() {
        e.born = born;
    }
    if transcript.is_some() {
        e.transcript = transcript;
    }
    if target != k {
        if live.session.is_none() {
            v.remove(k);
        } else {
            let old = &mut v[k];
            old.closed = Some(now);
            old.closed_as = Some(Closed::Idle);
        }
    }
}

/// Entries whose process is gone are closed, with how; entries with
/// nothing to resume are dropped, and old ones aged out.
fn close_out(v: &mut Vec<Entry>, seen: &[Seen], now: DateTime<Utc>) {
    for e in v.iter_mut().filter(|e| e.is_open()) {
        let Some(pid) = e.pid else {
            // A launch whose agent never showed up in a minute.
            if now - e.first_seen > chrono::Duration::minutes(1) {
                e.closed = Some(now);
                e.closed_as = Some(Closed::Unstarted);
            }
            continue;
        };
        if seen
            .iter()
            .any(|s| s.pid == pid && s.provider == e.provider)
        {
            continue;
        }
        read_words(e, true);
        e.closed = Some(now);
        e.closed_as = Some(if e.transcript.is_none() {
            Closed::Unstarted
        } else if e.asking.is_some() {
            Closed::Asking
        } else {
            let state = e
                .transcript
                .as_deref()
                .and_then(|p| crate::provider::conversation::turn_state(e.provider, p));
            match state {
                Some(crate::provider::conversation::TurnState::Working) => Closed::Working,
                _ => Closed::Idle,
            }
        });
    }
    v.retain(|e| {
        e.is_open()
            || (e.closed_as != Some(Closed::Unstarted) || e.prompt.is_some())
                && e.closed
                    .is_some_and(|t| now - t < chrono::Duration::days(KEEP_DAYS))
    });
}

/// Find the transcript of each open session that doesn't have one yet, and
/// what it is about once it says.
fn resolve_transcripts(v: &mut [Entry]) {
    for e in v.iter_mut().filter(|e| e.is_open()) {
        if e.transcript.is_none()
            && let Some(s) = &e.session
            && let Ok(found) =
                crate::provider::open(&crate::provider::Target::Id(s.clone()), Some(e.provider))
        {
            if e.born.is_none() {
                e.born = first_timestamp(&found.root.path);
            }
            e.transcript = Some(found.root.path);
        }
        if e.about.is_none() && e.transcript.is_some() {
            read_words(e, false);
        }
    }
}

/// Fill in what `e` is about from its transcript, and with `last`, its last
/// prompt and reply.
fn read_words(e: &mut Entry, last: bool) {
    use crate::provider::conversation::{Conversation, Turn};
    let Some(path) = e.transcript.clone() else {
        return;
    };
    let mut conv = Conversation::new(e.provider, path);
    conv.refresh();
    let clip = |s: &str| crate::provider::summary::truncate_summary(s);
    let prompts: Vec<&str> = conv
        .turns
        .iter()
        .filter_map(|t| match t {
            Turn::User(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    if e.about.is_none() {
        e.about = prompts.first().map(|s| clip(s));
    }
    if last {
        e.last_prompt = prompts.last().map(|s| clip(s)).or(e.last_prompt.take());
        e.last_reply = conv.last_agent_text().map(clip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap()
    }

    fn proc(pid: u32, at: i64, session: Option<&str>) -> Seen {
        Seen {
            pid,
            started: Some(t(at)),
            provider: Provider::Codex,
            cwd: PathBuf::from("/p"),
            argv: vec!["codex".into()],
            session: session.map(String::from),
            window: Some(crate::kitty::Window {
                socket: "unix:/tmp/kitty-1".into(),
                id: pid,
                tty: None,
            }),
        }
    }

    fn born(id: &str, at: i64) -> Born {
        Born {
            cwd: PathBuf::from("/p"),
            session: id.into(),
            at: t(at),
            path: PathBuf::from(format!("/r/{id}.jsonl")),
        }
    }

    fn session_of(v: &[Entry], pid: u32) -> Option<String> {
        v.iter()
            .find(|e| e.is_open() && e.pid == Some(pid))
            .and_then(|e| e.session.clone())
    }

    #[test]
    fn a_fresh_codex_keeps_its_session_once_tied() {
        let mut v = Vec::new();
        let none = |_: &Entry, _: &Born| 0;
        apply(&mut v, &[proc(1, 0, None)], &[born("a", 30)], t(31), &none);
        assert_eq!(session_of(&v, 1).as_deref(), Some("a"));
        // A second fresh window opens later: the first keeps "a", and the
        // second gets its own session once that begins.
        let both = [proc(1, 0, None), proc(2, 60, None)];
        apply(&mut v, &both, &[born("a", 30)], t(61), &none);
        apply(&mut v, &both, &[born("a", 30), born("b", 90)], t(91), &none);
        assert_eq!(session_of(&v, 1).as_deref(), Some("a"));
        assert_eq!(session_of(&v, 2).as_deref(), Some("b"));
    }

    #[test]
    fn two_fresh_windows_wait_for_the_screen_to_tell() {
        let mut v = Vec::new();
        let procs = [proc(1, 0, None), proc(2, 5, None)];
        apply(&mut v, &procs, &[born("a", 30)], t(31), &|_, _| 0);
        assert_eq!(session_of(&v, 1), None);
        assert_eq!(session_of(&v, 2), None);
        let shows_on_2 = |e: &Entry, _: &Born| usize::from(e.pid == Some(2));
        apply(&mut v, &procs, &[born("a", 30)], t(34), &shows_on_2);
        assert_eq!(session_of(&v, 2).as_deref(), Some("a"));
        assert_eq!(session_of(&v, 1), None);
    }

    #[test]
    fn a_resume_takes_over_the_closed_entry() {
        let mut v = Vec::new();
        let none = |_: &Entry, _: &Born| 0;
        apply(&mut v, &[proc(1, 0, None)], &[born("a", 30)], t(31), &none);
        v[0].transcript = Some(PathBuf::from("/r/a.jsonl"));
        close_out(&mut v, &[], t(40));
        assert!(!v[0].is_open());
        apply(&mut v, &[proc(7, 100, Some("a"))], &[], t(101), &none);
        assert_eq!(v.len(), 1);
        assert!(v[0].is_open());
        assert_eq!(v[0].pid, Some(7));
        assert_eq!(v[0].bound, Some(Bound::Exact));
    }

    #[test]
    fn a_launch_that_never_started_keeps_its_prompt() {
        let mut v = Vec::new();
        let none = |_: &Entry, _: &Born| 0;
        let w = crate::kitty::Window {
            socket: "unix:/tmp/kitty-1".into(),
            id: 4,
            tty: None,
        };
        v.push(Entry {
            prompt: Some("fix the login".into()),
            from_lama: true,
            socket: Some(w.socket.clone()),
            window: Some(4),
            ..fresh(Provider::Claude, t(0))
        });
        let mut p = proc(4, 1, Some("c1"));
        p.provider = Provider::Claude;
        apply(&mut v, &[p], &[], t(2), &none);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].session.as_deref(), Some("c1"));
        // Closed on the trust question: no transcript was ever written.
        close_out(&mut v, &[], t(10));
        assert_eq!(v[0].closed_as, Some(Closed::Unstarted));
        assert_eq!(v[0].prompt.as_deref(), Some("fix the login"));
        // A process nobody started from lama, gone before writing anything,
        // has nothing to come back to.
        apply(&mut v, &[proc(9, 20, None)], &[], t(21), &none);
        close_out(&mut v, &[], t(30));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn model_flags_carry_over() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(model_args(&v("codex -m gpt-5 resume x")), v("-m gpt-5"));
        assert_eq!(model_args(&v("claude --model=opus hi")), v("--model opus"));
        assert!(model_args(&v("claude hi")).is_empty());
    }

    fn fresh(provider: Provider, now: DateTime<Utc>) -> Entry {
        Entry::blank(provider, PathBuf::from("/p"), now)
    }
}
