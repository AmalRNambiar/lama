//! The shared plane: several live sessions on one canvas.
//!
//! Each session keeps its own [`App`] — its own tailer, model, timeline and
//! flow — exactly as when it runs alone. The plane adds one more `App`, the
//! *host*, whose model is empty and whose flow is the canvas on screen: every
//! session's agents are projected into it under an id prefix. Because the host
//! is an ordinary `App`, the camera, selection, keyboard and mouse handling all
//! work on the combined graph unchanged; only the panel and the timeline reach
//! back into the session that owns a node.
//!
//! Two shapes:
//!
//! - a **project** plane: one directory's sessions hanging from a hub;
//! - a **universe**: every recently active project on the machine. Each
//!   session is a *block* of its own — its main card and its helpers — that
//!   you can drag anywhere, whatever project it belongs to; it stays where you
//!   drop it. A new session lands in the free space nearest its project's
//!   newest one, and nothing else moves to make room. The projects themselves
//!   are chips on a strip above the canvas; lighting one dims every other
//!   project's cards and outlines its own. Projects with nothing running
//!   stay off the canvas until opened.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use super::graph::{self, Placement};
use super::lanes;
use super::session::{AgentStatus, SessionModel};
use super::task::{PlannedView, TaskHits, TaskView};
use super::usage::{self, UsageSummary};
use super::{App, Camera, Mode};
use crate::ui::nodes::{AgentNode, Feed, HUB_NODE_DIMS, HubInfo, MAIN_NODE_DIMS, SUB_NODE_DIMS};

/// A project on the plane: what its hub or zone shows.
#[derive(Debug, Clone)]
pub struct Group {
    /// Grouping key ([`crate::provider::group_key`] of the cwd).
    pub key: String,
    /// The directory's name.
    pub name: String,
    pub branch: Option<String>,
    /// The working directory itself, when known.
    pub cwd: Option<std::path::PathBuf>,
}

/// One project as the universe's strip lists it.
#[derive(Debug, Clone)]
pub struct RailItem {
    pub group: usize,
    pub name: String,
    pub live: usize,
    pub agents: usize,
    pub sessions: usize,
    /// How many of its sessions are a task's agents.
    pub on_task: usize,
    /// Its sessions are on the canvas; a quiet one folds into the strip's
    /// `◇ N quiet` chip instead.
    pub open: bool,
    /// Provider emblems present, e.g. `❋❂`.
    pub marks: String,
    pub last: Option<DateTime<Utc>>,
}

pub struct Plane {
    /// Owns the on-screen flow, camera, selection and panel scroll.
    pub host: App,
    pub groups: Vec<Group>,
    /// Group index of each session, by the session's index in `apps`.
    membership: Vec<usize>,
    /// Each session's own working directory, by session index.
    cwds: Vec<Option<std::path::PathBuf>>,
    /// Universe mode: sessions quiet for longer than this leave the plane.
    pub window: Option<chrono::Duration>,
    /// Put every quiet project's sessions on the canvas (`e`, or the strip's
    /// quiet chip).
    pub expand_all: bool,
    /// Quiet projects opened one at a time (`[` `]` onto one).
    expanded: HashSet<usize>,
    /// The universe's projects, live first, then most recently active: the
    /// chips on the strip.
    pub rail: Vec<RailItem>,
    /// Index into `rail` of the project `[` `]` stepped to.
    pub cursor: usize,
    /// A project to zoom onto once the next pack has placed it.
    pending_zoom: Option<usize>,
    /// A node to zoom onto once the next pack has placed it (a console just
    /// opened).
    pending_zoom_node: Option<String>,
    /// Open session consoles on the canvas, by session id, with their size in
    /// world cells. Remembered across runs, like `pins`.
    pub consoles: HashMap<String, (f64, f64)>,
    /// The card each open console is drawn on (the session's main card), by
    /// session index.
    console_hosts: HashMap<usize, String>,
    /// Where the project strip was drawn last frame, and each of its chips,
    /// for the mouse.
    pub strip_area: Option<ratatui::layout::Rect>,
    pub strip_hits: Vec<(ratatui::layout::Rect, Chip)>,
    /// The project lit from the strip: its cards stay bright inside an
    /// outline, every other project's dim.
    pub spot: Option<usize>,
    /// The lama living on the cards (`l` sends him away).
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) mascot: crate::ui::mascot::Mascot,
    /// Sessions waiting on a permission prompt, by session id: the lama
    /// goes and waits by their cards.
    pub asking: HashSet<String>,
    /// Where the `＋ new session` button was drawn last frame (zoomed into a
    /// project), for the mouse.
    pub new_button: Option<ratatui::layout::Rect>,
    /// Camera motion of its own: a zoom easing toward its target, and the
    /// momentum a pan keeps after the pointer lets go.
    motion: Motion,
    /// Subagent cards being born: growing out of their parent into place.
    births: HashMap<String, Birth>,
    /// Sessions synced at least once. A card is only born (animated) in a
    /// session already on screen; a session's first load just appears.
    seen: HashSet<String>,
    /// A layout ran during this sync: births then follow it to the places it
    /// gave their cards.
    laid_out: bool,
    /// A sync wants the home view re-framed, once births are known.
    want_fit: bool,
    /// Repack on the next sync even without a structural change (`r`).
    pub repack: bool,
    /// Where you left each session (the top-left of its block), by session
    /// id. A pinned session stays put through every repack; the packing only
    /// places the rest.
    pub pins: HashMap<String, (f64, f64)>,
    /// `pins` changed since the frontend last saved them.
    pub pins_dirty: bool,
    /// The project the camera is zoomed into, if any (rail highlight and
    /// breadcrumb).
    pub focus: Option<usize>,
    /// Where the last pack put each block, by its key, as `(x, y, w, h)`: an
    /// unpinned block keeps its place while it fits there. Saved with the
    /// pins, so the whole arrangement comes back after a restart.
    pub placed: HashMap<String, (f64, f64, f64, f64)>,
    /// The blocks as of the last pack.
    blocks: Vec<Block>,
    /// The block the camera last zoomed onto by a click.
    focus_block: Option<String>,
    /// The mouse is holding part of a project; see [`Press`].
    press: Option<Press>,
    /// Sessions that finished their turn while lama watched and that you
    /// haven't looked at since, by session id: their main card blinks green.
    pub finished: HashSet<String>,
    /// Sessions whose agent process is running right now, by session id.
    /// They stay on the map however long they have been quiet.
    pub alive: HashSet<String>,
    /// Each session's context and cost, by session id. The frontend meters
    /// transcripts off the UI thread and refreshes this; the next sync
    /// copies it onto the cards and project labels.
    pub usage: HashMap<String, UsageSummary>,
    /// The task running through the lamaverse, if any: its agents are
    /// marked on their cards, its conductor leaves the zones for the left
    /// panel. The frontend refills it every tick.
    pub task: Option<TaskView>,
    /// Where the task's panel, intake and log were drawn last frame, for
    /// the mouse.
    pub task_hits: TaskHits,
    /// The projects the task works in, as of the last sync: always zones,
    /// and what the camera flies to.
    task_zones: Vec<usize>,
    /// The task's cards as of the last sync, as (task id, node id): its
    /// agents' main cards and the planned ones. The camera frames these
    /// when the zones are too wide to read, or one task's when it is picked.
    task_cards: Vec<(String, String)>,
    /// The task's plan and arrivals as the camera last answered them: a
    /// change flies it to the task's zones.
    task_cue: Option<TaskCue>,
    /// When the user last moved the camera themselves. For a while after,
    /// the task leaves it where they put it.
    touched: Option<web_time::Instant>,
}

/// What of a task moves the camera: a new plan revision, a plan growing
/// (another task's), an agent joining, the task's cards taking a new place
/// (rounded world rect).
type TaskCue = (u32, usize, usize, Option<[i64; 4]>);

/// A chip on the universe's project strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chip {
    /// A project: a click lights it up (or puts it out).
    Project(usize),
    /// The quiet projects folded together: a click opens them all (or folds
    /// them back).
    Quiet,
    /// A swatch in the theme picker: a click switches to that theme.
    Theme(crate::ui::fx::Skin),
}

/// One session on the universe canvas — its main card, helpers and lane
/// headers — or a planned card waiting for its session. It moves as one.
#[derive(Debug, Clone)]
struct Block {
    /// The session id, or the planned card's node id.
    key: String,
    group: usize,
    nodes: Vec<String>,
}

/// A press on one of a session's cards. The held node is what the flow
/// drags; the rest of its block follows it. A release in place is a click
/// (zoom in), a release elsewhere was a move.
#[derive(Debug, Clone)]
struct Press {
    group: usize,
    /// The held block's index in `blocks`.
    block: usize,
    held: String,
    start: (f64, f64),
    /// The held node's size at the press: a console whose size changed was
    /// resized, not clicked.
    size: (f64, f64),
    last: (f64, f64),
}

/// The id prefix for session `i`'s nodes.
fn ns(i: usize) -> String {
    format!("{i}/")
}

/// Node id of group `g`'s hub (project plane).
pub fn hub_id(g: usize) -> String {
    format!("◈hub/{g}")
}

/// Node id of group `g`'s zone or tile (universe).
pub fn zone_id(g: usize) -> String {
    format!("◈zone/{g}")
}

/// Node id of the planned card for the task agent `name` in group `g`'s
/// zone: its place until its session arrives.
pub fn ghost_id(g: usize, name: &str) -> String {
    format!("◈ghost/{g}/{name}")
}

/// Session `i`'s main card: where its console opens, in place.
fn root_of(i: usize, app: &App) -> Option<String> {
    app.session
        .spawn_order()
        .next()
        .map(|r| format!("{}{r}", ns(i)))
}

/// A new console's size in world cells: room for a readable conversation and
/// the prompt line at full zoom.
pub const CONSOLE_DIMS: (f64, f64) = (66.0, 26.0);

/// The group behind a zone id.
fn zone_group(id: &str) -> Option<usize> {
    id.strip_prefix("◈zone/")?.parse().ok()
}

/// The group behind a planned card's id.
fn ghost_group(id: &str) -> Option<usize> {
    id.strip_prefix("◈ghost/")?.split_once('/')?.0.parse().ok()
}

/// Whether `id` is plane structure (a hub, zone or tile) rather than an agent.
pub fn is_structure(id: &str) -> bool {
    id.starts_with('◈')
}

/// A lit project's outline round its blocks, in world cells: sides, top
/// (the label), bottom.
const ZONE_PAD_X: f64 = 3.0;
const ZONE_PAD_TOP: f64 = 3.0;
const ZONE_PAD_BOTTOM: f64 = 2.0;
/// Gap between project clusters when the canvas is tidied.
const ZONE_GAP: f64 = 6.0;
/// The least room kept between two blocks: across, and down.
const BLOCK_GAP: (f64, f64) = (6.0, 3.0);
/// A planned card: a compact card's size, small beside the live ones.
pub const GHOST_DIMS: (f64, f64) = (32.0, 7.0);
/// Horizontal gap between sibling cards in a tree.
const TREE_GAP: f64 = 4.0;
/// Vertical gap between a card and its children.
const TREE_RANK: f64 = 5.0;
/// A phase lane's header (see [`crate::state::lanes`]): its title row and
/// one more, so it still has a row when the view is zoomed out.
const LANE_HEAD_DIMS: (f64, f64) = (26.0, 2.0);
/// A helper in a lane, folded to its name; selected, it opens to full size.
const LANE_CARD_DIMS: (f64, f64) = (30.0, 4.0);
/// How far a lane's cards sit right of its spine.
const LANE_INDENT: f64 = 3.0;
/// Rows between cards stacked in a lane.
const LANE_GAP: f64 = 1.0;

/// How far one wheel notch zooms. Small: a trackpad's two-finger scroll
/// sends many notches a second, and they compound.
const WHEEL_STEP: f64 = 1.1;
/// How far ahead of the current zoom a burst of notches may run, so a fast
/// flick can't overshoot.
const WHEEL_LEAD: f64 = 1.8;
const ZOOM_RANGE: (f64, f64) = (0.1, 4.0);
/// Easing times: how fast a zoom closes on its target, how long a flung pan
/// takes to coast to rest.
const ZOOM_EASE: f64 = 0.08;
/// How closely the drawn view trails a drag-pan: a pan reaches the screen a
/// cell at a time, the eased view glides between them.
const PAN_EASE: f64 = 0.04;
const PAN_FRICTION: f64 = 0.28;
/// Below this a coasting pan stops (canvas cells per second).
const PAN_REST: f64 = 3.0;
/// How long the camera takes to fly to the task's projects.
const FLY_SECS: f64 = 1.2;
/// How long after the user last moved the camera the task leaves it be.
const HANDS_OFF_SECS: f64 = 10.0;
/// The closest the camera flies in: cards at full size, no nearer.
const FLY_ZOOM_MAX: f64 = 1.0;
/// The farthest out the camera frames the task's whole zones: below it the
/// planned cards' text is gone, so it frames the task's cards instead.
const READABLE_ZOOM: f64 = 0.6;

/// Smooth camera motion: wheel zoom eases instead of jumping, and a pan
/// released mid-drag coasts to a stop.
#[derive(Default)]
struct Motion {
    /// Zoom target and the canvas point it is anchored at.
    zoom: Option<(f64, rataflow::types::Position)>,
    /// Coasting pan velocity, canvas cells per second.
    coast: Option<(f64, f64)>,
    /// Where a drag-pan has put the view; the drawn view eases to it.
    pan_to: Option<(f64, f64)>,
    /// Recent viewport positions during a drag: (when, x, y).
    trail: Vec<(web_time::Instant, f64, f64)>,
    /// A flight to the task's projects under way.
    fly: Option<Fly>,
}

/// The camera flying from one view to another: the world point at the
/// canvas's centre and the zoom, at each end. The centre glides and the
/// zoom scales evenly, so the move reads as one sweep.
#[derive(Debug, Clone, Copy)]
struct Fly {
    from: (f64, f64, f64),
    to: (f64, f64, f64),
    /// How far through, 0..1.
    t: f64,
}

/// Agents laid out as trees: the roots, each node's children, and the nodes
/// whose children stack in a column (phase lanes) instead of a row.
struct Forest {
    roots: Vec<String>,
    kids: HashMap<String, Vec<String>>,
    stacks: HashSet<String>,
}

/// Lay trees out top-down, every node at its own size: a node centred over
/// its children, siblings side by side, trees next to each other. Unlike the
/// library's layout — which spaces every node by the largest — a grown card
/// (an open console) takes only the room it needs. A `stacks` node (a phase
/// lane's header) sits at the left of its column instead, its children
/// stacked below it, indented past the spine. Positions start at (0, 0).
fn tree_layout(
    roots: &[String],
    kids: &HashMap<String, Vec<String>>,
    stacks: &HashSet<String>,
    sizes: &HashMap<String, (f64, f64)>,
) -> Vec<(String, f64, f64)> {
    struct Ctx<'a> {
        kids: &'a HashMap<String, Vec<String>>,
        stacks: &'a HashSet<String>,
        sizes: &'a HashMap<String, (f64, f64)>,
        wide: HashMap<String, f64>,
        tall: HashMap<String, f64>,
        out: Vec<(String, f64, f64)>,
    }
    impl Ctx<'_> {
        fn size(&self, id: &str) -> (f64, f64) {
            self.sizes.get(id).copied().unwrap_or(MAIN_NODE_DIMS)
        }
        fn kids(&self, id: &str, depth: usize) -> Vec<String> {
            match self.kids.get(id) {
                Some(ks) if depth < 64 => ks.clone(),
                _ => Vec::new(),
            }
        }
        /// The width a node's subtree takes.
        fn span(&mut self, id: &str, depth: usize) -> f64 {
            if let Some(&w) = self.wide.get(id) {
                return w;
            }
            let own = self.size(id).0;
            let ks = self.kids(id, depth);
            let below = if ks.is_empty() {
                0.0
            } else if self.stacks.contains(id) {
                LANE_INDENT
                    + ks.iter()
                        .map(|k| self.span(k, depth + 1))
                        .fold(0.0, f64::max)
            } else {
                ks.iter().map(|k| self.span(k, depth + 1)).sum::<f64>()
                    + TREE_GAP * (ks.len() - 1) as f64
            };
            let w = own.max(below);
            self.wide.insert(id.to_string(), w);
            w
        }
        /// The height a node's subtree takes.
        fn height(&mut self, id: &str, depth: usize) -> f64 {
            if let Some(&h) = self.tall.get(id) {
                return h;
            }
            let own = self.size(id).1;
            let ks = self.kids(id, depth);
            let below = if ks.is_empty() {
                0.0
            } else if self.stacks.contains(id) {
                ks.iter()
                    .map(|k| LANE_GAP + self.height(k, depth + 1))
                    .sum::<f64>()
            } else {
                TREE_RANK
                    + ks.iter()
                        .map(|k| self.height(k, depth + 1))
                        .fold(0.0, f64::max)
            };
            let h = own + below;
            self.tall.insert(id.to_string(), h);
            h
        }
        fn place(&mut self, id: &str, left: f64, top: f64, depth: usize) {
            let s = self.span(id, depth);
            let (w, h) = self.size(id);
            let ks = self.kids(id, depth);
            if self.stacks.contains(id) {
                self.out.push((id.to_string(), left, top));
                let mut y = top + h;
                for k in ks {
                    y += LANE_GAP;
                    self.place(&k, left + LANE_INDENT, y, depth + 1);
                    y += self.height(&k, depth + 1);
                }
                return;
            }
            self.out.push((id.to_string(), left + (s - w) / 2.0, top));
            if ks.is_empty() {
                return;
            }
            let below: f64 = ks.iter().map(|k| self.span(k, depth + 1)).sum::<f64>()
                + TREE_GAP * ks.len().saturating_sub(1) as f64;
            let mut x = left + (s - below) / 2.0;
            for k in ks {
                self.place(&k, x, top + h + TREE_RANK, depth + 1);
                x += self.span(&k, depth + 1) + TREE_GAP;
            }
        }
    }
    let mut ctx = Ctx {
        kids,
        stacks,
        sizes,
        wide: HashMap::new(),
        tall: HashMap::new(),
        out: Vec::new(),
    };
    let mut x = 0.0;
    for r in roots {
        ctx.place(r, x, 0.0, 0);
        x += ctx.span(r, 0) + TREE_GAP * 2.0;
    }
    ctx.out
}

/// How long a new subagent card takes to grow out of its parent into place.
const BIRTH_SECS: f64 = 0.6;
/// The size a card is born at, under its parent.
const BIRTH_DIMS: (f64, f64) = (4.0, 2.0);

/// A card growing out of its parent: from where, to where, at what final
/// size, since when — and where it is drawn now.
#[derive(Debug, Clone, Copy)]
struct Birth {
    from: (f64, f64),
    to: (f64, f64),
    size: (f64, f64),
    start: web_time::Instant,
    at: (f64, f64, f64, f64),
}

/// Per-project tallies for one sync.
#[derive(Default, Clone)]
struct Tally {
    sessions: usize,
    agents: usize,
    live: usize,
    tokens: u64,
    last: Option<DateTime<Utc>>,
    marks: String,
    /// Sessions whose agent process is running.
    open: usize,
    /// Sessions with a console open on the canvas.
    consoles: usize,
    /// Sessions that are a task's agents.
    task: usize,
    /// The task's planned agents that work here, launched or not.
    planned: usize,
}

/// A world rect as `(x, y, w, h)`.
type Rect4 = (f64, f64, f64, f64);

/// A block laid out for a pack: its nodes relative to its top-left corner.
struct Laid {
    key: String,
    g: usize,
    w: f64,
    h: f64,
    nodes: Vec<(String, f64, f64)>,
}

impl Laid {
    /// A block from a tree layout, moved so its top-left corner is (0, 0).
    /// `None` when it has no nodes.
    fn new(
        key: String,
        g: usize,
        placed: Vec<(String, f64, f64)>,
        sizes: &HashMap<String, (f64, f64)>,
    ) -> Option<Laid> {
        let size = |id: &str| sizes.get(id).copied().unwrap_or(MAIN_NODE_DIMS);
        let x0 = placed.iter().map(|n| n.1).reduce(f64::min)?;
        let y0 = placed.iter().map(|n| n.2).reduce(f64::min)?;
        let nodes: Vec<(String, f64, f64)> = placed
            .into_iter()
            .map(|(id, x, y)| (id, x - x0, y - y0))
            .collect();
        let w = nodes
            .iter()
            .map(|(id, x, _)| x + size(id).0)
            .fold(0.0, f64::max);
        let h = nodes
            .iter()
            .map(|(id, _, y)| y + size(id).1)
            .fold(0.0, f64::max);
        Some(Laid {
            key,
            g,
            w,
            h,
            nodes,
        })
    }
}

/// Whether two rects come closer than [`BLOCK_GAP`].
fn crowds(a: Rect4, b: Rect4) -> bool {
    a.0 < b.0 + b.2 + BLOCK_GAP.0
        && b.0 < a.0 + a.2 + BLOCK_GAP.0
        && a.1 < b.1 + b.3 + BLOCK_GAP.1
        && b.1 < a.1 + a.3 + BLOCK_GAP.1
}

/// The free place nearest `want` for a block of `size`, clear of every
/// `taken` rect: `want` itself if it is free, else a spot flush beside,
/// above or below one of them. Across counts half as far as down, as a
/// terminal cell is about twice as tall as it is wide.
fn land(want: (f64, f64), size: (f64, f64), taken: &[Rect4]) -> (f64, f64) {
    let free = |(x, y): (f64, f64)| taken.iter().all(|&t| !crowds((x, y, size.0, size.1), t));
    if free(want) {
        return want;
    }
    let (gx, gy) = BLOCK_GAP;
    let cost = |(x, y): (f64, f64)| ((x - want.0) / 2.0).powi(2) + (y - want.1).powi(2);
    taken
        .iter()
        .flat_map(|&(x, y, w, h)| {
            [
                (x + w + gx, y),
                (x - size.0 - gx, y),
                (x, y + h + gy),
                (x, y - size.1 - gy),
                (x + w + gx, want.1),
                (x - size.0 - gx, want.1),
                (want.0, y + h + gy),
                (want.0, y - size.1 - gy),
            ]
        })
        .filter(|&c| free(c))
        .min_by(|a, b| cost(*a).total_cmp(&cost(*b)))
        .unwrap_or_else(|| {
            let floor = taken.iter().map(|t| t.1 + t.3).fold(want.1, f64::max);
            (want.0, floor + gy)
        })
}

/// The tidy arrangement: each project's blocks side by side, the projects
/// packed in rows about as wide as their total area would make square on a
/// terminal (cells are ~2:1, so a wider-than-tall row reads square).
fn cluster(laid: &[Laid]) -> HashMap<String, (f64, f64)> {
    let mut groups: Vec<(usize, Vec<&Laid>)> = Vec::new();
    for b in laid {
        match groups.iter_mut().find(|(g, _)| *g == b.g) {
            Some((_, list)) => list.push(b),
            None => groups.push((b.g, vec![b])),
        }
    }
    let gap = TREE_GAP * 2.0;
    let dims: Vec<(f64, f64)> = groups
        .iter()
        .map(|(_, list)| {
            let w = list.iter().map(|b| b.w).sum::<f64>() + gap * (list.len() - 1) as f64;
            (w, list.iter().map(|b| b.h).fold(0.0, f64::max))
        })
        .collect();
    let area: f64 = dims.iter().map(|(w, h)| w * h).sum();
    let widest = dims.iter().map(|d| d.0).fold(0.0, f64::max);
    let row_w = widest.max((area * 3.0).sqrt());
    let mut out = HashMap::new();
    let (mut x, mut y, mut row_h) = (0.0, 0.0, 0.0_f64);
    for ((_, list), (w, h)) in groups.iter().zip(dims) {
        if x > 0.0 && x + w > row_w {
            x = 0.0;
            y += row_h + ZONE_GAP;
            row_h = 0.0;
        }
        let mut bx = x;
        for b in list {
            out.insert(b.key.clone(), (bx, y));
            bx += b.w + gap;
        }
        x += w + ZONE_GAP;
        row_h = row_h.max(h);
    }
    out
}

fn hub_node(info: HubInfo) -> AgentNode {
    AgentNode {
        title: info.project.clone(),
        description: None,
        status: if info.live > 0 {
            AgentStatus::Running
        } else {
            AgentStatus::Idle
        },
        tool_count: 0,
        last_tool: None,
        output_tokens: info.tokens,
        interactive: true,
        feed: Feed {
            hub: Some(info),
            ..Feed::default()
        },
    }
}

impl Plane {
    /// A project plane; sessions join it with [`add_session`](Self::add_session).
    pub fn new() -> Self {
        Plane {
            host: App::new(String::new(), Mode::Live),
            groups: Vec::new(),
            membership: Vec::new(),
            cwds: Vec::new(),
            window: None,
            expand_all: false,
            expanded: HashSet::new(),
            rail: Vec::new(),
            cursor: 0,
            pending_zoom: None,
            pending_zoom_node: None,
            consoles: HashMap::new(),
            console_hosts: HashMap::new(),
            strip_area: None,
            strip_hits: Vec::new(),
            spot: None,
            mascot: crate::ui::mascot::Mascot::new(),
            asking: HashSet::new(),
            new_button: None,
            motion: Motion::default(),
            births: HashMap::new(),
            seen: HashSet::new(),
            laid_out: false,
            want_fit: false,
            repack: false,
            pins: HashMap::new(),
            pins_dirty: false,
            focus: None,
            placed: HashMap::new(),
            blocks: Vec::new(),
            focus_block: None,
            press: None,
            alive: HashSet::new(),
            finished: HashSet::new(),
            usage: HashMap::new(),
            task: None,
            task_hits: TaskHits::default(),
            task_zones: Vec::new(),
            task_cards: Vec::new(),
            task_cue: None,
            touched: None,
        }
    }

    /// A universe: every project, sessions active within `window`.
    pub fn universe(window: chrono::Duration) -> Self {
        Plane {
            window: Some(window),
            ..Self::new()
        }
    }

    /// Register the next session (index `apps.len()` once pushed) under its
    /// project, creating the project on first sight.
    ///
    /// `group.cwd` is the session's own working directory. The project's may
    /// differ: a task agent's worktree belongs to its project's zone, which
    /// is named and rooted at the project itself.
    pub fn add_session(&mut self, mut group: Group) {
        self.cwds.push(group.cwd.clone());
        let g = match self.groups.iter().position(|x| x.key == group.key) {
            Some(g) => g,
            None => {
                if let Some(cwd) = group.cwd.take() {
                    let project = crate::provider::project_dir(&cwd).to_path_buf();
                    if project != cwd {
                        if let Some(name) = project.file_name() {
                            group.name = name.to_string_lossy().into_owned();
                        }
                        // The worktree's branch is the agent's, not the project's.
                        group.branch = None;
                    }
                    group.cwd = Some(project);
                }
                self.groups.push(group);
                self.groups.len() - 1
            }
        };
        self.membership.push(g);
    }

    /// The directory session `i` runs in: its own, which for a task agent in
    /// a worktree is not its project's.
    pub fn session_cwd(&self, i: usize) -> Option<&std::path::Path> {
        self.cwds
            .get(i)
            .and_then(|c| c.as_deref())
            .or_else(|| self.groups.get(self.group_of(i))?.cwd.as_deref())
    }

    /// Group index of session `i`.
    pub fn group_of(&self, i: usize) -> usize {
        self.membership.get(i).copied().unwrap_or(0)
    }

    /// Sessions on the plane: they have shown up (a provider with nothing in a
    /// project stays off until it starts) and, in a universe, were active
    /// within the window or still have their agent process running.
    pub fn shown(&self, apps: &[App]) -> Vec<usize> {
        let cutoff = self.window.map(|w| Utc::now() - w);
        (0..apps.len())
            .filter(|&i| !apps[i].timeline.items.is_empty())
            .filter(|&i| match cutoff {
                None => true,
                Some(c) => {
                    self.alive.contains(&apps[i].current_session_id)
                        || apps[i].session.last_activity.is_some_and(|t| t >= c)
                }
            })
            .collect()
    }

    /// The shown sessions that get cards: all but a task's conductor,
    /// which the left panel shows instead.
    pub fn on_canvas(&self, apps: &[App]) -> Vec<usize> {
        let mut shown = self.shown(apps);
        if let Some(m) = &self.task {
            shown.retain(|&i| !m.is_conductor(&apps[i].current_session_id));
        }
        shown
    }

    fn tallies(&self, apps: &[App], shown: &[usize]) -> Vec<Tally> {
        let mut t = vec![Tally::default(); self.groups.len()];
        for &i in shown {
            let g = &mut t[self.group_of(i)];
            g.sessions += 1;
            g.open += usize::from(self.alive.contains(&apps[i].current_session_id));
            g.consoles += usize::from(self.consoles.contains_key(&apps[i].current_session_id));
            g.task += usize::from(
                self.task
                    .as_ref()
                    .is_some_and(|m| m.is_agent_session(&apps[i].current_session_id)),
            );
            let s = &apps[i].session;
            g.last = g.last.max(s.last_activity);
            let mark = match s.provider() {
                Some(crate::provider::Provider::Claude) => "❋",
                Some(crate::provider::Provider::Codex) => "❂",
                None => "",
            };
            if !g.marks.contains(mark) {
                g.marks.push_str(mark);
            }
            for id in s.spawn_order() {
                if let Some(a) = s.agent(id) {
                    g.agents += 1;
                    g.live += usize::from(a.status == AgentStatus::Running);
                    g.tokens += a.output_tokens;
                }
            }
        }
        let plan = self.task.iter().flat_map(|m| &m.plan);
        for p in plan.flat_map(|p| &p.agents) {
            if let Some(g) = self.group_keyed(&p.project_key) {
                t[g].planned += 1;
            }
        }
        t
    }

    /// The group whose key is `key`, if the plane has it.
    fn group_keyed(&self, key: &str) -> Option<usize> {
        if key.is_empty() {
            return None;
        }
        self.groups.iter().position(|g| g.key == key)
    }

    /// The task's planned agents without a card yet, as (group, name, task): the
    /// plan names their project and the plane has it, but their session
    /// isn't on the canvas — not launched, or no transcript yet. An agent
    /// whose card is shown leaves this list in the same sync.
    fn ghosts(&self, apps: &[App], shown: &[usize]) -> Vec<(usize, String, String)> {
        let Some(m) = &self.task else {
            return Vec::new();
        };
        let carded: HashSet<&str> = shown
            .iter()
            .map(|&i| apps[i].current_session_id.as_str())
            .collect();
        let mut out: Vec<(usize, String, String)> = Vec::new();
        for p in m.plan.iter().flat_map(|p| &p.agents) {
            let Some(g) = self.group_keyed(&p.project_key) else {
                continue;
            };
            let has_card = m.agent_session(&p.name).is_some_and(|s| carded.contains(s));
            if has_card || out.iter().any(|(h, n, _)| *h == g && *n == p.name) {
                continue;
            }
            out.push((g, p.name.clone(), p.task.clone()));
        }
        out
    }

    /// Whether a universe project shows as a zone (its agents on the canvas)
    /// rather than a tile: something is running, a session is open in a
    /// terminal, the task works there, or the user opened it.
    fn is_open(&self, g: usize, tally: &Tally) -> bool {
        tally.live > 0
            || tally.open > 0
            || tally.consoles > 0
            || tally.task > 0
            || tally.planned > 0
            || self.expand_all
            || self.expanded.contains(&g)
    }

    /// Project every shown session into the host flow. `force` rebuilds
    /// every card (the once-a-second refresh that keeps sparklines sliding).
    pub fn sync(&mut self, apps: &[App], force: bool) {
        if self.window.is_some() {
            self.sync_universe(apps, force);
        } else {
            self.sync_project(apps, force);
        }
    }

    /// Upsert a structure node; its content is rebuilt every sync.
    fn put_structure(&mut self, id: &str, info: HubInfo, dims: (f64, f64), frame: bool) -> bool {
        let flow = &mut self.host.flow;
        if let Some(existing) = flow.node_content_mut(id) {
            *existing = hub_node(info);
            return false;
        }
        // A hub has no panel of its own: out of selection, so tab and
        // clicks go straight to agents.
        let mut node = graph::new_node(id.to_string(), (0.0, 0.0), dims, hub_node(info))
            .with_selectable(false);
        if frame {
            // Behind its agents, and see-through: only the label is drawn,
            // so edges, cards and the canvas show inside.
            node = node.with_z_index(-1).with_opaque(false);
        }
        flow.add_node(node).is_ok()
    }

    /// Upsert the phase-lane headers of a session's agents with many
    /// helpers. Returns whether any was added, and each laned helper's
    /// header (by agent id).
    fn put_lanes(
        &mut self,
        prefix: &str,
        model: &SessionModel,
        keep: &mut HashSet<String>,
    ) -> (bool, HashMap<String, String>) {
        let mut added = false;
        let mut map = HashMap::new();
        for lane in lanes::lanes(model) {
            let id = lane.node_id(prefix);
            let live = lane
                .kids
                .iter()
                .filter(|k| {
                    model
                        .agent(k)
                        .is_some_and(|a| a.status == AgentStatus::Running)
                })
                .count();
            let info = HubInfo {
                project: lane.phase.title().to_string(),
                agents: lane.kids.len(),
                live,
                lane: true,
                ..HubInfo::default()
            };
            added |= self.put_structure(&id, info, LANE_HEAD_DIMS, false);
            for k in &lane.kids {
                map.insert(k.clone(), id.clone());
            }
            keep.insert(id);
        }
        (added, map)
    }

    /// After a session's cards synced: each lane header hangs from its
    /// parent, and each laned helper is folded unless selected. Returns
    /// whether anything changed shape.
    fn finish_lanes(
        &mut self,
        prefix: &str,
        model: &SessionModel,
        lanes: &HashMap<String, String>,
    ) -> bool {
        let mut changed = false;
        let selected = self.host.selected_agent_id();
        for lane in lanes::lanes(model) {
            let id = lane.node_id(prefix);
            let live = lane.kids.iter().any(|k| {
                model
                    .agent(k)
                    .is_some_and(|a| a.status == AgentStatus::Running)
            });
            changed |= graph::add_or_refresh_edge(
                &mut self.host.flow,
                &format!("{prefix}{}", lane.parent),
                &id,
                live,
            );
        }
        for agent in lanes.keys() {
            let node = format!("{prefix}{agent}");
            let want = if selected.as_deref() == Some(node.as_str()) {
                SUB_NODE_DIMS
            } else {
                LANE_CARD_DIMS
            };
            let Some(n) = self.host.flow.node(&node) else {
                continue;
            };
            if (n.width, n.height) != want && !self.births.contains_key(&node) {
                self.host.flow.set_node_dimensions(&node, want.0, want.1);
                changed = true;
            }
        }
        changed
    }

    fn sync_project(&mut self, apps: &[App], force: bool) {
        let before: HashSet<String> = self.host.flow.nodes().map(|n| n.id.clone()).collect();
        self.sync_project_inner(apps, force);
        let shown = self.on_canvas(apps);
        self.animate_births(apps, &shown, &before);
        if std::mem::take(&mut self.want_fit) {
            self.fit_all();
        }
    }

    fn sync_project_inner(&mut self, apps: &[App], force: bool) {
        let shown = self.on_canvas(apps);
        let tallies = self.tallies(apps, &shown);
        let mut structural = false;
        let mut keep: HashSet<String> = HashSet::new();

        for (g, t) in tallies.iter().enumerate() {
            if t.sessions == 0 {
                continue;
            }
            let hub = hub_id(g);
            let info = HubInfo {
                project: self.groups[g].name.clone(),
                branch: self.groups[g].branch.clone(),
                agents: t.agents,
                live: t.live,
                tokens: t.tokens,
                sessions: t.sessions,
                cost_today: self.cost_today_of(apps, g),
                on_task: t.task,
                ..HubInfo::default()
            };
            structural |= self.put_structure(&hub, info, HUB_NODE_DIMS, false);
            keep.insert(hub);
        }

        for &i in &shown {
            let app = &apps[i];
            let prefix = ns(i);
            let hub = hub_id(self.group_of(i));
            let (added, lanes) = self.put_lanes(&prefix, &app.session, &mut keep);
            structural |= added;
            let at = Placement {
                ns: &prefix,
                root_parent: Some(&hub),
                force,
                now: app.timeline.now_reference(),
                wall_clock: app.timeline.follow_head,
                lanes: Some(&lanes),
            };
            structural |= graph::sync_placed(&mut self.host.flow, &app.session, false, &at);
            structural |= self.finish_lanes(&prefix, &app.session, &lanes);
            keep.extend(app.session.spawn_order().map(|id| format!("{prefix}{id}")));
            structural |= self.host_console(i, app);
            self.mark_finished(i, app);
        }

        structural |= self.drop_stale(&keep);
        if structural || std::mem::take(&mut self.repack) {
            // Sessions arrive at different times and stack under the hub, so
            // local placement alone overlaps whole trees. Overview is "show me
            // everything", so it keeps the plane tidy; once the user takes the
            // camera (Follow, Manual) nothing moves until they ask (`r`).
            if self.host.camera == Camera::Overview || self.pending_zoom_node.is_some() {
                self.layout_project(apps, &shown);
                match self.pending_zoom_node.take() {
                    Some(id) => self.zoom_to_node(&id),
                    None => self.want_fit = true,
                }
            } else {
                self.host.layout_dirty = true;
            }
        }
    }

    /// A session's agents as a forest: its root(s), and each agent's
    /// children, by host node id.
    fn forest(&self, apps: &[App], members: &[usize]) -> Forest {
        let mut roots = Vec::new();
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        let mut stacks = HashSet::new();
        for &i in members {
            let prefix = ns(i);
            let s = &apps[i].session;
            // A helper in a phase lane hangs from the lane's header, which
            // hangs from the parent (lanes in phase order).
            let mut laned: HashMap<String, String> = HashMap::new();
            for lane in lanes::lanes(s) {
                let head = lane.node_id(&prefix);
                if self.host.flow.node(&head).is_none() {
                    continue;
                }
                kids.entry(format!("{prefix}{}", lane.parent))
                    .or_default()
                    .push(head.clone());
                for k in &lane.kids {
                    laned.insert(k.clone(), head.clone());
                }
                stacks.insert(head);
            }
            for id in s.spawn_order() {
                let node = format!("{prefix}{id}");
                if self.host.flow.node(&node).is_none() {
                    continue;
                }
                if let Some(head) = laned.get(id) {
                    kids.entry(head.clone()).or_default().push(node);
                    continue;
                }
                match s.agent(id).and_then(|a| a.parent.as_deref()) {
                    Some(p) if s.agent(p).is_some() => {
                        kids.entry(format!("{prefix}{p}")).or_default().push(node)
                    }
                    _ => roots.push(node),
                }
            }
        }
        Forest {
            roots,
            kids,
            stacks,
        }
    }

    /// Every node's current size in the host flow.
    fn sizes(&self) -> HashMap<String, (f64, f64)> {
        self.host
            .flow
            .nodes()
            .map(|n| {
                // A card still growing is laid out at the size it grows to.
                let size = self
                    .births
                    .get(&n.id)
                    .map_or((n.width, n.height), |b| b.size);
                (n.id.clone(), size)
            })
            .collect()
    }

    /// After a sync: cards new since `before` in sessions already on screen
    /// are born — set back to a dot under their parent, to grow into the
    /// place the layout just gave them. Births already under way follow the
    /// layout to their new place without restarting.
    fn animate_births(&mut self, apps: &[App], shown: &[usize], before: &HashSet<String>) {
        // Only a layout moves a growing card's destination; otherwise its
        // node holds the drawn (animated) position, which is no target.
        if std::mem::take(&mut self.laid_out) {
            for (id, b) in self.births.iter_mut() {
                if let Some(n) = self.host.flow.node(id) {
                    b.to = (n.position.x, n.position.y);
                }
            }
        }
        let flow = &mut self.host.flow;
        for (id, b) in &self.births {
            flow.set_node_position(id, (b.at.0, b.at.1));
            flow.set_node_dimensions(id, b.at.2, b.at.3);
        }
        let new: Vec<String> = self
            .host
            .flow
            .nodes()
            .filter(|n| !before.contains(&n.id) && !is_structure(&n.id))
            .map(|n| n.id.clone())
            .collect();
        for id in new {
            let Some((i, agent)) = Self::resolve(&id) else {
                continue;
            };
            let Some(app) = apps.get(i) else { continue };
            if !self.seen.contains(&app.current_session_id) {
                continue;
            }
            let Some(parent) = app.session.agent(agent).and_then(|a| a.parent.clone()) else {
                continue;
            };
            let flow = &self.host.flow;
            let (Some(p), Some(n)) = (flow.node(&format!("{}{parent}", ns(i))), flow.node(&id))
            else {
                continue;
            };
            let from = (
                p.position.x + p.width / 2.0 - BIRTH_DIMS.0 / 2.0,
                p.position.y + p.height - BIRTH_DIMS.1,
            );
            let birth = Birth {
                from,
                to: (n.position.x, n.position.y),
                size: (n.width, n.height),
                start: web_time::Instant::now(),
                at: (from.0, from.1, BIRTH_DIMS.0, BIRTH_DIMS.1),
            };
            self.host.flow.set_node_position(&id, from);
            self.host
                .flow
                .set_node_dimensions(&id, BIRTH_DIMS.0, BIRTH_DIMS.1);
            self.births.insert(id, birth);
        }
        for &i in shown {
            self.seen.insert(apps[i].current_session_id.clone());
        }
    }

    /// Advance the births: ease out, so a card leaves its parent quickly and
    /// settles gently into place.
    fn tick_births(&mut self) {
        let flow = &mut self.host.flow;
        self.births.retain(|id, b| {
            if flow.node(id).is_none() {
                return false;
            }
            let k = (b.start.elapsed().as_secs_f64() / BIRTH_SECS).min(1.0);
            let e = 1.0 - (1.0 - k).powi(3);
            let lerp = |a: f64, z: f64| a + (z - a) * e;
            b.at = (
                lerp(b.from.0, b.to.0),
                lerp(b.from.1, b.to.1),
                lerp(BIRTH_DIMS.0, b.size.0),
                lerp(BIRTH_DIMS.1, b.size.1),
            );
            flow.set_node_position(id, (b.at.0, b.at.1));
            flow.set_node_dimensions(id, b.at.2, b.at.3);
            k < 1.0
        });
    }

    /// Project plane: the hub over its sessions' trees, every card at its own
    /// size — an open console is its session's main card, grown in place.
    fn layout_project(&mut self, apps: &[App], shown: &[usize]) {
        let Forest {
            roots: session_roots,
            mut kids,
            stacks,
        } = self.forest(apps, shown);
        let hubs: Vec<String> = (0..self.groups.len())
            .map(hub_id)
            .filter(|h| self.host.flow.node(h).is_some())
            .collect();
        let roots = match hubs.first() {
            Some(hub) => {
                kids.insert(hub.clone(), session_roots);
                hubs.clone()
            }
            None => session_roots,
        };
        let sizes = self.sizes();
        for (id, x, y) in tree_layout(&roots, &kids, &stacks, &sizes) {
            self.host.flow.set_node_position(&id, (x, y));
        }
        self.laid_out = true;
    }

    fn sync_universe(&mut self, apps: &[App], force: bool) {
        let before: HashSet<String> = self.host.flow.nodes().map(|n| n.id.clone()).collect();
        self.sync_universe_inner(apps, force);
        let shown = self.on_canvas(apps);
        self.animate_births(apps, &shown, &before);
        // A flight to the task wins over the home view's re-fit.
        let flew = self.follow_task();
        if std::mem::take(&mut self.want_fit) && !flew {
            self.fit_all();
        }
    }

    fn sync_universe_inner(&mut self, apps: &[App], force: bool) {
        let shown = self.on_canvas(apps);
        let tallies = self.tallies(apps, &shown);

        // Strip order: live projects first, then the most recently active.
        // A project the task plans to work in is on the map before any
        // session of its own is.
        let mut order: Vec<usize> = (0..self.groups.len())
            .filter(|&g| tallies[g].sessions > 0 || tallies[g].planned > 0)
            .collect();
        self.task_zones = order
            .iter()
            .copied()
            .filter(|&g| tallies[g].task > 0 || tallies[g].planned > 0)
            .collect();
        order.sort_by_key(|&g| {
            let t = &tallies[g];
            (t.live == 0, t.open == 0, std::cmp::Reverse(t.last))
        });
        let highlighted = self.rail.get(self.cursor).map(|r| r.group);
        self.rail = order
            .iter()
            .map(|&g| RailItem {
                group: g,
                name: self.groups[g].name.clone(),
                live: tallies[g].live,
                agents: tallies[g].agents,
                sessions: tallies[g].sessions,
                on_task: tallies[g].task,
                open: self.is_open(g, &tallies[g]),
                marks: tallies[g].marks.clone(),
                last: tallies[g].last,
            })
            .collect();
        // The highlight follows its project when the order shifts.
        if let Some(g) = highlighted {
            self.cursor = self.rail.iter().position(|r| r.group == g).unwrap_or(0);
        }

        let mut structural = false;
        let mut keep: HashSet<String> = HashSet::new();
        // The lit project's outline: placed round its blocks after the pack.
        if let Some(g) = self.spot.filter(|&g| order.contains(&g)) {
            let t = &tallies[g];
            let id = zone_id(g);
            let info = HubInfo {
                project: self.groups[g].name.clone(),
                branch: self.groups[g].branch.clone(),
                agents: t.agents,
                live: t.live,
                tokens: t.tokens,
                sessions: t.sessions,
                zone: true,
                marks: t.marks.clone(),
                last: t.last,
                cost_today: self.cost_today_of(apps, g),
                on_task: t.task,
                ..HubInfo::default()
            };
            structural |= self.put_structure(&id, info, MAIN_NODE_DIMS, true);
            keep.insert(id);
        }

        for &i in &shown {
            let g = self.group_of(i);
            if !self.is_open(g, &tallies[g]) {
                continue;
            }
            let app = &apps[i];
            let prefix = ns(i);
            let (added, lanes) = self.put_lanes(&prefix, &app.session, &mut keep);
            structural |= added;
            let at = Placement {
                ns: &prefix,
                root_parent: None,
                force,
                now: app.timeline.now_reference(),
                wall_clock: app.timeline.follow_head,
                lanes: Some(&lanes),
            };
            structural |= graph::sync_placed(&mut self.host.flow, &app.session, false, &at);
            structural |= self.finish_lanes(&prefix, &app.session, &lanes);
            keep.extend(app.session.spawn_order().map(|id| format!("{prefix}{id}")));
            structural |= self.host_console(i, app);
            self.mark_finished(i, app);
        }
        // The task's agents not here yet wait as planned cards in their
        // zones; each goes the sync its own card comes.
        self.task_cards.clear();
        for (g, name, task) in self.ghosts(apps, &shown) {
            let id = ghost_id(g, &name);
            structural |= self.put_ghost(&id, &name);
            keep.insert(id.clone());
            self.task_cards.push((task, id));
        }
        if let Some(m) = &self.task {
            for &i in &shown {
                let sid = apps[i].current_session_id.as_str();
                let link = m.agents.iter().find(|a| a.session.as_deref() == Some(sid));
                if let (Some(a), Some(root)) = (link, root_of(i, &apps[i])) {
                    self.task_cards.push((a.task.clone(), root));
                }
            }
        }

        structural |= self.drop_stale(&keep);
        if structural || std::mem::take(&mut self.repack) {
            // Zones must contain their agents, so a change in what a project
            // holds always repacks; the camera only moves on its own in
            // Overview (the home view).
            self.pack(apps, &shown, &tallies, &order);
            if let Some(id) = self.pending_zoom_node.take() {
                self.zoom_to_node(&id);
            } else if let Some(g) = self.pending_zoom.take() {
                self.zoom_to(g);
            } else if self.host.camera == Camera::Overview {
                self.want_fit = true;
            }
        }
        self.fit_hull();
        self.mark_spot();
    }

    /// Size the lit project's outline round its blocks, wherever they are.
    fn fit_hull(&mut self) {
        let Some(g) = self.spot else { return };
        let id = zone_id(g);
        if self.host.flow.node(&id).is_none() {
            return;
        }
        let ids: Vec<String> = self
            .blocks
            .iter()
            .filter(|b| b.group == g)
            .flat_map(|b| b.nodes.iter().cloned())
            .collect();
        let Some((x0, y0, x1, y1)) = self.bounds_of(&ids) else {
            return;
        };
        let flow = &mut self.host.flow;
        flow.set_node_position(&id, (x0 - ZONE_PAD_X, y0 - ZONE_PAD_TOP));
        flow.set_node_dimensions(
            &id,
            x1 - x0 + 2.0 * ZONE_PAD_X,
            y1 - y0 + ZONE_PAD_TOP + ZONE_PAD_BOTTOM,
        );
    }

    /// Dim every card outside the lit project; with none lit, none.
    fn mark_spot(&mut self) {
        let ids: Vec<String> = self
            .host
            .flow
            .nodes()
            .filter(|n| zone_group(&n.id).is_none())
            .map(|n| n.id.clone())
            .collect();
        for id in ids {
            let dim = self
                .spot
                .is_some_and(|g| self.project_of(&id).is_some_and(|h| h != g));
            if let Some(n) = self.host.flow.node_content_mut(&id) {
                n.feed.dim = dim;
            }
        }
    }

    /// Light project `g` up from the strip, or put it out when it already
    /// is. Takes effect on the next sync.
    pub fn toggle_spot(&mut self, g: usize) {
        self.spot = if self.spot == Some(g) { None } else { Some(g) };
    }

    /// Every card on screen, as (node id, `(x, y, w, h)` in terminal cells):
    /// where the lama can stand, and what he must never cover.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn card_rects(&self) -> Vec<crate::ui::mascot::Card> {
        self.host
            .flow
            .nodes()
            .filter(|n| {
                !is_structure(&n.id) || ghost_group(&n.id).is_some() || n.id.starts_with("◈lane/")
            })
            .filter_map(|n| {
                let (l, t, r, b) = self.host.flow.node_terminal_rect(&n.id)?;
                let rect = (l as f64, t as f64, (r - l) as f64, (b - t) as f64);
                Some((n.id.clone(), rect))
            })
            .collect()
    }

    /// Session `sid`'s main card, if it is on the canvas.
    #[cfg_attr(not(feature = "native"), allow(dead_code))]
    pub(crate) fn main_card(&self, apps: &[App], sid: &str) -> Option<String> {
        let i = apps.iter().position(|a| a.current_session_id == sid)?;
        root_of(i, &apps[i]).filter(|id| self.host.flow.node(id).is_some())
    }

    /// The strip's chip under a terminal cell.
    pub fn chip_hit(&self, column: u16, row: u16) -> Option<Chip> {
        self.strip_hits
            .iter()
            .find(|(r, _)| column >= r.x && column < r.right() && row >= r.y && row < r.bottom())
            .map(|(_, c)| *c)
    }

    /// Upsert a planned card's placeholder: the task overlay draws the card
    /// over it, so it only holds the room. Returns whether it was added.
    fn put_ghost(&mut self, id: &str, name: &str) -> bool {
        if self.host.flow.node(id).is_some() {
            return false;
        }
        let content = AgentNode {
            title: name.to_string(),
            description: None,
            status: AgentStatus::Idle,
            tool_count: 0,
            last_tool: None,
            output_tokens: 0,
            interactive: true,
            feed: Feed {
                ghost: true,
                ..Feed::default()
            },
        };
        // Out of selection like the hubs: a press on it takes the zone.
        let node = graph::new_node(id.to_string(), (0.0, 0.0), GHOST_DIMS, content)
            .with_selectable(false)
            .with_opaque(false);
        self.host.flow.add_node(node).is_ok()
    }

    /// Drop every node no shown session or project owns any more: a session
    /// that switched (its old subagents), one that aged out of the window, a
    /// project that folded into a tile.
    fn drop_stale(&mut self, keep: &HashSet<String>) -> bool {
        let stale: Vec<String> = self
            .host
            .flow
            .nodes()
            .filter(|n| !keep.contains(&n.id))
            .map(|n| n.id.clone())
            .collect();
        if stale.is_empty() {
            return false;
        }
        graph::remove_agents(&mut self.host.flow, &stale);
        true
    }

    /// Lay the universe out: each open project's sessions (and planned
    /// cards) as blocks, every tree in a layout of its own. A pinned block
    /// goes where you left it; one placed before keeps its place while it
    /// fits there; a new one lands in the free space nearest its project's
    /// newest block. With nothing placed yet — the first pack, or after a
    /// tidy (`r`) — the blocks cluster by project in rows instead.
    fn pack(&mut self, apps: &[App], shown: &[usize], tallies: &[Tally], order: &[usize]) {
        self.laid_out = true;
        let sizes = self.sizes();
        let mut laid: Vec<Laid> = Vec::new();
        for &g in order {
            if !self.is_open(g, &tallies[g]) {
                continue;
            }
            for &i in shown.iter().filter(|&&i| self.group_of(i) == g) {
                let Forest {
                    roots,
                    kids,
                    stacks,
                } = self.forest(apps, &[i]);
                let placed = tree_layout(&roots, &kids, &stacks, &sizes);
                let key = apps[i].current_session_id.clone();
                laid.extend(Laid::new(key, g, placed, &sizes));
            }
            for (_, name, _) in self
                .ghosts(apps, shown)
                .into_iter()
                .filter(|(h, _, _)| *h == g)
            {
                let id = ghost_id(g, &name);
                if self.host.flow.node(&id).is_some() {
                    laid.extend(Laid::new(id.clone(), g, vec![(id, 0.0, 0.0)], &sizes));
                }
            }
        }

        let fresh = laid
            .iter()
            .all(|b| !self.placed.contains_key(&b.key) && !self.pins.contains_key(&b.key));
        let at = if fresh {
            cluster(&laid)
        } else {
            self.arrange(&laid)
        };
        let before = std::mem::take(&mut self.placed);
        for b in &laid {
            let (x, y) = at.get(&b.key).copied().unwrap_or_default();
            for (id, dx, dy) in &b.nodes {
                self.host.flow.set_node_position(id, (x + dx, y + dy));
            }
            self.placed.insert(b.key.clone(), (x, y, b.w, b.h));
        }
        // A session that left the canvas (quiet, or out of the window)
        // keeps its place for when it comes back, while lama still knows it.
        for (k, r) in before.iter() {
            let known = apps.iter().any(|a| a.current_session_id == *k);
            if known && !self.placed.contains_key(k) {
                self.placed.insert(k.clone(), *r);
            }
        }
        self.pins_dirty |= self.placed != before;
        self.blocks = laid
            .into_iter()
            .map(|b| Block {
                key: b.key,
                group: b.g,
                nodes: b.nodes.into_iter().map(|(id, _, _)| id).collect(),
            })
            .collect();
    }

    /// Where each block goes when some already have a place: pinned ones
    /// where you left them, then the ones placed before where they were —
    /// unless a block that just grew (new helpers, a console opened) now
    /// reaches over one, which then steps aside. A block you dropped on
    /// another stays where you dropped it. One coming back to the canvas (a
    /// restart, a quiet session waking) takes its old place if it is still
    /// free. Then the new ones, each beside its project's newest block.
    fn arrange(&self, laid: &[Laid]) -> HashMap<String, (f64, f64)> {
        let mut out: HashMap<String, (f64, f64)> = HashMap::new();
        let mut taken: Vec<Rect4> = Vec::new();
        let mut grown: Vec<Rect4> = Vec::new();
        let grew = |b: &Laid| {
            self.placed
                .get(&b.key)
                .is_some_and(|r| (b.w - r.2).abs() >= 0.5 || (b.h - r.3).abs() >= 0.5)
        };
        for b in laid {
            if let Some(&(x, y)) = self.pins.get(&b.key) {
                out.insert(b.key.clone(), (x, y));
                taken.push((x, y, b.w, b.h));
                if grew(b) {
                    grown.push((x, y, b.w, b.h));
                }
            }
        }
        let mut known: Vec<(&Laid, Rect4)> = laid
            .iter()
            .filter(|b| !out.contains_key(&b.key))
            .filter_map(|b| Some((b, *self.placed.get(&b.key)?)))
            .collect();
        let back = |b: &Laid| !self.blocks.iter().any(|o| o.key == b.key);
        // Grown first, then the steady ones, then the ones coming back.
        let rank = |b: &Laid| match (grew(b), back(b)) {
            (true, _) => 0,
            (false, false) => 1,
            (false, true) => 2,
        };
        known.sort_by(|(a, ra), (b, rb)| {
            rank(a)
                .cmp(&rank(b))
                .then(ra.1.total_cmp(&rb.1))
                .then(ra.0.total_cmp(&rb.0))
        });
        for (b, r) in known {
            let here = (r.0, r.1, b.w, b.h);
            let at = if back(b) && !grew(b) {
                land((r.0, r.1), (b.w, b.h), &taken)
            } else if grew(b) {
                let at = land((r.0, r.1), (b.w, b.h), &taken);
                grown.push((at.0, at.1, b.w, b.h));
                at
            } else if grown.iter().any(|&g| crowds(here, g)) {
                land((r.0, r.1), (b.w, b.h), &taken)
            } else {
                (r.0, r.1)
            };
            out.insert(b.key.clone(), at);
            taken.push((at.0, at.1, b.w, b.h));
        }
        for (k, b) in laid.iter().enumerate() {
            if out.contains_key(&b.key) {
                continue;
            }
            // Beside the project's newest block that has a place; a new
            // project starts right of everything.
            let mate = laid[..k]
                .iter()
                .chain(&laid[k + 1..])
                .filter(|m| m.g == b.g)
                .filter_map(|m| {
                    let &(x, y) = out.get(&m.key)?;
                    Some((x, y, m.w, m.h))
                })
                .next_back();
            let want = match mate {
                Some((x, y, w, _)) => (x + w + BLOCK_GAP.0, y),
                None => taken
                    .iter()
                    .map(|t| (t.0 + t.2 + BLOCK_GAP.0 * 2.0, t.1))
                    .fold(None, |best: Option<(f64, f64)>, c| match best {
                        Some(b) if b.0 >= c.0 => Some((b.0, b.1.min(c.1))),
                        _ => Some(c),
                    })
                    .unwrap_or_default(),
            };
            let at = land(want, (b.w, b.h), &taken);
            out.insert(b.key.clone(), at);
            taken.push((at.0, at.1, b.w, b.h));
        }
        out
    }

    /// The home view: everything on screen, with a little air.
    fn fit_all(&mut self) {
        let area = self.host.flow.canvas_area();
        if self.births.is_empty() || area.width == 0 || area.height == 0 {
            self.host.flow.request_fit_view_with_options(
                rataflow::FitViewOptions::default().with_padding(2.0),
            );
            return;
        }
        // Cards are still growing out of their parents: frame where they
        // will land, not the dots they are now, or the view zooms in on the
        // parent and the grown cards end up off screen.
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for n in self.host.flow.nodes() {
            let (x, y, w, h) = match self.births.get(&n.id) {
                Some(b) => (b.to.0, b.to.1, b.size.0, b.size.1),
                None => (n.position.x, n.position.y, n.width, n.height),
            };
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x + w);
            y1 = y1.max(y + h);
        }
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let pad = 2.0;
        let (cw, ch) = (area.width as f64, area.height as f64);
        let zoom = ((cw - 2.0 * pad) / (x1 - x0))
            .min((ch - 2.0 * pad) / (y1 - y0))
            .clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        let v = &mut self.host.flow.viewport;
        v.zoom = zoom;
        v.x = cw / 2.0 - (x0 + x1) / 2.0 * zoom;
        v.y = ch / 2.0 - (y0 + y1) / 2.0 * zoom;
    }

    /// The cards (and planned cards) of project `g` on the canvas.
    fn project_nodes(&self, g: usize) -> Vec<String> {
        self.blocks
            .iter()
            .filter(|b| b.group == g)
            .flat_map(|b| b.nodes.iter().cloned())
            .filter(|id| self.host.flow.node(id).is_some())
            .collect()
    }

    /// Zoom the camera onto project `g`'s sessions, wherever they are.
    pub fn zoom_to(&mut self, g: usize) {
        let nodes = self.project_nodes(g);
        if nodes.is_empty() {
            return;
        }
        if let Some(k) = self.rail.iter().position(|r| r.group == g) {
            self.cursor = k;
        }
        self.focus = Some(g);
        self.focus_block = None;
        self.fit_nodes(nodes);
    }

    /// Fit the camera to the nodes `ids`, with a little air.
    fn fit_nodes(&mut self, nodes: Vec<String>) {
        self.touch_camera();
        self.host.camera = Camera::Manual;
        self.host.flow.clear_selection();
        self.host
            .flow
            .request_fit_view_with_options(rataflow::FitViewOptions {
                nodes,
                padding: 1.0,
                ..Default::default()
            });
    }

    /// Back to the home view: every project on screen.
    pub fn go_home(&mut self) {
        self.touch_camera();
        self.focus = None;
        self.focus_block = None;
        self.host.camera = Camera::Overview;
        self.host.flow.clear_selection();
        self.fit_all();
    }

    /// Zoom the camera onto one node (a console), with a little air.
    pub fn zoom_to_node(&mut self, id: &str) {
        if self.host.flow.node(id).is_none() {
            return;
        }
        if let Some(g) = self.project_of(id) {
            self.focus = Some(g);
        }
        self.touch_camera();
        self.host.camera = Camera::Manual;
        self.host.flow.clear_selection();
        self.host
            .flow
            .request_fit_view_with_options(rataflow::FitViewOptions {
                nodes: vec![id.to_string()],
                padding: 1.0,
                ..Default::default()
            });
    }

    /// Open session `i`'s console where its card is, without moving the
    /// camera — an agent asking for input while you look elsewhere.
    pub fn open_console_in_place(&mut self, i: usize, apps: &[App]) {
        self.open_console(i, apps);
        self.pending_zoom_node = None;
    }

    /// Open session `i`'s console on the canvas (remembering it), and zoom
    /// onto it once it is placed.
    pub fn open_console(&mut self, i: usize, apps: &[App]) {
        let Some(app) = apps.get(i) else { return };
        let sid = app.current_session_id.clone();
        self.consoles.entry(sid).or_insert(CONSOLE_DIMS);
        self.pins_dirty = true;
        // A console opens its project even when the project is quiet.
        self.expanded.insert(self.group_of(i));
        self.repack = true;
        self.pending_zoom_node = root_of(i, app);
    }

    /// Close session `i`'s console.
    pub fn close_console(&mut self, i: usize, apps: &[App]) {
        if let Some(app) = apps.get(i) {
            self.consoles.remove(&app.current_session_id);
            self.pins_dirty = true;
            self.repack = true;
        }
    }

    /// Pick up console sizes the user changed by dragging a corner. Returns
    /// whether any changed (the layout then repacks around them).
    pub fn capture_console_sizes(&mut self, apps: &[App]) -> bool {
        let mut changed = false;
        for (i, app) in apps.iter().enumerate() {
            let Some(size) = self.consoles.get(&app.current_session_id).copied() else {
                continue;
            };
            let Some(host) = self.console_hosts.get(&i).cloned() else {
                continue;
            };
            if let Some(now) = self.node_size(&host)
                && ((now.0 - size.0).abs() >= 0.5 || (now.1 - size.1).abs() >= 0.5)
            {
                self.consoles.insert(app.current_session_id.clone(), now);
                changed = true;
            }
        }
        if changed {
            self.pins_dirty = true;
            self.repack = true;
        }
        changed
    }

    /// Resize session `i`'s console to `size` world cells (the frontend's
    /// grip drag). Kept by [`capture_console_sizes`](Self::capture_console_sizes).
    pub fn resize_console(&mut self, i: usize, size: (f64, f64)) {
        let (w, h) = (size.0.max(30.0), size.1.max(10.0));
        if let Some(host) = self.console_hosts.get(&i).cloned() {
            self.host.flow.set_node_dimensions(&host, w, h);
        }
    }

    /// The current world size of session `i`'s console node.
    pub fn console_size(&self, i: usize) -> Option<(f64, f64)> {
        self.node_size(self.console_hosts.get(&i)?)
    }

    /// The screen rect of session `i`'s console, if it is on the canvas —
    /// where the frontend draws it. May extend past the canvas edges.
    pub fn console_rect(&self, i: usize) -> Option<(f64, f64, f64, f64)> {
        self.screen_rect(self.console_hosts.get(&i)?)
    }

    /// Where node `id` is on screen, as of the last render: `(x, y, w, h)`
    /// in terminal cells. May extend past the canvas edges.
    pub fn screen_rect(&self, id: &str) -> Option<(f64, f64, f64, f64)> {
        let flow = &self.host.flow;
        let n = flow.node(id)?;
        let area = flow.canvas_area();
        let v = flow.viewport;
        let at = v.world_to_canvas(rataflow::types::Position::new(n.position.x, n.position.y));
        Some((
            area.x as f64 + at.x,
            area.y as f64 + at.y,
            n.width * v.zoom,
            n.height * v.zoom,
        ))
    }

    /// The main card of the task agent named `name`, once it has a
    /// session on the canvas — where its messages leave from and land.
    pub fn task_card(&self, apps: &[App], name: &str) -> Option<String> {
        let sid = self.task.as_ref()?.agent_session(name)?;
        let i = apps.iter().position(|a| a.current_session_id == sid)?;
        root_of(i, &apps[i]).filter(|id| self.host.flow.node(id).is_some())
    }

    /// The planned card waiting for agent `p`, while it waits: its node id.
    pub fn ghost_card(&self, p: &PlannedView) -> Option<String> {
        let id = ghost_id(self.group_keyed(&p.project_key)?, &p.name);
        self.host.flow.node(&id).is_some().then_some(id)
    }

    /// The planned card of the task agent named `name`, before its own card
    /// is on the canvas: where its arrival and early messages land.
    pub fn task_ghost(&self, name: &str) -> Option<String> {
        let plan = self.task.as_ref()?.plan.as_ref()?;
        plan.agents
            .iter()
            .filter(|p| p.name == name)
            .find_map(|p| self.ghost_card(p))
    }

    /// Size session `i`'s main card for its console when one is open, back
    /// to a card when not. Returns whether its size changed (relayout).
    fn host_console(&mut self, i: usize, app: &App) -> bool {
        let Some(root) = root_of(i, app) else {
            return false;
        };
        let want = self.consoles.get(&app.current_session_id).copied();
        match want {
            Some(_) => {
                self.console_hosts.insert(i, root.clone());
            }
            None => {
                self.console_hosts.remove(&i);
            }
        }
        let size = want.unwrap_or(MAIN_NODE_DIMS);
        match self.node_size(&root) {
            Some(now) if (now.0 - size.0).abs() >= 0.5 || (now.1 - size.1).abs() >= 0.5 => {
                self.host.flow.set_node_dimensions(&root, size.0, size.1);
                true
            }
            _ => false,
        }
    }

    /// Tell session `i`'s main card whether to blink green (finished, not
    /// yet looked at). Set after every sync, since a sync may rebuild it.
    /// Also copies the session's context and cost gauge onto that card, and
    /// marks it `◈` when it is a task's agent — `owner` when it owns its
    /// task, lit when its task is the picked one.
    fn mark_finished(&mut self, i: usize, app: &App) {
        let sid = app.current_session_id.as_str();
        let done = self.finished.contains(sid);
        let usage = self.usage.get(sid).copied();
        let (task, owner, picked) = self
            .task
            .as_ref()
            .and_then(|m| {
                let a = m
                    .agents
                    .iter()
                    .find(|a| a.session.as_deref() == Some(sid))?;
                let picked = m.selected.as_deref() == Some(a.task.as_str());
                Some((true, a.parent.is_none(), picked))
            })
            .unwrap_or_default();
        let project = self
            .window
            .is_some()
            .then(|| self.groups[self.group_of(i)].name.clone());
        if let Some(root) = root_of(i, app)
            && let Some(card) = self.host.flow.node_content_mut(&root)
        {
            card.feed.finished = done;
            card.feed.context = usage.and_then(|u| u.context());
            card.feed.cost = usage.and_then(|u| u.cost_usd);
            card.feed.task = task;
            card.feed.owner = owner;
            card.feed.picked = picked;
            card.feed.project = project;
        }
    }

    /// What project `g`'s sessions cost today, shown or not: a session that
    /// aged out of the window still spent today's money.
    fn cost_today_of(&self, apps: &[App], g: usize) -> Option<f64> {
        usage::sum_costs(
            (0..apps.len())
                .filter(|&i| self.group_of(i) == g)
                .map(|i| self.usage.get(&apps[i].current_session_id)?.cost_today_usd),
        )
    }

    /// What every metered session cost today, for the status bar; `None`
    /// until one is priced.
    pub fn cost_today(&self) -> Option<f64> {
        usage::sum_costs(self.usage.values().map(|u| u.cost_today_usd))
    }

    /// You looked at session `i` (clicked it, typed in its console): stop
    /// its finished blink.
    pub fn acknowledge(&mut self, sid: &str) {
        self.finished.remove(sid);
    }

    /// The session whose console is drawn on `id`, if one is.
    fn console_on(&self, id: &str) -> Option<usize> {
        self.console_hosts
            .iter()
            .find(|(_, host)| host.as_str() == id)
            .map(|(&i, _)| i)
    }

    /// Step to the next (or previous) project: light it up and zoom onto it.
    pub fn step_project(&mut self, forward: bool) {
        if self.rail.is_empty() {
            return;
        }
        let n = self.rail.len();
        self.cursor = if forward {
            (self.cursor + 1) % n
        } else {
            (self.cursor + n - 1) % n
        };
        let g = self.rail[self.cursor].group;
        self.spot = Some(g);
        self.open_project(g);
    }

    /// Zoom onto a project, putting a quiet one's sessions on the canvas
    /// first.
    pub fn open_project(&mut self, g: usize) {
        if self.project_nodes(g).is_empty() {
            // Opening adds its agents, which repacks on the next sync; the
            // zoom waits for that so it lands on them.
            self.expanded.insert(g);
            self.pending_zoom = Some(g);
            self.repack = true;
        } else {
            self.zoom_to(g);
        }
    }

    /// Tidy up: forget where every session was left, and cluster them by
    /// project again on the next sync. They stay free to move after.
    pub fn reset_layout(&mut self) {
        self.pins.clear();
        self.placed.clear();
        self.pins_dirty = true;
        self.repack = true;
    }

    /// The project a node belongs to: an outline's own, or the group of the
    /// session a card comes from.
    fn project_of(&self, id: &str) -> Option<usize> {
        zone_group(id)
            .or_else(|| ghost_group(id))
            .or_else(|| self.session_of(id).map(|i| self.group_of(i)))
    }

    /// The session a card (or one of its lane headers, `◈lane/<i>/…`) comes
    /// from.
    fn session_of(&self, id: &str) -> Option<usize> {
        let card = id.strip_prefix("◈lane/").unwrap_or(id);
        Self::resolve(card).map(|(i, _)| i)
    }

    fn node_at(&self, id: &str) -> Option<(f64, f64)> {
        self.host
            .flow
            .node(id)
            .map(|n| (n.position.x, n.position.y))
    }

    /// The card under a terminal cell, if any. Hit-tested here rather than
    /// read off the selection, because the flow selects a draggable node only
    /// once a drag passes its threshold or the button comes back up.
    pub fn node_under(&self, column: u16, row: u16) -> Option<String> {
        let flow = &self.host.flow;
        let area = flow.canvas_area();
        if column < area.x || row < area.y || column >= area.right() || row >= area.bottom() {
            return None;
        }
        let canvas = rataflow::types::Position::new(
            (column - area.x) as f64 + 0.5,
            (row - area.y) as f64 + 0.5,
        );
        let world = flow.viewport.canvas_to_world(canvas);
        flow.nodes()
            .filter(|n| {
                world.x >= n.position.x
                    && world.y >= n.position.y
                    && world.x < n.position.x + n.width
                    && world.y < n.position.y + n.height
            })
            .find(|n| !is_structure(&n.id))
            .map(|n| n.id.clone())
    }

    /// A mouse press just landed at a terminal cell. In a universe a press on
    /// a card takes hold of its whole session; the release decides between
    /// a click and a move.
    pub fn press(&mut self, column: u16, row: u16) {
        if self.window.is_none() {
            return;
        }
        let Some(held) = self.node_under(column, row) else {
            return;
        };
        let Some(block) = self.blocks.iter().position(|b| b.nodes.contains(&held)) else {
            return;
        };
        let (Some(at), size) = (
            self.node_at(&held),
            self.node_size(&held).unwrap_or_default(),
        ) else {
            return;
        };
        self.press = Some(Press {
            group: self.blocks[block].group,
            block,
            held,
            start: at,
            size,
            last: at,
        });
    }

    fn node_size(&self, id: &str) -> Option<(f64, f64)> {
        self.host.flow.node(id).map(|n| (n.width, n.height))
    }

    /// The mouse is dragging. The flow moves only the held node, so carry the
    /// rest of its session — its other cards and lane headers — by the same
    /// amount. Other sessions, of its project or not, stay where they are.
    pub fn dragged(&mut self) {
        let Some(press) = self.press.as_mut() else {
            return;
        };
        // The flow selects a card once its drag passes the threshold, and a
        // selection pans the view to centre it: the view would slide under
        // the card being dragged.
        self.host.pending_center = None;
        self.host.camera_glide = None;
        // You are arranging: the home view must not re-fit under you.
        self.host.camera = Camera::Manual;
        let Some(now) = self
            .host
            .flow
            .node(&press.held)
            .map(|n| (n.position.x, n.position.y))
        else {
            return;
        };
        let delta = (now.0 - press.last.0, now.1 - press.last.1);
        if delta == (0.0, 0.0) {
            return;
        }
        press.last = now;
        let (block, held) = (press.block, press.held.clone());
        let members: Vec<String> = self
            .blocks
            .get(block)
            .map(|b| b.nodes.iter().filter(|id| **id != held).cloned().collect())
            .unwrap_or_default();
        for id in members {
            self.host.flow.move_node(&id, delta);
        }
        self.fit_hull();
    }

    /// The mouse let go. In place, it was a click: zoom onto the session —
    /// never a log panel. Moved, it was an arrangement: pin the session
    /// where it now stands.
    pub fn release(&mut self) {
        let Some(press) = self.press.take() else {
            return;
        };
        // A click in the universe never moves the camera: the host would
        // centre the card it just selected, sliding it out from under a
        // second click (a double-click opens the console).
        self.host.flow.clear_selection();
        self.host.pending_center = None;
        self.host.camera_glide = None;
        let end = self.node_at(&press.held).unwrap_or(press.start);
        let moved = (end.0 - press.start.0).abs() >= 0.5 || (end.1 - press.start.1).abs() >= 0.5;
        // A console's corner was dragged: a resize, kept by the size sweep
        // after every release — not a click and not a move.
        if self.node_size(&press.held).is_some_and(|s| s != press.size) {
            return;
        }
        // A click on a console reads it: zoom onto it if it is small on
        // screen. Typing focus is the frontend's business.
        if !moved && self.console_on(&press.held).is_some() {
            let small = self.host.flow.node(&press.held).is_some_and(|n| {
                n.width * self.host.flow.viewport.zoom
                    < self.host.flow.canvas_area().width as f64 * 0.4
            });
            if small {
                self.zoom_to_node(&press.held);
            }
            return;
        }
        let Some(block) = self.blocks.get(press.block).cloned() else {
            return;
        };
        if !moved {
            if self.focus_block.as_deref() != Some(block.key.as_str()) {
                self.focus = Some(press.group);
                self.fit_nodes(block.nodes);
                self.focus_block = Some(block.key);
            }
            return;
        }
        let Some((x0, y0, x1, y1)) = self.bounds_of(&block.nodes) else {
            return;
        };
        self.placed
            .insert(block.key.clone(), (x0, y0, x1 - x0, y1 - y0));
        // A planned card is only waiting for its session: it moves, but
        // isn't remembered.
        if !is_structure(&block.key) {
            self.pins.insert(block.key, (x0, y0));
            self.pins_dirty = true;
        }
    }

    /// A wheel notch at a terminal cell: ease the zoom one step toward
    /// in (`up`) or out, anchored under the pointer. Notches in quick
    /// succession compound.
    pub fn wheel_zoom(&mut self, up: bool, column: u16, row: u16) {
        let area = self.host.flow.canvas_area();
        let anchor = rataflow::types::Position::new(
            column.saturating_sub(area.x) as f64 + 0.5,
            row.saturating_sub(area.y) as f64 + 0.5,
        );
        let from = self
            .motion
            .zoom
            .map(|(z, _)| z)
            .unwrap_or(self.host.flow.viewport.zoom);
        let step = if up { WHEEL_STEP } else { 1.0 / WHEEL_STEP };
        let now = self.host.flow.viewport.zoom;
        let target = (from * step)
            .clamp(now / WHEEL_LEAD, now * WHEEL_LEAD)
            .clamp(ZOOM_RANGE.0, ZOOM_RANGE.1);
        self.motion.zoom = Some((target, anchor));
        self.touch_camera();
        self.host.camera = Camera::Manual;
        self.host.camera_glide = None;
    }

    /// A tap on session `i`'s console: if it is small on screen, zoom onto
    /// it so it can be read.
    pub fn tap_console(&mut self, i: usize) {
        let Some(id) = self.console_hosts.get(&i).cloned() else {
            return;
        };
        let small = self.host.flow.node(&id).is_some_and(|n| {
            n.width * self.host.flow.viewport.zoom < self.host.flow.canvas_area().width as f64 * 0.4
        });
        if small {
            self.zoom_to_node(&id);
        }
    }

    /// A press on the canvas: whatever was coasting stops under the finger.
    pub fn halt(&mut self) {
        self.touch_camera();
        self.motion.coast = None;
        self.motion.trail.clear();
        if let Some((x, y)) = self.motion.pan_to.take() {
            self.host.flow.viewport.x = x;
            self.host.flow.viewport.y = y;
        }
    }

    /// After the flow handled a drag that moved the view from `before`: take
    /// the new view as the pan's target and draw from `before` easing toward
    /// it, so the canvas glides between the cells the pointer reports.
    pub fn smooth_pan(&mut self, before: (f64, f64)) {
        let v = &mut self.host.flow.viewport;
        let to = (v.x, v.y);
        if to == before {
            return;
        }
        self.motion.pan_to = Some(to);
        v.x = before.0;
        v.y = before.1;
        self.touch_camera();
        // The user has the camera now: the home view must stop re-fitting
        // over their pan whenever the map changes (`o` hands it back).
        self.host.camera = Camera::Manual;
        self.host.camera_glide = None;
    }

    /// During a drag: remember where the view is, so a release can keep its
    /// speed. Only a pan moves the view, so a node drag leaves no trail.
    pub fn track_pan(&mut self) {
        let v = self.host.flow.viewport;
        let (vx, vy) = self.motion.pan_to.unwrap_or((v.x, v.y));
        let now = web_time::Instant::now();
        let moved = self
            .motion
            .trail
            .last()
            .is_none_or(|&(_, x, y)| x != vx || y != vy);
        if moved {
            self.motion.trail.push((now, vx, vy));
            // Only the last moments of the drag decide the fling.
            self.motion
                .trail
                .retain(|(t, _, _)| now.duration_since(*t).as_millis() <= 120);
        }
    }

    /// The pointer let go: a pan still moving keeps going and eases to rest.
    pub fn release_pan(&mut self) {
        let trail = std::mem::take(&mut self.motion.trail);
        let (Some(first), Some(last)) = (trail.first(), trail.last()) else {
            return;
        };
        let dt = last.0.duration_since(first.0).as_secs_f64();
        // A drag that paused before release has nothing left to fling.
        if dt < 0.02 || last.0.elapsed().as_millis() > 80 {
            return;
        }
        let v = ((last.1 - first.1) / dt, (last.2 - first.2) / dt);
        if v.0.hypot(v.1) > PAN_REST * 4.0 {
            // Coast on from where the pan was headed.
            if let Some((x, y)) = self.motion.pan_to.take() {
                self.host.flow.viewport.x = x;
                self.host.flow.viewport.y = y;
            }
            self.motion.coast = Some(v);
            self.host.camera = Camera::Manual;
        }
    }

    /// The user moved the camera (a pan, a zoom, a click to fly
    /// somewhere): whatever flight was under way stops, and for a while the
    /// task doesn't take the camera back. The frontend calls it for the
    /// keyboard's camera moves too.
    pub fn touch_camera(&mut self) {
        self.touched = Some(web_time::Instant::now());
        self.motion.fly = None;
    }

    /// Whether the user moved the camera recently enough that it is theirs.
    fn hands_on(&self) -> bool {
        self.press.is_some()
            || self
                .touched
                .is_some_and(|t| t.elapsed().as_secs_f64() < HANDS_OFF_SECS)
    }

    /// Take the frontend's fresh view of the task. A change in who has a
    /// card, what the plan holds or which task is picked re-syncs (zones
    /// open, planned cards come and go, cards light up); a new plan or a
    /// new arrival flies the camera there.
    pub fn set_task(&mut self, view: TaskView, apps: &[App]) {
        let shape = |m: &TaskView| {
            let sessions: Vec<(Option<String>, String, Option<String>)> = m
                .agents
                .iter()
                .map(|a| (a.session.clone(), a.task.clone(), a.parent.clone()))
                .collect();
            let planned: Vec<(String, String)> = m
                .plan
                .iter()
                .flat_map(|p| &p.agents)
                .map(|a| (a.name.clone(), a.project_key.clone()))
                .collect();
            (sessions, planned, m.selected.clone())
        };
        let changed = self.task.as_ref().map(shape) != Some(shape(&view));
        self.task = Some(view);
        if changed {
            self.sync(apps, false);
        } else if self.window.is_some() {
            self.follow_task();
        }
    }

    /// What of the task should move the camera now; `None` with nothing on
    /// its way yet. Its times stay out: the frontend re-bases them onto one
    /// clock every frame, and they jitter.
    fn task_cue(&self) -> Option<TaskCue> {
        let m = self.task.as_ref()?;
        let joined = m.agents.iter().filter(|a| a.joined.is_some()).count();
        let plan = m.plan.as_ref();
        if plan.is_none() && joined == 0 {
            return None;
        }
        let target = self
            .camera_target()
            .map(|(x0, y0, x1, y1)| [x0, y0, x1, y1].map(|v| v.round() as i64));
        Some((
            plan.map_or(0, |p| p.version),
            plan.map_or(0, |p| p.agents.len()),
            joined,
            target,
        ))
    }

    /// The world rect `(x0, y0, x1, y1)` round every session of the task —
    /// whole blocks, helpers and all — and its planned cards, wherever they
    /// sit, once they are placed.
    pub fn task_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        let ids: Vec<String> = self
            .blocks
            .iter()
            .filter(|b| {
                self.task_cards
                    .iter()
                    .any(|(_, card)| b.nodes.contains(card))
            })
            .flat_map(|b| b.nodes.iter().cloned())
            .collect();
        self.bounds_of(&ids)
    }

    /// The world rect round the task's own cards — its agents' main cards
    /// and the planned ones.
    fn task_cards_bounds(&self) -> Option<(f64, f64, f64, f64)> {
        self.bounds_of(
            &self
                .task_cards
                .iter()
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>(),
        )
    }

    /// The world rect round the nodes `ids` that are on the canvas.
    fn bounds_of(&self, ids: &[String]) -> Option<(f64, f64, f64, f64)> {
        let mut out: Option<(f64, f64, f64, f64)> = None;
        for n in ids.iter().filter_map(|id| self.host.flow.node(id)) {
            let (x, y) = (n.position.x, n.position.y);
            let (w, h) = (n.width, n.height);
            out = Some(match out {
                None => (x, y, x + w, y + h),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x + w), y1.max(y + h)),
            });
        }
        out
    }

    /// The zoom that frames world rect `(x0, y0, x1, y1)` on the canvas with
    /// a little air; `None` while the canvas has no size.
    fn zoom_framing(&self, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Option<f64> {
        let area = self.host.flow.canvas_area();
        if area.width == 0 || area.height == 0 || x1 <= x0 || y1 <= y0 {
            return None;
        }
        let pad = 2.0;
        let (cw, ch) = (area.width as f64, area.height as f64);
        Some(
            ((cw - 2.0 * pad) / (x1 - x0))
                .min((ch - 2.0 * pad) / (y1 - y0))
                .clamp(ZOOM_RANGE.0, FLY_ZOOM_MAX),
        )
    }

    /// Where the camera goes for the task: its sessions whole when they fit
    /// at a zoom the cards can be read at, else just its main cards (a
    /// spread-out task can be far wider than the screen).
    fn camera_target(&self) -> Option<(f64, f64, f64, f64)> {
        let blocks = self.task_bounds()?;
        if self
            .zoom_framing(blocks)
            .is_some_and(|z| z >= READABLE_ZOOM)
        {
            return Some(blocks);
        }
        self.task_cards_bounds().or(Some(blocks))
    }

    /// Fly the camera to the task when the plan, its arrivals or the place
    /// its cards take changed since it last did — unless the user has the
    /// camera. A cue the camera can't answer yet (nothing placed, no
    /// canvas) waits for a later sync. Returns whether a flight started.
    fn follow_task(&mut self) -> bool {
        let cue = self.task_cue();
        if cue == self.task_cue {
            return false;
        }
        if cue.is_none() || self.hands_on() {
            self.task_cue = cue;
            return false;
        }
        let Some(target) = self.camera_target() else {
            return false;
        };
        if !self.fly_to(target) {
            return false;
        }
        self.task_cue = cue;
        self.focus = match self.task_zones.as_slice() {
            [g] => Some(*g),
            _ => None,
        };
        true
    }

    /// Ease the camera to task `id`'s own cards — its agents' and its
    /// planned ones — as when it is picked in the TASKS box. The user asked
    /// for it, so for a while the task's own cues leave the camera there.
    /// `false` when none of its cards is placed, or the canvas has no size.
    pub fn fly_to_task(&mut self, id: &str) -> bool {
        let ids: Vec<String> = self
            .task_cards
            .iter()
            .filter(|(task, _)| task == id)
            .map(|(_, card)| card.clone())
            .collect();
        let Some(target) = self.bounds_of(&ids) else {
            return false;
        };
        if !self.fly_to(target) {
            return false;
        }
        self.touched = Some(web_time::Instant::now());
        let mut zones: Vec<usize> = ids.iter().filter_map(|c| self.project_of(c)).collect();
        zones.sort_unstable();
        zones.dedup();
        self.focus = match zones.as_slice() {
            [g] => Some(*g),
            _ => None,
        };
        true
    }

    /// Start the camera flying to frame world rect `(x0, y0, x1, y1)`, with
    /// a little air. `false` while the canvas has no size yet. A flight
    /// already headed there carries on rather than starting over.
    fn fly_to(&mut self, rect: (f64, f64, f64, f64)) -> bool {
        let Some(zoom) = self.zoom_framing(rect) else {
            return false;
        };
        let (x0, y0, x1, y1) = rect;
        let to = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, zoom);
        let same = |a: (f64, f64, f64)| {
            (a.0 - to.0).abs() < 0.5 && (a.1 - to.1).abs() < 0.5 && (a.2 - to.2).abs() < 0.01
        };
        if self.motion.fly.is_some_and(|f| same(f.to)) {
            return true;
        }
        let area = self.host.flow.canvas_area();
        let (cw, ch) = (area.width as f64, area.height as f64);
        let v = self.host.flow.viewport;
        let from = ((cw / 2.0 - v.x) / v.zoom, (ch / 2.0 - v.y) / v.zoom, v.zoom);
        self.motion.zoom = None;
        self.motion.coast = None;
        self.motion.pan_to = None;
        self.motion.fly = Some(Fly { from, to, t: 0.0 });
        self.host.camera = Camera::Manual;
        self.host.camera_glide = None;
        self.host.flow.clear_selection();
        true
    }

    /// Advance a flight: ease in and out, the zoom scaling evenly.
    fn tick_fly(&mut self, dt: f64) {
        let Some(f) = self.motion.fly.as_mut() else {
            return;
        };
        f.t = (f.t + dt / FLY_SECS).min(1.0);
        let k = f.t;
        let e = if k < 0.5 {
            4.0 * k * k * k
        } else {
            1.0 - (-2.0 * k + 2.0).powi(3) / 2.0
        };
        let (from, to, done) = (f.from, f.to, f.t >= 1.0);
        let cx = from.0 + (to.0 - from.0) * e;
        let cy = from.1 + (to.1 - from.1) * e;
        let z = from.2 * (to.2 / from.2).powf(e);
        let area = self.host.flow.canvas_area();
        let v = &mut self.host.flow.viewport;
        v.zoom = z;
        v.x = area.width as f64 / 2.0 - cx * z;
        v.y = area.height as f64 / 2.0 - cy * z;
        if done {
            self.motion.fly = None;
        }
    }

    /// Advance the camera's own motion by `dt`.
    pub fn tick_motion(&mut self, dt: std::time::Duration) {
        self.tick_births();
        let dt = dt.as_secs_f64().min(0.1);
        self.tick_fly(dt);
        if let Some((target, anchor)) = self.motion.zoom {
            let v = &mut self.host.flow.viewport;
            let k = 1.0 - (-dt / ZOOM_EASE).exp();
            let mut z = v.zoom + (target - v.zoom) * k;
            if (z - target).abs() < target * 0.002 {
                z = target;
                self.motion.zoom = None;
            }
            // Keep the world point under the anchor fixed: canvas = world·z + pan.
            let wx = (anchor.x - v.x) / v.zoom;
            let wy = (anchor.y - v.y) / v.zoom;
            v.zoom = z;
            v.x = anchor.x - wx * z;
            v.y = anchor.y - wy * z;
        }
        if let Some((tx, ty)) = self.motion.pan_to {
            let v = &mut self.host.flow.viewport;
            let k = 1.0 - (-dt / PAN_EASE).exp();
            v.x += (tx - v.x) * k;
            v.y += (ty - v.y) * k;
            if (tx - v.x).abs() < 0.05 && (ty - v.y).abs() < 0.05 {
                v.x = tx;
                v.y = ty;
                self.motion.pan_to = None;
            }
        }
        if let Some((vx, vy)) = self.motion.coast {
            let v = &mut self.host.flow.viewport;
            v.x += vx * dt;
            v.y += vy * dt;
            let f = (-dt / PAN_FRICTION).exp();
            let (vx, vy) = (vx * f, vy * f);
            self.motion.coast = (vx.hypot(vy) > PAN_REST).then_some((vx, vy));
        }
    }

    /// The session and agent id behind a host node id.
    pub fn resolve(id: &str) -> Option<(usize, &str)> {
        let (i, rest) = id.split_once('/')?;
        Some((i.parse().ok()?, rest))
    }

    /// Follow mode: keep the most recently active agent across every session
    /// centred and narrated. The host's own tracking sees an empty model, so
    /// the plane does it.
    pub fn follow(&mut self, apps: &[App]) {
        if self.host.camera != Camera::Follow {
            return;
        }
        let newest = self
            .shown(apps)
            .into_iter()
            .filter_map(|i| {
                let s = &apps[i].session;
                Some((s.last_activity?, i, s.last_active_agent_id()?))
            })
            .max_by_key(|(ts, _, _)| *ts);
        let Some((_, i, id)) = newest else {
            return;
        };
        let target = format!("{}{id}", ns(i));
        if self.host.flow.node(&target).is_none() {
            return;
        }
        self.host.center_node(&target, true);
        if self.host.selected_agent_id().as_deref() != Some(target.as_str()) {
            self.host.flow.select_node(&target);
            self.host.detail_scroll = 0;
            self.host.detail_follow = true;
        }
    }

    /// Keep plane structure out of the selection. Tab cycling walks every
    /// node, selectable or not, so stepping onto a hub or zone steps on in the
    /// same direction until an agent; anything else that lands on one just
    /// clears. Native only: the browser has no crossterm events.
    #[cfg(feature = "native")]
    pub fn skip_structure(&mut self, event: &crossterm::event::Event) {
        use crossterm::event::{Event, KeyCode};
        let on_structure = |p: &Plane| {
            p.host
                .selected_agent_id()
                .is_some_and(|id| is_structure(&id))
        };
        // A zone held by the mouse stays selected until release decides.
        if self.press.is_some() || !on_structure(self) {
            return;
        }
        let step: Option<fn(&mut graph::AgentFlow)> = match event {
            Event::Key(k) if k.code == KeyCode::BackTab => Some(|f| f.select_prev_node()),
            Event::Key(k) if k.code == KeyCode::Tab => Some(|f| f.select_next_node()),
            _ => None,
        };
        match step {
            Some(step) => {
                for _ in 0..self.host.flow.nodes().count() {
                    step(&mut self.host.flow);
                    if !on_structure(self) {
                        break;
                    }
                }
                if on_structure(self) {
                    self.host.flow.clear_selection();
                }
            }
            None => self.host.flow.clear_selection(),
        }
        if let Some(id) = self.host.selected_agent_id() {
            self.host.pending_center = Some(id);
            self.host.detail_scroll = 0;
            self.host.detail_follow = true;
        }
    }

    /// The selected node's session index and agent id, if a real agent (not
    /// plane structure) is selected.
    pub fn selection(&self) -> Option<(usize, String)> {
        // A card held by the mouse is being moved, not read.
        if self.press.is_some() {
            return None;
        }
        let id = self.host.selected_agent_id()?;
        let (i, agent) = Self::resolve(&id)?;
        Some((i, agent.to_string()))
    }
}

impl Default for Plane {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn resolve_splits_the_prefix() {
        assert_eq!(Plane::resolve("1/main"), Some((1, "main")));
        assert_eq!(Plane::resolve("0/agent-a1/b"), Some((0, "agent-a1/b")));
        assert_eq!(Plane::resolve(&hub_id(3)), None);
        assert_eq!(Plane::resolve(&zone_id(2)), None);
    }

    #[test]
    fn a_worktree_session_keeps_its_own_folder_in_its_projects_zone() {
        let mut p = Plane::new();
        let at = |dir: &str| Group {
            key: crate::provider::group_key(std::path::Path::new(dir)),
            name: dir.rsplit('/').next().unwrap_or(dir).into(),
            branch: Some("lama/t1/fixer".into()),
            cwd: Some(dir.into()),
        };
        // The worktree agent shows up first and makes the zone.
        p.add_session(at("/src/shop/.lama/worktrees/t1-fixer"));
        p.add_session(at("/src/shop"));
        assert_eq!(p.groups.len(), 1);
        let zone = &p.groups[0];
        assert_eq!(zone.name, "shop");
        assert_eq!(zone.cwd.as_deref(), Some(std::path::Path::new("/src/shop")));
        assert_eq!(zone.branch, None);
        assert_eq!(
            p.session_cwd(0),
            Some(std::path::Path::new("/src/shop/.lama/worktrees/t1-fixer"))
        );
        assert_eq!(p.session_cwd(1), Some(std::path::Path::new("/src/shop")));
    }

    #[test]
    fn sessions_group_by_project_key() {
        let mut p = Plane::new();
        let g = |k: &str| Group {
            key: k.into(),
            name: k.into(),
            branch: None,
            cwd: None,
        };
        p.add_session(g("a"));
        p.add_session(g("b"));
        p.add_session(g("a"));
        assert_eq!(p.groups.len(), 2);
        assert_eq!((p.group_of(0), p.group_of(1), p.group_of(2)), (0, 1, 0));
    }

    #[test]
    fn structure_ids_are_never_agents() {
        assert!(is_structure(&hub_id(0)));
        assert!(is_structure(&zone_id(4)));
        assert_eq!(zone_group(&zone_id(4)), Some(4));
        assert_eq!(zone_group(&hub_id(4)), None);
        assert!(!is_structure("0/main"));
    }

    /// A universe holding one session's main card as one block, as `pack`
    /// leaves it.
    fn one_zone() -> Plane {
        let mut p = Plane::universe(chrono::Duration::hours(1));
        p.add_session(Group {
            key: "k".into(),
            name: "proj".into(),
            branch: None,
            cwd: None,
        });
        let agent = graph::new_node(
            "0/main".into(),
            (3.0, 3.0),
            MAIN_NODE_DIMS,
            hub_node(HubInfo::default()),
        );
        p.host.flow.add_node(agent).unwrap();
        p.blocks = vec![Block {
            key: "s".into(),
            group: 0,
            nodes: vec!["0/main".into()],
        }];
        // One render, as every frame does: it tells the flow where its canvas
        // sits, which hit-testing the pointer needs.
        let area = ratatui::layout::Rect::new(0, 0, 120, 40);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut p.host.flow, area, &mut buf);
        p
    }

    fn pos(p: &Plane, id: &str) -> (f64, f64) {
        let n = p.host.flow.node(id).unwrap();
        (n.position.x, n.position.y)
    }

    fn rect(p: &Plane, id: &str) -> Rect4 {
        let n = p.host.flow.node(id).unwrap();
        (n.position.x, n.position.y, n.width, n.height)
    }

    /// The terminal cell at the middle of node `id`, as last drawn.
    fn cell_of(p: &Plane, id: &str) -> (u16, u16) {
        let (x, y, w, h) = p.screen_rect(id).unwrap();
        ((x + w / 2.0) as u16, (y + h / 2.0) as u16)
    }

    /// A universe of two projects: `k` with sessions `a` and `b`, `w` with
    /// `c`, laid out and drawn once.
    fn two_projects() -> (Plane, Vec<App>) {
        let mut p = Plane::universe(chrono::Duration::hours(1));
        let apps = vec![live_app("a"), live_app("b"), live_app("c")];
        for key in ["k", "k", "w"] {
            p.add_session(Group {
                key: key.into(),
                name: key.into(),
                branch: None,
                cwd: None,
            });
        }
        p.sync(&apps, true);
        draw(&mut p);
        (p, apps)
    }

    fn draw(p: &mut Plane) {
        let area = ratatui::layout::Rect::new(0, 0, 200, 60);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut p.host.flow, area, &mut buf);
    }

    fn main_card(apps: &[App], i: usize) -> String {
        root_of(i, &apps[i]).unwrap()
    }

    #[test]
    fn dragging_a_card_moves_only_its_session_and_pins_it() {
        let (mut p, apps) = two_projects();
        let (a, b) = (main_card(&apps, 0), main_card(&apps, 1));
        let (b0, a0) = (pos(&p, &b), pos(&p, &a));
        let (col, row) = cell_of(&p, &a);
        p.press(col, row);
        // What the flow does to the dragged node itself.
        p.host.flow.move_node(&a, (10.0, 40.0));
        p.dragged();
        p.release();
        assert_eq!(pos(&p, &b), b0, "its project's other session stayed");
        let a1 = pos(&p, &a);
        assert_eq!(a1, (a0.0 + 10.0, a0.1 + 40.0));
        assert_eq!(p.pins.get("a"), Some(&a1), "pinned by session");
        assert!(p.pins_dirty);
        assert!(p.selection().is_none(), "no log panel after a drag");
        // It stays there through the next pack.
        p.repack = true;
        p.sync(&apps, true);
        assert_eq!(pos(&p, &a), a1);
        assert_eq!(pos(&p, &b), b0);
    }

    #[test]
    fn the_view_holds_still_under_a_dragged_card() {
        let (mut p, apps) = two_projects();
        let a = main_card(&apps, 0);
        let (col, row) = cell_of(&p, &a);
        p.press(col, row);
        // What the flow does once the drag passes its threshold: it selects
        // the card, and the host asks to centre it.
        p.host.flow.select_node(&a);
        p.host.pending_center = Some(a.clone());
        p.host.flow.move_node(&a, (4.0, 2.0));
        p.dragged();
        assert_eq!(p.host.pending_center, None);
        assert_eq!(p.host.camera, Camera::Manual, "no re-fit after the drop");
    }

    #[test]
    fn the_whole_arrangement_comes_back_after_a_restart() {
        let (mut p, apps) = two_projects();
        let a = main_card(&apps, 0);
        let (col, row) = cell_of(&p, &a);
        p.press(col, row);
        p.host.flow.move_node(&a, (25.0, 30.0));
        p.dragged();
        p.release();
        let at = |p: &Plane| -> Vec<(f64, f64)> {
            (0..3).map(|i| pos(p, &main_card(&apps, i))).collect()
        };
        let before = at(&p);
        // A new run reads the saved layout, and its sessions arrive one at
        // a time, the last first.
        let mut q = Plane::universe(chrono::Duration::hours(1));
        for key in ["k", "k", "w"] {
            q.add_session(Group {
                key: key.into(),
                name: key.into(),
                branch: None,
                cwd: None,
            });
        }
        q.pins = p.pins.clone();
        q.placed = p.placed.clone();
        let mut late: Vec<App> = vec![
            App::new("a".into(), Mode::Live),
            App::new("b".into(), Mode::Live),
            live_app("c"),
        ];
        q.sync(&late, true);
        late[0] = live_app("a");
        late[1] = live_app("b");
        q.sync(&late, true);
        assert_eq!(at(&q), before);
    }

    #[test]
    fn a_new_session_lands_beside_its_project_and_nothing_moves() {
        let (mut p, mut apps) = two_projects();
        let before: Vec<(f64, f64)> = (0..3).map(|i| pos(&p, &main_card(&apps, i))).collect();
        apps.push(live_app("d"));
        p.add_session(Group {
            key: "k".into(),
            name: "k".into(),
            branch: None,
            cwd: None,
        });
        p.sync(&apps, true);
        let after: Vec<(f64, f64)> = (0..3).map(|i| pos(&p, &main_card(&apps, i))).collect();
        assert_eq!(before, after, "the others kept their places");
        let d = rect(&p, &main_card(&apps, 3));
        for i in 0..3 {
            assert!(!crowds(d, rect(&p, &main_card(&apps, i))), "clear of {i}");
        }
        let b = rect(&p, &main_card(&apps, 1));
        let c = rect(&p, &main_card(&apps, 2));
        let far = |r: Rect4| ((r.0 - d.0) / 2.0).powi(2) + (r.1 - d.1).powi(2);
        assert!(far(b) < far(c), "nearer its own project's newest session");
    }

    #[test]
    fn a_card_dropped_on_another_session_leaves_it_be() {
        let (mut p, apps) = two_projects();
        let (a, b) = (main_card(&apps, 0), main_card(&apps, 1));
        let b0 = pos(&p, &b);
        let a0 = pos(&p, &a);
        let (col, row) = cell_of(&p, &a);
        p.press(col, row);
        p.host
            .flow
            .move_node(&a, (b0.0 - a0.0 + 2.0, b0.1 - a0.1 + 1.0));
        p.dragged();
        p.release();
        p.repack = true;
        p.sync(&apps, true);
        assert_eq!(pos(&p, &b), b0, "nothing you didn't touch moves");
        assert_eq!(pos(&p, &a), (b0.0 + 2.0, b0.1 + 1.0));
    }

    #[test]
    fn a_session_that_grows_keeps_its_place_and_its_neighbour_steps_aside() {
        let (mut p, apps) = two_projects();
        let (a, b) = (main_card(&apps, 0), main_card(&apps, 1));
        let a0 = pos(&p, &a);
        p.open_console_in_place(0, &apps);
        p.sync(&apps, true);
        assert_eq!(pos(&p, &a), a0, "the console opens where its card was");
        let ra = rect(&p, &a);
        assert_eq!((ra.2, ra.3), CONSOLE_DIMS);
        for i in 1..3 {
            assert!(!crowds(ra, rect(&p, &main_card(&apps, i))), "{i} clear");
        }
        assert!(p.host.flow.node(&b).is_some());
    }

    #[test]
    fn a_lit_project_is_outlined_and_the_rest_dim() {
        let (mut p, apps) = two_projects();
        p.toggle_spot(1);
        p.sync(&apps, false);
        let hull = rect(&p, &zone_id(1));
        let c = rect(&p, &main_card(&apps, 2));
        assert!(hull.0 < c.0 && hull.1 < c.1, "round its session");
        assert!(hull.0 + hull.2 > c.0 + c.2 && hull.1 + hull.3 > c.1 + c.3);
        let dim = |p: &Plane, i: usize| {
            p.host
                .flow
                .node(&main_card(&apps, i))
                .unwrap()
                .content
                .feed
                .dim
        };
        assert!(dim(&p, 0) && dim(&p, 1) && !dim(&p, 2));
        p.toggle_spot(1);
        p.sync(&apps, false);
        assert!(p.host.flow.node(&zone_id(1)).is_none(), "put out");
        assert!(!dim(&p, 0) && !dim(&p, 2));
    }

    #[test]
    fn a_main_card_wears_its_project() {
        let (p, apps) = two_projects();
        let feed = &p.host.flow.node(&main_card(&apps, 2)).unwrap().content.feed;
        assert_eq!(feed.project.as_deref(), Some("w"));
    }

    #[test]
    fn tidying_forgets_where_sessions_were_left() {
        let (mut p, apps) = two_projects();
        let a = main_card(&apps, 0);
        let home = pos(&p, &a);
        p.press(cell_of(&p, &a).0, cell_of(&p, &a).1);
        p.host.flow.move_node(&a, (30.0, 30.0));
        p.dragged();
        p.release();
        p.reset_layout();
        p.sync(&apps, false);
        assert!(p.pins.is_empty());
        assert_eq!(pos(&p, &a), home, "back with its project");
    }

    #[test]
    fn dragging_a_project_carries_its_lane_headers() {
        let mut p = one_zone();
        let lane = format!("◈lane/0/main/{}", "check");
        p.put_structure(&lane, HubInfo::default(), LANE_HEAD_DIMS, false);
        p.host.flow.set_node_position(&lane, (3.0, 12.0));
        p.blocks[0].nodes.push(lane.clone());
        p.press(10, 5);
        p.host.flow.move_node("0/main", (5.0, 2.0));
        p.dragged();
        assert_eq!(
            pos(&p, &lane),
            (8.0, 14.0),
            "the header moved with its cards"
        );
        assert_eq!(p.project_of(&lane), Some(0));
    }

    #[test]
    fn a_click_on_a_card_zooms_in_without_a_panel() {
        let mut p = one_zone();
        p.press(10, 5);
        p.release();
        assert_eq!(p.focus, Some(0));
        assert_eq!(p.focus_block.as_deref(), Some("s"));
        assert!(p.selection().is_none());
        assert!(p.pins.is_empty(), "a click pins nothing");
    }

    #[test]
    fn wheel_zoom_eases_to_its_target_around_the_pointer() {
        let mut p = one_zone();
        let v0 = p.host.flow.viewport;
        let anchor = (20.5 - v0.x) / v0.zoom; // world x under column 20
        p.wheel_zoom(true, 20, 10);
        p.tick_motion(std::time::Duration::from_millis(16));
        let mid = p.host.flow.viewport.zoom;
        assert!(
            mid > v0.zoom && mid < v0.zoom * WHEEL_STEP,
            "eases, doesn't jump"
        );
        for _ in 0..60 {
            p.tick_motion(std::time::Duration::from_millis(16));
        }
        let v = p.host.flow.viewport;
        assert!(
            (v.zoom - v0.zoom * WHEEL_STEP).abs() < 1e-9,
            "lands on the step"
        );
        assert!(
            ((20.5 - v.x) / v.zoom - anchor).abs() < 1e-6,
            "pointer stays put"
        );
    }

    #[test]
    fn a_drag_pan_glides_to_where_the_pointer_put_it() {
        let mut p = one_zone();
        let before = (p.host.flow.viewport.x, p.host.flow.viewport.y);
        // What the flow does on a drag: the view jumps a whole cell.
        p.host.flow.viewport.x += 4.0;
        p.smooth_pan(before);
        assert_eq!(p.host.flow.viewport.x, before.0, "drawn from where it was");
        p.tick_motion(std::time::Duration::from_millis(16));
        let x = p.host.flow.viewport.x;
        assert!(x > before.0 && x < before.0 + 4.0, "partway there");
        for _ in 0..30 {
            p.tick_motion(std::time::Duration::from_millis(16));
        }
        assert_eq!(p.host.flow.viewport.x, before.0 + 4.0, "arrives exactly");
    }

    #[test]
    fn a_pan_takes_the_camera_out_of_the_home_view() {
        let mut p = one_zone();
        p.host.camera = Camera::Overview;
        let before = (p.host.flow.viewport.x, p.host.flow.viewport.y);
        p.host.flow.viewport.x += 4.0;
        p.smooth_pan(before);
        assert_eq!(
            p.host.camera,
            Camera::Manual,
            "no re-fit snaps the pan back"
        );
    }

    #[test]
    fn a_trackpad_burst_cannot_overshoot() {
        let mut p = one_zone();
        let z0 = p.host.flow.viewport.zoom;
        for _ in 0..15 {
            p.wheel_zoom(true, 20, 10);
        }
        for _ in 0..120 {
            p.tick_motion(std::time::Duration::from_millis(16));
        }
        assert!(p.host.flow.viewport.zoom <= z0 * WHEEL_LEAD + 1e-9);
    }

    #[test]
    fn a_flung_pan_coasts_then_rests() {
        let mut p = one_zone();
        p.motion.coast = Some((60.0, 0.0));
        let x0 = p.host.flow.viewport.x;
        p.tick_motion(std::time::Duration::from_millis(16));
        assert!(p.host.flow.viewport.x > x0, "keeps moving after release");
        for _ in 0..200 {
            p.tick_motion(std::time::Duration::from_millis(16));
        }
        assert!(p.motion.coast.is_none(), "comes to rest");
    }

    #[test]
    fn a_grown_card_makes_room_without_spreading_the_rest() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let kids: HashMap<String, Vec<String>> =
            HashMap::from([("a".to_string(), ids(&["a1", "a2"]))]);
        let mut sizes: HashMap<String, (f64, f64)> = ["a", "a1", "a2", "b"]
            .iter()
            .map(|id| (id.to_string(), MAIN_NODE_DIMS))
            .collect();
        let at = |out: &[(String, f64, f64)], id: &str| {
            out.iter()
                .find(|(n, _, _)| n == id)
                .map(|(_, x, y)| (*x, *y))
                .unwrap()
        };
        let roots = ids(&["a", "b"]);
        let flat = tree_layout(&roots, &kids, &HashSet::new(), &sizes);
        // The console opens on `a`: it grows in place.
        sizes.insert("a".into(), CONSOLE_DIMS);
        let grown = tree_layout(&roots, &kids, &HashSet::new(), &sizes);
        let (_, a1_y) = at(&grown, "a1");
        assert_eq!(
            a1_y,
            CONSOLE_DIMS.1 + TREE_RANK,
            "its children move below it"
        );
        let gap = |out: &[(String, f64, f64)]| at(out, "a2").0 - at(out, "a1").0;
        assert_eq!(gap(&flat), gap(&grown), "siblings keep their spacing");
        assert!(
            at(&grown, "b").0 >= CONSOLE_DIMS.0,
            "the next tree steps aside"
        );
    }

    #[test]
    fn a_lane_stacks_its_helpers_under_its_header() {
        let ids = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let kids: HashMap<String, Vec<String>> = HashMap::from([
            ("main".to_string(), ids(&["check", "build"])),
            ("check".to_string(), ids(&["c1", "c2", "c3"])),
            ("build".to_string(), ids(&["b1"])),
        ]);
        let stacks: HashSet<String> = ["check", "build"].iter().map(|s| s.to_string()).collect();
        let mut sizes: HashMap<String, (f64, f64)> = HashMap::from([
            ("main".to_string(), MAIN_NODE_DIMS),
            ("check".to_string(), LANE_HEAD_DIMS),
            ("build".to_string(), LANE_HEAD_DIMS),
        ]);
        for k in ["c1", "c2", "c3", "b1"] {
            sizes.insert(k.to_string(), LANE_CARD_DIMS);
        }
        // c2 is selected: open at full size.
        sizes.insert("c2".to_string(), SUB_NODE_DIMS);
        let out = tree_layout(&ids(&["main"]), &kids, &stacks, &sizes);
        let at = |id: &str| {
            out.iter()
                .find(|(n, _, _)| n == id)
                .map(|(_, x, y)| (*x, *y))
                .unwrap()
        };
        let (hx, hy) = at("check");
        assert_eq!(hy, MAIN_NODE_DIMS.1 + TREE_RANK, "lanes sit one rank down");
        let (c1x, c1y) = at("c1");
        assert_eq!(c1x, hx + LANE_INDENT, "cards sit right of the spine");
        assert_eq!(c1y, hy + LANE_HEAD_DIMS.1 + LANE_GAP);
        let (c2x, c2y) = at("c2");
        assert_eq!(c2x, c1x, "one column");
        assert_eq!(c2y, c1y + LANE_CARD_DIMS.1 + LANE_GAP);
        let (_, c3y) = at("c3");
        assert_eq!(
            c3y,
            c2y + SUB_NODE_DIMS.1 + LANE_GAP,
            "the open card pushes the rest down"
        );
        let (bx, by) = at("build");
        assert_eq!(by, hy, "lanes side by side");
        assert!(
            bx >= c2x + SUB_NODE_DIMS.0,
            "the next lane clears the widest card"
        );
    }

    #[test]
    fn a_born_card_grows_from_its_parent_into_place() {
        let mut p = one_zone();
        let from = (10.0, 12.0);
        p.births.insert(
            "0/main".into(),
            Birth {
                from,
                to: (30.0, 20.0),
                size: MAIN_NODE_DIMS,
                start: web_time::Instant::now(),
                at: (from.0, from.1, BIRTH_DIMS.0, BIRTH_DIMS.1),
            },
        );
        p.tick_births();
        let n = p.host.flow.node("0/main").unwrap();
        assert!(n.width < MAIN_NODE_DIMS.0, "starts small");
        assert!(n.position.x < 30.0, "starts near its parent");
        // Backdate the birth past its end: it lands exactly.
        p.births.get_mut("0/main").unwrap().start =
            web_time::Instant::now() - std::time::Duration::from_secs(2);
        p.tick_births();
        let n = p.host.flow.node("0/main").unwrap();
        assert_eq!((n.position.x, n.position.y), (30.0, 20.0));
        assert_eq!((n.width, n.height), MAIN_NODE_DIMS);
        assert!(p.births.is_empty());
    }

    #[test]
    fn only_a_card_can_be_held() {
        let p = one_zone();
        assert_eq!(p.node_under(10, 5).as_deref(), Some("0/main"));
        assert_eq!(p.node_under(100, 30), None);
    }

    /// A live session `sid` with one main agent, active now.
    pub(crate) fn live_app(sid: &str) -> App {
        use crate::provider::claude::{Record, Source};
        let line = format!(
            r#"{{"type":"assistant","uuid":"u-{sid}","timestamp":"{}","message":{{"role":"assistant","content":[{{"type":"text","text":"hi"}}]}}}}"#,
            Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
        let entry = crate::provider::claude::wire::parse_line(&line).unwrap();
        let statement = Record::Entry {
            source: Source::Main,
            entry,
        }
        .statement()
        .unwrap();
        let mut app = App::new(sid.into(), Mode::Live);
        app.handle_ui_event(crate::tailer::UiEvent::Batch {
            session_id: sid.into(),
            statements: vec![statement],
        });
        app
    }

    /// A universe of one project holding your session, a task agent and
    /// the task's conductor.
    pub(crate) fn task_universe() -> (Plane, Vec<App>) {
        use super::super::task::{AgentLink, TaskView};
        let mut p = Plane::universe(chrono::Duration::hours(1));
        let apps = vec![live_app("mine"), live_app("agent"), live_app("cond")];
        for _ in &apps {
            p.add_session(Group {
                key: "k".into(),
                name: "proj".into(),
                branch: None,
                cwd: None,
            });
        }
        p.task = Some(TaskView {
            conductor_session: "cond".into(),
            agents: vec![AgentLink {
                name: "api-forge".into(),
                session: Some("agent".into()),
                ..AgentLink::default()
            }],
            ..TaskView::default()
        });
        p.sync(&apps, true);
        (p, apps)
    }

    #[test]
    fn a_task_marks_its_agents_and_hides_its_conductor() {
        let (p, apps) = task_universe();
        let card = |i: usize| root_of(i, &apps[i]).and_then(|id| p.host.flow.node(&id));
        assert!(
            card(0).is_some_and(|n| !n.content.feed.task),
            "yours, unmarked"
        );
        assert!(
            card(1).is_some_and(|n| n.content.feed.task),
            "the agent's, ◈"
        );
        assert!(card(2).is_none(), "the conductor lives in the panel");
        assert_eq!(p.rail[0].on_task, 1);
        assert_eq!(
            p.rail[0].sessions, 2,
            "the conductor isn't counted in its project"
        );
        assert_eq!(
            p.task_card(&apps, "api-forge"),
            root_of(1, &apps[1]),
            "messages find the agent's card"
        );
        assert_eq!(p.task_card(&apps, "nobody"), None);
    }

    /// A universe of one project where task `t1`'s owner works beside the
    /// peer it asked for, and the conductor. The canvas has its size, as
    /// after a first frame.
    pub(crate) fn owner_universe() -> (Plane, Vec<App>) {
        use super::super::task::{AgentLink, TaskRow, TaskView};
        let mut p = Plane::universe(chrono::Duration::hours(1));
        let apps = vec![live_app("owner"), live_app("peer"), live_app("cond")];
        for _ in &apps {
            p.add_session(Group {
                key: "k".into(),
                name: "proj".into(),
                branch: None,
                cwd: None,
            });
        }
        let area = ratatui::layout::Rect::new(0, 0, 160, 50);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut p.host.flow, area, &mut buf);
        p.task = Some(TaskView {
            conductor_session: "cond".into(),
            tasks: vec![TaskRow {
                id: "t1".into(),
                title: "the api drops meta".into(),
                projects: vec!["proj".into()],
                ..TaskRow::default()
            }],
            agents: vec![
                AgentLink {
                    name: "api-owner".into(),
                    session: Some("owner".into()),
                    task: "t1".into(),
                    ..AgentLink::default()
                },
                AgentLink {
                    name: "reviewer".into(),
                    session: Some("peer".into()),
                    task: "t1".into(),
                    parent: Some("api-owner".into()),
                    ..AgentLink::default()
                },
            ],
            ..TaskView::default()
        });
        p.sync(&apps, true);
        (p, apps)
    }

    #[test]
    fn owners_wear_their_tag_and_a_picked_task_lights_up_and_takes_the_camera() {
        let (mut p, apps) = owner_universe();
        let feed = |p: &Plane, i: usize| {
            let id = root_of(i, &apps[i]).unwrap();
            p.host.flow.node(&id).unwrap().content.feed.clone()
        };
        assert!(feed(&p, 0).task && feed(&p, 0).owner, "the owner");
        assert!(feed(&p, 1).task && !feed(&p, 1).owner, "its peer");
        assert!(!feed(&p, 0).picked && !feed(&p, 1).picked);

        let mut view = p.task.clone().unwrap();
        view.selected = Some("t1".into());
        p.set_task(view, &apps);
        assert!(feed(&p, 0).picked && feed(&p, 1).picked, "picked: lit");

        assert!(!p.fly_to_task("nope"), "no cards, no flight");
        assert!(p.fly_to_task("t1"));
        assert!(p.motion.fly.is_some(), "the camera is on its way");
        assert_eq!(p.focus, Some(0), "to the task's one project");
    }

    /// A universe of two projects: yours, live, and `web`, where the task
    /// plans `ui-guard` and nothing of it has a transcript yet. The canvas
    /// has its size, as after a first frame.
    pub(crate) fn planned_universe() -> (Plane, Vec<App>) {
        use super::super::task::{AgentLink, PlanView, PlannedView, TaskView};
        let mut p = Plane::universe(chrono::Duration::hours(1));
        let apps = vec![live_app("mine"), App::new("agent".into(), Mode::Live)];
        for key in ["k", "w"] {
            p.add_session(Group {
                key: key.into(),
                name: key.into(),
                branch: None,
                cwd: None,
            });
        }
        let area = ratatui::layout::Rect::new(0, 0, 160, 50);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut p.host.flow, area, &mut buf);
        p.task = Some(TaskView {
            plan: Some(PlanView {
                version: 1,
                planned_at: 3.0,
                agents: vec![PlannedView {
                    name: "ui-guard".into(),
                    project_key: "w".into(),
                    ..PlannedView::default()
                }],
                ..PlanView::default()
            }),
            agents: vec![AgentLink {
                name: "ui-guard".into(),
                session: Some("agent".into()),
                ..AgentLink::default()
            }],
            ..TaskView::default()
        });
        p.sync(&apps, true);
        (p, apps)
    }

    #[test]
    fn a_planned_agent_opens_its_project_and_waits_there_as_a_planned_card() {
        let (mut p, mut apps) = planned_universe();
        assert!(
            p.rail.iter().any(|r| r.group == 1 && r.open),
            "its project is on the map, with no session of its own"
        );
        let ghost = ghost_id(1, "ui-guard");
        let g = p.host.flow.node(&ghost).expect("a planned card");
        assert!(g.content.feed.ghost);
        assert_eq!((g.width, g.height), GHOST_DIMS);
        let mine = rect(&p, &root_of(0, &apps[0]).unwrap());
        assert!(!crowds(rect(&p, &ghost), mine), "a place of its own");
        assert_eq!(p.project_of(&ghost), Some(1));
        assert_eq!(p.task_ghost("ui-guard"), Some(ghost.clone()));

        // Its session arrives: the card takes the place in the same sync.
        apps[1] = live_app("agent");
        p.sync(&apps, false);
        assert!(
            p.host.flow.node(&ghost).is_none(),
            "the planned card is gone"
        );
        let card = root_of(1, &apps[1]).unwrap();
        assert!(p.host.flow.node(&card).is_some_and(|n| n.content.feed.task));
        assert_eq!(p.task_ghost("ui-guard"), None);
    }

    #[test]
    fn the_camera_flies_to_the_task_sessions_unless_you_have_it() {
        let (mut p, apps) = planned_universe();
        let (x0, y0, x1, y1) = p.task_bounds().expect("the task's card is placed");
        let (gx, gy, gw, gh) = rect(&p, &ghost_id(1, "ui-guard"));
        assert_eq!(
            (x0, y0, x1, y1),
            (gx, gy, gx + gw, gy + gh),
            "the task's cards only, not yours"
        );
        assert!(p.motion.fly.is_some(), "the plan flies the camera");
        assert_eq!(p.focus, Some(1));
        p.tick_motion(std::time::Duration::from_millis(16));
        assert!(p.motion.fly.is_some(), "a flight, not a jump");
        for _ in 0..100 {
            p.tick_motion(std::time::Duration::from_millis(16));
        }
        assert!(p.motion.fly.is_none(), "it lands");
        let v = p.host.flow.viewport;
        let area = p.host.flow.canvas_area();
        let centre = (
            (area.width as f64 / 2.0 - v.x) / v.zoom,
            (area.height as f64 / 2.0 - v.y) / v.zoom,
        );
        assert!((centre.0 - (x0 + x1) / 2.0).abs() < 1e-6);
        assert!((centre.1 - (y0 + y1) / 2.0).abs() < 1e-6);
        assert!(v.zoom <= FLY_ZOOM_MAX);

        // The frontend re-bases the plan's time every frame: no new flight.
        let mut view = p.task.clone().unwrap();
        view.plan.as_mut().unwrap().planned_at += 1e-9;
        p.set_task(view, &apps);
        assert!(p.motion.fly.is_none(), "jitter is no cue");

        // You pan: the next revision leaves the camera where you put it.
        p.touch_camera();
        let before = p.host.flow.viewport;
        let m = p.task.as_mut().unwrap();
        m.plan.as_mut().unwrap().version = 2;
        let view = m.clone();
        p.set_task(view, &apps);
        assert!(p.motion.fly.is_none());
        assert_eq!(p.host.flow.viewport.x, before.x);
    }

    #[test]
    fn a_task_too_wide_to_read_frames_its_main_cards_instead() {
        let (mut p, _apps) = planned_universe();
        // A canvas so narrow the task's blocks would be unreadable.
        let area = ratatui::layout::Rect::new(0, 0, 20, 50);
        let mut buf = ratatui::buffer::Buffer::empty(area);
        ratatui::widgets::Widget::render(&mut p.host.flow, area, &mut buf);
        let (x, y, w, h) = rect(&p, &ghost_id(1, "ui-guard"));
        assert_eq!(p.camera_target(), Some((x, y, x + w, y + h)));
    }
}
