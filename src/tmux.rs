//! The tmux side of [`crate::kitty`]: the same reach into an agent's
//! terminal, for sessions that run in a tmux pane — whatever terminal tmux
//! itself runs in.
//!
//! A pane is a [`Window`] whose `socket` is `tmux:<server socket>` and whose
//! `id` is the pane's number (`%7` → 7). tmux needs no setup: its server
//! socket takes commands from the user who started it.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::kitty::Window;

/// What marks a [`Window`]'s socket as a tmux server's.
pub const PREFIX: &str = "tmux:";

/// `tmux -S <server socket>`.
fn tmux(socket: &str) -> Command {
    let mut c = Command::new("tmux");
    c.arg("-S").arg(socket);
    c
}

/// The server socket a [`Window`] names, when it is a tmux pane.
pub(crate) fn server(w: &Window) -> Option<&str> {
    w.socket.strip_prefix(PREFIX)
}

fn target(pane: u32) -> String {
    format!("%{pane}")
}

/// `$TMUX` is `<server socket>,<server pid>,<session>`.
fn socket_of(tmux_var: &str) -> Option<&str> {
    tmux_var.split(',').next().filter(|s| !s.is_empty())
}

/// `$TMUX_PANE` is `%<n>`.
fn pane_number(pane: &str) -> Option<u32> {
    pane.trim().strip_prefix('%')?.parse().ok()
}

/// The tmux pane a process runs in, from its environment — if that pane is
/// still there on a terminal `tty`. A pane's variables are inherited by
/// whatever it starts, so they are only believed while the pane they name is
/// the one on the process's own terminal.
pub(crate) fn pane_of(env: &HashMap<String, String>, tty: Option<&str>) -> Option<Window> {
    let socket = socket_of(env.get("TMUX")?)?;
    let id = pane_number(env.get("TMUX_PANE")?)?;
    let out = tmux(socket)
        .args(["display-message", "-p", "-t", &target(id), "#{pane_tty}"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let pane_tty = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tty.is_some_and(|t| t != pane_tty) {
        return None;
    }
    Some(Window {
        socket: format!("{PREFIX}{socket}"),
        id,
        tty: Some(pane_tty),
    })
}

/// The tmux server lama itself runs under, if it runs in a pane.
pub(crate) fn own_server() -> Option<String> {
    std::env::var("TMUX_PANE").ok()?;
    socket_of(&std::env::var("TMUX").ok()?).map(String::from)
}

/// The default server of yours, when one is running.
pub(crate) fn any_server() -> Option<String> {
    let out = Command::new("tmux")
        .args(["display-message", "-p", "#{socket_path}"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !path.is_empty()).then_some(path)
}

/// `send-keys -t %<pane> <args…>`: key names (`Enter`, `Escape`).
pub(crate) fn keys(w: &Window, args: &[&str]) -> Command {
    let mut c = tmux(server(w).unwrap_or_default());
    c.args(["send-keys", "-t", &target(w.id)]).args(args);
    c
}

/// `send-keys -l`: `text` typed literally, never read as key names.
pub(crate) fn text(w: &Window, text: &str) -> Command {
    keys(w, &["-l", "--", text])
}

/// The pane's text: its screen, or with `all` its whole history too, wrapped
/// lines joined.
pub(crate) fn capture(w: &Window, all: bool) -> Command {
    let mut c = tmux(server(w).unwrap_or_default());
    c.args(["capture-pane", "-p", "-t", &target(w.id)]);
    if all {
        c.args(["-J", "-S", "-"]);
    }
    c
}

/// Show the pane: its window and session become current in the client that
/// last used them. tmux can't raise the terminal it runs in.
pub(crate) fn focus(w: &Window) -> Result<(), String> {
    let socket = server(w).unwrap_or_default();
    let t = target(w.id);
    // No attached client is no error: the pane is still made current.
    let _ = tmux(socket)
        .args(["switch-client", "-t", &t])
        .stderr(Stdio::null())
        .status();
    for cmd in ["select-window", "select-pane"] {
        let out = tmux(socket)
            .args([cmd, "-t", &t])
            .output()
            .map_err(|e| format!("can't run tmux: {e}"))?;
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
        }
    }
    Ok(())
}

/// Run `line` in `dir` in a new tmux window named `title`, in the background.
/// `line` is passed as arguments, so tmux runs it without a shell of its own.
pub(crate) fn open_window(
    socket: &str,
    dir: &Path,
    title: &str,
    line: &[String],
) -> Result<Window, String> {
    let out = tmux(socket)
        .args(["new-window", "-d", "-P", "-F", "#{pane_id}", "-c"])
        .arg(dir)
        .args(["-n", title, "--"])
        .args(line)
        .output()
        .map_err(|e| format!("can't run tmux: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let id = pane_number(&String::from_utf8_lossy(&out.stdout))
        .ok_or("tmux didn't report the new pane")?;
    Ok(Window {
        socket: format!("{PREFIX}{socket}"),
        id,
        tty: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(c: &Command) -> Vec<String> {
        c.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn pane() -> Window {
        Window {
            socket: "tmux:/tmp/tmux-1000/default".into(),
            id: 7,
            tty: None,
        }
    }

    #[test]
    fn the_environment_names_the_server_and_pane() {
        assert_eq!(
            socket_of("/tmp/tmux-1000/default,4242,0"),
            Some("/tmp/tmux-1000/default")
        );
        assert_eq!(socket_of(""), None);
        assert_eq!(pane_number("%12\n"), Some(12));
        assert_eq!(pane_number("12"), None);
    }

    #[test]
    fn text_is_typed_literally_into_one_pane() {
        // `-l --`: a prompt starting with `-`, or naming a key, stays text.
        assert_eq!(
            args(&text(&pane(), "-rf Enter")),
            [
                "-S",
                "/tmp/tmux-1000/default",
                "send-keys",
                "-t",
                "%7",
                "-l",
                "--",
                "-rf Enter"
            ]
        );
        assert_eq!(
            args(&keys(&pane(), &["Enter"])),
            [
                "-S",
                "/tmp/tmux-1000/default",
                "send-keys",
                "-t",
                "%7",
                "Enter"
            ]
        );
    }

    #[test]
    fn a_capture_reads_the_screen_or_the_whole_history() {
        assert_eq!(
            args(&capture(&pane(), false))[2..],
            ["capture-pane", "-p", "-t", "%7"]
        );
        assert_eq!(args(&capture(&pane(), true))[6..], ["-J", "-S", "-"]);
    }

    #[test]
    fn only_a_tmux_socket_is_a_pane() {
        assert_eq!(server(&pane()), Some("/tmp/tmux-1000/default"));
        let kitty = Window {
            socket: "unix:/tmp/kitty-42".into(),
            id: 7,
            tty: None,
        };
        assert_eq!(server(&kitty), None);
    }

    /// A real round trip on a private server of its own (its socket in a
    /// fresh temp directory, panes running `cat`), when tmux is installed.
    #[test]
    fn a_private_server_takes_text_keys_and_new_windows() {
        if Command::new("tmux").arg("-V").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("lama-tmux-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s").to_string_lossy().into_owned();
        let started = tmux(&socket)
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-x",
                "80",
                "-y",
                "10",
            ])
            .args(["-P", "-F", "#{pane_id}", "cat"])
            .output()
            .unwrap();
        let id = pane_number(&String::from_utf8_lossy(&started.stdout)).unwrap();
        let w = Window {
            socket: format!("{PREFIX}{socket}"),
            id,
            tty: None,
        };
        let shows = |w: &Window, want: &str| {
            (0..40).any(|_| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let out = capture(w, false).output().unwrap();
                String::from_utf8_lossy(&out.stdout).contains(want)
            })
        };

        text(&w, "-l Enter hello").status().unwrap();
        keys(&w, &["Enter"]).status().unwrap();
        assert!(shows(&w, "-l Enter hello"), "typed text reaches the pane");

        // The pane is found from a process's environment, and only on its tty.
        let env: HashMap<String, String> = [
            ("TMUX".to_string(), format!("{socket},1,0")),
            ("TMUX_PANE".to_string(), format!("%{id}")),
        ]
        .into();
        let found = pane_of(&env, None).expect("the pane is found");
        assert_eq!((found.id, found.socket.as_str()), (id, w.socket.as_str()));
        assert!(pane_of(&env, Some("/dev/pts/nowhere")).is_none());

        // lama's own lookup: the process running in the pane maps to it.
        let pid = tmux(&socket)
            .args(["display-message", "-p", "-t", &target(id), "#{pane_pid}"])
            .output()
            .unwrap();
        let pid: u32 = String::from_utf8_lossy(&pid.stdout).trim().parse().unwrap();
        let seen = crate::kitty::window_of_pid(pid).expect("cat's pane is found");
        assert_eq!((seen.id, seen.label()), (id, format!("tmux %{id}")));
        assert!(seen.alive());

        let line = ["sh".to_string(), "-c".into(), "echo opened; cat".into()];
        let opened = open_window(&socket, &dir, "lama test", &line).unwrap();
        assert_ne!(opened.id, id);
        assert!(shows(&opened, "opened"), "the new window runs its command");
        assert!(focus(&opened).is_ok());

        let _ = tmux(&socket).arg("kill-server").status();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
