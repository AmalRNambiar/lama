//! lama — visualize coding-agent sessions as a live flow graph.
//!
//! CLI (hand-rolled over `std::env::args`, no clap):
//!
//! ```text
//! lama                       follow the current project's Claude + Codex sessions, one plane
//! lama <file.jsonl>          replay a recording, played from the start
//! lama <id>                  replay a session by id (or a unique prefix)
//! lama <dir>                 the same, for another project
//! lama <file> --follow       follow a file's live edge instead of replaying
//! lama <file> --speed N      playback speed multiplier (default 8.0)
//! lama --provider <name> ... force the transcript format instead of detecting it
//! lama inspect <file|id|dir> headless: print the session tree + info
//! ```

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::mpsc;

use lama::provider::{Provider, ReadMode, Target, open};
use lama::state::plane::{Group, Plane};
use lama::state::session::SessionModel;
use lama::state::{App, Mode};
use lama::tailer::{TailRequest, UiEvent};
use lama::{tailer, tui};

/// Channel capacity for the bounded request/event channels.
const CHANNEL_CAP: usize = 32;

/// Parsed CLI invocation.
///
/// One TUI command (`View`) over the unified timeline engine, plus the headless
/// `Inspect`. The launch only sets defaults: *what* to open and *where the
/// playhead starts*. Everything (scrub, follow, go-live, pause) is available
/// once running, regardless of how it launched.
#[derive(Debug, Clone)]
pub enum Cli {
    /// View a session in the TUI. `target`: a session file (replay it from the
    /// start), a project dir (follow its live session), a session id, or
    /// `None` (the current project). `follow` starts at the live edge instead
    /// of replaying. `provider` forces the format instead of detecting it.
    View {
        target: Option<String>,
        follow: bool,
        speed: f64,
        provider: Option<Provider>,
    },
    /// Every recently active session on the machine, grouped by project under
    /// one universe root. `since` is the activity window.
    Universe { since: std::time::Duration },
    /// Headless: parse and print the session tree + info; no TUI. The target
    /// resolves like `View`'s: a file, a session id, or a project directory.
    Inspect {
        target: String,
        provider: Option<Provider>,
    },
    /// Headless MCP server on stdio: the lama bus one task agent talks
    /// through. Started by lama inside the agent's config, never by hand.
    Bus { task: String, agent: String },
}

/// Default replay speed multiplier.
const DEFAULT_REPLAY_SPEED: f64 = 8.0;

const USAGE: &str = "\
lama — visualize coding-agent sessions as a flow graph

USAGE:
    lama                     follow the current project's Claude + Codex sessions, one plane
                            (from your home directory: the lamaverse)
    lama <file.jsonl>        replay a recording, played from the start
    lama <id>                replay a session by id, or a unique prefix of one
    lama <dir>               the same, for another project
    lama <file> --follow     follow a file's live edge instead of replaying
    lama <file> --speed N    playback speed (default 8.0)
    lama --provider <name>   force the format (claude, codex) instead of detecting it
    lama --lamaverse         every project with recent sessions, on one map
    lama --lamaverse --since 4h  widen the activity window (default 1h; s m h d)
    lama inspect <file|id>   headless: print the session tree + info
    lama bus --task <id> --agent <name>  the MCP bus server (started by lama for task agents)
    lama --version           print the version and exit

Once open, scrub/follow/pause/go-live are available no matter how you launched.";

/// Parse `std::env::args` into a [`Cli`]. Returns a usage error on bad input.
fn parse_cli(args: impl Iterator<Item = String>) -> Result<Cli> {
    // Skip argv[0].
    let mut args = args.skip(1).peekable();

    let provider_flag = |args: &mut std::iter::Peekable<_>| -> Result<Option<Provider>> {
        let v: String = args
            .next()
            .ok_or_else(|| anyhow!("--provider requires a name\n\n{USAGE}"))?;
        Provider::parse(&v)
            .map(Some)
            .ok_or_else(|| anyhow!("unknown provider {v:?}; known: claude, codex"))
    };

    // `inspect <file>` is the one distinct (headless) subcommand.
    if args.peek().map(String::as_str) == Some("inspect") {
        args.next();
        let mut target: Option<String> = None;
        let mut provider = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--provider" => provider = provider_flag(&mut args)?,
                other if other.starts_with('-') => bail!("unknown flag {other:?}\n\n{USAGE}"),
                _ => {
                    if target.is_some() {
                        bail!("inspect takes a single target argument\n\n{USAGE}");
                    }
                    target = Some(arg);
                }
            }
        }
        let target = target.ok_or_else(|| {
            anyhow!("inspect requires a file, a session id or a directory\n\n{USAGE}")
        })?;
        return Ok(Cli::Inspect { target, provider });
    }

    // `bus` is the MCP server each task agent runs.
    if args.peek().map(String::as_str) == Some("bus") {
        args.next();
        let (mut task, mut agent) = (None, None);
        while let Some(arg) = args.next() {
            let slot = match arg.as_str() {
                "--task" => &mut task,
                "--agent" => &mut agent,
                other => bail!("unknown bus argument {other:?}\n\n{USAGE}"),
            };
            *slot = Some(
                args.next()
                    .ok_or_else(|| anyhow!("{arg} requires a value\n\n{USAGE}"))?,
            );
        }
        return match (task, agent) {
            (Some(task), Some(agent)) => Ok(Cli::Bus { task, agent }),
            _ => bail!("bus requires --task <id> and --agent <name>\n\n{USAGE}"),
        };
    }

    // Otherwise: an optional positional target + flags.
    let mut target: Option<String> = None;
    let mut follow = false;
    let mut speed = DEFAULT_REPLAY_SPEED;
    let mut provider = None;
    let mut universe = false;
    let mut since: Option<std::time::Duration> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-u" | "--lamaverse" | "--universe" => universe = true,
            "--since" => {
                let v = args.next().ok_or_else(|| {
                    anyhow!("--since requires a duration like 30m or 4h\n\n{USAGE}")
                })?;
                since = Some(parse_window(&v)?);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                std::process::exit(0);
            }
            // Packaging depends on this: the Homebrew formula's `test do`
            // block runs `lama --version`, and it has to exit 0.
            "-V" | "--version" => {
                println!("lama {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--follow" => follow = true,
            "--speed" => {
                let v = args
                    .next()
                    .ok_or_else(|| anyhow!("--speed requires a number\n\n{USAGE}"))?;
                speed = v
                    .parse::<f64>()
                    .with_context(|| format!("invalid --speed value: {v:?}"))?;
                if !(speed.is_finite() && speed > 0.0) {
                    bail!("--speed must be a positive number, got {v:?}");
                }
            }
            "--provider" => provider = provider_flag(&mut args)?,
            other if other.starts_with('-') => {
                bail!("unknown flag {other:?}\n\n{USAGE}");
            }
            _ => {
                if target.is_some() {
                    bail!("expected a single target argument\n\n{USAGE}");
                }
                target = Some(arg);
            }
        }
    }

    if universe {
        if target.is_some() {
            bail!("--lamaverse takes no target: it follows every project\n\n{USAGE}");
        }
        return Ok(Cli::Universe {
            since: since.unwrap_or(DEFAULT_WINDOW),
        });
    }
    if since.is_some() {
        bail!("--since only applies to --lamaverse\n\n{USAGE}");
    }
    Ok(Cli::View {
        target,
        follow,
        speed,
        provider,
    })
}

/// Default universe activity window.
const DEFAULT_WINDOW: std::time::Duration = std::time::Duration::from_secs(3600);

/// A window like `90s`, `30m`, `4h` or `2d`.
fn parse_window(v: &str) -> Result<std::time::Duration> {
    let (num, unit) = v.split_at(v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len()));
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid --since value: {v:?}"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => bail!("--since needs a unit (s, m, h, d), got {v:?}"),
    };
    if n == 0 {
        bail!("--since must be positive, got {v:?}");
    }
    Ok(std::time::Duration::from_secs(n * secs))
}

/// Fully parse a session, every file of it, into a [`SessionModel`] and its
/// [`SessionInfo`](lama::state::SessionInfo). Shared by `inspect`; the
/// live/replay path uses the tailer instead.
fn parse_session_fully(
    target: &Target,
    only: Option<Provider>,
) -> Result<(SessionModel, lama::state::SessionInfo)> {
    let session = open(target, only)?;
    let mut model = SessionModel::new(session.id.clone());
    let mut info = lama::state::SessionInfo::default();
    let p = session.provider;
    let mut apply = |mut statement: lama::fact::Statement| {
        for f in statement.take_session_meta() {
            info.apply(&f);
        }
        for f in &statement.facts {
            model.apply_fact(f);
        }
    };
    // Order is not critical — the model is fold-order independent — so files
    // go in as the session lists them, root first.
    for f in session.every_file() {
        let text = std::fs::read_to_string(&f.path)
            .with_context(|| format!("reading {}", f.path.display()))?;
        match f.read {
            ReadMode::Tail => {
                let mut stream = p.stream_for(f);
                for statement in text.lines().filter_map(|l| stream.push(l)) {
                    apply(statement);
                }
            }
            ReadMode::Whole => {
                if let Some(statement) = p.sidecar(f, &text) {
                    apply(statement);
                }
            }
        }
    }

    // Group nodes have no direct completion signal — roll them up from their
    // children once everything is folded.
    model.recompute_group_status();

    // Interactive agents (main, forks) have no completion signal: derive
    // their liveness against the wall clock — `inspect` is a point-in-time
    // view, so a recently active session shows `running`, a long-quiet one `idle`.
    model.recompute_liveness(Some(chrono::Utc::now()));

    Ok((model, info))
}

/// Run the `inspect` subcommand: fully parse the session and print a tree to
/// stdout. Returns an error (non-zero exit) on an unreadable file. This is the
/// headless smoke test — no TTY required.
async fn run_inspect(target: String, provider: Option<Provider>) -> Result<()> {
    let target = resolve_target(target)?;
    let (model, info) = parse_session_fully(&target, provider)?;
    print!("{}", lama::state::render::report(&model, &info));
    Ok(())
}

/// Resolve a `View` invocation into (session id, target, mode, feeder, speed),
/// then spawn the tailer and run the TUI.
///
/// A **file** or an **id** bulk-loads + tails (`replay` feeder); `--follow`
/// only changes the start position (head vs beginning) via the mode. A
/// **dir** (or no target → the current project) discovers the newest session
/// there, any provider, and live-tails it.
async fn run_tui(cli: Cli) -> Result<()> {
    let Cli::View {
        target,
        follow,
        speed,
        provider,
    } = cli
    else {
        unreachable!("inspect handled in main");
    };

    let target = match target {
        Some(t) => resolve_target(t)?,
        None => {
            let cwd = std::env::current_dir().context("resolving current directory")?;
            // Home (or the filesystem root) is not a project: from there,
            // `lama` means everything.
            if provider.is_none() && (is_home(&cwd) || cwd == std::path::Path::new("/")) {
                return run_universe(DEFAULT_WINDOW).await;
            }
            Target::Here(cwd)
        }
    };
    // A project with no provider forced follows every provider at once on one
    // shared plane, so a Claude task started while Codex runs (or the reverse)
    // shows up beside it, under the same project hub, instead of replacing it.
    if let (Target::Here(dir), None) = (&target, provider) {
        let group = project_group(dir);
        let mut panes = Vec::new();
        for p in Provider::ALL {
            let session_id = open(&target, Some(p)).map(|s| s.id).unwrap_or_default();
            let pane = spawn_pane(
                target.clone(),
                session_id,
                Mode::Live,
                false,
                DEFAULT_REPLAY_SPEED,
                Some(p),
            )
            .await?;
            panes.push((pane, group.clone()));
        }
        return tui::run_plane(Plane::new(), panes, None).await;
    }

    let (session_id, mode, replay, speed) = match &target {
        // A concrete file, or a stored session by id → bulk-load + tail. Paced
        // from the start unless `--follow` asks to ride the (possibly still
        // growing) edge.
        Target::Path(_) | Target::Id(_) => {
            let session_id = open(&target, provider)?.id;
            let mode = if follow { Mode::Live } else { Mode::Replay };
            (session_id, mode, true, speed)
        }
        // A directory (or none → cwd) → live: discover the newest session of
        // that project and follow it. (It need not exist yet; the tailer
        // waits.) The id is best-effort so stale events filter; the tailer
        // re-discovers and may switch.
        Target::Here(_) => {
            let session_id = open(&target, provider).map(|s| s.id).unwrap_or_default();
            (session_id, Mode::Live, false, DEFAULT_REPLAY_SPEED)
        }
    };

    let pane = spawn_pane(target, session_id, mode, replay, speed, provider).await?;
    tui::run(pane.app, pane.tail_tx, pane.ui_rx).await
}

/// Whether `dir` is the user's home directory.
fn is_home(dir: &std::path::Path) -> bool {
    std::env::var_os("HOME").is_some_and(|home| {
        let home = std::path::PathBuf::from(home);
        let canon = |p: &std::path::Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        canon(dir) == canon(&home)
    })
}

/// How often the universe looks for sessions that started since.
const SWEEP_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Run the universe: every session active within `since`, across every
/// project, on one plane. A background sweep keeps adding sessions as they
/// start anywhere on the machine; the plane drops them once they age out.
async fn run_universe(since: std::time::Duration) -> Result<()> {
    let (tx, rx) = mpsc::unbounded_channel();
    let window = chrono::Duration::from_std(since).unwrap_or(chrono::Duration::hours(1));
    tokio::spawn(discover(since, tx));
    tui::run_plane(Plane::universe(window), Vec::new(), Some(rx)).await
}

/// The universe's sweep: find the sessions written to within `since`, plus
/// the newest sessions of every agent process running right now however
/// quiet they are; start a tailer on each one not seen before and hand it
/// over with its project; then report which sessions are open.
async fn discover(since: std::time::Duration, tx: mpsc::UnboundedSender<tui::Found>) {
    use lama::provider::{Scope, sweep};
    let mut seen: HashSet<String> = HashSet::new();
    let mut groups: std::collections::HashMap<PathBuf, Group> = Default::default();
    loop {
        let cutoff = std::time::SystemTime::now() - since;
        let (found, alive) = tokio::task::spawn_blocking(move || {
            let scope = Scope {
                project: None,
                since: Some(cutoff),
                id_prefix: None,
            };
            let mut found: Vec<_> = sweep(&scope, None)
                .into_iter()
                .filter(|s| s.last_modified >= cutoff)
                .collect();
            // An open terminal session is live however long it has idled:
            // each one lama's record ties to a running process, and for the
            // processes it doesn't, their project's newest sessions of that
            // provider (one per process).
            let mut alive = HashSet::new();
            let mut counts = lama::procs::counts();
            for e in lama::registry::all() {
                let (true, Some(id), Some(_)) = (e.is_open(), e.session.clone(), e.pid) else {
                    continue;
                };
                let scope = Scope {
                    project: Some(e.cwd.clone()),
                    since: None,
                    id_prefix: Some(id.clone()),
                };
                if let Some(s) = sweep(&scope, Some(e.provider))
                    .into_iter()
                    .find(|s| s.id == id)
                {
                    if let Some(n) = counts.get_mut(&(e.provider, e.cwd.clone())) {
                        *n = n.saturating_sub(1);
                    }
                    alive.insert(s.id.clone());
                    found.push(s);
                }
            }
            for ((provider, cwd), n) in counts {
                let newest: Vec<_> = sweep(&Scope::project(&cwd), Some(provider))
                    .into_iter()
                    .filter(|s| !alive.contains(&s.id))
                    .take(n)
                    .collect();
                for s in newest {
                    alive.insert(s.id.clone());
                    found.push(s);
                }
            }
            let found = found
                .into_iter()
                .map(|s| {
                    let cwd = s.provider.cwd(&s.root);
                    (s, cwd)
                })
                .collect::<Vec<_>>();
            (found, alive)
        })
        .await
        .unwrap_or_default();
        for (session, cwd) in found {
            if !seen.insert(format!("{}:{}", session.provider.name(), session.id)) {
                continue;
            }
            let group = match &cwd {
                Some(dir) => match groups.get(dir) {
                    Some(g) => g.clone(),
                    None => {
                        let dir2 = dir.clone();
                        let g = tokio::task::spawn_blocking(move || project_group(&dir2))
                            .await
                            .unwrap_or_else(|_| fallback_group(&session.project_key));
                        groups.insert(dir.clone(), g.clone());
                        g
                    }
                },
                None => fallback_group(&session.project_key),
            };
            // Pinned to the file, at its live edge: the sweep, not the
            // tailer, decides which sessions are on the plane.
            let pane = spawn_pane(
                Target::Path(session.root.path.clone()),
                session.id.clone(),
                Mode::Live,
                true,
                DEFAULT_REPLAY_SPEED,
                Some(session.provider),
            )
            .await;
            let Ok(pane) = pane else { continue };
            if tx.send(tui::Found::Session(Box::new(pane), group)).is_err() {
                return;
            }
        }
        if tx.send(tui::Found::Alive(alive)).is_err() {
            return;
        }
        tokio::time::sleep(SWEEP_EVERY).await;
    }
}

/// A project known only by its provider key (no cwd recorded).
fn fallback_group(key: &str) -> Group {
    Group {
        cwd: None,
        key: key.to_string(),
        name: key
            .rsplit(['/', '-'])
            .find(|s| !s.is_empty())
            .unwrap_or(key)
            .to_string(),
        branch: None,
    }
}

/// The hub's label for a project directory: its name, and the git branch
/// checked out there if it is a repository (read once, when first seen).
fn project_group(dir: &std::path::Path) -> Group {
    let (name, branch) = project_identity(dir);
    Group {
        key: lama::provider::group_key(dir),
        name,
        branch,
        cwd: Some(dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())),
    }
}

fn project_identity(dir: &std::path::Path) -> (String, Option<String>) {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let project = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    let branch = std::process::Command::new("git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|b| !b.is_empty());
    (project, branch)
}

/// Start a tailer on `target` and pair it with a fresh app: one screen pane.
async fn spawn_pane(
    target: Target,
    session_id: String,
    mode: Mode,
    replay: bool,
    speed: f64,
    provider: Option<Provider>,
) -> Result<tui::Pane> {
    // Bounded request/event channels for backpressure.
    let (tail_tx, tail_rx) = mpsc::channel::<TailRequest>(CHANNEL_CAP);
    let (ui_tx, ui_rx) = mpsc::channel::<UiEvent>(CHANNEL_CAP);

    // Kick off the watch before the tailer task starts consuming.
    tail_tx
        .send(TailRequest::Watch(target))
        .await
        .map_err(|_| anyhow!("tailer channel closed before start"))?;

    // Spawn the tailer task — owns all files of the watched session.
    tokio::spawn(async move {
        if let Err(e) = tailer::run(tail_rx, ui_tx.clone(), replay, speed, provider).await {
            let _ = ui_tx.send(UiEvent::Error(e.to_string())).await;
        }
    });

    Ok(tui::Pane {
        app: App::new(session_id, mode),
        tail_tx,
        ui_rx,
    })
}

/// What a positional argument means: an existing file is a session's file, an
/// existing directory is a project to follow, anything shaped like a path
/// that does not exist is a typo, and the rest is a session id or a prefix
/// of one. Shared by `lama <target>` and `lama inspect <target>`.
fn resolve_target(arg: String) -> Result<Target> {
    let path = PathBuf::from(&arg);
    if path.is_file() {
        return Ok(Target::Path(path));
    }
    if path.is_dir() {
        return Ok(Target::Here(path));
    }
    if path.components().count() > 1 || path.extension().is_some() {
        bail!("not found: {}", path.display());
    }
    Ok(Target::Id(arg))
}

/// Serve the bus for one agent. stdout belongs to the protocol, so every
/// failure goes to stderr through the `Result`.
fn run_bus(task: &str, agent: String) -> Result<()> {
    use lama::task::{CONDUCTOR, Store, valid_name};
    let store = Store::open(task).ok_or_else(|| anyhow!("invalid task id {task:?}"))?;
    if agent != CONDUCTOR && !valid_name(&agent) {
        bail!("invalid agent name {agent:?}");
    }
    lama::task::bus::serve(store, agent)?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Answer and exit before the TUI touches the terminal, so this works over a
    // pipe — assets/build.sh asks for it and has no tty to spare. The tape's
    // Sleep has to be at least this long or the recording cuts mid-gesture, and
    // that number used to be copied into the tape by hand.
    if std::env::var("LAMA_DEMO").as_deref() == Ok("duration") {
        println!("{:.2}", lama::autopilot::tour_secs());
        return Ok(());
    }

    let cli = parse_cli(std::env::args())?;
    match cli {
        Cli::Bus { task, agent } => run_bus(&task, agent),
        Cli::Inspect { target, provider } => run_inspect(target, provider).await,
        Cli::Universe { since } => run_universe(since).await,
        other => run_tui(other).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lama::state::session::AgentStatus;

    fn cli(args: &[&str]) -> Result<Cli> {
        // parse_cli skips argv[0], so prepend a fake program name.
        let mut v = vec!["lama".to_string()];
        v.extend(args.iter().map(|s| s.to_string()));
        parse_cli(v.into_iter())
    }

    #[test]
    fn bus_subcommand_takes_task_and_agent() {
        match cli(&["bus", "--task", "m1", "--agent", "api"]).unwrap() {
            Cli::Bus { task, agent } => {
                assert_eq!((task.as_str(), agent.as_str()), ("m1", "api"))
            }
            other => panic!("got {other:?}"),
        }
        assert!(cli(&["bus", "--task", "m1"]).is_err());
    }

    #[test]
    fn bare_invocation_is_live_for_cwd() {
        match cli(&[]).unwrap() {
            Cli::View {
                target: None,
                follow: false,
                ..
            } => {}
            other => panic!("expected View{{target:None}}, got {other:?}"),
        }
    }

    #[test]
    fn positional_argument_is_the_target() {
        match cli(&["/tmp/foo"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, "/tmp/foo"),
            other => panic!("got {other:?}"),
        }
        // A session file, a directory or an id: told apart at run time.
        match cli(&["01a03eb1"]).unwrap() {
            Cli::View {
                target: Some(p), ..
            } => assert_eq!(p, "01a03eb1"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn default_speed_and_no_follow() {
        match cli(&["s.jsonl"]).unwrap() {
            Cli::View {
                speed,
                follow,
                provider,
                ..
            } => {
                assert_eq!(speed, DEFAULT_REPLAY_SPEED);
                assert!(!follow);
                assert_eq!(provider, None);
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn speed_follow_and_provider_flags_in_any_order() {
        match cli(&["s.jsonl", "--speed", "4", "--follow", "--provider", "codex"]).unwrap() {
            Cli::View {
                target: Some(p),
                follow,
                speed,
                provider,
            } => {
                assert_eq!(p, "s.jsonl");
                assert_eq!(speed, 4.0);
                assert!(follow);
                assert_eq!(provider, Some(Provider::Codex));
            }
            other => panic!("got {other:?}"),
        }
        // Flags before the path, too.
        match cli(&["--provider", "Claude", "--speed", "2.5", "s.jsonl"]).unwrap() {
            Cli::View {
                target: Some(p),
                speed,
                provider,
                ..
            } => {
                assert_eq!(p, "s.jsonl");
                assert_eq!(speed, 2.5);
                assert_eq!(provider, Some(Provider::Claude));
            }
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn rejects_bad_speed_and_unknown_provider() {
        assert!(cli(&["s.jsonl", "--speed", "nope"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "0"]).is_err());
        assert!(cli(&["s.jsonl", "--speed", "-3"]).is_err());
        assert!(cli(&["s.jsonl", "--speed"]).is_err());
        assert!(cli(&["s.jsonl", "--provider", "gemini"]).is_err());
        assert!(cli(&["s.jsonl", "--provider"]).is_err());
    }

    #[test]
    fn rejects_extra_positional_and_unknown_flags() {
        assert!(cli(&["a.jsonl", "b.jsonl"]).is_err());
        assert!(cli(&["--bogus"]).is_err());
    }

    #[test]
    fn inspect_takes_one_target_and_a_provider() {
        match cli(&["inspect", "s.jsonl"]).unwrap() {
            Cli::Inspect { target, provider } => {
                assert_eq!(target, "s.jsonl");
                assert_eq!(provider, None);
            }
            other => panic!("got {other:?}"),
        }
        // An id resolves for inspect the way it does for the TUI.
        match cli(&["inspect", "01a03eb1"]).unwrap() {
            Cli::Inspect { target, .. } => assert_eq!(target, "01a03eb1"),
            other => panic!("got {other:?}"),
        }
        match cli(&["inspect", "--provider", "codex", "s.jsonl"]).unwrap() {
            Cli::Inspect { provider, .. } => assert_eq!(provider, Some(Provider::Codex)),
            other => panic!("got {other:?}"),
        }
        assert!(cli(&["inspect"]).is_err());
        assert!(cli(&["inspect", "a", "b"]).is_err());
    }

    #[test]
    fn a_missing_path_is_a_typo_and_a_bare_word_is_an_id() {
        assert!(resolve_target("./nope.jsonl".into()).is_err());
        assert!(resolve_target("nope.jsonl".into()).is_err());
        assert!(
            matches!(resolve_target("01a03eb1".into()), Ok(Target::Id(id)) if id == "01a03eb1")
        );
        assert!(matches!(resolve_target(".".into()), Ok(Target::Here(_))));
    }

    #[test]
    fn parse_session_fully_marks_quiet_main_idle() {
        // Inspect is a point-in-time view: a transcript whose last activity is
        // far in the past reports main as Idle (interactive agents never claim
        // completion — the format has no end marker to prove it).
        let dir = std::env::temp_dir().join(format!("lama-fullparse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("77777777-7777-7777-7777-777777777777.jsonl");
        std::fs::write(
            &tmp,
            b"{\"type\":\"user\",\"uuid\":\"u\",\"parentUuid\":null,\"timestamp\":\"2026-06-05T13:51:00.000Z\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
        )
        .unwrap();

        let (model, _info) = parse_session_fully(&Target::Path(tmp.clone()), None).expect("parses");
        let main = model.agent(lama::state::session::MAIN_ID).unwrap();
        assert_eq!(main.status, AgentStatus::Idle);
        // The root's name comes from the provider now, not the model.
        assert_eq!(main.agent_type.as_deref(), Some("claude"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn universe_flag_and_window() {
        match cli(&["--lamaverse"]).unwrap() {
            Cli::Universe { since } => assert_eq!(since, DEFAULT_WINDOW),
            other => panic!("got {other:?}"),
        }
        match cli(&["-u", "--since", "4h"]).unwrap() {
            Cli::Universe { since } => assert_eq!(since.as_secs(), 4 * 3600),
            other => panic!("got {other:?}"),
        }
        assert!(
            cli(&["--since", "4h"]).is_err(),
            "--since needs --lamaverse"
        );
        assert!(
            matches!(cli(&["--universe"]).unwrap(), Cli::Universe { .. }),
            "the old flag still works"
        );
        assert!(
            cli(&["--lamaverse", "/tmp/x"]).is_err(),
            "no target in the lamaverse"
        );
        assert!(parse_window("30").is_err(), "a unit is required");
        assert!(parse_window("0m").is_err());
        assert_eq!(parse_window("90s").unwrap().as_secs(), 90);
        assert_eq!(parse_window("2d").unwrap().as_secs(), 2 * 86_400);
    }
}
