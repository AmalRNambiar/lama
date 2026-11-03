//! [`AgentEdge`]: the `EdgeContent` for parent→agent edges.
//!
//! Wraps the built-in `StepEdge` routing and colors the edge green while the
//! target agent is running ("alive") — so liveness reads on the graph structure
//! at any zoom. The agent's tool detail lives in its chips + detail panel, not
//! on the edge (no label).

use rataflow::{EdgeContent, EdgePathContext, EdgeRenderContext, EdgeStyle, Path, StepEdge};
use ratatui::buffer::Buffer;
use ratatui::style::Style;

// (Running edges use `palette.success` at render time — green = alive, the
// same presence language as the node glyphs. Resolved in `render`, not a
// const, so theme switches carry through.)

/// Step-routed parent edge with a distinct running color (no label).
#[derive(Debug, Default, Clone)]
pub struct AgentEdge {
    inner: StepEdge,
    /// Mirrored from the target agent's status by the graph sync; switches the
    /// edge chars to the palette's success green ("alive").
    pub running: bool,
}

impl EdgeContent for AgentEdge {
    fn compute_path(&self, ctx: &EdgePathContext) -> Path {
        self.inner.compute_path(ctx)
    }

    fn render(&self, ctx: &EdgeRenderContext, buf: &mut Buffer) {
        // Running edges get their distinct green so liveness survives overlapping
        // paths. No selection guard: edges are built non-selectable
        // (`with_selectable(false)` in graph sync), so `ctx.selected` is never
        // true here. No label — tool detail lives in the child's chips + panel.
        let style = if self.running {
            // Green in the classic skin; the neon cycle in the cyberpunk one.
            let color = if crate::ui::fx::motion() {
                crate::ui::fx::neon_cycle(2000, 0)
            } else {
                ctx.theme.palette().success
            };
            EdgeStyle::default().with_stroke_style(Style::default().fg(color))
        } else {
            EdgeStyle::default()
        };
        ctx.render_path(&style, None, buf);
    }
}
