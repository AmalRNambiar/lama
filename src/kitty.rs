//! The bridge to the terminal: find the window a session runs in, type into
//! it, bring it to the front.
//!
//! This is the one place lama writes anywhere. It types a prompt into the
//! agent's own terminal exactly as if you had typed it there, through kitty's
//! remote control — so the agent's approvals and UI keep working, and the
//! transcript lama reads shows the result like any other turn. It needs
//! remote control enabled in kitty (see [`SETUP`]). A session in a tmux pane
//! is reached through tmux instead ([`crate::tmux`]), in any terminal.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::procs;
use crate::provider::Provider;

/// The kitty.conf lines that let lama reach a window.
pub const SETUP: [&str; 2] = [
    "allow_remote_control socket-only",
    "listen_on unix:/tmp/kitty-{kitty_pid}",
];

/// A kitty window or tmux pane a session runs in.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// `unix:/tmp/kitty-1234`, the remote-control socket of its kitty, or
    /// `tmux:/tmp/tmux-1000/default`, the server socket of its tmux.
    pub socket: String,
    /// The window's id within that kitty instance, or the pane's number.
    pub id: u32,
    /// The terminal device, for display (`/dev/pts/7`).
    pub tty: Option<String>,
}

impl Window {
    /// "kitty #3" or "tmux %3", for the UI.
    pub fn label(&self) -> String {
        if crate::tmux::server(self).is_some() {
            format!("tmux %{}", self.id)
        } else {
            format!("kitty #{}", self.id)
        }
    }

    /// Whether the kitty or tmux server it belongs to still runs. Ids are
    /// counted afresh by each one, so a window outliving its server is no
    /// address at all.
    pub fn alive(&self) -> bool {
        let path = crate::tmux::server(self).or_else(|| self.socket.strip_prefix("unix:"));
        path.is_some_and(|p| Path::new(p).exists())
    }
}

/// Where a session's prompts can go — or why they can't.
#[derive(Debug, Clone, PartialEq)]
pub enum Reach {
    Ready(Window),
    /// The session's process runs in kitty, but kitty's remote control is
    /// off (no socket to talk to).
    RemoteOff,
    /// No running process for this session (it was closed).
    NoProcess,
    /// The process runs neither inside kitty nor in a tmux pane.
    NotKitty,
    /// Several windows run this provider in the project and none of their
    /// screens shows this session.
    Ambiguous(usize),
}

/// Find the window session `id` runs in.
///
/// Claude sessions map exactly through Claude's own process registry. Codex
/// keeps no such record: a Codex session is the window lama's own record
/// ([`crate::registry`]) ties to it, or the one started as
/// `codex resume <id>` for it, else its project's only other Codex window,
/// or — with several — the one whose screen shows the session's latest
/// reply (`hint`). A window resuming another session is never it.
pub fn locate(
    provider: Provider,
    session_id: &str,
    cwd: Option<&Path>,
    hints: &[String],
    born: Option<std::time::SystemTime>,
) -> Reach {
    let pid = match provider {
        Provider::Claude => match procs::claude_pid(session_id) {
            Some(pid) => pid,
            None => return Reach::NoProcess,
        },
        Provider::Codex => {
            if let Some(pid) = crate::registry::pid_of(provider, session_id) {
                return match window_of(pid) {
                    Ok(w) => Reach::Ready(w),
                    Err(r) => r,
                };
            }
            let elsewhere = crate::registry::claimed_elsewhere(provider, session_id);
            let here: Vec<procs::AgentProc> = procs::agents()
                .into_iter()
                .filter(|a| a.provider == Provider::Codex && cwd.is_some_and(|c| a.cwd == c))
                .filter(|a| !elsewhere.contains(&a.pid))
                .collect();
            // A window started as `codex resume <id>` runs that session: this
            // one if it names it, else never this one.
            let candidates: Vec<u32> = match here
                .iter()
                .find(|a| a.session.as_deref() == Some(session_id))
            {
                Some(a) => vec![a.pid],
                None => here
                    .iter()
                    .filter(|a| a.session.is_none())
                    .map(|a| a.pid)
                    .collect(),
            };
            match candidates.as_slice() {
                [] => return Reach::NoProcess,
                [only] => *only,
                many => return pick_codex(many, hints, born),
            }
        }
    };
    match window_of(pid) {
        Ok(w) => Reach::Ready(w),
        Err(r) => r,
    }
}

/// Several Codex windows in one project: which one is this session?
///
/// First by what the windows show — each window's whole scrollback is
/// checked for the session's recent prompts and replies, and the one showing
/// the most wins. Failing that, by timing: a session starts moments after the
/// process that runs it, so the process started closest before the session
/// (within ten minutes, and clearly closer than the next) is it. Neither
/// settling it, nothing is guessed.
fn pick_codex(pids: &[u32], hints: &[String], born: Option<std::time::SystemTime>) -> Reach {
    let windows: Vec<(u32, Window)> = pids
        .iter()
        .filter_map(|&p| window_of(p).ok().map(|w| (p, w)))
        .collect();
    if windows.is_empty() {
        return Reach::RemoteOff;
    }
    let scores: Vec<usize> = windows.iter().map(|(_, w)| score(w, hints)).collect();
    let best = scores.iter().copied().max().unwrap_or(0);
    if best > 0 && scores.iter().filter(|&&s| s == best).count() == 1 {
        let k = scores.iter().position(|&s| s == best).unwrap_or(0);
        return Reach::Ready(windows[k].1.clone());
    }
    if let Some(born) = born {
        let mut gaps: Vec<(std::time::Duration, &Window)> = windows
            .iter()
            .filter_map(|(p, w)| {
                let started = procs::start_time(*p)?;
                // A few seconds of slack for clocks and file timestamps.
                let gap = born
                    .duration_since(started - std::time::Duration::from_secs(5))
                    .ok()?;
                Some((gap, w))
            })
            .collect();
        gaps.sort_by_key(|(g, _)| *g);
        let clear = match gaps.as_slice() {
            [(g, _)] => *g,
            [(g, _), (next, _), ..] if *next > *g + std::time::Duration::from_secs(30) => *g,
            _ => std::time::Duration::MAX,
        };
        if clear < std::time::Duration::from_secs(600) {
            return Reach::Ready(gaps[0].1.clone());
        }
    }
    Reach::Ambiguous(pids.len())
}

/// Only letters and digits, lowercased: what survives an agent UI's line
/// wrapping, borders and indentation intact enough to compare.
fn bare(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// How many of `hints` a window's scrollback shows.
pub(crate) fn score(w: &Window, hints: &[String]) -> usize {
    let read = match crate::tmux::server(w) {
        Some(_) => crate::tmux::capture(w, true),
        None => kitten(w, &["get-text", "--extent", "all"]),
    };
    let Ok(out) = read_out(read) else {
        return 0;
    };
    let text = bare(&String::from_utf8_lossy(&out.stdout));
    hints
        .iter()
        .map(|h| bare(h).chars().take(40).collect::<String>())
        .filter(|h| h.len() >= 16 && text.contains(h.as_str()))
        .count()
}

/// A command's output, its errors dropped.
fn read_out(mut c: Command) -> std::io::Result<std::process::Output> {
    c.stderr(Stdio::null()).output()
}

/// The kitty window or tmux pane process `pid` runs in, if any.
pub(crate) fn window_of_pid(pid: u32) -> Option<Window> {
    window_of(pid).ok()
}

/// The window a process runs in, from its environment: its tmux pane when
/// it runs in one (a pane inside kitty is the narrower address), else its
/// kitty window.
fn window_of(pid: u32) -> Result<Window, Reach> {
    let env = procs::env(pid);
    if let Some(w) = crate::tmux::pane_of(&env, procs::tty(pid).as_deref()) {
        return Ok(w);
    }
    let id = env
        .get("KITTY_WINDOW_ID")
        .and_then(|v| v.parse().ok())
        .ok_or(Reach::NotKitty)?;
    // A kitty started with `listen_on` hands its socket to its children; one
    // configured with `{kitty_pid}` but started earlier is found by its pid.
    let socket = env
        .get("KITTY_LISTEN_ON")
        .cloned()
        .or_else(|| {
            let pid = env.get("KITTY_PID")?;
            let path = format!("/tmp/kitty-{pid}");
            Path::new(&path).exists().then(|| format!("unix:{path}"))
        })
        .ok_or(Reach::RemoteOff)?;
    Ok(Window {
        socket,
        id,
        tty: procs::tty(pid),
    })
}

/// The `kitten` command: on `PATH`, else inside the kitty app bundle, where
/// a macOS install keeps it without putting it on `PATH`.
fn kitten_bin() -> &'static str {
    static BIN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BIN.get_or_init(|| {
        let on_path = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("kitten").is_file()));
        let bundled = "/Applications/kitty.app/Contents/MacOS/kitten";
        if !on_path && Path::new(bundled).is_file() {
            bundled.to_string()
        } else {
            "kitten".to_string()
        }
    })
}

/// `kitten @ --to <socket> <cmd> --match id:<window> <rest…>` — the match
/// goes before the rest, which may be positional (key names).
fn kitten(w: &Window, args: &[&str]) -> Command {
    let mut c = Command::new(kitten_bin());
    c.arg("@").arg("--to").arg(&w.socket);
    if let Some((cmd, rest)) = args.split_first() {
        c.arg(cmd)
            .arg("--match")
            .arg(format!("id:{}", w.id))
            .args(rest);
    }
    c
}

/// How long to wait between typing a prompt and pressing Enter. Agent UIs
/// tell a paste from typing by how fast characters arrive: Codex treats an
/// Enter inside a fast burst as a newline in the prompt, not a submit. A real
/// key press after a pause reads as you pressing Enter.
const SUBMIT_AFTER: std::time::Duration = std::time::Duration::from_millis(400);

/// A key lama presses in an agent's window.
#[derive(Clone, Copy)]
enum Press {
    Enter,
    Escape,
}

/// The command that presses `key` in the window.
fn press(w: &Window, key: Press) -> Command {
    match (crate::tmux::server(w), key) {
        (Some(_), Press::Enter) => crate::tmux::keys(w, &["Enter"]),
        (Some(_), Press::Escape) => crate::tmux::keys(w, &["Escape"]),
        (None, Press::Enter) => kitten(w, &["send-key", "enter"]),
        (None, Press::Escape) => kitten(w, &["send-key", "escape"]),
    }
}

/// A command's failure as its own words.
fn checked(out: std::io::Result<std::process::Output>, tool: &str) -> Result<(), String> {
    let out = out.map_err(|e| format!("can't run {tool}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn tool(w: &Window) -> &'static str {
    if crate::tmux::server(w).is_some() {
        "tmux"
    } else {
        "kitten"
    }
}

/// Type `text` into the window, then press Enter.
///
/// The text goes in at once; Enter follows on its own thread after
/// `SUBMIT_AFTER`, so the UI never stalls on the pause. kitty reports no
/// errors for key presses, so only the typing can fail.
pub fn send(w: &Window, text: &str) -> Result<(), String> {
    // One line: a newline would break the prompt in the agent's UI.
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if crate::tmux::server(w).is_some() {
        checked(crate::tmux::text(w, &line).output(), "tmux")?;
        submit_later(w);
        return Ok(());
    }
    let mut child = kitten(w, &["send-text", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("can't run kitten: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(line.as_bytes())
        .map_err(|e| e.to_string())?;
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    submit_later(w);
    Ok(())
}

/// Press Enter after [`SUBMIT_AFTER`], on its own thread.
fn submit_later(w: &Window) {
    let w = w.clone();
    std::thread::spawn(move || {
        std::thread::sleep(SUBMIT_AFTER);
        let _ = press(&w, Press::Enter)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// A kitty that will take remote commands: the one lama itself runs in when
/// it is reachable, else any listening kitty of yours.
pub(crate) fn any_socket() -> Option<String> {
    if let Ok(on) = std::env::var("KITTY_LISTEN_ON") {
        return Some(on);
    }
    if let Ok(pid) = std::env::var("KITTY_PID") {
        let path = format!("/tmp/kitty-{pid}");
        if Path::new(&path).exists() {
            return Some(format!("unix:{path}"));
        }
    }
    let mut socks: Vec<_> = std::fs::read_dir("/tmp")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("kitty-"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    // The most recently started kitty is the likeliest one in use.
    socks.sort_by_key(|(t, _)| std::cmp::Reverse(*t));
    socks
        .into_iter()
        .next()
        .map(|(_, p)| format!("unix:{}", p.display()))
}

/// The command line that runs `provider`'s binary with `args`.
///
/// It runs through your login shell (`-i`), so the agent is found on the
/// PATH your terminal has (nvm-installed Codex lives there), and every
/// argument — a prompt, a session id — travels as one, never spliced into
/// shell text.
pub(crate) fn agent_args(provider: Provider, args: &[&str]) -> Vec<String> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let bin = match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    };
    let mut line = vec![shell, "-ic".into(), "exec \"$@\"".into(), "lama".into()];
    line.push(bin.into());
    line.extend(args.iter().map(|a| a.to_string()));
    line
}

/// The command line that starts a new `provider` session with `prompt`.
fn launch_args(provider: Provider, prompt: &str) -> Vec<String> {
    agent_args(provider, &[prompt])
}

/// The command line that picks session `session_id` back up.
fn resume_args(provider: Provider, session_id: &str) -> Vec<String> {
    match provider {
        Provider::Claude => agent_args(provider, &["--resume", session_id]),
        Provider::Codex => agent_args(provider, &["resume", session_id]),
    }
}

/// Run `line` in `dir` in a new tab titled `title` that does not take
/// focus: a tmux window when lama runs in tmux, else a kitty tab, else a
/// window in your running tmux server. Returns the new window.
pub(crate) fn open_tab(dir: &Path, title: &str, line: Vec<String>) -> Result<Window, String> {
    if let Some(server) = crate::tmux::own_server() {
        return crate::tmux::open_window(&server, dir, title, &line);
    }
    let Some(socket) = any_socket() else {
        return match crate::tmux::any_server() {
            Some(server) => crate::tmux::open_window(&server, dir, title, &line),
            None => Err("no kitty with remote control is listening, and no tmux is running".into()),
        };
    };
    let out = Command::new(kitten_bin())
        .arg("@")
        .arg("--to")
        .arg(&socket)
        .arg("launch")
        .arg("--type=tab")
        .arg("--keep-focus")
        .arg("--cwd")
        .arg(dir)
        .arg("--tab-title")
        .arg(title)
        .arg("--")
        .args(line)
        .output()
        .map_err(|e| format!("can't run kitten: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let id = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .map_err(|_| "kitty didn't report the new window".to_string())?;
    Ok(Window {
        socket,
        id,
        tty: None,
    })
}

/// Start a new `provider` session in `dir` with a first prompt, in a new
/// kitty tab titled `title` that does not take focus. Returns the new
/// window's id.
pub fn launch(dir: &Path, provider: Provider, prompt: &str, title: &str) -> Result<u32, String> {
    launch_window(dir, provider, prompt, title).map(|w| w.id)
}

/// [`launch`], returning the whole window.
pub fn launch_window(
    dir: &Path,
    provider: Provider,
    prompt: &str,
    title: &str,
) -> Result<Window, String> {
    open_tab(dir, title, launch_args(provider, prompt))
}

/// Reopen a closed session: `claude --resume` / `codex resume` in `dir`, in
/// a new tab like [`launch`]'s. Returns the new window.
pub fn resume(
    dir: &Path,
    provider: Provider,
    session_id: &str,
    title: &str,
) -> Result<Window, String> {
    resume_with(dir, provider, session_id, title, &[])
}

/// [`resume`] with `extra` arguments ahead of the resume (`--model x`).
pub fn resume_with(
    dir: &Path,
    provider: Provider,
    session_id: &str,
    title: &str,
    extra: &[String],
) -> Result<Window, String> {
    let mut line = resume_args(provider, session_id);
    let at = line.len() - 2;
    line.splice(at..at, extra.iter().cloned());
    open_tab(dir, title, line)
}

/// Interrupt the agent's current turn: Escape, which both Claude Code and
/// Codex take as "stop what you're doing".
pub fn interrupt(w: &Window) -> Result<(), String> {
    checked(press(w, Press::Escape).output(), tool(w))
}

/// Whether session `session_id` is showing a permission dialog right now,
/// and which. Matching is by process and timing only (no conversation to
/// hand), so an ambiguous Codex window reads as no dialog.
pub fn asking(
    provider: Provider,
    session_id: &str,
    cwd: Option<&Path>,
    born: Option<std::time::SystemTime>,
) -> Option<crate::ask::Ask> {
    match locate(provider, session_id, cwd, &[], born) {
        Reach::Ready(w) => screen(&w).and_then(|s| crate::ask::parse(&s)),
        _ => None,
    }
}

/// What a window shows right now (its screen, without scrollback).
pub fn screen(w: &Window) -> Option<String> {
    let read = match crate::tmux::server(w) {
        Some(_) => crate::tmux::capture(w, false),
        None => kitten(w, &["get-text"]),
    };
    let out = read_out(read).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Answer a permission dialog: press the key that picks `choice`.
///
/// A digit only moves some dialogs' highlight, so if the same dialog is
/// still up afterwards with that choice highlighted, Enter confirms it.
pub fn answer(
    w: &Window,
    ask: &crate::ask::Ask,
    choice: &crate::ask::Choice,
) -> Result<(), String> {
    use crate::ask::Key;
    let sent = match (&choice.key, crate::tmux::server(w)) {
        (Key::Escape, _) => press(w, Press::Escape).output(),
        (Key::Char(c), Some(_)) => crate::tmux::text(w, &c.to_string()).output(),
        (Key::Char(c), None) => kitten(w, &["send-text", &c.to_string()]).output(),
    };
    checked(sent, tool(w))?;
    let w = w.clone();
    let (question, number) = (ask.question.clone(), choice.number);
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(350));
        let still = screen(&w).and_then(|s| crate::ask::parse(&s));
        if let Some(now) = still
            && now.question == question
            && now.selected == Some(number as usize - 1)
        {
            let _ = press(&w, Press::Enter)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    });
    Ok(())
}

/// Bring the window (and its kitty) to the front; a tmux pane becomes the
/// current one.
pub fn focus(w: &Window) -> Result<(), String> {
    if crate::tmux::server(w).is_some() {
        return crate::tmux::focus(w);
    }
    checked(kitten(w, &["focus-window"]).output(), "kitten")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kitten_targets_one_window_on_one_kitty() {
        let w = Window {
            socket: "unix:/tmp/kitty-42".into(),
            id: 7,
            tty: None,
        };
        let c = kitten(&w, &["send-text", "--stdin"]);
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "@",
                "--to",
                "unix:/tmp/kitty-42",
                "send-text",
                "--match",
                "id:7",
                "--stdin"
            ]
        );
        // Enter is a real key press, sent on its own; the match comes before
        // the key name, which is positional.
        let c = kitten(&w, &["send-key", "enter"]);
        let args: Vec<String> = c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "@",
                "--to",
                "unix:/tmp/kitty-42",
                "send-key",
                "--match",
                "id:7",
                "enter"
            ]
        );
    }

    #[test]
    fn a_launch_passes_the_prompt_as_an_argument() {
        let args = launch_args(Provider::Codex, "fix it; rm -rf x");
        assert_eq!(
            &args[1..],
            ["-ic", "exec \"$@\"", "lama", "codex", "fix it; rm -rf x"]
        );
    }

    #[test]
    fn a_resume_passes_the_session_id_as_an_argument() {
        let args = resume_args(Provider::Claude, "abc-123");
        assert_eq!(
            &args[1..],
            [
                "-ic",
                "exec \"$@\"",
                "lama",
                "claude",
                "--resume",
                "abc-123"
            ]
        );
        let args = resume_args(Provider::Codex, "abc-123");
        assert_eq!(&args[4..], ["codex", "resume", "abc-123"]);
    }

    #[test]
    fn matching_ignores_wrapping_and_borders() {
        let screen = bare("│ The MR is 4 commits ahead,\n│   0 behind, so no rebase is needed. │");
        let hint = bare("The MR is 4 commits ahead, 0 behind, so no rebase");
        assert!(screen.contains(&hint));
    }

    #[test]
    fn a_process_outside_kitty_is_not_kitty() {
        // This test process is not a kitty child (or is, under kitty — then
        // it has no socket unless remote control is on). Either way it must
        // never resolve to a window without both id and socket.
        match window_of(std::process::id()) {
            Ok(w) => assert!(!w.socket.is_empty() && w.id > 0),
            Err(r) => assert!(matches!(r, Reach::NotKitty | Reach::RemoteOff)),
        }
    }
}
