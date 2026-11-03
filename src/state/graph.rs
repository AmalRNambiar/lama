//! Incremental projection of [`SessionModel`] onto a rataflow `Flow`.
//!
//! Never rebuilds: per agent, either mutate the existing node content in place
//! via `node_content_mut`, or `add_node` + `add_edge` (duplicate-id `Err` is an
//! idempotent no-op). Nodes are added before their edges. A structural change
//! (node/edge added) marks layout dirty; at sync end we run Sugiyama. Selection
//! survives because node ids are stable and we never clear-and-re-add.

use rataflow::{Edge, Flow, Handle, HandlePosition, Node, Reconnectable, Sugiyama, Theme};

use super::session::{AgentInfo, AgentKind, AgentStatus, SessionModel};
use crate::ui::edges::AgentEdge;
use crate::ui::nodes::{AgentNode, Feed, FeedCall, MAIN_NODE_DIMS, SUB_NODE_DIMS};

/// The concrete `Flow` type lama uses: agent-card nodes, step-routed parent
/// edges (no labels — liveness reads from color alone).
pub type AgentFlow = Flow<AgentNode, AgentEdge>;

/// Build an empty, fully-configured `Flow` for lama.
///
/// Config: `with_deselect_on_pane_click(false)`, `deselect_on_drag = false`
/// (detail panel persists), `with_min_zoom(0.1)` (Sugiyama trees outgrow the
/// default fit-view limit). Hidden source/target handles for a clean look.
pub fn new_flow() -> AgentFlow {
    // lama identity palette: stock dark base, but `accent` becomes GOLD —
    // selection highlights, done medals, the REPLAY badge. Green stays
    // exclusively "alive" (status), red "failed". Every surface resolves from
    // flow.theme, so this one assignment brands the whole app.
    let palette = crate::ui::fx::palette();
    let mut flow = Flow::new()
        .with_theme(Theme::Custom(palette))
        .with_deselect_on_pane_click(false)
        // We drive the camera on selection ourselves (a center-glide via
        // `pending_center`), so suppress the library's instant ensure-visible pan
        // — otherwise the two stack into a jump-then-glide on off-screen nodes.
        .with_selection_reveal(rataflow::SelectionReveal::None)
        .with_min_zoom(0.1);
    flow.deselect_on_drag = false;
    flow
}

/// Title line for a node: the agent type the provider recorded, else the
/// generic label for its kind. No provider name appears here; the root
/// agent's is stated by its provider like any other agent's.
fn node_title(info: &AgentInfo) -> String {
    info.agent_type
        .clone()
        .unwrap_or_else(|| info.kind.default_label().to_string())
}

/// Fixed card dimensions for a node kind.
fn node_dims(kind: AgentKind) -> (f64, f64) {
    match kind {
        AgentKind::Main | AgentKind::Group => MAIN_NODE_DIMS,
        AgentKind::Subagent => SUB_NODE_DIMS,
    }
}

/// Whether a node's content already mirrors the agent — allocation-free
/// comparison so unchanged agents skip [`build_content`]'s String clones on
/// every sync (the steady state for almost all agents on almost all ticks).
fn content_matches(info: &AgentInfo, node: &AgentNode, reasoning: Option<&str>) -> bool {
    let title_ok = node.title
        == info
            .agent_type
            .as_deref()
            .unwrap_or(info.kind.default_label());
    title_ok
        && node.description.as_deref() == info.description.as_deref()
        && node.status == info.status
        && node.tool_count == info.tool_calls.len()
        && node.last_tool.as_deref() == info.last_tool()
        && node.output_tokens == info.output_tokens
        && node.interactive == info.is_interactive()
        && node.feed.reasoning.as_deref() == reasoning
        && node.feed.calls.first().map(|c| c.state) == info.tool_calls.last().map(|t| t.state)
}

/// How many recent calls a card's feed carries.
const FEED_CALLS: usize = 4;
/// Activity sparkline: buckets × seconds per bucket.
const ACTIVITY_BUCKETS: usize = 16;
const ACTIVITY_BUCKET_SECS: i64 = 15;

/// Build the [`AgentNode`] content mirrored from an [`AgentInfo`].
fn build_content(info: &AgentInfo, model: &SessionModel, id: &str, at: &Placement) -> AgentNode {
    let calls = info
        .tool_calls
        .iter()
        .rev()
        .take(FEED_CALLS)
        .map(|t| FeedCall {
            name: t.name.clone(),
            summary: t.summary.clone(),
            state: t.state,
            start: t.ts,
            end: t.end_ts,
        })
        .collect();
    // Tool-call starts per bucket, oldest bucket first, ending at the
    // reference time. Walks back from the newest call and stops at the window.
    let now = at.now.or(model.last_activity);
    let mut activity = vec![0u16; ACTIVITY_BUCKETS];
    if let Some(now) = now {
        let window = ACTIVITY_BUCKET_SECS * ACTIVITY_BUCKETS as i64;
        for t in info.tool_calls.iter().rev() {
            let Some(ts) = t.ts else { continue };
            let ago = (now - ts).num_seconds();
            if ago >= window {
                break;
            }
            if ago >= 0 {
                let b = ACTIVITY_BUCKETS - 1 - (ago / ACTIVITY_BUCKET_SECS) as usize;
                activity[b] += 1;
            }
        }
    }
    AgentNode {
        title: node_title(info),
        description: info.description.clone(),
        status: info.status,
        tool_count: info.tool_calls.len(),
        last_tool: info.last_tool().map(str::to_string),
        output_tokens: info.output_tokens,
        interactive: info.is_interactive(),
        feed: Feed {
            provider: model.provider(),
            model: info.model.clone(),
            calls,
            reasoning: model.last_reasoning(id).map(str::to_string),
            first_ts: info.first_ts,
            busy: info.busy,
            open_since: info
                .last_ts
                .filter(|_| info.working && info.status == AgentStatus::Running),
            now,
            wall_clock: at.wall_clock,
            activity,
            hub: None,
            // The plane sets these after every sync (`Plane::mark_finished`).
            finished: false,
            context: None,
            cost: None,
            task: false,
            owner: false,
            picked: false,
            ghost: false,
            dim: false,
            project: None,
        },
    }
}

/// Where a model's agents land in a flow. The default is a lone session: ids
/// as the model has them, roots without a parent. The plane puts several sessions
/// on one canvas by giving each an id prefix and hanging its roots from a hub.
#[derive(Debug, Clone, Copy, Default)]
pub struct Placement<'a> {
    /// Prefix for every node and edge id from this model.
    pub ns: &'a str,
    /// Node the model's root agents get an edge from, if any.
    pub root_parent: Option<&'a str>,
    /// Rebuild every card, even unchanged ones — keeps sparklines sliding
    /// when the caller re-syncs on a clock rather than on new facts.
    pub force: bool,
    /// The reference "now" for activity buckets (the timeline's).
    pub now: Option<chrono::DateTime<chrono::Utc>>,
    /// Cards tick running durations off the wall clock (live sessions).
    pub wall_clock: bool,
    /// Helpers that sit in a phase lane: agent id → the lane header's node
    /// id. Their edge comes from the header's spine instead of the parent.
    pub lanes: Option<&'a std::collections::HashMap<String, String>>,
}

/// Horizontal gap between locally-placed siblings (world units).
const LOCAL_H_GAP: f64 = 4.0;
/// Vertical gap below a parent for locally-placed children (world units).
const LOCAL_V_GAP: f64 = 5.0;

/// Incrementally sync `flow` to `model` — a lone session, see [`sync_placed`].
#[cfg(test)]
fn sync(flow: &mut AgentFlow, model: &SessionModel, relayout: bool) -> bool {
    sync_placed(flow, model, relayout, &Placement::default())
}

/// Incrementally sync `flow` to `model`, placed per `at`.
///
/// For each agent in spawn order: mutate the existing node content in place, or
/// add the node (then its parent edge). Updates edge `animated` from target
/// status. New nodes get LOCAL placement (below their parent, offset past
/// siblings) so they land somewhere sensible even without a relayout.
///
/// When `relayout` is true, any structural change ends with a full
/// `Sugiyama::vertical()` pass (which overwrites the local placements). When
/// false — Manual camera: the user owns the view — nothing existing moves;
/// the caller tracks dirtiness and relayouts when the camera re-engages.
/// Returns `true` if structure changed.
pub fn sync_placed(
    flow: &mut AgentFlow,
    model: &SessionModel,
    relayout: bool,
    at: &Placement,
) -> bool {
    let mut structural = false;
    let nid = |id: &str| format!("{}{id}", at.ns);
    let parent_of = |info: &AgentInfo| match &info.parent {
        Some(p) => Some(nid(p)),
        None => at.root_parent.map(str::to_string),
    };

    // First pass: nodes (must exist before their edges).
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        let node_id = nid(id);
        if let Some(existing) = flow.node_content_mut(&node_id) {
            // Steady state: only rebuild (String clones) when something
            // visible changed — the per-second status tick and per-batch
            // syncs walk every agent, and most are unchanged.
            if at.force || !content_matches(info, existing, model.last_reasoning(id)) {
                *existing = build_content(info, model, id, at);
            }
        } else {
            // Sibling index for local placement — computed only for the rare
            // new node; the no-new-nodes steady state skips it entirely.
            let siblings = info
                .parent
                .as_deref()
                .map(|p| {
                    model
                        .spawn_order
                        .iter()
                        .take_while(|x| *x != id)
                        .filter(|x| model.agent(x).and_then(|a| a.parent.as_deref()) == Some(p))
                        .count()
                })
                .unwrap_or(0);
            let content = build_content(info, model, id, at);
            let (w, h) = node_dims(info.kind);
            // Local placement: below the parent, fanned past prior siblings.
            // Overwritten by Sugiyama when `relayout` runs; kept verbatim in
            // Manual so existing nodes never move underneath the user.
            let pos = parent_of(info)
                .and_then(|p| flow.node(&p))
                .map(|parent| {
                    (
                        parent.position.x + siblings as f64 * (w + LOCAL_H_GAP),
                        parent.position.y + parent.height + LOCAL_V_GAP,
                    )
                })
                .unwrap_or((0.0, 0.0));
            // Read-only monitor: nodes are selectable (detail panel) and
            // draggable (manual arrangement) — but never deletable and never
            // connection sources. Enforced at the DTO level, not just the key
            // whitelist, so no input path can mutate the graph.
            let node = new_node(node_id, pos, (w, h), content);
            // Duplicate-id is an idempotent no-op; a genuine add is structural.
            if flow.add_node(node).is_ok() {
                structural = true;
            }
        }
    }

    // Second pass: edges from each agent to its parent.
    for id in &model.spawn_order {
        let Some(info) = model.agent(id) else {
            continue;
        };
        let Some(parent) = parent_of(info) else {
            continue;
        };
        let animated = info.status == AgentStatus::Running;
        let lane = at.lanes.and_then(|l| l.get(id.as_str()));
        let (parent, spine) = match lane {
            Some(header) => (header.clone(), true),
            None => (parent, false),
        };
        if add_or_refresh_edge_from(flow, &parent, &nid(id), animated, spine) {
            structural = true;
        }
    }

    if structural && relayout {
        self::relayout(flow);
    }
    structural
}

/// The left-side handles a phase lane's spine runs between: out of the
/// lane's header, into each of its cards. Handle ids are unique per node.
const SPINE_OUT: &str = "spine-out";
const SPINE_IN: &str = "spine-in";
/// The parent edge's handles: out of the bottom, into the top.
const DOWN: &str = "down";
const UP: &str = "up";

/// A read-only card node with hidden handles: bottom to top for a parent
/// edge, and a left pair for a phase lane's spine (see [`super::lanes`]).
pub(crate) fn new_node(
    id: String,
    pos: (f64, f64),
    dims: (f64, f64),
    content: AgentNode,
) -> Node<AgentNode> {
    Node::new(id, pos, dims, content)
        .with_deletable(false)
        .with_connectable(false)
        .with_handles(vec![
            Handle::source(HandlePosition::Bottom)
                .with_id(DOWN)
                .with_hidden(true),
            Handle::target(HandlePosition::Top)
                .with_id(UP)
                .with_hidden(true),
            Handle::source(HandlePosition::Left)
                .with_id(SPINE_OUT)
                .with_hidden(true),
            Handle::target(HandlePosition::Left)
                .with_id(SPINE_IN)
                .with_hidden(true),
        ])
}

/// Ensure the parent edge into `child` exists and mirrors `animated`.
/// Returns `true` if the edge was added (structural).
pub(crate) fn add_or_refresh_edge(
    flow: &mut AgentFlow,
    parent: &str,
    child: &str,
    animated: bool,
) -> bool {
    add_or_refresh_edge_from(flow, parent, child, animated, false)
}

/// [`add_or_refresh_edge`], from `parent` — which may change when a helper
/// moves into a phase lane or out of one — and, for a lane's `spine`, left
/// side to left side.
pub(crate) fn add_or_refresh_edge_from(
    flow: &mut AgentFlow,
    parent: &str,
    child: &str,
    animated: bool,
    spine: bool,
) -> bool {
    let edge_id = edge_id(child);
    let (from, to) = if spine {
        (SPINE_OUT, SPINE_IN)
    } else {
        (DOWN, UP)
    };
    let (from, to) = (Some(from.to_string()), Some(to.to_string()));
    let moved = flow
        .edge(&edge_id)
        .is_some_and(|e| e.source != parent || e.target_handle != to);
    if moved {
        flow.remove_edge(&edge_id);
    }
    // Edge already present (the steady state on every sync): just refresh
    // animation — probing via `edge_content_mut` first avoids building a
    // throwaway Edge (three String clones) per agent per sync only for
    // `add_edge` to reject it as a duplicate. Edges carry no selectable
    // meaning in lama (no edge panel), and a stray edge click would pin
    // Follow mode while closing the node panel — a dead state. Fully inert:
    // not selectable, deletable, or reconnectable. Liveness shows as the
    // running color + marching ants, NOT a label — the current tool already
    // shows in the child's chips and detail panel.
    if let Some(content) = flow.edge_content_mut(&edge_id) {
        content.running = animated;
        flow.set_edge_animated(&edge_id, animated);
        return false;
    }
    let edge = Edge::new(edge_id.clone(), parent.to_string(), child.to_string())
        .with_source_handle(from)
        .with_target_handle(to)
        .with_animated(animated)
        .with_selectable(false)
        .with_deletable(false)
        .with_reconnectable(Reconnectable::None);
    let added = flow.add_edge(edge).is_ok();
    if let Some(content) = flow.edge_content_mut(&edge_id) {
        content.running = animated;
    }
    added
}

/// Stable id for the (single) parent edge of `child`.
///
/// Keyed by the child alone: every agent has exactly one parent edge, and
/// `sync` never removes edges — so the id must never change once created.
/// Keying on `spawned_by` or the parent would orphan a stale edge if
/// either field were filled in after the edge existed (latent today, armed by
/// any future meta re-emission).
fn edge_id(child: &str) -> String {
    format!("e-{child}")
}

/// Remove several agents from the canvas in ONE pass.
///
/// Bulk, not a loop of single removals: `Flow::remove_node` costs O(nodes plus
/// edges) every time — it shifts every lookup index and rebuilds the edge
/// lookup per call — so removing k of them is quadratic. `retain_nodes` makes a
/// single pass over each vec and drops the connected edges itself.
pub fn remove_agents(flow: &mut AgentFlow, agents: &[String]) {
    if agents.is_empty() {
        return;
    }
    let doomed: std::collections::HashSet<&str> = agents.iter().map(String::as_str).collect();
    flow.retain_nodes(|n| !doomed.contains(n.id.as_str()));
}

/// Apply the Sugiyama vertical layout to `flow`.
///
/// Split out so it can be called explicitly and unit-tested independently of
/// the per-agent diffing in [`sync`].
pub fn relayout(flow: &mut AgentFlow) {
    flow.apply_layout(Sugiyama::vertical());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::claude::wire::SubagentMeta;

    /// A model with main + one direct subagent (running).
    fn model_with_subagent() -> SessionModel {
        let mut m = SessionModel::new("s1".into());
        // The root's name is the provider's to state.
        m.apply_fact(&crate::fact::Fact {
            agent: Some(super::super::session::MAIN_ID.to_string()),
            ts: None,
            kind: crate::fact::FactKind::Agent {
                kind: AgentKind::Main,
                parent: None,
                agent_type: Some("claude".into()),
                description: None,
                spawned_by: None,
                interactive: true,
            },
        });
        let meta = SubagentMeta {
            agent_type: Some("guide".into()),
            description: Some("research".into()),
            tool_use_id: Some("ag1".into()),
            stopped_by_user: None,
        };
        m.apply_meta("abc123", None, &meta);
        m
    }

    #[test]
    fn sync_creates_nodes_and_edge() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let structural = sync(&mut flow, &model, true);
        assert!(structural);
        assert!(flow.node_content_mut("main").is_some());
        assert!(flow.node_content_mut("abc123").is_some());
        // One edge main -> abc123.
        assert_eq!(flow.edges().len(), 1);
    }

    #[test]
    fn sync_idempotent() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        let first = sync(&mut flow, &model, true);
        assert!(first);
        let node_count = flow.nodes().count();
        let edge_count = flow.edges().len();

        // Applying the same model again adds nothing structural.
        let second = sync(&mut flow, &model, true);
        assert!(!second);
        assert_eq!(flow.nodes().count(), node_count);
        assert_eq!(flow.edges().len(), edge_count);
    }

    #[test]
    fn sync_preserves_selection() {
        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.select_node("abc123");
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );

        // Re-sync after a non-structural change (e.g. a tool call added).
        let mut model2 = model;
        if let Some(a) = model2.agents.get_mut("abc123") {
            a.output_tokens += 100;
        }
        sync(&mut flow, &model2, true);
        assert_eq!(
            flow.selected_nodes().next().map(|n| n.id.clone()),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn edge_animation_follows_status() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        // Running subagent -> animated edge.
        let edge_id = edge_id("abc123");
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(true));

        // The edge content mirrors running-ness (drives the distinct color).
        assert!(flow.edge_content_mut(&edge_id).unwrap().running);

        // Mark done, re-sync -> no longer animated, color back to default.
        if let Some(a) = model.agents.get_mut("abc123") {
            a.status = AgentStatus::Done;
        }
        sync(&mut flow, &model, true);
        let animated = flow
            .edges()
            .iter()
            .find(|e| e.id == edge_id)
            .map(|e| e.animated);
        assert_eq!(animated, Some(false));
        assert!(!flow.edge_content_mut(&edge_id).unwrap().running);
    }

    #[test]
    fn graph_is_structurally_read_only() {
        use rataflow::Reconnectable;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);

        for node in flow.nodes() {
            assert!(node.selectable, "nodes stay selectable (detail panel)");
            assert!(node.draggable, "nodes stay draggable (manual arranging)");
            assert!(!node.deletable, "nodes must not be deletable");
            assert!(!node.connectable, "nodes must not start connections");
        }
        for edge in flow.edges() {
            assert!(!edge.selectable, "edges carry no selectable meaning");
            assert!(!edge.deletable);
            assert_eq!(edge.reconnectable, Reconnectable::None);
        }
    }

    #[test]
    fn manual_mode_local_placement_moves_nothing_existing() {
        let mut model = model_with_subagent();
        let mut flow = new_flow();
        // Initial layout (camera engaged).
        sync(&mut flow, &model, true);
        let main_pos = flow.node("main").unwrap().position;
        let first_sub = flow.node("abc123").unwrap().position;

        // Camera now Manual: a second subagent arrives, relayout deferred.
        let meta2 = SubagentMeta {
            agent_type: Some("guide".into()),
            description: None,
            tool_use_id: Some("ag2".into()),
            stopped_by_user: None,
        };
        model.apply_meta("def456", None, &meta2);
        let structural = sync(&mut flow, &model, false);
        assert!(structural);

        // Nothing existing moved...
        assert_eq!(flow.node("main").unwrap().position, main_pos);
        assert_eq!(flow.node("abc123").unwrap().position, first_sub);
        // ...and the newcomer landed below its parent, not at the origin.
        let new_pos = flow.node("def456").unwrap().position;
        assert!(new_pos.y > main_pos.y, "child placed below parent");
        assert_ne!((new_pos.x, new_pos.y), (0.0, 0.0));
    }

    #[test]
    fn cards_render_into_buffer() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget;

        let model = model_with_subagent();
        let mut flow = new_flow();
        sync(&mut flow, &model, true);
        flow.request_fit_view();

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        (&mut flow).render(area, &mut buf);

        let mut text = String::new();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                text.push_str(buf[(x, y)].symbol());
            }
            text.push('\n');
        }
        assert!(
            text.contains("claude"),
            "main card title missing from render:\n{text}"
        );
        assert!(
            text.contains("guide"),
            "subagent card title missing from render:\n{text}"
        );
    }
}
