//! A task as the lamaverse draws it: plain data, refilled from disk by the
//! frontend every tick.
//!
//! The task itself — its store, bus and launcher — is native-only and
//! lives outside `state`; this is the portable picture of it, so the plane
//! and the renderer compile everywhere (the browser too) and never reach
//! back into the files. Nothing here is computed from a clock: the frontend
//! stamps [`TaskView::now`] each frame, and every animation reads it.

use ratatui::layout::Rect;

use crate::provider::Provider;

/// How long a message takes to travel from its sender's card to its
/// receiver's: slow enough to read the bubble riding with it.
pub const TRAVEL_SECS: f64 = 3.2;
/// How long the bubble stays by the receiver after it lands, fading out.
pub const LINGER_SECS: f64 = 2.0;
/// How long the receiver's border flashes the message's colour on arrival.
pub const FLASH_SECS: f64 = 0.7;
/// How long a new agent's arrival takes: the beam reaching its card (from
/// its task's row, or a peer's from its owner), then the burst around it. Its border then flashes for
/// [`FLASH_SECS`].
pub const ARRIVE_SECS: f64 = 0.9;
/// The conductor's name on the bus: its endpoint is its panel, not a card.
pub const CONDUCTOR: &str = "conductor";
/// A broadcast: drawn to the conductor's panel, or not at all when the
/// conductor sent it.
pub const EVERYONE: &str = "all";
/// A task's name on the bus is this and its id (`task:<id>`): its endpoint
/// is its row in the TASKS box, where its owners report.
pub const TASK: &str = "task:";

/// Everything the lamaverse shows of the running task.
#[derive(Debug, Clone, Default)]
pub struct TaskView {
    /// Every task, for the TASKS box: newest last.
    pub tasks: Vec<TaskRow>,
    /// The task picked in the TASKS box (its id): its row opens up with its
    /// goals, and its owners' cards light up.
    pub selected: Option<String>,
    /// The conductor, which only dispatches: what the CONDUCTOR box shows.
    pub conductor: ConductorView,
    /// The conductor stopped to ask you something (a ticket it can't read):
    /// its question, shown in the intake; your next line answers it.
    pub asking: Option<String>,
    /// The question is a tool the conductor waits for you to allow: y or n
    /// answers it.
    pub approving: bool,
    /// A plan is waiting for you to choose who runs it: Claude, Codex, or
    /// as planned.
    pub choosing: bool,
    /// While choosing: each owner and who will run it, and the one the
    /// keys act on.
    pub choice: Option<ChoiceView>,
    /// The task's id (its directory name).
    pub id: String,
    /// What the user handed over: a link or the text itself. Empty until
    /// they do — the intake then shows its drop zone.
    pub source: String,
    /// Seconds since the task started, set every frame; the clock every
    /// message's flight is measured against.
    pub now: f64,
    /// Where the task is, in the conductor's words: `PLANNING`,
    /// `AWAITING APPROVAL`, `RUNNING`, `COMPLETE`… Shown as given; the
    /// radar sweeps faster while planning.
    pub phase: String,
    /// The conductor's own session id: hidden from the zones, since it lives
    /// in the left panel.
    pub conductor_session: String,
    /// The conductor's provider and model, for its panel header.
    pub conductor_provider: Option<Provider>,
    pub conductor_model: Option<String>,
    /// The intake conversation, oldest first. Not drawn: the intake only
    /// takes input now, and the conductor's work shows on the lamaverse.
    pub chat: Vec<ChatLine>,
    /// The conductor's recent tool calls and short notes, oldest first.
    pub thoughts: Vec<String>,
    /// The team the conductor proposes; its agents not launched yet show in
    /// the conductor's TEAM as planned.
    pub plan: Option<PlanView>,
    /// The task's goals and whether each is met.
    pub goals: Vec<(String, bool)>,
    /// The agents launched (or about to be), by name.
    pub agents: Vec<AgentLink>,
    /// The bus, oldest first.
    pub messages: Vec<MessageView>,
    /// What the task has spent so far in USD, when priced.
    pub cost: Option<f64>,
    /// The prompt line at the bottom of the intake.
    pub intake: IntakeView,
    /// The message log drawer is open (`⇄` chip toggles it).
    pub log_open: bool,
}

/// Who said a line of the intake chat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Who {
    #[default]
    You,
    Conductor,
    /// lama itself: launches, errors, "plan approved".
    Lama,
}

/// One turn of the intake chat.
#[derive(Debug, Clone, Default)]
pub struct ChatLine {
    pub who: Who,
    pub text: String,
}

/// The conductor's proposed team.
#[derive(Debug, Clone, Default)]
pub struct PlanView {
    /// Bumped on every revision, so an approval names what it approves.
    pub version: u32,
    pub summary: String,
    /// The user said go: the plan leaves the intake.
    pub approved: bool,
    /// When this revision appeared, in [`TaskView::now`]'s seconds: its
    /// planned cards draw themselves in from then, and the camera flies to
    /// the task's projects.
    pub planned_at: f64,
    pub agents: Vec<PlannedView>,
}

/// One agent of a proposed team.
#[derive(Debug, Clone)]
pub struct PlannedView {
    pub name: String,
    /// The project it works in, by directory name.
    pub project_name: String,
    /// The same project as the plane groups it
    /// ([`crate::provider::group_key`] of its directory): the zone its
    /// planned card waits in.
    pub project_key: String,
    pub provider: Provider,
    pub model: String,
    pub role: String,
    /// Why this model for this job, in a line.
    pub why: String,
    /// Launched later by the conductor, not with the rest of the team.
    pub start_later: bool,
    /// The task it belongs to (its id).
    pub task: String,
    /// The owner that asked for it, for a requested peer: its planned card
    /// says so, and its beam starts at that owner.
    pub parent: Option<String>,
}

impl Default for PlannedView {
    fn default() -> Self {
        PlannedView {
            name: String::new(),
            project_name: String::new(),
            project_key: String::new(),
            provider: Provider::Claude,
            model: String::new(),
            role: String::new(),
            why: String::new(),
            start_later: false,
            task: String::new(),
            parent: None,
        }
    }
}

/// A task agent and, once it has one, its session on the canvas.
#[derive(Debug, Clone, Default)]
pub struct AgentLink {
    pub name: String,
    /// The session id once the launched agent has written its transcript;
    /// until then the agent has no card to mark or send from.
    pub session: Option<String>,
    /// The task it works on (its id): a message to or from `task:<id>` is
    /// that task's row in the TASKS box.
    pub task: String,
    /// Who asked for it: an owner's name for a peer (its arrival beam starts
    /// at that owner's card); `None` for an owner, whose beam starts at its
    /// task's row.
    pub parent: Option<String>,
    /// When its session first became known, in [`TaskView::now`]'s
    /// seconds: its card's arrival (a beam from its task's row or its
    /// owner, a burst)
    /// plays from then. `None` for one already there when the task was
    /// opened, so a reopened task doesn't replay its launches.
    pub joined: Option<f64>,
}

/// What a message is for; each kind has its own colour and glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MsgKind {
    Assign,
    Contract,
    Question,
    Answer,
    Blocker,
    #[default]
    Report,
}

/// One message on the bus.
#[derive(Debug, Clone, Default)]
pub struct MessageView {
    /// Sender and receiver by agent name; [`CONDUCTOR`] for the conductor,
    /// [`EVERYONE`] for a broadcast, [`TASK`] and its id for a task.
    pub from: String,
    pub to: String,
    pub kind: MsgKind,
    pub text: String,
    /// When it set off, in [`TaskView::now`]'s seconds. The frontend
    /// stamps it once, when it first reads the message, so a task
    /// reopened later doesn't replay its whole history in flight.
    pub shown_at: f64,
}

impl MessageView {
    /// Seconds since it set off; negative before.
    pub fn age(&self, now: f64) -> f64 {
        now - self.shown_at
    }

    /// Still on screen: travelling or lingering by its receiver.
    pub fn in_flight(&self, now: f64) -> bool {
        (0.0..=TRAVEL_SECS + LINGER_SECS).contains(&self.age(now))
    }

    /// It has reached its receiver (and the log).
    pub fn delivered(&self, now: f64) -> bool {
        self.age(now) >= TRAVEL_SECS
    }
}

/// The intake's prompt line.
#[derive(Debug, Clone, Default)]
pub struct IntakeView {
    /// What the user has typed or pasted so far; may span lines
    /// (Shift+Enter, or a paste).
    pub draft: String,
    /// How many lines the intake's text is scrolled up from its end (0:
    /// following the end, where the cursor is).
    pub scroll: usize,
    /// Keys go to the intake: the cursor shows.
    pub focused: bool,
    /// A one-line note above the prompt: "launching 3 agents…", an error.
    pub notice: Option<String>,
}

impl TaskView {
    /// The session of the agent named `name`, once known.
    pub fn agent_session(&self, name: &str) -> Option<&str> {
        self.agents
            .iter()
            .find(|a| a.name == name)
            .and_then(|a| a.session.as_deref())
    }

    /// Whether session `sid` is one of the task's agents (not the
    /// conductor).
    pub fn is_agent_session(&self, sid: &str) -> bool {
        self.agents
            .iter()
            .any(|a| a.session.as_deref() == Some(sid))
    }

    /// Whether session `sid` is the conductor's, which the zones leave out.
    pub fn is_conductor(&self, sid: &str) -> bool {
        !self.conductor_session.is_empty() && self.conductor_session == sid
    }

    /// Messages that have reached their receiver.
    pub fn delivered(&self) -> usize {
        self.messages
            .iter()
            .filter(|m| m.delivered(self.now))
            .count()
    }

    /// A plan is waiting for the user's go.
    pub fn awaiting_approval(&self) -> bool {
        self.plan.as_ref().is_some_and(|p| !p.approved)
    }
}

/// Where the task's parts were drawn last frame, for the frontend's mouse
/// hits. All `None` when no task is shown.
#[derive(Debug, Clone, Default)]
pub struct TaskHits {
    /// The whole left panel.
    pub panel: Option<Rect>,
    /// The intake box; a click focuses its prompt.
    pub intake: Option<Rect>,
    /// The intake's prompt line itself.
    pub prompt: Option<Rect>,
    /// The conductor box.
    pub conductor: Option<Rect>,
    /// The `⇄ N msgs` chip on the top line, which opens and closes the log.
    pub log_chip: Option<Rect>,
    /// The log drawer, while open.
    pub log: Option<Rect>,
    /// Each task's block in the TASKS box, by task id, top to bottom; a
    /// click selects that task. Messages to `task:<id>` land on it.
    pub rows: Vec<(String, Rect)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(shown_at: f64) -> MessageView {
        MessageView {
            from: "a".into(),
            to: "b".into(),
            shown_at,
            ..MessageView::default()
        }
    }

    #[test]
    fn a_message_flies_then_lingers_then_leaves() {
        let m = msg(10.0);
        assert!(!m.in_flight(9.9), "not yet sent");
        assert!(m.in_flight(10.0) && !m.delivered(11.0));
        assert!(m.delivered(10.0 + TRAVEL_SECS + 0.01));
        assert!(m.in_flight(10.0 + TRAVEL_SECS + LINGER_SECS - 0.01));
        assert!(!m.in_flight(10.0 + TRAVEL_SECS + LINGER_SECS + 0.01));
    }

    #[test]
    fn sessions_tell_agents_from_the_conductor() {
        let v = TaskView {
            conductor_session: "c".into(),
            agents: vec![
                AgentLink {
                    name: "api".into(),
                    session: Some("s1".into()),
                    ..AgentLink::default()
                },
                AgentLink {
                    name: "ui".into(),
                    ..AgentLink::default()
                },
            ],
            messages: vec![msg(0.0), msg(5.0)],
            now: 4.0,
            ..TaskView::default()
        };
        assert!(v.is_agent_session("s1") && !v.is_agent_session("c"));
        assert!(v.is_conductor("c") && !v.is_conductor(""));
        assert_eq!(v.agent_session("api"), Some("s1"));
        assert_eq!(v.agent_session("ui"), None);
        assert_eq!(v.delivered(), 1);
    }
}

/// One task as the TASKS box lists it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TaskRow {
    pub id: String,
    /// What it is about, in a line: the conductor's summary once it has
    /// one, the source until then.
    pub title: String,
    /// The projects its owners work in, by name.
    pub projects: Vec<String>,
    pub status: RowStatus,
    /// Messages delivered so far within this task.
    pub msgs: usize,
    pub cost: Option<f64>,
    /// What "done" means, ticked off by its owners.
    pub goals: Vec<(String, bool)>,
    /// The owners' last report, e.g. "MR !812 reviewed".
    pub note: Option<String>,
}

/// Where a task stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RowStatus {
    /// The conductor is reading it and choosing projects.
    #[default]
    Dispatching,
    /// The conductor asked you something.
    Asking,
    /// Its owners are planned; you choose Claude or Codex.
    Choosing,
    /// Owners launched, their sessions not up yet.
    Launching,
    /// This many of its agents are working.
    Working(usize),
    /// One of its agents waits on a permission prompt.
    Waiting,
    Done,
}

/// The conductor as a dispatcher: busy only while it routes a task.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConductorView {
    /// It is dispatching a task right now.
    pub dispatching: bool,
    /// What it is reading or doing right now (its latest tool call).
    pub reading: Option<String>,
    /// Tasks it has routed to owners.
    pub routed: usize,
    /// Owners it created, over all tasks.
    pub owners: usize,
    pub spent: Option<f64>,
    /// One line per routed task: "SHOP-42 → shop · 1 owner".
    pub log: Vec<String>,
}

/// Who runs a dispatched task's owners, as you set it before they launch.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChoiceView {
    /// The task (its id).
    pub task: String,
    pub owners: Vec<ChoiceOwner>,
    /// The owner ←/→ act on.
    pub cursor: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceOwner {
    pub name: String,
    /// Its project, by directory name.
    pub project: String,
    /// Who runs it: the conductor's suggestion until you change it.
    pub provider: Provider,
    pub model: String,
}
