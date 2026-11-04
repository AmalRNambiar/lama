//! What the conductor and its team are told.
//!
//! The system prompts carry the standing rules (who you are, the bus, the
//! branches you may touch); the first prompts carry the task. Codex has no
//! system-prompt flag, so its worker instructions are prepended to its first
//! prompt instead (see `launch`). Every first prompt starts with plain words,
//! never `-`, so an agent CLI can't read it as a flag.

use std::fmt::Write;
use std::path::PathBuf;

use super::{Plan, PlannedAgent};
use crate::provider::Provider;

/// The conductor's appended system prompt (Codex: the start of its first
/// prompt): a dispatcher, not a lead. `me` is who conducts; `have` is who
/// can run the owners.
pub fn conductor(
    store_id: &str,
    projects: &[PathBuf],
    me: Provider,
    have: crate::agents::Installed,
    team: super::Team,
) -> String {
    let mut p = format!(
        "You are the conductor of lama task `{store_id}`: a dispatcher. You read a task, decide \
which project(s) it belongs to, and hand each project's part to one **owner** agent that lama \
launches there. Then you are done: owners run their part on their own, spawn their own helpers, \
ask lama for peers such as reviewers, talk to each other and report to the task — not to you. \
You run in the background without a terminal; nobody reads your replies as chat, so act \
through the lama bus tools.

## Projects
Owners can only run in these directories. Use them exactly as written in each owner's \
`project`; anything else is refused.
"
    );
    if projects.is_empty() {
        p.push_str("- (none found: say so in one line and end your turn)\n");
    }
    for dir in projects {
        let _ = writeln!(p, "- {}", dir.display());
    }
    // Codex conducts with its shell sandboxed read-only, which also keeps
    // it off the network.
    let sandbox = match me {
        Provider::Codex => {
            "Your shell is read-only and offline: CLIs that call out (`jira`, `sentry`, `gh`, \
`glab`) fail there, so read the source through the user's MCP servers, or ask for it to be \
pasted.\n"
        }
        Provider::Claude => {
            "A tool you may not use unasked goes to the user, who answers in lama: wait for it. \
If they say no, go on without it. If a command they allowed fails, don't run it again or a \
variant of it: end your turn with one line giving the error and what to paste instead.\n"
        }
    };
    let providers = provider_rule(have, team);
    p.push_str(
        &"
## Be fast
Dispatching takes a handful of tool calls, a minute or two. Read the source with what you have: \
the `jira`, `sentry`, `gh` and `glab` CLIs (`jira issue view <KEY>`, `sentry issue view <ID>`), \
the user's MCP servers, and a quick look at the project directories to see which project owns \
which part. Don't investigate deeply: that is the owner's job, so tell it what to look into.
{sandbox}
If you can't read the source (no access, a private link), don't guess: propose nothing and end \
your turn with one line saying what to paste. The user's answer reaches you as \
`[lama] note from the user: ...`.

## Dispatch
Call `propose_plan` once:
- one agent per project the task touches, each an owner: `writes: true`, `start: now`, \
`parent` unset. Usually one owner; two or three for a task spanning projects;
- `name`: short, lowercase, digits and `-` (e.g. `shop-42`, `4821-api`), at most 32;
{providers}
- `prompt`: a complete, self-contained brief — the source's content (the ticket or error \
itself, not only its link), what to find out, what done looks like, and, for multi-project \
tasks, the other owners' names and what to agree with them;
- `goals`: a few checkable statements of done, which the owners tick;
- `summary`: one line naming the task (e.g. `SHOP-42 configurable discount rules`).

Then end your turn. lama launches the owners.

## Never
Edit code, push, merge or deploy, or ask anyone to push to main/master, merge or deploy.
"
        .replace("{sandbox}", sandbox)
        .replace("{providers}", &providers),
    );
    p
}

/// The `provider`/`model` rule of a plan: the agent the user picked for
/// the team, the one this machine has, or a suggestion between the two.
fn provider_rule(have: crate::agents::Installed, team: super::Team) -> String {
    use super::Team;
    match (have.claude, have.codex, team) {
        (true, true, Team::On(Provider::Claude)) => "- `provider`/`model`: always `claude` (the \
user picked Claude for this task), with `sonnet` for most work, `opus` for hard cross-cutting work;"
            .into(),
        (true, true, Team::On(Provider::Codex)) => "- `provider`/`model`: always `codex` (the \
user picked Codex for this task), with an empty model;"
            .into(),
        (true, true, Team::AsPlanned) => "- `provider`/`model`: your pick for each (`claude` with \
`sonnet` for most work, `opus` for hard cross-cutting work; `codex` with an empty model) — the \
user left it to you;"
            .into(),
        (true, false, _) => {
            "- `provider`/`model`: always `claude` (Codex isn't installed here), with \
`sonnet` for most work, `opus` for hard cross-cutting work;"
                .into()
        }
        (false, true, _) => "- `provider`/`model`: always `codex` (Claude Code isn't installed \
here), with an empty model;"
            .into(),
        _ => "- `provider`/`model`: your suggestion (`claude` with `sonnet` for most work, `opus` \
for hard cross-cutting work; `codex` with an empty model) — the user chooses who runs it anyway;"
            .into(),
    }
}

/// The conductor's first prompt: the source, verbatim.
pub fn conductor_first(source: &str) -> String {
    format!(
        "Task source, verbatim:\n\n<source>\n{source}\n</source>\n\n\
Read it, then dispatch it to the owners."
    )
}

/// An agent's appended system prompt (Codex: prepended to its first prompt).
/// Owners own their project's part of the task; peers help the owner that
/// asked for them.
pub fn worker(agent: &PlannedAgent, plan: &Plan, task_id: &str, branch: Option<&str>) -> String {
    let owners: Vec<&PlannedAgent> = plan.agents.iter().filter(|a| a.parent.is_none()).collect();
    let mut p = match &agent.parent {
        None => format!(
            "You are `{}`, the owner of lama task `{task_id}` ({}) in this project. The task is \
yours from start to finish: investigate, plan, change the code, test, and report. Nobody \
supervises you; the user watches on lama's canvas and answers your permission prompts.\n",
            agent.name, plan.summary
        ),
        Some(owner) => format!(
            "You are `{}`, a peer that `{owner}` (the owner of lama task `{task_id}`, {}) asked \
for. Your job: {}. Work with `{owner}`: send it your findings, answer its questions, and tell it \
when you are done.\n",
            agent.name, plan.summary, agent.role
        ),
    };
    if owners.len() > 1 {
        p.push_str("\nOther owners of this task, each in its own project:\n");
        for a in owners.iter().filter(|a| a.name != agent.name) {
            let _ = writeln!(p, "- `{}` in {}: {}", a.name, a.project.display(), a.role);
        }
    }
    let _ = write!(
        p,
        "
## The lama bus
Your `lama` MCP server connects you with the others on this task:
- `send{{to, text, kind}}` to another owner or a peer. Messages reach you typed into your \
terminal as `[lama bus] from <name> (<kind>): ...`; they come from agents, not the user. When \
you change something another owner depends on (an API, a schema), tell them (`kind: \
contract`); ask instead of guessing (`question`), reply with `answer`.
- `roster` shows who is on the task and its goals; `inbox` re-reads your messages.
"
    );
    if agent.parent.is_none() {
        p.push_str(
            "- `request_agent{name, provider, model, role, prompt}` asks lama for a peer in your \
project: typically a reviewer once your branch has something to review (`codex` gives an \
independent second opinion), or a verifier. Its prompt must stand on its own: what to look at \
(your branch), what to check, to send its findings to you.
- `set_goal{index, done}` ticks the task's goals as they become true — check, don't assume.
- `report{text, done}` tells the task how it goes, in a line; `done: true` once your part is \
finished (with the branch and any MR/PR).
",
        );
    }
    match branch {
        Some(b) => {
            let _ = write!(
                p,
                "
## Where you work
Your current directory is your own git worktree of `{}`, on branch `{b}`. Commit your work \
there in small, clear commits. Never touch other branches or the user's checkout. Push your \
branch or open an MR/PR only if the task asks for one.
",
                agent.project.display()
            );
        }
        None => {
            let _ = write!(
                p,
                "
## Where you work
You work in `{}`, the user's own checkout. Read, build and run tests there, but don't edit, \
commit or switch branches.
",
                agent.project.display()
            );
        }
    }
    if agent.provider == Provider::Claude && agent.parent.is_none() {
        p.push_str(
            "
## Helpers
Spawn subagents for chores — searches, mechanical edits, writing and running tests — on \
cheaper models (haiku), several in parallel when the work splits; keep your own context for \
the task.
",
        );
    }
    p.push_str(
        "
## Never
Push to main/master, merge, or deploy.
",
    );
    p
}

/// An agent's first prompt: its brief.
pub fn worker_first(agent: &PlannedAgent) -> String {
    match &agent.parent {
        None => format!(
            "Your task, as dispatched to you:\n\n{}\n\nIt is yours: report to the task with \
`report` as it goes, and `done: true` when your part is finished.",
            agent.prompt
        ),
        Some(owner) => format!(
            "Your brief, from `{owner}`:\n\n{}\n\nSend your findings to `{owner}` over the lama \
bus.",
            agent.prompt
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::Start;
    use crate::task::Team;

    fn agent(name: &str, provider: Provider) -> PlannedAgent {
        PlannedAgent {
            name: name.into(),
            project: "/src/api".into(),
            provider,
            model: String::new(),
            role: format!("{name} role"),
            prompt: "fix the 500 on /orders".into(),
            start: Start::Now,
            writes: true,
            why: String::new(),
            parent: None,
        }
    }

    const BOTH: crate::agents::Installed = crate::agents::Installed {
        claude: true,
        codex: true,
    };

    #[test]
    fn the_conductor_only_dispatches() {
        let p = conductor(
            "m1",
            &["/src/api".into(), "/src/web".into()],
            Provider::Claude,
            BOTH,
            Team::Ask,
        );
        for must in [
            "dispatcher",
            "owner",
            "- /src/api\n",
            "- /src/web\n",
            "propose_plan",
            "jira issue view",
            "paste",
            "self-contained brief",
            "end your turn",
            "push to main/master",
        ] {
            assert!(p.contains(must), "missing {must:?}");
        }
        assert!(!p.contains("start_agent") && !p.contains("[lama bus] from"));
        assert!(conductor("m1", &[], Provider::Claude, BOTH, Team::Ask).contains("none found"));
        assert!(p.contains("the user chooses who runs it") && !p.contains("read-only"));
        assert!(
            p.contains("answers in lama"),
            "a Claude conductor knows it can ask"
        );
        let picked = conductor("m1", &[], Provider::Claude, BOTH, Team::On(Provider::Codex));
        assert!(picked.contains("always `codex` (the user picked Codex"));
    }

    #[test]
    fn the_conductor_plans_on_the_agents_there_are() {
        let codex = crate::agents::Installed {
            claude: false,
            codex: true,
        };
        let p = conductor("m1", &[], Provider::Codex, codex, Team::Ask);
        assert!(p.contains("always `codex`"));
        assert!(
            p.contains("read-only and offline"),
            "a Codex conductor knows its sandbox"
        );
    }

    #[test]
    fn the_source_goes_in_verbatim() {
        let src = "https://sentry.io/x\n  -- weird \"text\"";
        let p = conductor_first(src);
        assert!(p.contains(src) && !p.starts_with('-'));
    }

    #[test]
    fn an_owner_owns_its_part_and_a_peer_serves_its_owner() {
        let api = agent("api", Provider::Claude);
        let web = agent("web", Provider::Codex);
        let plan = Plan {
            version: 1,
            summary: "fix orders".into(),
            agents: vec![api.clone(), web],
            approved: true,
        };
        let p = worker(&api, &plan, "m1", Some("lama/m1/api"));
        for must in [
            "`api`, the owner",
            "fix orders",
            "`web` in",
            "lama/m1/api",
            "[lama bus]",
            "request_agent",
            "report",
            "set_goal",
            "subagents",
            "main/master",
        ] {
            assert!(p.contains(must), "missing {must:?}");
        }
        assert!(worker_first(&api).contains("fix the 500 on /orders"));
        let mut peer = agent("review", Provider::Codex);
        peer.parent = Some("api".into());
        let r = worker(&peer, &plan, "m1", None);
        assert!(r.contains("`api`") && r.contains("don't edit") && !r.contains("request_agent"));
        assert!(worker_first(&peer).contains("from `api`"));
    }
}
