//! Running agent processes: `/proc` on Linux, the kernel's process table
//! (through `sysinfo`) on macOS.
//!
//! Two uses: the universe keeps a session on the map while its agent process
//! is open in a terminal, and the console finds the terminal window a prompt
//! should be typed into. Elsewhere there is no process table to read and both
//! fall back gracefully (nothing found).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::provider::Provider;

/// One interactive Claude Code or Codex process.
#[derive(Debug, Clone)]
pub struct AgentProc {
    pub pid: u32,
    pub provider: Provider,
    pub cwd: PathBuf,
    /// The session its command line names (`codex resume <id>`), if any.
    pub session: Option<String>,
}

/// Every interactive agent process running now.
pub fn agents() -> Vec<AgentProc> {
    let mut out = Vec::new();
    for (pid, argv) in os::processes() {
        let Some(provider) = agent_of(&argv) else {
            continue;
        };
        if let Some(cwd) = os::cwd(pid) {
            let session = session_of(&argv);
            out.push(AgentProc {
                pid,
                provider,
                cwd,
                session,
            });
        }
    }
    // Codex's node wrapper either replaces itself with the native binary or
    // (on older Node) runs it as a child; a child repeats its wrapper's
    // session, so only the wrapper counts.
    let pids: std::collections::HashSet<u32> = out.iter().map(|a| a.pid).collect();
    out.retain(|a| {
        a.provider != Provider::Codex || !os::parent(a.pid).is_some_and(|p| pids.contains(&p))
    });
    out
}

/// [`agents`] counted per provider and working directory.
pub fn counts() -> HashMap<(Provider, PathBuf), usize> {
    let mut out = HashMap::new();
    for a in agents() {
        *out.entry((a.provider, a.cwd)).or_insert(0) += 1;
    }
    out
}

/// A process's environment.
pub fn env(pid: u32) -> HashMap<String, String> {
    os::env(pid)
        .into_iter()
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

/// When a process started.
pub fn start_time(pid: u32) -> Option<std::time::SystemTime> {
    os::start_time(pid)
}

/// The terminal device a process reads from (`/dev/pts/7`, `/dev/ttys003`).
pub fn tty(pid: u32) -> Option<String> {
    os::tty(pid)
}

/// A process's command line.
pub fn argv(pid: u32) -> Vec<String> {
    os::argv(pid)
}

/// Whether process `pid` is running.
pub fn alive(pid: u32) -> bool {
    os::alive(pid)
}

/// Every running process's command line, its arguments joined by spaces.
pub fn command_lines() -> Vec<String> {
    os::processes()
        .into_iter()
        .map(|(_, argv)| argv.join(" "))
        .collect()
}

/// Arguments split at NUL, as `/proc/<pid>/cmdline` and `environ` hold them.
#[cfg(target_os = "linux")]
fn nul_split(raw: &[u8]) -> Vec<String> {
    raw.split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
mod os {
    use std::path::PathBuf;

    /// Every process as `(pid, argv)`.
    pub fn processes() -> Vec<(u32, Vec<String>)> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let pid = e.file_name().to_str()?.parse::<u32>().ok()?;
                let raw = std::fs::read(e.path().join("cmdline")).ok()?;
                Some((pid, super::nul_split(&raw)))
            })
            .collect()
    }

    pub fn argv(pid: u32) -> Vec<String> {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| super::nul_split(&raw))
            .unwrap_or_default()
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// `KEY=value` strings.
    pub fn env(pid: u32) -> Vec<String> {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|raw| super::nul_split(&raw))
            .unwrap_or_default()
    }

    /// A process's parent, from `/proc/<pid>/stat`.
    pub fn parent(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 4, counted after the parenthesised command name, which may
        // itself contain spaces.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()
    }

    /// From `/proc/<pid>/stat` and the boot time.
    pub fn start_time(pid: u32) -> Option<std::time::SystemTime> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field 22 (starttime, in clock ticks since boot), counted after the
        // parenthesised command name, which may itself contain spaces.
        let after = stat.rsplit_once(')')?.1;
        let ticks: u64 = after.split_whitespace().nth(19)?.parse().ok()?;
        let boot: u64 = std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok())?;
        // Linux reports in USER_HZ, 100 on every mainstream configuration.
        let secs = boot as f64 + ticks as f64 / 100.0;
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(secs))
    }

    pub fn tty(pid: u32) -> Option<String> {
        std::fs::read_link(format!("/proc/{pid}/fd/0"))
            .ok()
            .map(|p| p.display().to_string())
            .filter(|p| p.starts_with("/dev/pts/"))
    }

    pub fn alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::path::PathBuf;

    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

    /// The process table, refreshed for `pids` (all with `None`) with just
    /// what `kind` asks for.
    fn table(pids: Option<&[Pid]>, kind: ProcessRefreshKind) -> System {
        let mut sys = System::new();
        let which = pids.map_or(ProcessesToUpdate::All, ProcessesToUpdate::Some);
        sys.refresh_processes_specifics(which, true, kind);
        sys
    }

    fn one(pid: u32, kind: ProcessRefreshKind) -> System {
        table(Some(&[Pid::from_u32(pid)]), kind)
    }

    fn strings(v: &[std::ffi::OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    /// Every process as `(pid, argv)`. Another user's processes come with
    /// no arguments, which no agent check matches.
    pub fn processes() -> Vec<(u32, Vec<String>)> {
        let sys = table(
            None,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        sys.processes()
            .iter()
            .map(|(pid, p)| (pid.as_u32(), strings(p.cmd())))
            .collect()
    }

    pub fn argv(pid: u32) -> Vec<String> {
        let sys = one(
            pid,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        sys.process(Pid::from_u32(pid))
            .map(|p| strings(p.cmd()))
            .unwrap_or_default()
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        let sys = one(
            pid,
            ProcessRefreshKind::nothing().with_cwd(UpdateKind::Always),
        );
        sys.process(Pid::from_u32(pid))?.cwd().map(PathBuf::from)
    }

    /// `KEY=value` strings.
    pub fn env(pid: u32) -> Vec<String> {
        let sys = one(
            pid,
            ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
        );
        sys.process(Pid::from_u32(pid))
            .map(|p| strings(p.environ()))
            .unwrap_or_default()
    }

    pub fn parent(pid: u32) -> Option<u32> {
        let sys = one(pid, ProcessRefreshKind::nothing());
        sys.process(Pid::from_u32(pid))?.parent().map(Pid::as_u32)
    }

    pub fn start_time(pid: u32) -> Option<std::time::SystemTime> {
        let sys = one(pid, ProcessRefreshKind::nothing());
        let secs = sys.process(Pid::from_u32(pid))?.start_time();
        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
    }

    /// From `ps`, which names a process's controlling terminal (`ttys003`).
    pub fn tty(pid: u32) -> Option<String> {
        let out = std::process::Command::new("ps")
            .args(["-o", "tty=", "-p", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
        name.starts_with("ttys").then(|| format!("/dev/{name}"))
    }

    pub fn alive(pid: u32) -> bool {
        one(pid, ProcessRefreshKind::nothing())
            .process(Pid::from_u32(pid))
            .is_some()
    }
}

/// No process table to read: nothing is found, and nothing breaks.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod os {
    use std::path::PathBuf;

    pub fn processes() -> Vec<(u32, Vec<String>)> {
        Vec::new()
    }
    pub fn argv(_: u32) -> Vec<String> {
        Vec::new()
    }
    pub fn cwd(_: u32) -> Option<PathBuf> {
        None
    }
    pub fn env(_: u32) -> Vec<String> {
        Vec::new()
    }
    pub fn parent(_: u32) -> Option<u32> {
        None
    }
    pub fn start_time(_: u32) -> Option<std::time::SystemTime> {
        None
    }
    pub fn tty(_: u32) -> Option<String> {
        None
    }
    pub fn alive(_: u32) -> bool {
        false
    }
}

/// The session Claude Code process `pid` runs, from Claude's own registry
/// (`~/.claude/sessions/<pid>.json`).
pub fn claude_session(pid: u32) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let path = Path::new(&home)
        .join(".claude")
        .join("sessions")
        .join(format!("{pid}.json"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("sessionId")?.as_str().map(String::from)
}

/// The Claude Code process running session `id`, from Claude's own
/// registry (`~/.claude/sessions/<pid>.json`) — exact, not inferred.
pub fn claude_pid(session_id: &str) -> Option<u32> {
    let home = std::env::var_os("HOME")?;
    let dir = Path::new(&home).join(".claude").join("sessions");
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if v.get("sessionId").and_then(|s| s.as_str()) == Some(session_id)
            && let Some(pid) = v.get("pid").and_then(|p| p.as_u64())
        {
            let pid = pid as u32;
            // The registry can outlive its process.
            if alive(pid) {
                return Some(pid);
            }
        }
    }
    None
}

/// Whether a command line is an interactive Claude Code or Codex session —
/// not a helper (Claude's browser bridge, Codex's app server) and not a
/// one-shot headless run.
pub fn agent_of(argv: &[String]) -> Option<Provider> {
    let base = |s: &str| s.rsplit('/').next().unwrap_or(s).to_string();
    let first = base(argv.first()?);
    let rest = &argv[1..];
    let headless = rest.iter().any(|a| {
        matches!(
            a.as_str(),
            "-p" | "--print" | "exec" | "app-server" | "mcp" | "--chrome-native-host"
        )
    });
    if headless {
        return None;
    }
    match first.as_str() {
        "claude" => Some(Provider::Claude),
        // Codex installs as a node wrapper around a native binary; which of
        // the two stands for the session is settled in `agents`.
        "node" if rest.first().is_some_and(|a| base(a) == "codex") => Some(Provider::Codex),
        "codex" => Some(Provider::Codex),
        _ => None,
    }
}

/// The session an agent's command line resumes: the id after Codex's
/// `resume` subcommand or Claude's `--resume`/`-r`. `codex resume --last` and
/// a bare `resume` (the picker) name none.
pub fn session_of(argv: &[String]) -> Option<String> {
    let at = argv
        .iter()
        .position(|a| matches!(a.as_str(), "resume" | "--resume" | "-r"))?;
    argv.get(at + 1).filter(|id| !id.starts_with('-')).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn this_process_is_seen_running() {
        let me = std::process::id();
        assert!(alive(me));
        assert!(!argv(me).is_empty());
        assert!(command_lines().iter().any(|c| !c.is_empty()));
        assert_eq!(os::cwd(me), std::env::current_dir().ok());
        assert!(
            env(me).contains_key("PATH"),
            "a process's environment is read"
        );
        let started = start_time(me).expect("a start time");
        let age = std::time::SystemTime::now()
            .duration_since(started)
            .unwrap_or_default();
        assert!(age < std::time::Duration::from_secs(3600), "{age:?}");
    }

    #[test]
    fn a_resumed_session_is_read_off_the_command_line() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            session_of(&v("codex resume 01a1-bc3")).as_deref(),
            Some("01a1-bc3")
        );
        assert_eq!(
            session_of(&v("codex -c model=\"x\" resume 01a1-bc3")).as_deref(),
            Some("01a1-bc3")
        );
        assert_eq!(
            session_of(&v("claude --resume abc --model opus")).as_deref(),
            Some("abc")
        );
        assert_eq!(session_of(&v("codex resume --last")), None);
        assert_eq!(session_of(&v("codex resume")), None);
        assert_eq!(session_of(&v("codex")), None);
    }

    #[test]
    fn agent_processes_are_told_from_helpers() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(agent_of(&v("claude")), Some(Provider::Claude));
        assert_eq!(
            agent_of(&v("/home/u/.local/bin/claude --resume abc")),
            Some(Provider::Claude)
        );
        assert_eq!(
            agent_of(&v("/home/u/.local/bin/claude --chrome-native-host")),
            None
        );
        assert_eq!(agent_of(&v("claude -p hi")), None);
        assert_eq!(
            agent_of(&v("node /home/u/.nvm/bin/codex")),
            Some(Provider::Codex)
        );
        assert_eq!(
            agent_of(&v(
                "/x/node_modules/@openai/codex-linux-x64/vendor/bin/codex"
            )),
            Some(Provider::Codex),
            "the native binary the node wrapper execs into"
        );
        assert_eq!(
            agent_of(&v("/home/u/.codex/bin/codex app-server --listen x")),
            None
        );
        assert_eq!(agent_of(&v("/bin/bash -c source ~/.claude/x.sh")), None);
    }
}
