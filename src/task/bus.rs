//! The lama bus: an MCP server over stdio that one task agent runs, so
//! the conductor and its workers can talk to each other, and the conductor
//! can propose the team. It speaks newline-delimited JSON-RPC and only ever
//! writes inside the task directory; lama does the delivering and
//! launching.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

use super::{
    Answer, Approval, CONDUCTOR, Goal, Kind, Message, Plan, PlannedAgent, Request, Start, Store,
    valid_name,
};

/// The protocol version offered when the client names none.
const DEFAULT_PROTOCOL: &str = "2025-06-18";
/// Longest message text, so one send can't flood a teammate's prompt.
const MAX_TEXT: usize = 2000;
const MAX_AGENTS: usize = 12;
const DEFAULT_INBOX: usize = 20;
/// How long the conductor waits on your answer to a tool it asked for
/// before it goes on without it. lama raises Claude's MCP timeout past it.
pub const APPROVAL_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

const INSTRUCTIONS: &str = "The lama bus connects you to your teammates in this task. \
Use `roster` to see the team and goals, `send` to message a teammate, the conductor or \"all\", \
and `inbox` to re-read messages sent to you. lama types new messages into your session when you \
are idle, so you rarely need to poll. Keep messages short and concrete.";

/// Serve one agent's bus until its stdin closes.
pub fn serve(store: Store, me: String) -> std::io::Result<()> {
    let mut bus = Bus { store, me };
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => bus.handle(req),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(reply) = reply {
            let mut out = stdout.lock();
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

struct Bus {
    store: Store,
    me: String,
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn text_result(text: impl Into<String>, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text.into()}], "isError": is_error})
}

impl Bus {
    /// One request in, its reply out; notifications (no `id`) get none.
    fn handle(&mut self, req: Value) -> Option<Value> {
        let id = req.get("id")?.clone();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => json!({
                "protocolVersion": params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "lama-bus", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            }),
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.tools()}),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(name, &args) {
                    Some(Ok(text)) => text_result(text, false),
                    Some(Err(text)) => text_result(text, true),
                    None => return Some(error(id, -32602, &format!("unknown tool {name:?}"))),
                }
            }
            _ => return Some(error(id, -32601, &format!("method not found: {method}"))),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn conductor(&self) -> bool {
        self.me == CONDUCTOR
    }

    /// The tools this agent may use: the conductor's are hidden from workers.
    fn tools(&self) -> Vec<Value> {
        let mut tools = vec![
            tool(
                "send",
                "Send a message to another agent on this task (another project's owner, or a \
                 peer). lama delivers it into the recipient's session when they are idle, so \
                 keep it short, concrete and self-contained.",
                json!({
                    "to": {"type": "string", "description":
                        "An agent's name (see roster), or \"all\" for everyone on the task."},
                    "text": {"type": "string", "description":
                        "The message, at most 2000 characters."},
                    "kind": {"type": "string",
                        "enum": ["assign", "contract", "question", "answer", "blocker", "report"],
                        "description": "assign: work for them. contract: an interface or \
                        agreement others rely on. question / answer. blocker: you cannot \
                        continue. report: progress or results (default)."},
                }),
                &["to", "text"],
            ),
            tool(
                "inbox",
                "Re-read recent messages sent to you or to \"all\", oldest first.",
                json!({"limit": {"type": "integer", "description":
                    "How many of the most recent to show (default 20)."}}),
                &[],
            ),
            tool(
                "roster",
                "List the planned team (name, role, project, provider, model, start, whether \
                 launched) and the task's goals.",
                json!({}),
                &[],
            ),
        ];
        if self.owner() {
            tools.push(tool(
                "report",
                "Tell the task how it goes, in a line: it shows on the task's row in lama. \
                 Set done when your part is finished (name the branch and any MR/PR).",
                json!({
                    "text": {"type": "string", "description": "One line, at most 2000 characters."},
                    "done": {"type": "boolean", "description": "Your part is finished."},
                }),
                &["text"],
            ));
            tools.push(tool(
                "request_agent",
                "Ask lama for a peer in your project — a reviewer once your branch has \
                 something to review, a verifier — which lama launches in your worktree, \
                 reading your branch. It talks to you over the bus.",
                json!({
                    "name": {"type": "string", "description":
                        "Lowercase letters, digits and '-', 1-32 long, new on this task."},
                    "provider": {"type": "string", "enum": ["claude", "codex"], "description":
                        "codex gives an independent second opinion."},
                    "model": {"type": "string", "description": "Model id, or empty for the default."},
                    "role": {"type": "string", "description": "A few words: what it is for."},
                    "prompt": {"type": "string", "description":
                        "Its brief, standing on its own: what to look at, what to check, to send \
                        its findings to you."},
                    "why": {"type": "string"},
                }),
                &["name", "provider", "role", "prompt"],
            ));
            tools.push(tool(
                "set_goal",
                "Tick a goal done (or undone) by its 0-based index in the roster's goal list.",
                json!({
                    "index": {"type": "integer", "description": "0-based goal index."},
                    "done": {"type": "boolean"},
                }),
                &["index", "done"],
            ));
        }
        if self.conductor() {
            tools.push(tool(
                "propose_plan",
                "Dispatch the task: one owner per project it touches. lama launches them on \
                 whoever the user picked to run them.",
                json!({
                    "summary": {"type": "string", "description":
                        "What the team will do and why this split."},
                    "agents": {"type": "array", "maxItems": MAX_AGENTS, "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description":
                                "Lowercase letters, digits and '-', 1-32 long, unique; not \
                                \"conductor\" or \"all\"."},
                            "project": {"type": "string", "description":
                                "Absolute path of the project directory it works in."},
                            "provider": {"type": "string", "enum": ["claude", "codex"]},
                            "model": {"type": "string", "description":
                                "Model id, or empty for the CLI's default."},
                            "role": {"type": "string", "description":
                                "A few words: what it is for."},
                            "prompt": {"type": "string", "description":
                                "Its first prompt: the task, the context and who to talk to."},
                            "start": {"type": "string", "enum": ["now", "later"], "description":
                                "now: as soon as the plan is proposed. later: when you call \
                                start_agent (e.g. a reviewer once there is code)."},
                            "writes": {"type": "boolean", "description":
                                "True if it changes code; it then gets its own git worktree."},
                            "why": {"type": "string", "description":
                                "Why this agent and model."},
                        },
                        "required": ["name", "project", "provider", "role", "prompt"],
                    }},
                    "goals": {"type": "array", "items": {"type": "string"}, "description":
                        "What has to be true for the task to be done."},
                }),
                &["summary", "agents", "goals"],
            ));
            tools.push(tool(
                "approve",
                "lama's permission prompt: Claude Code calls it, not you.",
                json!({
                    "tool_name": {"type": "string"},
                    "input": {"type": "object"},
                    "tool_use_id": {"type": "string"},
                }),
                &["tool_name", "input"],
            ));
        }
        tools
    }

    /// An owner: an agent on the plan that nobody asked for (the conductor
    /// dispatched it).
    fn owner(&self) -> bool {
        self.store.plan().is_some_and(|p| {
            p.agents
                .iter()
                .any(|a| a.name == self.me && a.parent.is_none())
        })
    }

    /// `None` for a tool that does not exist (for this agent).
    fn call(&self, name: &str, args: &Value) -> Option<Result<String, String>> {
        if matches!(name, "propose_plan" | "approve") && !self.conductor() {
            return Some(Err(format!("{name} is only for the conductor")));
        }
        if matches!(name, "report" | "request_agent" | "set_goal") && !self.owner() {
            return Some(Err(format!("{name} is only for the task's owners")));
        }
        Some(match name {
            "send" => self.send(args),
            "inbox" => Ok(self.inbox(args)),
            "roster" => Ok(self.roster()),
            "propose_plan" => self.propose_plan(args),
            "approve" => self.approve(args, APPROVAL_WAIT),
            "report" => self.report(args),
            "request_agent" => self.request_agent(args),
            "set_goal" => self.set_goal(args),
            _ => return None,
        })
    }

    fn send(&self, args: &Value) -> Result<String, String> {
        let to = str_arg(args, "to")?;
        let text = str_arg(args, "text")?;
        let kind = args.get("kind").and_then(Value::as_str).unwrap_or("report");
        if ![
            "assign", "contract", "question", "answer", "blocker", "report",
        ]
        .contains(&kind)
        {
            return Err(format!(
                "unknown kind {kind:?}; use assign, contract, question, answer, blocker or report"
            ));
        }
        if text.trim().is_empty() {
            return Err("text is empty".into());
        }
        if text.chars().count() > MAX_TEXT {
            return Err(format!(
                "text is {} characters; the limit is {MAX_TEXT}. Shorten it.",
                text.chars().count()
            ));
        }
        if to == self.me {
            return Err("that is you; send to a teammate".into());
        }
        let team = self.team();
        if to != "all" && !team.contains(&to.to_string()) {
            let mut valid = vec!["all".to_string()];
            valid.extend(team);
            return Err(format!(
                "no recipient {to:?}. Valid names: {}",
                valid.join(", ")
            ));
        }
        let msg = Message {
            at: chrono::Utc::now(),
            from: self.me.clone(),
            to: to.to_string(),
            kind: Kind::parse(kind),
            text: text.to_string(),
            done: false,
        };
        self.store.send(&msg).map_err(|e| e.to_string())?;
        Ok(format!(
            "Sent to {to}. lama will deliver it when they are idle; you do not need to resend."
        ))
    }

    /// The planned agents' names.
    fn team(&self) -> Vec<String> {
        self.store
            .plan()
            .map(|p| p.agents.into_iter().map(|a| a.name).collect())
            .unwrap_or_default()
    }

    fn inbox(&self, args: &Value) -> String {
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_INBOX, |n| n.max(1) as usize);
        let mine: Vec<Message> = self
            .store
            .messages()
            .into_iter()
            .filter(|m| m.from != self.me && (m.to == self.me || m.to == "all"))
            .collect();
        if mine.is_empty() {
            return "No messages.".into();
        }
        mine[mine.len().saturating_sub(limit)..]
            .iter()
            .map(|m| {
                let to = if m.to == "all" { " (to all)" } else { "" };
                format!(
                    "[{}] {}{to} ({}): {}",
                    m.at.format("%H:%M:%S"),
                    m.from,
                    format!("{:?}", m.kind).to_lowercase(),
                    m.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn roster(&self) -> String {
        let launched = self.store.launched();
        let mut out = String::new();
        match self.store.plan() {
            None => out.push_str("No plan yet.\n"),
            Some(plan) => {
                let state = if plan.approved {
                    "approved"
                } else {
                    "launching"
                };
                out.push_str(&format!(
                    "Plan v{} ({state}): {}\n",
                    plan.version, plan.summary
                ));
                for a in &plan.agents {
                    let start = match a.start {
                        Start::Now => "now",
                        Start::Later => "later",
                    };
                    let model = if a.model.is_empty() {
                        "default"
                    } else {
                        &a.model
                    };
                    out.push_str(&format!(
                        "- {} ({}): {} | {} | {:?} {model} | start {start} | {}\n",
                        a.name,
                        a.role,
                        if a.writes { "writes" } else { "read-only" },
                        a.project.display(),
                        a.provider,
                        if launched.contains_key(&a.name) {
                            "launched"
                        } else {
                            "not launched"
                        },
                    ));
                }
            }
        }
        let goals = self.store.goals();
        if goals.is_empty() {
            out.push_str("No goals.");
        } else {
            out.push_str("Goals:\n");
            for (i, g) in goals.iter().enumerate() {
                out.push_str(&format!(
                    "{i}. [{}] {}\n",
                    if g.done { "x" } else { " " },
                    g.text
                ));
            }
        }
        out.trim_end().to_string()
    }

    fn propose_plan(&self, args: &Value) -> Result<String, String> {
        let summary = str_arg(args, "summary")?.to_string();
        let raw = args
            .get("agents")
            .and_then(Value::as_array)
            .ok_or("agents must be an array")?;
        if raw.is_empty() || raw.len() > MAX_AGENTS {
            return Err(format!(
                "propose 1 to {MAX_AGENTS} agents, not {}",
                raw.len()
            ));
        }
        let mut agents: Vec<PlannedAgent> = Vec::new();
        for (i, v) in raw.iter().enumerate() {
            let a: PlannedAgent =
                serde_json::from_value(v.clone()).map_err(|e| format!("agents[{i}]: {e}"))?;
            if !valid_name(&a.name) || a.name == CONDUCTOR || a.name == "all" {
                return Err(format!(
                    "agents[{i}]: name {:?} must be 1-32 lowercase letters, digits or '-', \
                     and not \"conductor\" or \"all\"",
                    a.name
                ));
            }
            if agents.iter().any(|b| b.name == a.name) {
                return Err(format!("agents[{i}]: duplicate name {:?}", a.name));
            }
            if !a.project.is_absolute() {
                return Err(format!(
                    "agents[{i}]: project {} must be an absolute path",
                    a.project.display()
                ));
            }
            if a.role.trim().is_empty() || a.prompt.trim().is_empty() {
                return Err(format!("agents[{i}]: role and prompt must not be empty"));
            }
            agents.push(a);
        }
        let goals: Vec<Goal> = args
            .get("goals")
            .and_then(Value::as_array)
            .ok_or("goals must be an array of strings")?
            .iter()
            .map(|g| {
                g.as_str()
                    .map(|text| Goal {
                        text: text.to_string(),
                        done: false,
                    })
                    .ok_or("goals must be an array of strings")
            })
            .collect::<Result<_, _>>()?;
        let version = self.store.plan().map_or(0, |p| p.version) + 1;
        let plan = Plan {
            version,
            summary,
            agents,
            approved: false,
        };
        self.store.set_plan(&plan).map_err(|e| e.to_string())?;
        self.store.set_goals(&goals).map_err(|e| e.to_string())?;
        Ok(format!(
            "Dispatched (plan v{version}). lama launches the owners on whoever the user picked \
             to run them. Your job is done: end your turn now."
        ))
    }

    /// The conductor's permission prompt (`--permission-prompt-tool`): the
    /// ask goes to lama, which shows it in the intake, and the conductor
    /// waits here for your answer — or goes on without the tool after
    /// `wait`.
    fn approve(&self, args: &Value, wait: std::time::Duration) -> Result<String, String> {
        let input = args.get("input").cloned().unwrap_or(json!({}));
        let tool = str_arg(args, "tool_name")?;
        if self.store.allowed_before(tool, &input) {
            return Ok(json!({"behavior": "allow", "updatedInput": input}).to_string());
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let a = Approval {
            id: format!("a{:x}", nanos as u64),
            tool: tool.to_string(),
            input: input.clone(),
        };
        self.store.ask_approval(&a).map_err(|e| e.to_string())?;
        let started = std::time::Instant::now();
        let answer = loop {
            if let Some(answer) = self.store.answer_of(&a.id) {
                break answer;
            }
            if started.elapsed() >= wait {
                let late = Answer {
                    allow: false,
                    note: "the user didn't answer in time".into(),
                };
                let _ = self.store.answer(&a.id, &late);
                break late;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        };
        let reply = if answer.allow {
            json!({"behavior": "allow", "updatedInput": input})
        } else {
            let why = if answer.note.trim().is_empty() {
                "The user said no.".to_string()
            } else {
                format!("The user said no: {}", answer.note.trim())
            };
            json!({"behavior": "deny", "message": why})
        };
        Ok(reply.to_string())
    }

    fn report(&self, args: &Value) -> Result<String, String> {
        let text = str_arg(args, "text")?;
        if text.trim().is_empty() || text.chars().count() > MAX_TEXT {
            return Err(format!("text must be 1 to {MAX_TEXT} characters"));
        }
        let done = args.get("done").and_then(Value::as_bool).unwrap_or(false);
        self.store
            .send(&Message {
                at: chrono::Utc::now(),
                from: self.me.clone(),
                to: "task".into(),
                kind: Kind::Report,
                text: text.to_string(),
                done,
            })
            .map_err(|e| e.to_string())?;
        Ok(if done {
            "Reported done. Thanks — your part is finished.".into()
        } else {
            "Reported; it shows on the task's row.".into()
        })
    }

    fn request_agent(&self, args: &Value) -> Result<String, String> {
        let name = str_arg(args, "name")?;
        let plan = self.store.plan().ok_or("there is no plan")?;
        if !valid_name(name) || name == CONDUCTOR || name == "all" || name == "task" {
            return Err(format!(
                "name {name:?} must be 1-32 lowercase letters, digits or '-'"
            ));
        }
        if plan.agents.iter().any(|a| a.name == name)
            || self.store.requests().iter().any(|r| r.start == name)
        {
            return Err(format!(
                "{name:?} is already on this task; pick another name"
            ));
        }
        let provider = match str_arg(args, "provider")? {
            "claude" => crate::provider::Provider::Claude,
            "codex" => crate::provider::Provider::Codex,
            other => return Err(format!("provider {other:?}: use claude or codex")),
        };
        let role = str_arg(args, "role")?;
        let prompt = str_arg(args, "prompt")?;
        if role.trim().is_empty() || prompt.trim().is_empty() {
            return Err("role and prompt must not be empty".into());
        }
        let me = plan
            .agents
            .iter()
            .find(|a| a.name == self.me)
            .ok_or("you are not on the plan")?;
        let agent = PlannedAgent {
            name: name.to_string(),
            project: me.project.clone(),
            provider,
            model: args
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            role: role.to_string(),
            prompt: prompt.to_string(),
            start: Start::Now,
            writes: false,
            why: args
                .get("why")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            parent: Some(self.me.clone()),
        };
        self.store
            .request(&Request {
                at: chrono::Utc::now(),
                start: name.to_string(),
                agent: Some(agent),
            })
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "Requested. lama launches {name} in your worktree and it will message you."
        ))
    }

    fn set_goal(&self, args: &Value) -> Result<String, String> {
        let index = args
            .get("index")
            .and_then(Value::as_u64)
            .ok_or("index must be a non-negative integer")? as usize;
        let done = args
            .get("done")
            .and_then(Value::as_bool)
            .ok_or("done must be true or false")?;
        let mut goals = self.store.goals();
        let n = goals.len();
        let goal = goals
            .get_mut(index)
            .ok_or_else(|| format!("no goal {index}; there are {n} (0-based)"))?;
        goal.done = done;
        let text = goal.text.clone();
        self.store.set_goals(&goals).map_err(|e| e.to_string())?;
        Ok(format!(
            "Goal {index} marked {}: {text}",
            if done { "done" } else { "not done" }
        ))
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{key} is required and must be a string"))
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
    })
}

// Tasks reach agents through kitty or tmux and use Unix paths, so their
// tests run where those do; Windows has neither.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn bus(name: &str, me: &str) -> Bus {
        let dir = std::env::temp_dir().join(format!("lama-bus-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Bus {
            store: Store::at(dir),
            me: me.into(),
        }
    }

    fn rpc(bus: &mut Bus, method: &str, params: Value) -> Value {
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        bus.handle(req).unwrap()
    }

    fn call(bus: &mut Bus, tool: &str, args: Value) -> (bool, String) {
        let r = rpc(bus, "tools/call", json!({"name": tool, "arguments": args}));
        let r = &r["result"];
        (
            r["isError"].as_bool().unwrap(),
            r["content"][0]["text"].as_str().unwrap().to_string(),
        )
    }

    fn plan_args(start: &str) -> Value {
        json!({
            "summary": "fix it",
            "agents": [{
                "name": "api", "project": "/p", "provider": "claude",
                "role": "API fix", "prompt": "go", "start": start,
            }],
            "goals": ["tests pass"],
        })
    }

    fn names(bus: &Bus) -> Vec<String> {
        bus.tools()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn initialize_echoes_the_protocol_version() {
        let mut b = bus("init", CONDUCTOR);
        let r = rpc(
            &mut b,
            "initialize",
            json!({"protocolVersion": "2024-11-05"}),
        );
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        assert_eq!(r["result"]["serverInfo"]["name"], "lama-bus");
        let r = rpc(&mut b, "initialize", json!({}));
        assert_eq!(r["result"]["protocolVersion"], DEFAULT_PROTOCOL);
        assert!(
            b.handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
        let r = rpc(&mut b, "nope", json!({}));
        assert_eq!(r["error"]["code"], -32601);
    }

    #[test]
    fn each_role_sees_its_own_tools() {
        let mut c = bus("tools-c", CONDUCTOR);
        assert!(names(&c).contains(&"propose_plan".to_string()));
        call(&mut c, "propose_plan", plan_args("now"));
        // The owner shares the task's directory with the conductor.
        let mut owner = Bus {
            store: c.store.clone(),
            me: "api".into(),
        };
        assert_eq!(
            names(&owner),
            [
                "send",
                "inbox",
                "roster",
                "report",
                "request_agent",
                "set_goal"
            ]
        );
        let (err, _) = call(&mut owner, "propose_plan", plan_args("now"));
        assert!(err, "only the conductor dispatches");
        let mut stranger = Bus {
            store: c.store.clone(),
            me: "who".into(),
        };
        assert_eq!(names(&stranger), ["send", "inbox", "roster"]);
        let (err, _) = call(&mut stranger, "set_goal", json!({"index": 0, "done": true}));
        assert!(err);
    }

    #[test]
    fn an_owner_reports_to_its_task_and_asks_for_a_peer() {
        let mut c = bus("owner", CONDUCTOR);
        call(&mut c, "propose_plan", plan_args("now"));
        let mut owner = Bus {
            store: c.store.clone(),
            me: "api".into(),
        };
        let (err, _) = call(
            &mut owner,
            "report",
            json!({"text": "MR !812", "done": true}),
        );
        assert!(!err);
        let m = owner.store.messages().pop().unwrap();
        assert_eq!((m.to.as_str(), m.done), ("task", true));
        let peer = json!({"name": "reviewer", "provider": "codex", "role": "review", "prompt": "review my branch"});
        let (err, text) = call(&mut owner, "request_agent", peer.clone());
        assert!(!err, "{text}");
        let r = owner.store.requests().pop().unwrap();
        let a = r.agent.unwrap();
        assert_eq!(a.parent.as_deref(), Some("api"));
        assert_eq!(a.project, std::path::PathBuf::from("/p"));
        assert!(!a.writes);
        let (err, _) = call(&mut owner, "request_agent", peer);
        assert!(err, "a name is taken once");
    }

    #[test]
    fn the_conductor_waits_on_your_answer_to_a_tool() {
        let c = bus("approve", CONDUCTOR);
        let store = c.store.clone();
        let answering = std::thread::spawn(move || {
            loop {
                if let Some(a) = store.open_approvals().pop() {
                    assert_eq!(a.describe(), "run `git log`");
                    let no = Answer {
                        allow: false,
                        note: "use rg".into(),
                    };
                    store.answer(&a.id, &no).unwrap();
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        });
        let ask = json!({"tool_name": "Bash", "input": {"command": "git log"}});
        let wait = std::time::Duration::from_secs(10);
        let reply: Value = serde_json::from_str(&c.approve(&ask, wait).unwrap()).unwrap();
        answering.join().unwrap();
        assert_eq!(reply["behavior"], "deny");
        assert_eq!(reply["message"], "The user said no: use rg");
        assert!(c.store.open_approvals().is_empty());

        // Unanswered, it gives up and says so; a yes passes the input on.
        let reply: Value =
            serde_json::from_str(&c.approve(&ask, std::time::Duration::ZERO).unwrap()).unwrap();
        assert_eq!(reply["behavior"], "deny");
        let a = Approval {
            id: "a1".into(),
            tool: "Bash".into(),
            input: json!({}),
        };
        c.store.ask_approval(&a).unwrap();
        assert_eq!(c.store.open_approvals().len(), 1);
        let yes = Answer {
            allow: true,
            note: String::new(),
        };
        c.store.answer("a1", &yes).unwrap();
        assert!(c.store.open_approvals().is_empty());

        // Allowed once, the same command (reworded) isn't asked again.
        let a3 = Approval {
            id: "a3".into(),
            tool: "Bash".into(),
            input: json!({"command": "sentry issue view 1", "description": "Read it"}),
        };
        c.store.ask_approval(&a3).unwrap();
        c.store.answer("a3", &yes).unwrap();
        let again = json!({"tool_name": "Bash",
            "input": {"command": "sentry issue view 1", "description": "Read the issue"}});
        let reply: Value =
            serde_json::from_str(&c.approve(&again, std::time::Duration::ZERO).unwrap()).unwrap();
        assert_eq!(reply["behavior"], "allow");
        assert!(c.store.open_approvals().is_empty(), "nothing was asked");

        let mut worker = bus("approve-worker", "api");
        let (err, _) = call(&mut worker, "approve", ask);
        assert!(err, "only the conductor's prompts come through here");
    }

    #[test]
    fn send_rejects_unknown_names() {
        let mut b = bus("send", "api");
        let (err, text) = call(&mut b, "send", json!({"to": "ghost", "text": "hi"}));
        assert!(err && text.contains("all"), "{text}");
        let (err, _) = call(&mut b, "send", json!({"to": "all", "text": "hi"}));
        assert!(!err);
        assert_eq!(b.store.messages().len(), 1);
        let (err, _) = call(
            &mut b,
            "send",
            json!({"to": "all", "text": "x".repeat(2001)}),
        );
        assert!(err);
    }

    #[test]
    fn propose_plan_writes_version_one() {
        let mut b = bus("plan", CONDUCTOR);
        let (err, text) = call(&mut b, "propose_plan", plan_args("later"));
        assert!(!err, "{text}");
        let plan = b.store.plan().unwrap();
        assert_eq!((plan.version, plan.approved), (1, false));
        assert_eq!(b.store.goals().len(), 1);
        let mut bad = plan_args("now");
        bad["agents"][0]["project"] = json!("relative");
        let (err, _) = call(&mut b, "propose_plan", bad);
        assert!(err);
        assert_eq!(b.store.plan().unwrap().version, 1);
    }

    #[test]
    fn inbox_holds_only_my_messages() {
        let mut b = bus("inbox", "api");
        let send = |from: &str, to: &str, text: &str| {
            b.store
                .send(&Message {
                    at: chrono::Utc::now(),
                    from: from.into(),
                    to: to.into(),
                    kind: Kind::Report,
                    text: text.into(),
                    done: false,
                })
                .unwrap();
        };
        send("conductor", "api", "for me");
        send("conductor", "web", "not me");
        send("conductor", "all", "everyone");
        let (err, text) = call(&mut b, "inbox", json!({}));
        assert!(!err);
        assert!(text.contains("for me") && text.contains("everyone") && !text.contains("not me"));
    }
}
