//! Tasks: you hand lama something to get done — a Sentry issue, a
//! ticket, a Teams thread, a sentence — and a conductor agent plans a team,
//! lama launches it across your projects, and the agents talk over the lama
//! bus while you watch it on the canvas.
//!
//! A task lives in one directory (`~/.local/share/lama/tasks/<id>/`)
//! that is the whole interface between its parts:
//!
//! - `task.json` — what you asked for, and the conductor's session.
//! - `plan.json` — the team the conductor proposes; lama marks it approved
//!   when you approve it, and launches nothing before that.
//! - `messages.jsonl` — every message on the bus, one per line, appended by
//!   the `lama bus` MCP server each agent runs.
//! - `requests.jsonl` — the conductor asking lama to start a planned agent
//!   later (a reviewer once there is code to review).
//! - `goals.json` — what "done" means, ticked off by the conductor.
//! - `agents.json` — what lama launched: name → session, window, worktree.
//!
//! The bus only ever writes inside this directory; launching, typing into
//! windows and creating worktrees stay with lama.

pub mod bus;
pub mod launch;
pub mod prompts;
pub mod run;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::provider::Provider;

/// The conductor's name on the bus.
pub const CONDUCTOR: &str = "conductor";

/// What you handed the conductor, and who is conducting.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Task {
    pub id: String,
    /// Your words, link or pasted text, as given.
    pub source: String,
    pub created: chrono::DateTime<chrono::Utc>,
    /// The conductor's session id: assigned at launch for Claude; for
    /// Codex, which picks its own, empty until its session appears.
    pub conductor_session: String,
    /// Who conducts. Tasks from before Codex could conduct are Claude's.
    #[serde(default = "claude")]
    pub conductor_provider: Provider,
}

fn claude() -> Provider {
    Provider::Claude
}

/// One agent the conductor wants on the team.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedAgent {
    /// Its name on the bus and its card: lowercase letters, digits and `-`.
    pub name: String,
    /// The project it works in: one of the project directories lama knows.
    pub project: PathBuf,
    pub provider: Provider,
    /// Model id passed to the agent CLI (`--model` / `-m`); empty for the
    /// CLI's default.
    #[serde(default)]
    pub model: String,
    /// What it is for, in a few words ("API fix", "review both fixes").
    pub role: String,
    /// Its first prompt.
    pub prompt: String,
    /// Start as soon as the plan is approved, or when the conductor asks.
    #[serde(default)]
    pub start: Start,
    /// Whether it changes code. Writers get their own git worktree and
    /// branch, so they never touch your checkout.
    #[serde(default)]
    pub writes: bool,
    /// Why the conductor chose this agent and model, for its card.
    #[serde(default)]
    pub why: String,
    /// The owner that asked for it (a reviewer, a second opinion); `None`
    /// for an owner the conductor dispatched.
    #[serde(default)]
    pub parent: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Start {
    #[default]
    Now,
    Later,
}

/// The team, as proposed; lama sets `approved` when you approve it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Plan {
    /// Bumped by each proposal; an approval is for one version.
    pub version: u32,
    pub summary: String,
    pub agents: Vec<PlannedAgent>,
    #[serde(default)]
    pub approved: bool,
}

/// What a message is, for its colour and glyph.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Assign,
    Contract,
    Question,
    Answer,
    Blocker,
    #[default]
    Report,
}

impl Kind {
    pub fn parse(s: &str) -> Kind {
        match s {
            "assign" => Kind::Assign,
            "contract" => Kind::Contract,
            "question" => Kind::Question,
            "answer" => Kind::Answer,
            "blocker" => Kind::Blocker,
            _ => Kind::Report,
        }
    }
}

/// One message on the bus.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub at: chrono::DateTime<chrono::Utc>,
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub kind: Kind,
    pub text: String,
    /// An owner's report saying its part of the task is finished.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// The conductor asking lama to start a planned agent now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub at: chrono::DateTime<chrono::Utc>,
    pub start: String,
    /// A peer an owner asks for (a reviewer): lama adds it to the team and
    /// launches it in that owner's project.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<PlannedAgent>,
}

/// One thing that has to be true for the task to be done.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Goal {
    pub text: String,
    #[serde(default)]
    pub done: bool,
}

/// What lama launched for one planned agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Launched {
    /// Its session id, once known (Claude: assigned at launch).
    #[serde(default)]
    pub session: Option<String>,
    /// The kitty window (or tmux pane) it runs in. Window ids are only
    /// unique within one kitty, so it means nothing without `socket`.
    #[serde(default)]
    pub window: Option<u32>,
    /// The remote-control socket of the kitty that window is in, or
    /// `tmux:<server socket>`.
    #[serde(default)]
    pub socket: Option<String>,
    /// Where it works: its worktree for writers, else the project.
    pub cwd: PathBuf,
    /// Its branch, for writers.
    #[serde(default)]
    pub branch: Option<String>,
    pub at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Where tasks live: `$XDG_DATA_HOME/lama/tasks`, else
/// `~/.local/share/lama/tasks`.
pub fn root() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(base.join("lama").join("tasks"))
}

/// Every git repository you have run a Claude Code or Codex session in,
/// read from their session history: where a task's agents may work, even
/// when that project isn't on the canvas right now. Your home directory and
/// `/` are not projects.
pub fn history_projects() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut cwds: Vec<PathBuf> = Vec::new();
    if let Some(h) = &home {
        // Claude: one directory per project; any transcript names its cwd.
        if let Ok(dirs) = std::fs::read_dir(h.join(".claude/projects")) {
            for d in dirs.filter_map(Result::ok) {
                let newest = std::fs::read_dir(d.path())
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter(|f| f.path().extension().is_some_and(|e| e == "jsonl"))
                    .max_by_key(|f| f.metadata().and_then(|m| m.modified()).ok());
                if let Some(f) = newest
                    && let Some(cwd) = first_cwd(&f.path(), |v| v.get("cwd"))
                {
                    cwds.push(cwd);
                }
            }
        }
        // Codex: each rollout starts with its session_meta.
        let mut rollouts = Vec::new();
        walk(&h.join(".codex/sessions"), 3, &mut rollouts);
        rollouts.sort();
        for f in rollouts.iter().rev().take(300) {
            if let Some(cwd) = first_cwd(f, |v| v.get("payload").and_then(|p| p.get("cwd"))) {
                cwds.push(cwd);
            }
        }
    }
    let tasks = root();
    let mut out: Vec<PathBuf> = cwds
        .into_iter()
        .map(|c| crate::provider::project_dir(&c).to_path_buf())
        .filter(|c| c.parent().is_some() && Some(c) != home.as_ref())
        .filter(|c| tasks.as_ref().is_none_or(|t| !c.starts_with(t)))
        .filter(|c| c.join(".git").exists())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The cwd recorded near the top of a session file.
fn first_cwd(
    path: &Path,
    pick: impl Fn(&serde_json::Value) -> Option<&serde_json::Value>,
) -> Option<PathBuf> {
    let f = std::fs::File::open(path).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .take(50)
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(&l).ok())
        .find_map(|v| pick(&v).and_then(|c| c.as_str()).map(PathBuf::from))
}

/// Files `depth` directories below `dir`.
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.filter_map(Result::ok) {
        let p = e.path();
        if depth == 0 {
            if p.is_file() {
                out.push(p);
            }
        } else if p.is_dir() {
            walk(&p, depth - 1, out);
        }
    }
}

/// A bus name: lowercase letters, digits and `-`, 1–32 long.
pub fn valid_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// One task's directory.
#[derive(Debug, Clone)]
pub struct Store {
    pub dir: PathBuf,
}

impl Store {
    pub fn open(id: &str) -> Option<Store> {
        if !valid_name(id) {
            return None;
        }
        Some(Store {
            dir: root()?.join(id),
        })
    }

    pub fn at(dir: PathBuf) -> Store {
        Store { dir }
    }

    pub fn id(&self) -> String {
        self.dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, name: &str) -> Option<T> {
        serde_json::from_slice(&std::fs::read(self.path(name)).ok()?).ok()
    }

    /// Whole-file writes go through a temp file and a rename, so a reader
    /// never sees half a plan.
    fn write_json<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let tmp = self.path(&format!(".{name}.tmp"));
        std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
        std::fs::rename(tmp, self.path(name))
    }

    /// One JSON line, appended in a single write so concurrent bus servers
    /// don't interleave.
    fn append<T: Serialize>(&self, name: &str, value: &T) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path(name))?
            .write_all(&line)
    }

    pub fn task(&self) -> Option<Task> {
        self.read_json("task.json")
    }
    pub fn set_task(&self, m: &Task) -> std::io::Result<()> {
        self.write_json("task.json", m)
    }
    pub fn plan(&self) -> Option<Plan> {
        self.read_json("plan.json")
    }
    pub fn set_plan(&self, p: &Plan) -> std::io::Result<()> {
        self.write_json("plan.json", p)
    }
    pub fn goals(&self) -> Vec<Goal> {
        self.read_json("goals.json").unwrap_or_default()
    }
    pub fn set_goals(&self, g: &[Goal]) -> std::io::Result<()> {
        self.write_json("goals.json", &g)
    }
    pub fn launched(&self) -> BTreeMap<String, Launched> {
        self.read_json("agents.json").unwrap_or_default()
    }
    pub fn set_launched(&self, l: &BTreeMap<String, Launched>) -> std::io::Result<()> {
        self.write_json("agents.json", l)
    }
    pub fn send(&self, m: &Message) -> std::io::Result<()> {
        self.append("messages.jsonl", m)
    }
    pub fn request(&self, r: &Request) -> std::io::Result<()> {
        self.append("requests.jsonl", r)
    }
    pub fn messages(&self) -> Vec<Message> {
        read_lines(&self.path("messages.jsonl"))
    }
    pub fn requests(&self) -> Vec<Request> {
        read_lines(&self.path("requests.jsonl"))
    }
}

fn read_lines<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    let Ok(f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

/// Reads a JSONL file from where it left off, holding back a half-written
/// last line.
#[derive(Debug, Default)]
pub struct Tail {
    offset: u64,
    partial: String,
}

impl Tail {
    pub fn read<T: for<'de> Deserialize<'de>>(&mut self, path: &Path) -> Vec<T> {
        let Ok(mut f) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            *self = Tail::default();
        }
        if f.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut text = String::new();
        if f.read_to_string(&mut text).is_err() {
            return Vec::new();
        }
        self.offset += text.len() as u64;
        let text = std::mem::take(&mut self.partial) + &text;
        let mut out = Vec::new();
        let mut rest = text.as_str();
        while let Some(nl) = rest.find('\n') {
            if let Ok(v) = serde_json::from_str(&rest[..nl]) {
                out.push(v);
            }
            rest = &rest[nl + 1..];
        }
        self.partial = rest.to_string();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> Store {
        let dir = std::env::temp_dir().join(format!("lama-task-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Store::at(dir)
    }

    #[test]
    fn messages_append_and_tail() {
        let s = temp("tail");
        let m = |t: &str| Message {
            at: chrono::Utc::now(),
            from: "a".into(),
            to: "b".into(),
            kind: Kind::Question,
            text: t.into(),
            done: false,
        };
        let mut tail = Tail::default();
        s.send(&m("one")).unwrap();
        let got: Vec<Message> = tail.read(&s.dir.join("messages.jsonl"));
        assert_eq!(got.len(), 1);
        s.send(&m("two")).unwrap();
        let got: Vec<Message> = tail.read(&s.dir.join("messages.jsonl"));
        assert_eq!(
            got.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["two"]
        );
        assert_eq!(s.messages().len(), 2);
    }

    #[test]
    fn plan_round_trips_with_defaults() {
        let s = temp("plan");
        let json = r#"{"version":1,"summary":"fix","agents":[{"name":"api","project":"/p","provider":"claude","role":"r","prompt":"go"}]}"#;
        std::fs::create_dir_all(&s.dir).unwrap();
        std::fs::write(s.dir.join("plan.json"), json).unwrap();
        let p = s.plan().unwrap();
        assert_eq!(p.agents[0].start, Start::Now);
        assert!(!p.approved && !p.agents[0].writes);
    }

    #[test]
    fn names_are_plain() {
        assert!(valid_name("api-forge") && valid_name("qa2"));
        assert!(!valid_name("") && !valid_name("../x") && !valid_name("Api") && !valid_name("a b"));
    }
}
