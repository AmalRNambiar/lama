//! Which agent CLIs you have, and which one conducts your tasks.
//!
//! lama works with Claude Code, Codex or both. What it offers — the
//! composer's choice, who runs a task's agents, who conducts — follows what
//! is installed, found once per run. The conductor you pick when both are
//! installed is kept in `~/.config/lama/config.json`.

use std::sync::OnceLock;

use crate::provider::Provider;

/// The agent CLIs found on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    pub claude: bool,
    pub codex: bool,
}

impl Installed {
    pub fn has(self, p: Provider) -> bool {
        match p {
            Provider::Claude => self.claude,
            Provider::Codex => self.codex,
        }
    }

    pub fn both(self) -> bool {
        self.claude && self.codex
    }

    /// The installed agents, Claude first.
    pub fn list(self) -> Vec<Provider> {
        [Provider::Claude, Provider::Codex]
            .into_iter()
            .filter(|&p| self.has(p))
            .collect()
    }

    /// `p` if it is installed, else the other agent if that one is.
    pub fn or_other(self, p: Provider) -> Option<Provider> {
        if self.has(p) {
            Some(p)
        } else {
            Some(other(p)).filter(|&o| self.has(o))
        }
    }
}

/// The other agent.
pub fn other(p: Provider) -> Provider {
    match p {
        Provider::Claude => Provider::Codex,
        Provider::Codex => Provider::Claude,
    }
}

/// What this machine has, looked up once: on lama's own `PATH` first, then
/// through your login shell, whose `PATH` is the one agent tabs start with
/// (an nvm-installed Codex lives only there).
pub fn installed() -> Installed {
    static FOUND: OnceLock<Installed> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let mut found = Installed {
            claude: on_path("claude"),
            codex: on_path("codex"),
        };
        if !found.both() {
            let shell = login_shell_lookup();
            found.claude |= shell.claude;
            found.codex |= shell.codex;
        }
        found
    })
}

/// Look the agents up in the background, so the first question asked of
/// [`installed`] doesn't wait on a login shell.
pub fn warm() {
    std::thread::spawn(installed);
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            let f = dir.join(bin);
            std::fs::metadata(&f).is_ok_and(|m| m.is_file() && executable(&m))
        })
    })
}

#[cfg(unix)]
fn executable(m: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    m.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    true
}

fn login_shell_lookup() -> Installed {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let out = std::process::Command::new(shell)
        .args(["-ic", "command -v claude; command -v codex"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();
    match out {
        Ok(o) => parse_lookup(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Installed {
            claude: false,
            codex: false,
        },
    }
}

/// `command -v` output: a path, or an alias or function definition, per
/// agent found. An interactive shell may print other things around it.
fn parse_lookup(out: &str) -> Installed {
    let found = |bin: &str| {
        out.lines().map(str::trim).any(|l| {
            l == bin || l.ends_with(&format!("/{bin}")) || l.starts_with(&format!("alias {bin}="))
        })
    };
    Installed {
        claude: found("claude"),
        codex: found("codex"),
    }
}

/// Who conducts a new task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conductor {
    Use(Provider),
    /// Both agents are installed and you haven't picked one yet.
    Ask,
    /// Neither agent is installed.
    Missing,
}

/// Who conducts, from what is installed and what you picked before. A pick
/// whose agent has since gone is passed over.
pub fn conductor() -> Conductor {
    decide(installed(), saved_conductor())
}

fn decide(have: Installed, saved: Option<Provider>) -> Conductor {
    match (saved.filter(|&p| have.has(p)), have.list().as_slice()) {
        (Some(p), _) => Conductor::Use(p),
        (None, [only]) => Conductor::Use(*only),
        (None, []) => Conductor::Missing,
        (None, _) => Conductor::Ask,
    }
}

fn saved_conductor() -> Option<Provider> {
    Provider::parse(&crate::config::get("conductor")?)
}

/// Keep `p` as the conductor for later tasks.
pub fn save_conductor(p: Provider) -> Result<(), String> {
    crate::config::set("conductor", p.name())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Installed = Installed {
        claude: true,
        codex: true,
    };
    const CLAUDE: Installed = Installed {
        claude: true,
        codex: false,
    };
    const CODEX: Installed = Installed {
        claude: false,
        codex: true,
    };

    #[test]
    fn a_shell_lookup_finds_paths_and_aliases_among_noise() {
        let out = "Welcome back!\n/home/u/.local/bin/claude\nalias codex='npx codex'\n";
        assert_eq!(parse_lookup(out), BOTH);
        assert_eq!(
            parse_lookup("/usr/bin/claude-ish\n"),
            Installed {
                claude: false,
                codex: false
            }
        );
        assert_eq!(parse_lookup("/home/u/.nvm/bin/codex\n"), CODEX);
    }

    #[test]
    fn the_conductor_follows_what_is_installed() {
        assert_eq!(decide(BOTH, None), Conductor::Ask);
        assert_eq!(
            decide(BOTH, Some(Provider::Codex)),
            Conductor::Use(Provider::Codex)
        );
        assert_eq!(decide(CLAUDE, None), Conductor::Use(Provider::Claude));
        // A pick whose agent is gone falls back to the one that's there.
        assert_eq!(
            decide(CODEX, Some(Provider::Claude)),
            Conductor::Use(Provider::Codex)
        );
        let none = Installed {
            claude: false,
            codex: false,
        };
        assert_eq!(decide(none, None), Conductor::Missing);
    }

    #[test]
    fn a_missing_agent_falls_back_to_the_other() {
        assert_eq!(CLAUDE.or_other(Provider::Codex), Some(Provider::Claude));
        assert_eq!(BOTH.or_other(Provider::Codex), Some(Provider::Codex));
        assert_eq!(CLAUDE.list(), [Provider::Claude]);
    }
}
