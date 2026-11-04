//! The task a lama window is running: starts the conductor, launches the
//! team once you approve, starts agents the conductor asks for later, and
//! carries bus messages to their recipients — typed into an agent's window
//! when it is between turns, so nothing interrupts its work.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::{
    Approval, CONDUCTOR, Goal, Kind, Launched, Message, Plan, Start, Store, Tail, Team, launch,
    valid_name,
};
use crate::provider::Provider;
use crate::provider::conversation::{Conversation, Turn, TurnState, turn_state};

/// Least time between two deliveries to one agent: typing makes it busy,
/// and its transcript takes a moment to say so.
const DELIVERY_GAP: Duration = Duration::from_secs(8);

/// Who said a line in the intake chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    You,
    Conductor,
    Lama,
}

/// A session lama can see, for matching launched Codex agents (whose
/// session id isn't known up front) to their sessions.
#[derive(Debug, Clone)]
pub struct Seen {
    pub session: String,
    pub provider: Provider,
    pub cwd: PathBuf,
    pub born: Option<chrono::DateTime<chrono::Utc>>,
    pub transcript: PathBuf,
}

/// The agent a Codex transcript was launched as, as `(task, name)`:
/// lama's first prompt to every agent opens "You are `<name>`, … lama task
/// `<task>` …", and the transcript keeps it among its first records. `None`
/// while that isn't written yet, or for a session lama didn't launch.
fn launched_as(transcript: &Path) -> Option<(String, String)> {
    use std::io::BufRead;
    let file = std::fs::File::open(transcript).ok()?;
    let quoted = |line: &str, after: &str| -> Option<String> {
        let rest = line.split_once(after)?.1;
        let word = rest.split_once('`')?.0;
        valid_name(word).then(|| word.to_string())
    };
    std::io::BufReader::new(file)
        .lines()
        .take(64)
        .map_while(Result::ok)
        .find_map(|line| Some((quoted(&line, "lama task `")?, quoted(&line, "You are `")?)))
}

pub struct Runner {
    pub store: Store,
    pub started: Instant,
    pub source: String,
    pub conductor_session: String,
    /// Who conducts.
    conductor_provider: Provider,
    /// The conductor's current turn, while it runs.
    conductor_proc: Option<std::process::Child>,
    conductor: Conversation,
    /// When lama last looked for a Codex conductor's session, which Codex
    /// names only once it starts.
    conductor_sought: Option<Instant>,
    /// The agents this machine has, for who may run the team.
    agents: crate::agents::Installed,
    /// Who runs the team, as you said when you gave the task.
    team: Team,
    /// A tool the conductor waits for you to allow.
    pub approval: Option<Approval>,
    /// Every message so far, with when it appeared (secs since start).
    pub messages: Vec<(Message, f64)>,
    pub plan: Option<Plan>,
    /// A plan is waiting for you to choose who runs it.
    pub choosing: bool,
    /// Your choices so far: owner → who runs it.
    picked: HashMap<String, Provider>,
    /// The owner the chooser's keys act on.
    cursor: usize,
    /// The owners' latest report to the task.
    pub report: Option<String>,
    /// Owners that reported their part done.
    finished: std::collections::HashSet<String>,
    /// The conductor's first turn hasn't run yet.
    first_turn: bool,
    /// What the conductor asked you, while it waits for your answer.
    pub asking: Option<String>,
    /// When the current plan revision appeared (secs since start), for its
    /// planned cards drawing themselves.
    planned_at: f64,
    pub goals: Vec<Goal>,
    pub launched: BTreeMap<String, Launched>,
    /// lama's own lines in the chat ("launched api-forge").
    pub notes: Vec<(f64, String)>,
    projects: Vec<PathBuf>,
    msgs: Tail,
    reqs: Tail,
    pending: BTreeMap<String, Vec<Message>>,
    delivered: HashMap<String, Instant>,
    transcripts: HashMap<String, PathBuf>,
    /// Which task and agent each Codex transcript was launched as, by its
    /// path, once read (see [`launched_as`]).
    identities: HashMap<PathBuf, (String, String)>,
}

impl Runner {
    /// Start a task from `source`: its directory, and the conductor's first
    /// turn in the background.
    pub fn start(
        source: &str,
        projects: Vec<PathBuf>,
        conductor: Provider,
        team: Team,
    ) -> Result<Runner, String> {
        let store = launch::new_task(source, conductor, team).map_err(|e| e.to_string())?;
        let mut r = Runner::open(store, projects).ok_or("the task didn't save")?;
        r.first_turn = true;
        r.begin();
        Ok(r)
    }

    /// The conductor's first turn, once lama knows your projects (just
    /// started, the lamaverse is still finding them) — or after 20s anyway.
    fn begin(&mut self) {
        if !self.first_turn || (self.projects.is_empty() && self.secs() < 20.0) {
            return;
        }
        self.first_turn = false;
        let prompt = super::prompts::conductor_first(&self.source);
        match launch::run_conductor(&self.store, &self.projects, &prompt, false) {
            Ok(child) => self.conductor_proc = Some(child),
            Err(e) => self.note(format!("✗ couldn't start the conductor ({e})")),
        }
    }

    /// Pick up a task that already exists (lama restarted).
    /// `None` when another lama is already running it.
    pub fn open(store: Store, projects: Vec<PathBuf>) -> Option<Runner> {
        let m = store.task()?;
        if !lock(&store) {
            return None;
        }
        let transcript = match m.conductor_provider {
            Provider::Claude => Some(
                crate::provider::claude::discovery::project_dir(&store.dir)?
                    .join(format!("{}.jsonl", m.conductor_session)),
            ),
            Provider::Codex => codex_transcript(&m.conductor_session),
        };
        let mut r = Runner {
            started: Instant::now(),
            source: m.source,
            conductor_session: m.conductor_session,
            conductor_provider: m.conductor_provider,
            conductor_proc: None,
            conductor: Conversation::new(
                m.conductor_provider,
                transcript.clone().unwrap_or_default(),
            ),
            conductor_sought: None,
            agents: crate::agents::installed(),
            team: m.team,
            approval: None,
            messages: Vec::new(),
            plan: store.plan(),
            goals: store.goals(),
            launched: store.launched(),
            notes: Vec::new(),
            projects,
            msgs: Tail::default(),
            reqs: Tail::default(),
            pending: BTreeMap::new(),
            delivered: HashMap::new(),
            transcripts: HashMap::new(),
            identities: HashMap::new(),
            planned_at: 0.0,
            first_turn: false,
            report: None,
            finished: Default::default(),
            choosing: false,
            picked: HashMap::new(),
            cursor: 0,
            asking: None,
            store,
        };
        if let Some(t) = transcript {
            r.transcripts.insert(CONDUCTOR.to_string(), t);
        }
        // What was already on the bus was already delivered.
        let now = r.secs();
        r.messages = r
            .msgs
            .read::<Message>(&r.store.dir.join("messages.jsonl"))
            .into_iter()
            .map(|m| (m, now - 60.0))
            .collect();
        for (m, _) in &r.messages {
            if m.to == "task" {
                r.report = Some(m.text.clone());
                if m.done {
                    r.finished.insert(m.from.clone());
                }
            }
        }
        r.reqs
            .read::<super::Request>(&r.store.dir.join("requests.jsonl"));
        // A team proposed but not yet launched still waits for you, or
        // launches on who you picked.
        if r.plan.as_ref().is_some_and(|p| !p.approved) {
            r.dispatched();
        }
        Some(r)
    }

    /// The projects agents may run in: the ones the lamaverse knows now.
    pub fn set_projects(&mut self, projects: Vec<PathBuf>) {
        self.projects = projects;
    }

    /// lama's latest line about the task (a launch that failed), while
    /// it is recent enough to show.
    pub fn last_note(&self, within: f64) -> Option<&str> {
        let (at, text) = self.notes.last()?;
        (self.secs() - at <= within).then_some(text.as_str())
    }

    pub fn secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    fn note(&mut self, text: impl Into<String>) {
        let at = self.secs();
        self.notes.push((at, text.into()));
    }

    /// The conductor's conversation, as chat lines and its recent tool calls.
    pub fn chat(&self) -> (Vec<(Who, String)>, Vec<String>) {
        let mut chat = Vec::new();
        let mut thoughts = Vec::new();
        for t in &self.conductor.turns {
            match t {
                // The first prompt is the source wrapped in instructions;
                // the chat shows the source itself.
                Turn::User(text) if chat.is_empty() => {
                    let _ = text;
                    chat.push((Who::You, self.source.clone()));
                }
                Turn::User(text) => match text.strip_prefix("[lama") {
                    Some(_) => chat.push((Who::Lama, text.clone())),
                    None => chat.push((Who::You, text.clone())),
                },
                Turn::Agent(text) => chat.push((Who::Conductor, text.clone())),
                Turn::Tool { name, summary, .. } => {
                    thoughts.push(format!("{name} {summary}").trim().to_string())
                }
            }
        }
        if chat.is_empty() {
            chat.push((Who::You, self.source.clone()));
        }
        for (_, n) in &self.notes {
            chat.push((Who::Lama, n.clone()));
        }
        let keep = thoughts.len().saturating_sub(8);
        (chat, thoughts.split_off(keep))
    }

    /// The window lama launched it in, while the kitty (or tmux) it was
    /// launched in still runs. A window id alone is no address: each kitty
    /// counts its windows from 1, so after a restart the same id may be
    /// another tab.
    fn window_of(&self, name: &str) -> Option<crate::kitty::Window> {
        let l = self.launched.get(name)?;
        let w = crate::kitty::Window {
            socket: l.socket.clone()?,
            id: l.window?,
            tty: None,
        };
        w.alive().then_some(w)
    }

    /// The window to type into: the one lama launched it in, or — after a
    /// restart or a resume — the one its agent process runs in.
    fn reach(&mut self, name: &str) -> Option<crate::kitty::Window> {
        if let Some(w) = self.window_of(name) {
            return Some(w);
        }
        let l = self.launched.get(name)?;
        let sid = l.session.clone()?;
        let cwd = l.cwd.clone();
        match crate::kitty::locate(self.provider_of(name), &sid, Some(&cwd), &[], None) {
            crate::kitty::Reach::Ready(w) => {
                if let Some(l) = self.launched.get_mut(name) {
                    l.window = Some(w.id);
                    l.socket = Some(w.socket.clone());
                }
                Some(w)
            }
            _ => None,
        }
    }

    /// The conductor dispatched: launch the team on who you picked, or ask.
    fn dispatched(&mut self) {
        match self.team {
            Team::Ask => self.choosing = true,
            Team::On(p) => self.choose(Some(p)),
            Team::AsPlanned => self.choose(None),
        }
    }

    /// Your answer to the tool the conductor waits on.
    pub fn answer(&mut self, allow: bool, note: &str) -> Result<(), String> {
        let Some(a) = self.approval.take() else {
            return Err("the conductor isn't waiting on anything".into());
        };
        self.asking = None;
        let answer = super::Answer {
            allow,
            note: note.to_string(),
        };
        self.store
            .answer(&a.id, &answer)
            .map_err(|e| e.to_string())?;
        let said = if allow { "allowed" } else { "refused" };
        self.note(format!("◈ {said}: {}", trim_to(&a.describe(), 60)));
        Ok(())
    }

    /// A note you typed into the intake, for the conductor's next turn —
    /// or, while it waits on a tool, your answer: yes allows it, anything
    /// else refuses it and says why.
    pub fn say(&mut self, text: &str) -> Result<(), String> {
        if self.approval.is_some() {
            let t = text.trim();
            return match t.to_lowercase().as_str() {
                "y" | "yes" | "allow" | "ok" => self.answer(true, ""),
                "n" | "no" | "deny" => self.answer(false, ""),
                _ => self.answer(false, t),
            };
        }
        self.asking = None;
        self.queue(
            CONDUCTOR,
            "lama",
            Kind::Report,
            &format!("[lama] note from the user: {text}"),
        );
        Ok(())
    }

    /// Who runs each owner of the waiting plan, as chosen so far: the
    /// conductor's suggestion until you change it.
    fn picks(&self) -> Vec<(String, Provider)> {
        let Some(plan) = self.plan.as_ref().filter(|p| !p.approved) else {
            return Vec::new();
        };
        plan.agents
            .iter()
            .filter(|a| a.parent.is_none())
            .map(|a| {
                let p = self.picked.get(&a.name).copied().unwrap_or(a.provider);
                (a.name.clone(), self.agents.or_other(p).unwrap_or(p))
            })
            .collect()
    }

    /// Move the chooser's cursor among the owners.
    pub fn choice_move(&mut self, down: bool) {
        let n = self.picks().len();
        if n > 0 {
            self.cursor = if down {
                (self.cursor + 1).min(n - 1)
            } else {
                self.cursor.saturating_sub(1)
            };
        }
    }

    /// Switch the owner under the cursor between Claude and Codex, when
    /// both are installed.
    pub fn choice_toggle(&mut self) {
        if let Some((name, p)) = self.picks().get(self.cursor).cloned()
            && self.agents.both()
        {
            self.picked.insert(name, crate::agents::other(p));
        }
    }

    /// Every owner on `provider`, or (`None`) back to the conductor's plan.
    pub fn choice_all(&mut self, provider: Option<Provider>) {
        match provider {
            Some(p) if !self.agents.has(p) => {}
            Some(p) => {
                for (name, _) in self.picks() {
                    self.picked.insert(name, p);
                }
            }
            None => self.picked.clear(),
        }
    }

    /// Launch the waiting plan with the owners as chosen, and any agent
    /// planned on a CLI this machine doesn't have on the one it does.
    pub fn choice_launch(&mut self) {
        let Some(mut plan) = self.plan.clone().filter(|p| !p.approved) else {
            return;
        };
        self.choosing = false;
        for a in &mut plan.agents {
            let to = self.picked.get(&a.name).copied().unwrap_or(a.provider);
            run_on(a, self.agents.or_other(to).unwrap_or(to));
        }
        self.picked.clear();
        self.launch_plan(plan);
    }

    /// Run every owner on `provider` (`None`: as planned) and launch.
    pub fn choose(&mut self, provider: Option<Provider>) {
        self.choice_all(provider);
        self.choice_launch();
    }

    /// Launch a newly proposed plan's `now` agents — no approval step: the
    /// agents themselves still ask before they edit or run anything.
    fn launch_plan(&mut self, mut plan: Plan) {
        plan.approved = true;
        let _ = self.store.set_plan(&plan);
        let mut up = Vec::new();
        let mut failed = Vec::new();
        for a in plan.agents.iter().filter(|a| a.start == Start::Now) {
            if self.launched.contains_key(&a.name) {
                continue;
            }
            match self.launch(a) {
                Ok(()) => up.push(a.name.clone()),
                Err(e) => failed.push(format!("{} ({e})", a.name)),
            }
        }
        let mut text = format!("[lama] plan v{} is running.", plan.version);
        if !up.is_empty() {
            text += &format!(" Launched: {}.", up.join(", "));
        }
        if !failed.is_empty() {
            text += &format!(" Failed to launch: {}.", failed.join("; "));
        }
        let _ = text;
        self.plan = Some(plan);
        for f in failed {
            self.note(format!("✗ couldn't launch {f}"));
        }
    }

    fn launch(&mut self, a: &super::PlannedAgent) -> Result<(), String> {
        // A peer works in its owner's worktree, reading the owner's branch.
        let at = a
            .parent
            .as_ref()
            .and_then(|o| self.launched.get(o))
            .map(|l| l.cwd.clone());
        let l = launch::start_agent(&self.store, a, &self.projects, at.as_deref())?;
        self.launched.insert(a.name.clone(), l);
        // Its brief flies to its card: an owner's from its task's row, a
        // peer's from the owner that asked for it.
        let now = self.secs();
        self.messages.push((
            Message {
                at: chrono::Utc::now(),
                from: a.parent.clone().unwrap_or_else(|| "task".into()),
                to: a.name.clone(),
                kind: Kind::Assign,
                text: a.role.clone(),
                done: false,
            },
            now,
        ));
        Ok(())
    }

    fn queue(&mut self, to: &str, from: &str, kind: Kind, text: &str) {
        self.pending
            .entry(to.to_string())
            .or_default()
            .push(Message {
                at: chrono::Utc::now(),
                from: from.into(),
                to: to.into(),
                kind,
                text: text.into(),
                done: false,
            });
    }

    /// Match launched agents whose session id isn't known yet (Codex) to
    /// the sessions lama sees: same provider and directory, born after the
    /// launch, and — what makes it exact — a transcript whose launch prompt
    /// names this agent of this task (see `launched_as`). Agents launched
    /// in the same directory in the same second are told apart only by
    /// that. A transcript that names no agent at all (one from before lama
    /// wrote the line) is taken only when it is the one candidate for the
    /// one agent waiting there; anything less certain waits.
    pub fn adopt(&mut self, seen: &[Seen]) {
        let task = self.store.id();
        let mut taken: Vec<String> = self
            .launched
            .values()
            .filter_map(|l| l.session.clone())
            .collect();
        let waiting: Vec<String> = self
            .launched
            .iter()
            .filter(|(_, l)| l.session.is_none() && l.at.is_some())
            .map(|(n, _)| n.clone())
            .collect();
        let mut changed = false;
        for name in &waiting {
            let provider = self.provider_of(name);
            let Some(l) = self.launched.get(name) else {
                continue;
            };
            let (Some(at), want) = (l.at, crate::provider::project_dir(&l.cwd).to_path_buf())
            else {
                continue;
            };
            let fits = |s: &Seen, taken: &[String]| {
                s.provider == provider
                    && crate::provider::project_dir(&s.cwd) == want
                    && !taken.contains(&s.session)
                    && s.born
                        .is_some_and(|b| b >= at - chrono::Duration::seconds(5))
            };
            let candidates: Vec<&Seen> = seen.iter().filter(|s| fits(s, &taken)).collect();
            let mut who = |s: &Seen| -> Option<(String, String)> {
                if let Some(id) = self.identities.get(&s.transcript) {
                    return Some(id.clone());
                }
                let id = launched_as(&s.transcript)?;
                self.identities.insert(s.transcript.clone(), id.clone());
                Some(id)
            };
            let named: Vec<(&Seen, Option<(String, String)>)> =
                candidates.iter().map(|&s| (s, who(s))).collect();
            let exact = named
                .iter()
                .find(|(_, id)| id.as_ref().is_some_and(|(t, n)| *t == task && n == name))
                .map(|(s, _)| *s);
            let alone = || {
                let rivals = waiting.iter().filter(|o| {
                    *o != name
                        && self.launched.get(*o).is_some_and(|ol| {
                            ol.session.is_none()
                                && self.provider_of(o) == provider
                                && crate::provider::project_dir(&ol.cwd) == want
                        })
                });
                match named.as_slice() {
                    [(s, None)] if rivals.count() == 0 => Some(*s),
                    _ => None,
                }
            };
            let Some(s) = exact.or_else(alone) else {
                continue;
            };
            let (session, transcript) = (s.session.clone(), s.transcript.clone());
            taken.push(session.clone());
            if let Some(l) = self.launched.get_mut(name) {
                l.session = Some(session);
            }
            self.transcripts.insert(name.clone(), transcript);
            changed = true;
        }
        if changed {
            let _ = self.store.set_launched(&self.launched);
        }
    }

    /// Two agents bound to one session can't both be right, and either may
    /// be wrong: bus messages would be typed by the wrong agent's turn, or
    /// into the wrong window once lama has to find it by session. Unbind
    /// them all, so [`adopt`](Self::adopt) matches each again by name.
    fn unbind_shared(&mut self) {
        let mut count: HashMap<String, usize> = HashMap::new();
        for l in self.launched.values() {
            if let Some(s) = &l.session {
                *count.entry(s.clone()).or_default() += 1;
            }
        }
        let shared: Vec<String> = self
            .launched
            .iter()
            .filter(|(_, l)| l.session.as_ref().is_some_and(|s| count[s] > 1))
            .map(|(n, _)| n.clone())
            .collect();
        if shared.is_empty() {
            return;
        }
        for name in &shared {
            if let Some(l) = self.launched.get_mut(name) {
                l.session = None;
            }
            self.transcripts.remove(name);
        }
        self.note(format!(
            "⚠ {} were bound to one session; matching each again by name",
            shared.join(", ")
        ));
        let _ = self.store.set_launched(&self.launched);
    }

    fn provider_of(&self, name: &str) -> Provider {
        self.plan
            .as_ref()
            .and_then(|p| p.agents.iter().find(|a| a.name == name))
            .map_or(Provider::Claude, |a| a.provider)
    }

    /// Whether a conductor turn is running: the one lama started, or — after
    /// a restart — one still going from before.
    fn conductor_busy(&mut self) -> bool {
        if let Some(c) = self.conductor_proc.as_mut() {
            match c.try_wait() {
                Ok(None) => return true,
                Ok(Some(status)) if !status.success() => {
                    self.note(
                        "✗ the conductor stopped with an error — see conductor.log in the task's folder",
                    );
                }
                _ => {}
            }
            self.conductor_proc = None;
        }
        // A Codex conductor whose session hasn't appeared can't be resumed
        // yet, and nothing of it can be read: busy until it shows.
        if self.conductor_session.is_empty() {
            return !self.first_turn;
        }
        let bin = match self.conductor_provider {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
        };
        running_with(&self.conductor_session, bin)
    }

    /// A Codex conductor's session, once it has one: the Codex session
    /// working in the task's directory, which only the conductor does.
    /// Looked for every few seconds until found, then kept in `task.json`.
    fn find_conductor(&mut self) {
        if self.conductor_provider != Provider::Codex
            || self.transcripts.contains_key(CONDUCTOR)
            || self
                .conductor_sought
                .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        self.conductor_sought = Some(Instant::now());
        let here = crate::provider::Target::Here(self.store.dir.clone());
        let Ok(s) = crate::provider::open(&here, Some(Provider::Codex)) else {
            return;
        };
        if crate::provider::codex::discovery::cwd_of(&s.root).as_deref() != Some(&self.store.dir) {
            return;
        }
        self.conductor_session = s.id.clone();
        self.conductor = Conversation::new(Provider::Codex, s.root.path.clone());
        self.transcripts.insert(CONDUCTOR.to_string(), s.root.path);
        if let Some(mut t) = self.store.task() {
            t.conductor_session = s.id;
            let _ = self.store.set_task(&t);
        }
    }

    fn idle(&self, name: &str) -> bool {
        let Some(path) = self.transcript_of(name) else {
            return false;
        };
        let provider = if name == CONDUCTOR {
            self.conductor_provider
        } else {
            self.provider_of(name)
        };
        turn_state(provider, &path) == Some(TurnState::Done)
    }

    fn transcript_of(&self, name: &str) -> Option<PathBuf> {
        if let Some(p) = self.transcripts.get(name) {
            return Some(p.clone());
        }
        // A Claude agent's transcript follows from its cwd and session id.
        let l = self.launched.get(name)?;
        let sid = l.session.as_ref()?;
        let p =
            crate::provider::claude::discovery::project_dir(&l.cwd)?.join(format!("{sid}.jsonl"));
        p.exists().then_some(p)
    }

    /// Read what changed on disk, start requested agents, and deliver
    /// messages to recipients that are between turns.
    pub fn tick(&mut self) {
        self.unbind_shared();
        self.begin();
        self.find_conductor();
        self.conductor.refresh();
        if let Some(p) = self.store.plan()
            && Some(&p) != self.plan.as_ref()
        {
            if self
                .plan
                .as_ref()
                .is_none_or(|old| old.version != p.version)
            {
                self.planned_at = self.secs();
            }
            let fresh = !p.approved;
            self.plan = Some(p);
            if fresh {
                self.dispatched();
            }
        }
        self.goals = self.store.goals();
        let reqs: Vec<super::Request> = self.reqs.read(&self.store.dir.join("requests.jsonl"));
        for r in reqs {
            // A peer an owner asked for joins the team, then launches.
            if let Some(mut peer) = r.agent
                && let Some(mut plan) = self.plan.clone().filter(|p| p.approved)
                && !plan.agents.iter().any(|a| a.name == peer.name)
            {
                let want = match self.team {
                    Team::On(p) => p,
                    _ => peer.provider,
                };
                let to = self.agents.or_other(want).unwrap_or(want);
                run_on(&mut peer, to);
                plan.agents.push(peer);
                let _ = self.store.set_plan(&plan);
                self.plan = Some(plan);
            }
            let Some(a) = self
                .plan
                .as_ref()
                .filter(|p| p.approved)
                .and_then(|p| p.agents.iter().find(|a| a.name == r.start))
                .cloned()
            else {
                continue;
            };
            if self.launched.contains_key(&a.name) {
                continue;
            }
            match self.launch(&a) {
                Ok(()) => self.note(format!("◈ launched {}", a.name)),
                Err(e) => {
                    self.note(format!("✗ couldn't launch {} ({e})", a.name));
                    if let Some(owner) = a.parent.clone() {
                        self.queue(
                            &owner,
                            "lama",
                            Kind::Blocker,
                            &format!("[lama] couldn't launch {}: {e}", a.name),
                        );
                    }
                }
            }
        }
        let now = self.secs();
        let new: Vec<Message> = self.msgs.read(&self.store.dir.join("messages.jsonl"));
        for m in new {
            // Reports go to the task's row, not to anyone's terminal; the
            // conductor only dispatches, so nothing is delivered to it.
            if m.to == "task" {
                self.report = Some(m.text.clone());
                if m.done {
                    self.finished.insert(m.from.clone());
                }
                self.messages.push((m, now));
                continue;
            }
            let to: Vec<String> = if m.to == "all" {
                self.launched
                    .keys()
                    .filter(|n| **n != m.from)
                    .cloned()
                    .collect()
            } else if m.to == CONDUCTOR {
                Vec::new()
            } else {
                vec![m.to.clone()]
            };
            for t in to {
                self.pending.entry(t).or_default().push(m.clone());
            }
            self.messages.push((m, now));
        }
        self.deliver();
        self.approval = self.waiting_approval();
        self.asking = match &self.approval {
            Some(a) => Some(format!("May the conductor {}?", a.describe())),
            None => self.question(),
        };
        if self.done() {
            let _ = std::fs::write(self.store.dir.join("done"), "");
        }
    }

    /// Every owner reported its part finished.
    pub fn done(&self) -> bool {
        let owners: Vec<&String> = self
            .plan
            .iter()
            .flat_map(|p| {
                p.agents
                    .iter()
                    .filter(|a| a.parent.is_none())
                    .map(|a| &a.name)
            })
            .collect();
        !owners.is_empty() && owners.iter().all(|o| self.finished.contains(*o))
    }

    /// The oldest tool the conductor is waiting for you to allow. Prompts
    /// left open by a turn that has ended are closed, not shown: answering
    /// one would reach nobody.
    fn waiting_approval(&mut self) -> Option<Approval> {
        let open = self.store.open_approvals();
        if open.is_empty() {
            return None;
        }
        if self.conductor_busy() {
            return open.into_iter().next();
        }
        let gone = super::Answer {
            allow: false,
            note: "the conductor's turn ended".into(),
        };
        for a in &open {
            let _ = self.store.answer(&a.id, &gone);
        }
        None
    }

    /// The conductor ended its turn without a plan and with words: it needs
    /// something from you.
    fn question(&mut self) -> Option<String> {
        if self.plan.is_some()
            || self.first_turn
            || self.pending.contains_key(CONDUCTOR)
            || self.conductor_busy()
        {
            return None;
        }
        match self.conductor.turns.last()? {
            Turn::Agent(text) => Some(text.split_whitespace().collect::<Vec<_>>().join(" ")),
            _ => None,
        }
    }

    fn deliver(&mut self) {
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, q)| !q.is_empty())
            .map(|(n, _)| n.clone())
            .filter(|n| {
                self.delivered
                    .get(n)
                    .is_none_or(|t| t.elapsed() >= DELIVERY_GAP)
            })
            .collect();
        let ready: Vec<String> = ready
            .into_iter()
            .filter(|n| {
                if n == CONDUCTOR {
                    !self.conductor_busy()
                } else {
                    self.idle(n)
                }
            })
            .collect();
        for name in ready {
            if name == CONDUCTOR {
                let q = self.pending.remove(&name).unwrap_or_default();
                match launch::run_conductor(&self.store, &self.projects, &bus_text(&q), true) {
                    Ok(child) => {
                        self.conductor_proc = Some(child);
                        self.delivered.insert(name, Instant::now());
                    }
                    Err(e) => {
                        self.note(format!("✗ couldn't wake the conductor ({e})"));
                        self.pending.insert(name, q);
                    }
                }
                continue;
            }
            let Some(w) = self.reach(&name) else {
                continue;
            };
            let q = self.pending.remove(&name).unwrap_or_default();
            match crate::kitty::send(&w, &bus_text(&q)) {
                Ok(()) => {
                    self.delivered.insert(name, Instant::now());
                }
                Err(_) => {
                    self.pending.insert(name, q);
                }
            }
        }
    }

    /// The task's directory, for hiding the conductor's own session from
    /// the zones.
    pub fn dir(&self) -> &Path {
        &self.store.dir
    }
}

impl Runner {
    /// The task's row in the TASKS box. A waiting permission prompt is the
    /// caller's to add: it knows which sessions are asking.
    pub fn row(&self) -> crate::state::task::TaskRow {
        use crate::state::task::RowStatus;
        let owners: Vec<&super::PlannedAgent> = self
            .plan
            .iter()
            .flat_map(|p| p.agents.iter().filter(|a| a.parent.is_none()))
            .collect();
        let mut projects: Vec<String> = owners
            .iter()
            .filter_map(|a| a.project.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        projects.dedup();
        let up = self
            .launched
            .values()
            .filter(|l| l.session.is_some())
            .count();
        let status = if self.done() {
            RowStatus::Done
        } else if self.asking.is_some() {
            RowStatus::Asking
        } else if self.choosing {
            RowStatus::Choosing
        } else if self.plan.is_none() {
            RowStatus::Dispatching
        } else if up == 0 {
            RowStatus::Launching
        } else {
            RowStatus::Working(up)
        };
        let title = self
            .plan
            .as_ref()
            .map(|p| p.summary.clone())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| self.source.clone());
        crate::state::task::TaskRow {
            id: self.store.id(),
            title: title.split_whitespace().collect::<Vec<_>>().join(" "),
            projects,
            status,
            msgs: self.messages.len(),
            cost: None,
            goals: self
                .goals
                .iter()
                .map(|g| (g.text.clone(), g.done))
                .collect(),
            note: self.report.clone(),
        }
    }

    /// The conductor as a dispatcher: busy until it has routed the task.
    pub fn conductor_view(&self) -> crate::state::task::ConductorView {
        let dispatching = self.plan.is_none() && self.asking.is_none() && !self.done();
        let owners = self
            .plan
            .iter()
            .flat_map(|p| p.agents.iter().filter(|a| a.parent.is_none()))
            .count();
        let row = self.row();
        let (_, thoughts) = self.chat();
        crate::state::task::ConductorView {
            dispatching,
            reading: if dispatching {
                thoughts.last().cloned()
            } else {
                None
            },
            routed: usize::from(self.plan.is_some()),
            owners,
            spent: None,
            log: self
                .plan
                .is_some()
                .then(|| {
                    format!(
                        "{} → {} · {owners} owner{}",
                        trim_to(&row.title, 18),
                        row.projects.join("+"),
                        if owners == 1 { "" } else { "s" }
                    )
                })
                .into_iter()
                .collect(),
        }
    }
}

impl Runner {
    fn endpoint(&self, who: &str) -> String {
        if who == "task" {
            format!("{}{}", crate::state::task::TASK, self.store.id())
        } else {
            who.to_string()
        }
    }
}

fn trim_to(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n.saturating_sub(1)).collect::<String>() + "…"
    }
}

/// Messages as one line of text for their recipient.
fn bus_text(q: &[Message]) -> String {
    q.iter()
        .map(|m| {
            if m.from == "lama" {
                m.text.clone()
            } else {
                format!(
                    "[lama bus] from {} ({}): {}",
                    m.from,
                    kind_name(m.kind),
                    m.text
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Whether some process's command line mentions `needle` (a session id).
fn running_with(needle: &str, bin: &str) -> bool {
    crate::procs::command_lines()
        .iter()
        .any(|c| c.contains(needle) && c.contains(bin))
}

/// `a` run on `to`: a switched agent gets that CLI's default model, since a
/// model id belongs to one CLI.
fn run_on(a: &mut super::PlannedAgent, to: Provider) {
    if a.provider != to {
        a.provider = to;
        a.model = match to {
            Provider::Codex => String::new(),
            Provider::Claude => "sonnet".into(),
        };
    }
}

/// A Codex session's transcript, by its id.
fn codex_transcript(session: &str) -> Option<PathBuf> {
    if session.is_empty() {
        return None;
    }
    let id = crate::provider::Target::Id(session.to_string());
    crate::provider::open(&id, Some(Provider::Codex))
        .ok()
        .map(|s| s.root.path)
}

/// Take a task for this lama: one lama runs a task (launching, delivering);
/// another one open at the same time leaves it alone.
fn lock(store: &Store) -> bool {
    let path = store.dir.join("lock");
    let me = std::process::id();
    if let Some(pid) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        && pid != me
        && crate::procs::argv(pid)
            .first()
            .is_some_and(|a| a.rsplit('/').next() == Some("lama"))
    {
        return false;
    }
    std::fs::write(&path, me.to_string()).is_ok()
}

pub fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Assign => "assign",
        Kind::Contract => "contract",
        Kind::Question => "question",
        Kind::Answer => "answer",
        Kind::Blocker => "blocker",
        Kind::Report => "report",
    }
}

impl Runner {
    /// What the canvas draws: the chat, the plan, goals, the team's
    /// sessions and the messages with when each appeared.
    pub fn view(&self) -> crate::state::task::TaskView {
        use crate::state::task as v;
        let (chat, thoughts) = self.chat();
        let goals_done = !self.goals.is_empty() && self.goals.iter().all(|g| g.done);
        let phase = match &self.plan {
            _ if goals_done => "COMPLETE",
            None => "INVESTIGATING",
            Some(_) => "RUNNING",
        };
        let names: Vec<String> = self
            .plan
            .iter()
            .flat_map(|p| p.agents.iter().map(|a| a.name.clone()))
            .chain(self.launched.keys().cloned())
            .fold(Vec::new(), |mut acc, n| {
                if !acc.contains(&n) {
                    acc.push(n);
                }
                acc
            });
        v::TaskView {
            id: self.store.id(),
            source: self.source.clone(),
            now: self.secs(),
            phase: phase.into(),
            conductor_session: self.conductor_session.clone(),
            conductor_provider: Some(self.conductor_provider),
            conductor_model: (self.conductor_provider == Provider::Claude).then(|| "sonnet".into()),
            choosing: self.choosing,
            choice: self.choosing.then(|| v::ChoiceView {
                task: self.store.id(),
                owners: self
                    .picks()
                    .into_iter()
                    .map(|(name, provider)| {
                        let a = self
                            .plan
                            .as_ref()
                            .and_then(|p| p.agents.iter().find(|a| a.name == name));
                        v::ChoiceOwner {
                            project: a
                                .and_then(|a| a.project.file_name())
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            model: a
                                .filter(|a| a.provider == provider)
                                .map(|a| a.model.clone())
                                .unwrap_or_default(),
                            name,
                            provider,
                        }
                    })
                    .collect(),
                cursor: self.cursor,
            }),
            tasks: vec![self.row()],
            conductor: self.conductor_view(),
            asking: self.asking.clone(),
            approving: self.approval.is_some(),
            chat: chat
                .into_iter()
                .map(|(who, text)| v::ChatLine {
                    who: match who {
                        Who::You => v::Who::You,
                        Who::Conductor => v::Who::Conductor,
                        Who::Lama => v::Who::Lama,
                    },
                    text,
                })
                .collect(),
            thoughts,
            plan: self.plan.as_ref().map(|p| v::PlanView {
                version: p.version,
                summary: p.summary.clone(),
                approved: p.approved,
                planned_at: self.planned_at,
                agents: p
                    .agents
                    .iter()
                    .map(|a| v::PlannedView {
                        project_key: crate::provider::group_key(&a.project),
                        name: a.name.clone(),
                        project_name: a
                            .project
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        provider: a.provider,
                        model: a.model.clone(),
                        role: a.role.clone(),
                        why: a.why.clone(),
                        start_later: a.start == Start::Later,
                        task: self.store.id(),
                        parent: a.parent.clone(),
                    })
                    .collect(),
            }),
            goals: self
                .goals
                .iter()
                .map(|g| (g.text.clone(), g.done))
                .collect(),
            agents: names
                .into_iter()
                .map(|name| v::AgentLink {
                    task: self.store.id(),
                    parent: self
                        .plan
                        .as_ref()
                        .and_then(|p| p.agents.iter().find(|a| a.name == name))
                        .and_then(|a| a.parent.clone()),
                    joined: None,
                    session: self.launched.get(&name).and_then(|l| l.session.clone()),
                    name,
                })
                .collect(),
            messages: self
                .messages
                .iter()
                .map(|(m, at)| v::MessageView {
                    // `task` is this task's row in the TASKS box.
                    from: self.endpoint(&m.from),
                    to: self.endpoint(&m.to),
                    kind: match m.kind {
                        Kind::Assign => v::MsgKind::Assign,
                        Kind::Contract => v::MsgKind::Contract,
                        Kind::Question => v::MsgKind::Question,
                        Kind::Answer => v::MsgKind::Answer,
                        Kind::Blocker => v::MsgKind::Blocker,
                        Kind::Report => v::MsgKind::Report,
                    },
                    text: m.text.clone(),
                    shown_at: *at,
                })
                .collect(),
            ..Default::default()
        }
    }
}

/// The tasks to pick back up when lama starts, oldest first: those touched
/// in the last 12 hours whose goals aren't all met.
pub fn recent() -> Vec<Store> {
    let cutoff = std::time::SystemTime::now() - Duration::from_secs(12 * 3600);
    let Some(root) = super::root() else {
        return Vec::new();
    };
    let Ok(dir) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, Store)> = dir
        .filter_map(Result::ok)
        .filter_map(|e| {
            let store = Store::at(e.path());
            let touched = ["messages.jsonl", "plan.json", "task.json"]
                .iter()
                .filter_map(|f| std::fs::metadata(store.dir.join(f)).ok()?.modified().ok())
                .max()?;
            let goals = store.goals();
            let done = (!goals.is_empty() && goals.iter().all(|g| g.done))
                || store.dir.join("done").exists();
            (touched >= cutoff && !done && store.task().is_some()).then_some((touched, store))
        })
        .collect();
    found.sort_by_key(|(t, _)| *t);
    found.into_iter().map(|(_, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::{PlannedAgent, Store};

    fn runner_with(plan: Plan) -> Runner {
        let dir =
            std::env::temp_dir().join(format!("lama-run-{}-{}", std::process::id(), plan.version));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::at(dir);
        store
            .set_task(&super::super::Task {
                id: "t1".into(),
                source: "x".into(),
                created: chrono::Utc::now(),
                conductor_session: "s".into(),
                conductor_provider: Provider::Claude,
                team: Team::Ask,
            })
            .unwrap();
        store.set_plan(&plan).unwrap();
        let mut r = Runner::open(store, Vec::new()).unwrap();
        r.agents = crate::agents::Installed {
            claude: true,
            codex: true,
        };
        r.plan = Some(plan);
        r.choosing = true;
        r
    }

    fn agent(name: &str, provider: Provider, model: &str) -> PlannedAgent {
        PlannedAgent {
            name: name.into(),
            project: "/nowhere".into(),
            provider,
            model: model.into(),
            role: "r".into(),
            prompt: "p".into(),
            start: Start::Later,
            writes: false,
            why: String::new(),
            parent: None,
        }
    }

    #[test]
    fn a_team_still_waiting_on_you_asks_again_after_a_restart() {
        let plan = Plan {
            version: 41,
            summary: String::new(),
            agents: vec![agent("a", Provider::Claude, "opus")],
            approved: false,
        };
        let r = runner_with(plan.clone());
        let store = Store::at(r.store.dir.clone());
        drop(r);
        let reopened = Runner::open(store, Vec::new()).unwrap();
        assert!(reopened.choosing, "the chooser comes back");
        reopened
            .store
            .set_plan(&Plan {
                approved: true,
                ..plan
            })
            .unwrap();
        let again = Runner::open(Store::at(reopened.store.dir.clone()), Vec::new()).unwrap();
        assert!(!again.choosing, "an approved team doesn't ask");
    }

    #[test]
    fn a_team_picked_up_front_launches_without_asking() {
        let plan = Plan {
            version: 42,
            summary: String::new(),
            agents: vec![agent("a", Provider::Codex, "")],
            approved: false,
        };
        let r = runner_with(plan);
        let mut t = r.store.task().unwrap();
        t.team = Team::On(Provider::Claude);
        r.store.set_task(&t).unwrap();
        let store = Store::at(r.store.dir.clone());
        drop(r);
        let r = Runner::open(store, Vec::new()).unwrap();
        let p = r.store.plan().unwrap();
        assert!(p.approved && !r.choosing, "nothing left to ask");
        assert_eq!(p.agents[0].provider, Provider::Claude);
    }

    #[test]
    fn your_line_answers_a_conductor_waiting_on_a_tool() {
        let mut r = runner_with(Plan {
            version: 43,
            summary: String::new(),
            agents: vec![],
            approved: false,
        });
        let ask = |r: &mut Runner, id: &str| {
            r.approval = Some(Approval {
                id: id.into(),
                tool: "Bash".into(),
                input: serde_json::json!({"command": "git log"}),
            });
        };
        ask(&mut r, "a1");
        r.say("yes").unwrap();
        assert!(r.store.answer_of("a1").unwrap().allow && r.approval.is_none());
        ask(&mut r, "a2");
        r.say("use the API instead").unwrap();
        let no = r.store.answer_of("a2").unwrap();
        assert!(!no.allow && no.note == "use the API instead");
        assert!(r.pending.is_empty(), "an answer isn't a note for later");
    }

    #[test]
    fn choosing_codex_moves_every_agent_to_codex_with_its_default_model() {
        let plan = Plan {
            version: 7,
            summary: String::new(),
            agents: vec![
                agent("a", Provider::Claude, "opus"),
                agent("b", Provider::Codex, ""),
            ],
            approved: false,
        };
        let mut r = runner_with(plan);
        r.choose(Some(Provider::Codex));
        let p = r.store.plan().unwrap();
        assert!(p.approved && !r.choosing);
        assert!(
            p.agents
                .iter()
                .all(|a| a.provider == Provider::Codex && a.model.is_empty())
        );
    }

    #[test]
    fn as_planned_keeps_the_mix() {
        let plan = Plan {
            version: 8,
            summary: String::new(),
            agents: vec![
                agent("a", Provider::Claude, "opus"),
                agent("b", Provider::Codex, ""),
            ],
            approved: false,
        };
        let mut r = runner_with(plan);
        r.choose(None);
        let p = r.store.plan().unwrap();
        assert_eq!(p.agents[0].provider, Provider::Claude);
        assert_eq!(p.agents[0].model, "opus");
        assert_eq!(p.agents[1].provider, Provider::Codex);
    }

    #[test]
    fn with_only_claude_installed_every_agent_runs_on_claude() {
        let plan = Plan {
            version: 61,
            summary: String::new(),
            agents: vec![
                agent("a", Provider::Codex, ""),
                agent("b", Provider::Claude, "opus"),
            ],
            approved: false,
        };
        let mut r = runner_with(plan);
        r.agents = crate::agents::Installed {
            claude: true,
            codex: false,
        };
        // The chooser shows Claude, and neither switching nor "all Codex"
        // can pick what isn't there.
        assert!(r.picks().iter().all(|(_, p)| *p == Provider::Claude));
        r.choice_toggle();
        r.choice_all(Some(Provider::Codex));
        assert!(r.picks().iter().all(|(_, p)| *p == Provider::Claude));
        r.choice_launch();
        let p = r.store.plan().unwrap();
        assert_eq!(
            (p.agents[0].provider, p.agents[0].model.as_str()),
            (Provider::Claude, "sonnet")
        );
        assert_eq!(
            p.agents[1].model, "opus",
            "an agent already on Claude keeps its model"
        );
    }

    /// A runner whose plan has the Codex peers `names`, each launched in
    /// `/src/shop` at the same moment, none bound yet.
    fn same_second(version: u32, names: &[&str]) -> Runner {
        let plan = Plan {
            version,
            summary: String::new(),
            agents: names
                .iter()
                .map(|n| agent(n, Provider::Codex, ""))
                .collect(),
            approved: true,
        };
        let mut r = runner_with(plan);
        let at = chrono::Utc::now();
        for n in names {
            r.launched.insert(
                n.to_string(),
                Launched {
                    session: None,
                    window: None,
                    socket: None,
                    cwd: "/src/shop".into(),
                    branch: None,
                    at: Some(at),
                },
            );
        }
        r
    }

    /// A Codex session in `/src/shop` born now, whose transcript opens with
    /// `first` (lama's launch prompt, or anything).
    fn seen(r: &Runner, session: &str, first: &str) -> Seen {
        let transcript = r.store.dir.join(format!("{session}.jsonl"));
        let meta = r#"{"type":"session_meta","payload":{"id":"x"}}"#;
        let prompt = serde_json::json!({"type": "response_item", "payload": {"text": first}});
        std::fs::write(&transcript, format!("{meta}\n{prompt}\n")).unwrap();
        Seen {
            session: session.into(),
            provider: Provider::Codex,
            cwd: "/src/shop".into(),
            born: Some(chrono::Utc::now()),
            transcript,
        }
    }

    fn bound(r: &Runner, name: &str) -> Option<String> {
        r.launched[name].session.clone()
    }

    #[test]
    fn agents_launched_in_the_same_second_each_get_their_own_session() {
        let mut r = same_second(51, &["reviewer", "test-reviewer"]);
        let task = r.store.id();
        let you = |n: &str| {
            format!(
                "You are `{n}`, a peer that `owner` (the owner of lama task `{task}`) asked for."
            )
        };
        // The test reviewer's session is listed first, as it was when this went wrong.
        let list = vec![
            seen(&r, "s-test", &you("test-reviewer")),
            seen(&r, "s-rev", &you("reviewer")),
        ];
        r.adopt(&list);
        assert_eq!(bound(&r, "reviewer").as_deref(), Some("s-rev"));
        assert_eq!(bound(&r, "test-reviewer").as_deref(), Some("s-test"));
    }

    #[test]
    fn a_session_bound_to_two_agents_is_matched_again_by_name() {
        let mut r = same_second(52, &["reviewer", "test-reviewer"]);
        let task = r.store.id();
        for n in ["reviewer", "test-reviewer"] {
            r.launched.get_mut(n).unwrap().session = Some("s-test".into());
        }
        r.unbind_shared();
        assert_eq!(bound(&r, "reviewer"), None);
        assert_eq!(bound(&r, "test-reviewer"), None);
        assert!(
            r.notes
                .iter()
                .any(|(_, n)| n.contains("bound to one session"))
        );
        let you = |n: &str| format!("You are `{n}`, … lama task `{task}` …");
        let list = vec![
            seen(&r, "s-test", &you("test-reviewer")),
            seen(&r, "s-rev", &you("reviewer")),
        ];
        r.adopt(&list);
        assert_eq!(bound(&r, "reviewer").as_deref(), Some("s-rev"));
        assert_eq!(bound(&r, "test-reviewer").as_deref(), Some("s-test"));
        assert_eq!(
            r.store.launched()["reviewer"].session.as_deref(),
            Some("s-rev"),
            "saved"
        );
    }

    #[test]
    fn a_transcript_naming_nobody_is_taken_only_when_nothing_else_could_be_meant() {
        let mut r = same_second(53, &["solo"]);
        let list = vec![seen(&r, "s-1", "hello")];
        r.adopt(&list);
        assert_eq!(
            bound(&r, "solo").as_deref(),
            Some("s-1"),
            "one agent, one session"
        );

        let mut r = same_second(54, &["a", "b"]);
        let list = vec![seen(&r, "s-1", "hello"), seen(&r, "s-2", "hi")];
        r.adopt(&list);
        assert_eq!(bound(&r, "a"), None, "two of each: wait");
        assert_eq!(bound(&r, "b"), None);

        // Another task's agent of the same name is not this one.
        let mut r = same_second(55, &["solo"]);
        let list = vec![seen(&r, "s-1", "You are `solo`, … lama task `t-other` …")];
        r.adopt(&list);
        assert_eq!(bound(&r, "solo"), None);
    }
}
