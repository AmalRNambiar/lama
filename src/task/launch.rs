//! Starting a task: its directory, the conductor, and each agent the
//! approved plan names.
//!
//! Everything an agent is started with comes from the plan the conductor
//! wrote, so this is where the plan is distrusted: names, projects and
//! models are checked before anything runs, and every argument travels as
//! its own argv element through `kitty::agent_args`, never as shell text.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{CONDUCTOR, Launched, PlannedAgent, Store, Task, prompts, root, valid_name};
use crate::kitty;
use crate::provider::Provider;

/// `n` random bytes from the kernel.
fn random(n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0; n];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A random (v4) UUID, the form Claude's `--session-id` takes.
fn uuid() -> std::io::Result<String> {
    let mut b = random(16)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

/// `m251029-143005-a3f9`: sorts by start time, and the random tail keeps
/// two tasks started in the same second apart.
fn task_id() -> std::io::Result<String> {
    Ok(format!(
        "t{}-{}",
        chrono::Local::now().format("%y%m%d-%H%M%S"),
        hex(&random(2)?)
    ))
}

/// A new task for `source`, with its conductor's session id chosen.
pub fn new_task(source: &str, conductor: Provider) -> std::io::Result<Store> {
    let root = root().ok_or_else(|| std::io::Error::other("no home directory"))?;
    new_task_in(&root, source, conductor)
}

fn new_task_in(root: &Path, source: &str, conductor: Provider) -> std::io::Result<Store> {
    std::fs::create_dir_all(root)?;
    // Creating the directory claims the id, so a collision retries rather
    // than sharing a task.
    let mut tries = 0;
    let (id, dir) = loop {
        let id = task_id()?;
        let dir = root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => break (id, dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && tries < 8 => tries += 1,
            Err(e) => return Err(e),
        }
    };
    let store = Store::at(dir);
    store.set_task(&Task {
        id,
        source: source.to_string(),
        created: chrono::Utc::now(),
        conductor_session: match conductor {
            Provider::Claude => uuid()?,
            Provider::Codex => String::new(),
        },
        conductor_provider: conductor,
    })?;
    Ok(store)
}

/// Write `<task>/mcp-<name>.json`, which gives the agent the bus as
/// `lama bus --task <id> --agent <name>`.
fn write_mcp_config(store: &Store, exe: &str, name: &str) -> Result<PathBuf, String> {
    let config = serde_json::json!({
        "mcpServers": { "lama": { "command": exe, "args": bus_args(&store.id(), name) } }
    });
    let path = store.dir.join(format!("mcp-{name}.json"));
    let text = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| format!("can't write {}: {e}", path.display()))?;
    Ok(path)
}

fn bus_args(task: &str, name: &str) -> [String; 5] {
    [
        "bus".into(),
        "--task".into(),
        task.into(),
        "--agent".into(),
        name.into(),
    ]
}

/// This binary, which the bus runs as. A path that isn't UTF-8 can't go
/// into a JSON or TOML config faithfully, so it is refused.
fn exe() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("can't find lama itself: {e}"))?;
    let exe = exe.to_str().ok_or("lama's path isn't UTF-8")?;
    // Reinstalled while running, Linux reports the old file as deleted;
    // the new one sits at the same path.
    Ok(exe.strip_suffix(" (deleted)").unwrap_or(exe).to_string())
}

/// What the conductor may use without asking. It has no terminal to ask
/// in, so anything else is refused: it reads, runs read-only commands and
/// talks over the bus, and never edits.
const CONDUCTOR_TOOLS: &str = "Read,Grep,Glob,WebFetch,WebSearch,TodoWrite,mcp__lama,\
Bash(sentry:*),Bash(gh:*),Bash(glab:*),Bash(git log:*),Bash(git show:*),Bash(git diff:*),\
Bash(git status:*),Bash(git branch:*),Bash(rg:*),Bash(grep:*),Bash(ls:*),Bash(cat:*),\
Bash(head:*),Bash(tail:*),Bash(find:*),Bash(wc:*),Bash(jira issue view:*),\
Bash(jira issue list:*)";

/// Run one turn of the conductor in the background — headless `claude -p`
/// or `codex exec` in the task directory, no window — starting its session
/// or resuming it with `prompt` (the source, or the messages that arrived).
/// Its output goes to `conductor.log`; lama follows its transcript.
pub fn run_conductor(
    store: &Store,
    projects: &[PathBuf],
    prompt: &str,
    resume: bool,
) -> Result<std::process::Child, String> {
    let task = store.task().ok_or("task.json is missing")?;
    let who = task.conductor_provider;
    let system = prompts::conductor(&task.id, projects, who, crate::agents::installed());
    let exe = exe()?;
    let args = match who {
        Provider::Claude => {
            let mcp = write_mcp_config(store, &exe, CONDUCTOR)?;
            conductor_args(
                &task.conductor_session,
                &mcp,
                &system,
                projects,
                prompt,
                resume,
            )
        }
        Provider::Codex => {
            if resume && task.conductor_session.is_empty() {
                return Err("its Codex session hasn't appeared yet".into());
            }
            let bus = bus_args(&task.id, CONDUCTOR);
            codex_conductor_args(&task.conductor_session, &exe, &bus, &system, prompt, resume)
        }
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let line = kitty::agent_args(who, &refs);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(store.dir.join("conductor.log"))
        .map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    Command::new(&line[0])
        .args(&line[1..])
        .current_dir(&store.dir)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err)
        .spawn()
        .map_err(|e| format!("can't start the conductor: {e}"))
}

/// Claude's `--mcp-config` and `--add-dir` take several values, so they go
/// in `--flag=value` form: a separate value would let them swallow the
/// prompt that follows. No `--strict-mcp-config`: the user's own servers
/// (Sentry, Jira) are how the conductor reads the source.
fn conductor_args(
    session: &str,
    mcp: &Path,
    system: &str,
    projects: &[PathBuf],
    prompt: &str,
    resume: bool,
) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-p".into(),
        "--output-format=stream-json".into(),
        "--verbose".into(),
        if resume { "--resume" } else { "--session-id" }.into(),
        session.into(),
        // Planning is quick and frequent; the team does the deep work.
        "--model".into(),
        "sonnet".into(),
        format!("--mcp-config={}", mcp.display()),
        format!("--allowed-tools={CONDUCTOR_TOOLS}"),
        "--disallowed-tools=Edit,Write,NotebookEdit".into(),
    ];
    a.extend(
        projects
            .iter()
            .map(|p| format!("--add-dir={}", p.display())),
    );
    a.extend([
        "--append-system-prompt".into(),
        system.into(),
        prompt.into(),
    ]);
    a
}

/// The Codex conductor's command line: `codex exec`, or `codex exec resume`
/// on its session. Its shell is sandboxed read-only, so it reads but can't
/// change anything, and it never stops to ask; the bus goes in as `-c`
/// overrides, as for a worker. Codex has no system-prompt flag, so the
/// standing rules open its first prompt; a resumed session still has them.
fn codex_conductor_args(
    session: &str,
    exe: &str,
    bus: &[String],
    system: &str,
    prompt: &str,
    resume: bool,
) -> Vec<String> {
    let list: Vec<String> = bus.iter().map(|s| toml_string(s)).collect();
    let mut a: Vec<String> = vec!["exec".into()];
    if resume {
        a.push("resume".into());
    }
    a.extend([
        "--json".into(),
        "--skip-git-repo-check".into(),
        "-c".into(),
        "sandbox_mode=\"read-only\"".into(),
        "-c".into(),
        "approval_policy=\"never\"".into(),
        "-c".into(),
        format!("mcp_servers.lama.command={}", toml_string(exe)),
        "-c".into(),
        format!("mcp_servers.lama.args=[{}]", list.join(",")),
    ]);
    if resume {
        a.extend([session.into(), prompt.into()]);
    } else {
        a.push(format!("{system}\n---\n\n{prompt}"));
    }
    a
}

/// A worker's Claude command line, shaped like [`conductor_args`].
fn claude_args(session: &str, model: &str, mcp: &Path, system: &str, prompt: &str) -> Vec<String> {
    let mut a = vec!["--session-id".into(), session.into()];
    if !model.is_empty() {
        a.extend(["--model".into(), model.into()]);
    }
    a.extend([
        format!("--mcp-config={}", mcp.display()),
        "--append-system-prompt".into(),
        system.into(),
        prompt.into(),
    ]);
    a
}

/// A worker's Codex command line. Codex takes MCP servers only from its
/// config, so the bus goes in as `-c` overrides, each value TOML.
fn codex_args(model: &str, exe: &str, bus: &[String], prompt: &str) -> Vec<String> {
    let mut a = Vec::new();
    if !model.is_empty() {
        a.extend(["-m".into(), model.into()]);
    }
    let list: Vec<String> = bus.iter().map(|s| toml_string(s)).collect();
    a.extend([
        "-c".into(),
        format!("mcp_servers.lama.command={}", toml_string(exe)),
        "-c".into(),
        format!("mcp_servers.lama.args=[{}]", list.join(",")),
        prompt.into(),
    ]);
    a
}

/// A TOML basic string: quoted, with `\`, `"` and control characters
/// escaped, so a path can't end the string early.
fn toml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A model id as agent CLIs spell them (`sonnet`, `gpt-5.1-codex`,
/// `claude-opus-4[1m]`), empty for the default — and nothing that could
/// pass for a flag or a path.
fn valid_model(m: &str) -> bool {
    m.len() <= 64
        && !m.starts_with('-')
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:[]-".contains(&b))
}

/// The planned project, if it is one of `projects`; both sides are
/// canonicalized so `..` and symlinks can't lead out of the list.
fn allowed_project(project: &Path, projects: &[PathBuf]) -> Result<PathBuf, String> {
    let wanted = project
        .canonicalize()
        .map_err(|e| format!("project {}: {e}", project.display()))?;
    projects
        .iter()
        .filter_map(|p| p.canonicalize().ok())
        .find(|p| *p == wanted)
        .ok_or_else(|| format!("{} isn't one of lama's projects", project.display()))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("can't run git: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A writer's own worktree, `<project>/.lama/worktrees/<task>-<name>`
/// on a new branch `lama/<task>/<name>` from the project's HEAD — where
/// `provider::project_dir` files it under its project. Returns the agent's
/// directory in it (the project's place in the repo) and the branch.
fn worktree(project: &Path, task: &str, name: &str) -> Result<(PathBuf, String), String> {
    let prefix = git(project, &["rev-parse", "--show-prefix"])
        .map_err(|e| format!("{} isn't a git repository: {e}", project.display()))?;
    exclude_lama(project)?;
    let path = project
        .join(".lama")
        .join("worktrees")
        .join(format!("{task}-{name}"));
    let branch = format!("lama/{task}/{name}");
    let path_s = path.to_str().ok_or("project path isn't UTF-8")?;
    git(project, &["worktree", "add", "-b", &branch, path_s, "HEAD"])?;
    let cwd = if prefix.is_empty() {
        path
    } else {
        path.join(prefix)
    };
    Ok((cwd, branch))
}

/// List `.lama/` in the repo's `info/exclude`, so worktrees never show up
/// as untracked files in your checkout.
fn exclude_lama(project: &Path) -> Result<(), String> {
    let common = PathBuf::from(git(project, &["rev-parse", "--git-common-dir"])?);
    let common = if common.is_absolute() {
        common
    } else {
        project.join(common)
    };
    let file = common.join("info").join("exclude");
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    if text.lines().any(|l| l.trim() == ".lama/") {
        return Ok(());
    }
    let sep = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let _ = std::fs::create_dir_all(common.join("info"));
    std::fs::write(&file, format!("{text}{sep}.lama/\n"))
        .map_err(|e| format!("can't write {}: {e}", file.display()))
}

/// Start one planned agent in a new tab, after checking what the plan
/// asks for, and record it in `agents.json`.
/// A reader may be given `at`, a directory inside its project (a peer
/// reviewing its owner's worktree) to run in instead of the project itself.
pub fn start_agent(
    store: &Store,
    agent: &PlannedAgent,
    projects: &[PathBuf],
    at: Option<&Path>,
) -> Result<Launched, String> {
    if !valid_name(&agent.name) || agent.name == CONDUCTOR {
        return Err(format!("bad agent name {:?}", agent.name));
    }
    if !valid_model(&agent.model) {
        return Err(format!("bad model {:?}", agent.model));
    }
    let project = allowed_project(&agent.project, projects)?;
    let task = store.id();
    let plan = store.plan().ok_or("plan.json is missing")?;
    let exe = exe()?;
    let (cwd, branch) = if agent.writes {
        let (cwd, branch) = worktree(&project, &task, &agent.name)?;
        (cwd, Some(branch))
    } else {
        let inside = at
            .and_then(|d| d.canonicalize().ok())
            .filter(|d| d.starts_with(&project));
        (inside.unwrap_or(project), None)
    };
    let mcp = write_mcp_config(store, &exe, &agent.name)?;
    let system = prompts::worker(agent, &plan, &task, branch.as_deref());
    let first = prompts::worker_first(agent);
    let (session, args) = match agent.provider {
        Provider::Claude => {
            let session = uuid().map_err(|e| e.to_string())?;
            let args = claude_args(&session, &agent.model, &mcp, &system, &first);
            (Some(session), args)
        }
        Provider::Codex => {
            let bus = bus_args(&task, &agent.name);
            let prompt = format!("{system}\n---\n\n{first}");
            (None, codex_args(&agent.model, &exe, &bus, &prompt))
        }
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let window = kitty::open_tab(
        &cwd,
        &format!("◈ {}", agent.name),
        kitty::agent_args(agent.provider, &refs),
    )?;
    let launched = Launched {
        session,
        window: Some(window.id),
        socket: Some(window.socket),
        cwd,
        branch,
        at: Some(chrono::Utc::now()),
    };
    let mut all = store.launched();
    all.insert(agent.name.clone(), launched.clone());
    store.set_launched(&all).map_err(|e| e.to_string())?;
    Ok(launched)
}

/// Reopen a task agent's closed session with the bus it was launched with,
/// in a new tab in its worktree or project. `None` when no task
/// launched `session`; it is then an ordinary session to resume.
pub fn resume_agent(session: &str) -> Option<Result<crate::kitty::Window, String>> {
    let root = super::root()?;
    let (store, name, launched) = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|d| Store::at(d.path()))
        .find_map(|store| {
            let (name, l) = store
                .launched()
                .into_iter()
                .find(|(_, l)| l.session.as_deref() == Some(session))?;
            Some((store, name, l))
        })?;
    Some(reopen(&store, &name, &launched, session))
}

fn reopen(
    store: &Store,
    name: &str,
    launched: &Launched,
    session: &str,
) -> Result<crate::kitty::Window, String> {
    if !valid_name(name) || name == CONDUCTOR {
        return Err(format!("bad agent name {name:?}"));
    }
    let task = store.id();
    let plan = store.plan().ok_or("plan.json is missing")?;
    let agent = plan
        .agents
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| format!("{name} is not in the plan"))?;
    if !valid_model(&agent.model) {
        return Err(format!("bad model {:?}", agent.model));
    }
    let exe = exe()?;
    let args = match agent.provider {
        Provider::Claude => {
            let mcp = write_mcp_config(store, &exe, name)?;
            let system = prompts::worker(agent, &plan, &task, launched.branch.as_deref());
            claude_resume_args(session, &agent.model, &mcp, &system)
        }
        Provider::Codex => codex_resume_args(session, &agent.model, &exe, &bus_args(&task, name)),
    };
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let window = kitty::open_tab(
        &launched.cwd,
        &format!("◈ {name}"),
        kitty::agent_args(agent.provider, &refs),
    )?;
    let mut all = store.launched();
    if let Some(l) = all.get_mut(name) {
        l.window = Some(window.id);
        l.socket = Some(window.socket.clone());
    }
    store.set_launched(&all).map_err(|e| e.to_string())?;
    Ok(window)
}

/// [`claude_args`] picking `session` back up: Claude keeps neither its MCP
/// servers nor the appended system prompt across a resume.
fn claude_resume_args(session: &str, model: &str, mcp: &Path, system: &str) -> Vec<String> {
    let mut a = vec!["--resume".into(), session.into()];
    if !model.is_empty() {
        a.extend(["--model".into(), model.into()]);
    }
    a.extend([
        format!("--mcp-config={}", mcp.display()),
        "--append-system-prompt".into(),
        system.into(),
    ]);
    a
}

/// [`codex_args`] picking `session` back up. The overrides go before
/// `resume`, as options of Codex itself; the instructions are already in
/// the session.
fn codex_resume_args(session: &str, model: &str, exe: &str, bus: &[String]) -> Vec<String> {
    let list: Vec<String> = bus.iter().map(|s| toml_string(s)).collect();
    let mut a = vec![
        "-c".into(),
        format!("mcp_servers.lama.command={}", toml_string(exe)),
        "-c".into(),
        format!("mcp_servers.lama.args=[{}]", list.join(",")),
    ];
    if !model.is_empty() {
        a.extend(["-c".into(), format!("model={}", toml_string(model))]);
    }
    a.extend(["resume".into(), session.into()]);
    a
}

// Tasks reach agents through kitty or tmux and use Unix paths, so their
// tests run where those do; Windows has neither.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lama-launch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn task_ids_are_names_and_distinct() {
        let a = task_id().unwrap();
        let b = task_id().unwrap();
        assert!(valid_name(&a) && a.len() == 19 && a.starts_with('t'), "{a}");
        assert_eq!(&a[7..8], "-");
        assert_eq!(&a[14..15], "-");
        assert_ne!(a, b);
    }

    #[test]
    fn a_new_task_has_its_conductor_session() {
        let root = temp("new");
        let a = new_task_in(&root, "fix it", Provider::Claude).unwrap();
        let b = new_task_in(&root, "fix it", Provider::Claude).unwrap();
        assert_ne!(a.dir, b.dir);
        let m = a.task().unwrap();
        assert_eq!((m.id, m.source.as_str()), (a.id(), "fix it"));
        assert_eq!(m.conductor_session.len(), 36);
        // Codex names its own session: none until it starts.
        let c = new_task_in(&root, "fix it", Provider::Codex).unwrap();
        let m = c.task().unwrap();
        assert_eq!(m.conductor_provider, Provider::Codex);
        assert!(m.conductor_session.is_empty());
    }

    #[test]
    fn a_task_from_before_codex_conducted_is_claudes() {
        let old =
            r#"{"id":"t1","source":"x","created":"2026-11-01T10:00:00Z","conductor_session":"s"}"#;
        let t: super::super::Task = serde_json::from_str(old).unwrap();
        assert_eq!(t.conductor_provider, Provider::Claude);
    }

    #[test]
    fn a_codex_conductor_runs_read_only_with_its_bus() {
        let bus = bus_args("t1", CONDUCTOR);
        let a = codex_conductor_args("", "/bin/lama", &bus, "SYS", "-- fix it", false);
        assert_eq!(&a[..3], ["exec", "--json", "--skip-git-repo-check"]);
        assert!(a.contains(&"sandbox_mode=\"read-only\"".to_string()));
        assert!(a.contains(&"approval_policy=\"never\"".to_string()));
        assert!(a.contains(&"mcp_servers.lama.command=\"/bin/lama\"".to_string()));
        assert!(a.iter().any(
            |x| x.starts_with("mcp_servers.lama.args=[\"bus\"") && x.contains("\"conductor\"")
        ));
        // The rules open the first prompt, which is one argument and never
        // starts like a flag.
        assert_eq!(a.last().map(String::as_str), Some("SYS\n---\n\n-- fix it"));

        let a = codex_conductor_args("019a-s", "/bin/lama", &bus, "SYS", "[lama bus] hi", true);
        assert_eq!(&a[..3], ["exec", "resume", "--json"]);
        assert!(a.contains(&"sandbox_mode=\"read-only\"".to_string()));
        assert_eq!(&a[a.len() - 2..], ["019a-s", "[lama bus] hi"]);
    }

    #[test]
    fn uuids_are_v4() {
        let u = uuid().unwrap();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(u.bytes().all(|b| b == b'-' || b.is_ascii_hexdigit()));
        assert!(parts[2].starts_with('4'));
        assert!("89ab".contains(&parts[3][..1]));
        assert_ne!(u, uuid().unwrap());
    }

    #[test]
    fn the_conductor_gets_its_bus_and_projects_but_keeps_your_servers() {
        let a = conductor_args(
            "s-1",
            Path::new("/m/mcp-conductor.json"),
            "SYS",
            &["/p/a".into(), "/p/b".into()],
            "fix it; rm -rf x",
            false,
        );
        assert_eq!(
            &a[..5],
            [
                "-p",
                "--output-format=stream-json",
                "--verbose",
                "--session-id",
                "s-1"
            ]
        );
        assert!(a.contains(&"--mcp-config=/m/mcp-conductor.json".to_string()));
        assert!(
            a.contains(&"--add-dir=/p/a".to_string()) && a.contains(&"--add-dir=/p/b".to_string())
        );
        assert!(
            a.iter()
                .any(|x| x.starts_with("--allowed-tools=") && x.contains("mcp__lama"))
        );
        assert!(
            a.iter()
                .any(|x| x.starts_with("--disallowed-tools=") && x.contains("Edit"))
        );
        assert_eq!(a.last().map(String::as_str), Some("fix it; rm -rf x"));
        assert!(!a.iter().any(|x| x.contains("strict")));
    }

    #[test]
    fn a_woken_conductor_resumes_its_session() {
        let a = conductor_args(
            "s-1",
            Path::new("/m/c.json"),
            "SYS",
            &[],
            "[lama bus] hi",
            true,
        );
        assert_eq!(&a[3..5], ["--resume", "s-1"]);
        assert_eq!(a.last().map(String::as_str), Some("[lama bus] hi"));
    }

    #[test]
    fn a_claude_worker_command_line() {
        let a = claude_args(
            "s-2",
            "sonnet",
            Path::new("/m/mcp-api.json"),
            "SYS",
            "go now",
        );
        assert_eq!(
            a,
            [
                "--session-id",
                "s-2",
                "--model",
                "sonnet",
                "--mcp-config=/m/mcp-api.json",
                "--append-system-prompt",
                "SYS",
                "go now"
            ]
        );
        let a = claude_args("s-2", "", Path::new("/m/x.json"), "SYS", "go");
        assert!(!a.contains(&"--model".to_string()));
    }

    #[test]
    fn a_codex_worker_command_line() {
        let bus = bus_args("m1", "qa");
        let a = codex_args("", "/opt/la\"ma", &bus, "two words");
        assert_eq!(
            a,
            [
                "-c",
                r#"mcp_servers.lama.command="/opt/la\"ma""#,
                "-c",
                r#"mcp_servers.lama.args=["bus","--task","m1","--agent","qa"]"#,
                "two words"
            ]
        );
        assert_eq!(&codex_args("o3", "x", &bus, "p")[..2], ["-m", "o3"]);
    }

    #[test]
    fn a_resumed_codex_worker_gets_its_bus_back() {
        let bus = bus_args("t1", "api");
        let a = codex_resume_args("s-9", "gpt-5", "/bin/lama", &bus);
        assert_eq!(a[0], "-c");
        assert_eq!(a[1], r#"mcp_servers.lama.command="/bin/lama""#);
        assert_eq!(
            a[3],
            r#"mcp_servers.lama.args=["bus","--task","t1","--agent","api"]"#
        );
        assert_eq!(a[5], r#"model="gpt-5""#);
        assert_eq!(&a[6..], ["resume", "s-9"]);
    }

    #[test]
    fn a_resumed_claude_worker_gets_its_bus_and_brief_back() {
        let a = claude_resume_args("s-1", "", Path::new("/t/mcp-api.json"), "be the API owner");
        assert_eq!(
            a,
            [
                "--resume",
                "s-1",
                "--mcp-config=/t/mcp-api.json",
                "--append-system-prompt",
                "be the API owner"
            ]
        );
    }

    #[test]
    fn toml_strings_stay_strings() {
        assert_eq!(toml_string(r"C:\a"), r#""C:\\a""#);
        assert_eq!(toml_string("a\"b"), r#""a\"b""#);
        assert_eq!(toml_string("a\nb"), r#""a\u000Ab""#);
    }

    #[test]
    fn models_are_plain_ids() {
        for ok in ["", "opus", "gpt-5.1-codex", "claude-opus-4[1m]", "a:b_c"] {
            assert!(valid_model(ok), "{ok}");
        }
        for bad in ["--yolo", "a b", "x;y", "$(id)", "a/b", &"x".repeat(65)] {
            assert!(!valid_model(bad), "{bad}");
        }
    }

    #[test]
    fn only_listed_projects_are_allowed() {
        let dir = temp("allow");
        let (ok, other) = (dir.join("ok"), dir.join("other"));
        std::fs::create_dir_all(&ok).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let projects = [ok.clone()];
        assert_eq!(
            allowed_project(&ok, &projects).unwrap(),
            ok.canonicalize().unwrap()
        );
        assert!(allowed_project(&ok.join("../other"), &projects).is_err());
        assert!(allowed_project(&other, &projects).is_err());
        assert!(allowed_project(&dir.join("missing"), &projects).is_err());
        // A path that leads into a project through `..` is still that project.
        assert!(allowed_project(&other.join("../ok"), &projects).is_ok());
    }

    #[test]
    fn a_writer_gets_its_own_worktree_and_branch() {
        let repo = temp("repo");
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-q"]) {
            return; // no git here
        }
        assert!(run(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init"
        ]));
        let (cwd, branch) = worktree(&repo, "m1", "api").unwrap();
        assert_eq!(branch, "lama/m1/api");
        assert_eq!(cwd, repo.join(".lama/worktrees/m1-api"));
        assert!(cwd.join(".git").exists());
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".lama/").count(), 1);
        exclude_lama(&repo).unwrap();
        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".lama/").count(), 1);
        assert_eq!(
            crate::provider::project_dir(&cwd).canonicalize().unwrap(),
            repo.canonicalize().unwrap()
        );
        assert!(worktree(&temp("plain"), "m1", "api").is_err());
    }
}
