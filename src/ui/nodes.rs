//! [`AgentNode`]: the `NodeContent` for an agent card.
//!
//! Renders a bordered card showing the agent type, a status glyph + color,
//! a truncated description, a tool count, the last tool name, and an output
//! token count. Visual state (status, tool tallies) is mirrored from the
//! domain model into the node via `flow.node_content_mut` on each sync, so the
//! card needs no back-reference to the `SessionModel`.

use rataflow::{NodeContent, NodeRenderContext};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Padding, Paragraph, Widget};

use chrono::{DateTime, Utc};

use crate::state::session::{AgentStatus, ToolState};

/// Fixed card dimensions for main / workflow nodes (world units). Sized for
/// the full HUD card at 1:1 zoom.
pub const MAIN_NODE_DIMS: (f64, f64) = (40.0, 11.0);
/// Fixed card dimensions for subagent nodes (world units).
pub const SUB_NODE_DIMS: (f64, f64) = (40.0, 11.0);
/// The project hub that roots the shared plane.
pub const HUB_NODE_DIMS: (f64, f64) = (64.0, 5.0);

/// From this on-screen size up a card draws the full HUD (live tool feed,
/// reasoning, gauges); below it, the compact card.
pub const HUD_MIN_WIDTH: u16 = 26;
/// See [`HUD_MIN_WIDTH`].
pub const HUD_MIN_HEIGHT: u16 = 8;

/// Below this on-screen size a card has no room for any text — it renders at
/// cell level instead (solid status-colored fill). Semantic zoom: zoomed out,
/// nodes show as status cells instead of empty bordered boxes.
pub const CELL_MIN_WIDTH: u16 = 10;
/// See [`CELL_MIN_WIDTH`].
pub const CELL_MIN_HEIGHT: u16 = 3;

/// Custom node content rendered as an agent card.
///
/// Plain `pub` fields: the graph layer reads them back and mutates them in
/// place during incremental sync (mirroring [`crate::state::session::AgentInfo`]).
#[derive(Debug, Clone)]
pub struct AgentNode {
    /// Title line — the agent type, which for the main agent is the provider's
    /// own name (`claude`, `codex`).
    pub title: String,
    /// Truncated description shown under the title.
    pub description: Option<String>,
    pub status: AgentStatus,
    /// Number of tool calls (for the `⚒ N tools` line).
    pub tool_count: usize,
    /// Name of the most recent tool call, if any.
    pub last_tool: Option<String>,
    pub output_tokens: u64,
    /// Interactive agents (main, forks) word `Running` as "active": we know
    /// there are recent entries, not that a task is executing.
    pub interactive: bool,
    /// What the HUD card shows beyond the compact card's summary.
    pub feed: Feed,
}

/// The live detail a full-size card draws: the newest calls, the latest
/// reasoning, timing and activity. Mirrored from the model at sync time.
#[derive(Debug, Clone, Default)]
pub struct Feed {
    pub provider: Option<crate::provider::Provider>,
    pub model: Option<String>,
    /// Newest first.
    pub calls: Vec<FeedCall>,
    pub reasoning: Option<String>,
    pub first_ts: Option<DateTime<Utc>>,
    /// Working time counted so far (closed gaps of open turns).
    pub busy: chrono::Duration,
    /// The last record of a turn still open, while the agent runs: the
    /// clock keeps counting from here.
    pub open_since: Option<DateTime<Utc>>,
    /// The reference time the feed was built at.
    pub now: Option<DateTime<Utc>>,
    /// Tick running durations off the wall clock instead of `now` — live
    /// sessions, where `now` is only refreshed once a second.
    pub wall_clock: bool,
    /// Tool-call starts per time bucket, oldest first.
    pub activity: Vec<u16>,
    /// Set only on the plane's project hub, which renders as a banner.
    pub hub: Option<HubInfo>,
    /// The session finished its turn and you haven't looked yet: the card
    /// blinks neon green.
    pub finished: bool,
    /// Context window `(used, size)` on the latest model call; set by the
    /// plane on a session's main card only.
    pub context: Option<(u64, u64)>,
    /// The session's cost so far in USD, when its model is priced.
    pub cost: Option<f64>,
    /// The session is one of a task's agents: a `◈` on its header tells
    /// it from your own sessions working in the same project.
    pub task: bool,
    /// The session owns its task in this project (the conductor created
    /// it), rather than a peer one of the owners asked for: `owner` on its
    /// header.
    pub owner: bool,
    /// Its task is the one picked in the TASKS box: the card lights up.
    pub picked: bool,
    /// A task's planned agent with no session yet: the node only holds its
    /// place in the zone, and the task overlay draws the card over it.
    pub ghost: bool,
    /// Another project is lit on the lamaverse's strip: the card fades back.
    pub dim: bool,
    /// The project a lamaverse session's main card belongs to, tagged on
    /// its header in the project's colour (its cards sit wherever you put
    /// them, so the tag says whose they are).
    pub project: Option<String>,
}

/// One row of the feed.
#[derive(Debug, Clone)]
pub struct FeedCall {
    pub name: String,
    pub summary: Option<String>,
    pub state: ToolState,
    pub start: Option<DateTime<Utc>>,
    pub end: Option<DateTime<Utc>>,
}

/// A hub banner: a project and the sessions under it, or the universe root
/// over every project.
#[derive(Debug, Clone, Default)]
pub struct HubInfo {
    pub project: String,
    pub branch: Option<String>,
    pub agents: usize,
    pub live: usize,
    pub tokens: u64,
    pub sessions: usize,
    /// Set on the universe root: how many projects hang from it.
    pub projects: Option<usize>,
    /// A quiet project in the universe, drawn without its sessions.
    pub collapsed: bool,
    /// A universe zone: only the frame and title tab are drawn, so the
    /// project's agents (laid out inside it) and the canvas show through.
    pub zone: bool,
    /// A universe tile: a quiet project folded to one line.
    pub tile: bool,
    /// Provider emblems present in the project, e.g. `❋❂`.
    pub marks: String,
    /// A session console's place on the canvas: the frontend draws the
    /// console over it; the node itself only holds the space.
    pub console: bool,
    pub last: Option<DateTime<Utc>>,
    /// What the project's sessions cost today, when any is priced.
    pub cost_today: Option<f64>,
    /// How many of the project's sessions are a task's agents.
    pub on_task: usize,
    /// A phase lane's header over a column of helpers: `project` is the
    /// phase, `agents` how many, `live` how many are running.
    pub lane: bool,
}

use crate::ui::truncate;

/// Compact human token count, e.g. `1.2k`, `34`, `2.0M`.
fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

impl NodeContent for AgentNode {
    fn render(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        self.draw(ctx, buf);
        if self.feed.dim {
            let back = ctx.theme.palette().canvas_bg;
            let fade = |c: Color| match c {
                Color::Reset => c,
                c => crate::ui::fx::mix(c, back, 0.7),
            };
            let area = ctx.area.intersection(buf.area);
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let cell = &mut buf[(x, y)];
                    let (fg, bg) = (fade(cell.fg), fade(cell.bg));
                    cell.set_fg(fg).set_bg(bg);
                }
            }
        }
    }
}

impl AgentNode {
    fn draw(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        let palette = ctx.theme.palette();
        let area = ctx.area;

        // Degenerate areas: a border needs at least 2x2; bail out gracefully.
        if area.width == 0 || area.height == 0 || self.feed.ghost {
            return;
        }
        if let Some(hub) = &self.feed.hub {
            if hub.console {
                // Drawn over by the frontend; a quiet backdrop until then.
                let bg = Style::default().bg(ctx.theme.palette().surface);
                for y in area.top()..area.bottom() {
                    for x in area.left()..area.right() {
                        buf[(x, y)].set_char(' ').set_style(bg);
                    }
                }
            } else if hub.zone {
                render_zone(hub, ctx, buf);
            } else if hub.tile {
                render_tile(hub, ctx, buf);
            } else if hub.lane {
                render_lane(hub, ctx, buf);
            } else {
                render_hub(hub, ctx, buf);
            }
            return;
        }

        // Single-source vocabulary + presence colors (cards/panel/inspect
        // share them; only the pulse override below is card-specific).
        let glyph_color = crate::ui::status_color(self.status, &palette);
        let status_text = crate::state::session::status_word(self.status, self.interactive);

        // Cell level (semantic zoom): too small for any text — a bordered card
        // carries no information here, so paint a solid status-colored block.
        if area.width < CELL_MIN_WIDTH || area.height < CELL_MIN_HEIGHT {
            let fill_color = if self.feed.finished {
                done_blink()
            } else {
                glyph_color
            };
            let mut fill = Style::default().bg(fill_color);
            if ctx.selected {
                fill = fill.add_modifier(Modifier::REVERSED);
            }
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    buf[(x, y)].set_char(' ').set_style(fill);
                }
            }
            return;
        }

        if area.width >= HUD_MIN_WIDTH && area.height >= HUD_MIN_HEIGHT {
            self.render_hud(ctx, buf);
            return;
        }

        // Selected cards throb pink; running ones cycle the neon stops, each
        // card on its own phase so a busy swarm shimmers instead of blinking.
        let phase = crate::ui::fx::hash_str(&self.title) % 4000;
        let running = self.status == AgentStatus::Running;
        let border_color = if ctx.selected {
            crate::ui::fx::glow(palette.accent, crate::ui::fx::pulse(1200, 0) * 0.6)
        } else if self.feed.finished {
            done_blink()
        } else if running && crate::ui::fx::motion() {
            crate::ui::fx::neon_cycle(4000, phase)
        } else {
            palette.muted
        };
        let bg_style = Style::default().bg(self.surface(&palette));
        let border_style = bg_style.fg(border_color);

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .style(bg_style);

        // Only pad when there's horizontal room to spare.
        if area.width >= 4 {
            block = block.padding(Padding::horizontal(1));
        }

        let inner = block.inner(area);
        block.render(area, buf);
        if running {
            crate::ui::fx::render_border_spark(buf, area, border_color, phase);
        }

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        // Pulse: alive agents breathe on the shared animation clock (~1s
        // cycle at the default 120ms phase step) — the wide-shot heartbeat.
        let glyph = if self.status == AgentStatus::Running && (ctx.animation_phase / 4) % 2 == 1 {
            '○'
        } else {
            self.status.glyph()
        };
        let inner_w = inner.width as usize;

        // Title row: status glyph + agent type, bold.
        let task = if self.feed.task { "◈ " } else { "" };
        let title_budget = inner_w.saturating_sub(2 + width(task) as usize); // glyph + space
        let title = truncate(&self.title, title_budget);
        let title_line = Line::from(vec![
            Span::styled(format!("{glyph} "), bg_style.fg(glyph_color)),
            Span::styled(
                task,
                bg_style.fg(palette.accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                title,
                bg_style.fg(palette.text).add_modifier(Modifier::BOLD),
            ),
        ]);

        // Build the candidate lines in priority order.
        let mut lines: Vec<Line> = Vec::new();
        lines.push(title_line);

        if let Some(desc) = self.description.as_ref().filter(|d| !d.is_empty()) {
            let desc = truncate(desc, inner_w);
            lines.push(Line::from(Span::styled(desc, bg_style.fg(palette.subtle))));
        }

        // Tools row: "⚒ N · last_tool".
        let tools_text = if let Some(last) = self.last_tool.as_ref() {
            let prefix = format!("⚒ {} · ", self.tool_count);
            let budget =
                inner_w.saturating_sub(unicode_width::UnicodeWidthStr::width(prefix.as_str()));
            format!("{prefix}{}", truncate(last, budget))
        } else {
            format!("⚒ {} tools", self.tool_count)
        };
        lines.push(Line::from(Span::styled(
            tools_text,
            bg_style.fg(palette.accent),
        )));

        // Footer row: status word + token count, separated to the edges.
        let tokens = fmt_tokens(self.output_tokens);
        let footer = if self.output_tokens > 0 {
            Line::from(vec![
                Span::styled(status_text, bg_style.fg(glyph_color)),
                Span::styled(format!("  {tokens} tok"), bg_style.fg(palette.muted)),
            ])
        } else {
            Line::from(Span::styled(status_text, bg_style.fg(glyph_color)))
        };
        lines.push(footer);

        // Each row is one cell tall, stacked from the top; render only what fits.
        for (i, line) in lines.into_iter().enumerate() {
            if i as u16 >= inner.height {
                break;
            }
            let rect = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
            Paragraph::new(line).style(bg_style).render(rect, buf);
        }
    }
}

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Bright neon green, blinking hard: this agent finished and is waiting.
pub(crate) fn done_blink() -> ratatui::style::Color {
    use ratatui::style::Color;
    if !crate::ui::fx::motion() || (crate::ui::fx::now_ms() / 350).is_multiple_of(2) {
        Color::Rgb(40, 255, 120)
    } else {
        Color::Rgb(0, 72, 36)
    }
}

/// How long a card's border flashes after its agent starts a tool call.
const TOOL_FLASH_MS: i64 = 700;

/// The card's border cells clockwise from the top-left corner, with the
/// rounded glyph each one takes.
pub(crate) fn perimeter(area: Rect) -> Vec<(u16, u16, char)> {
    let (l, t, r, b) = (area.left(), area.top(), area.right() - 1, area.bottom() - 1);
    let mut cells = Vec::with_capacity(2 * (area.width + area.height) as usize);
    for x in l..=r {
        let ch = if x == l {
            '╭'
        } else if x == r {
            '╮'
        } else {
            '─'
        };
        cells.push((x, t, ch));
    }
    for y in t + 1..b {
        cells.push((r, y, '│'));
    }
    for x in (l..=r).rev() {
        let ch = if x == r {
            '╯'
        } else if x == l {
            '╰'
        } else {
            '─'
        };
        cells.push((x, b, ch));
    }
    for y in (t + 1..b).rev() {
        cells.push((l, y, '│'));
    }
    cells
}

fn width(s: &str) -> u16 {
    unicode_width::UnicodeWidthStr::width(s) as u16
}

/// Write `text` at a card-relative cell, clipped short of the right border.
/// Returns the card-relative column after the last cell written.
fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) -> u16 {
    if y >= area.height || x >= area.width.saturating_sub(1) {
        return x;
    }
    let room = (area.width - 1 - x) as usize;
    let (end, _) = buf.set_stringn(area.x + x, area.y + y, text, room, style);
    end - area.x
}

/// Write `text` ending `pad` cells from the card's right edge.
fn put_right(buf: &mut Buffer, area: Rect, pad: u16, y: u16, text: &str, style: Style) {
    let x = area.width.saturating_sub(pad + width(text));
    put(buf, area, x, y, text, style);
}

fn fmt_secs(secs: f64) -> String {
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{secs:.0}s")
    } else if secs < 3600.0 {
        format!("{}m{:02}s", secs as u64 / 60, secs as u64 % 60)
    } else {
        format!("{}h{:02}m", secs as u64 / 3600, secs as u64 / 60 % 60)
    }
}

fn fmt_clock(secs: i64) -> String {
    let secs = secs.max(0);
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{:02}:{:02}", secs / 60, secs % 60)
    }
}

/// Split `text` into display-width chunks of `cols`, at most `lines` of them;
/// the last one ends in an ellipsis when the text runs on.
fn wrap(text: &str, cols: usize, lines: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut w = 0;
    for ch in text.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if w + cw > cols {
            if out.len() == lines {
                let last = out.last_mut().expect("non-empty");
                *last = truncate(last, cols.saturating_sub(1)) + "…";
                return out;
            }
            out.push(String::new());
            w = 0;
        }
        out.last_mut().expect("non-empty").push(ch);
        w += cw;
    }
    out
}

impl AgentNode {
    /// The card's backdrop: tinted pink while its task is the one picked.
    fn surface(&self, palette: &rataflow::Palette) -> Color {
        if self.feed.picked {
            crate::ui::fx::mix(palette.surface, palette.accent, 0.12)
        } else {
            palette.surface
        }
    }

    /// The full card: title and model on the top border, a live feed of the
    /// newest calls, the latest reasoning, and an activity sparkline, call
    /// count and token gauge on the bottom border.
    fn render_hud(&self, ctx: &NodeRenderContext, buf: &mut Buffer) {
        use crate::provider::Provider;
        use crate::ui::fx;

        let palette = ctx.theme.palette();
        let area = ctx.area;
        let (w, h) = (area.width, area.height);
        let bg = Style::default().bg(self.surface(&palette));
        let bold = Modifier::BOLD;
        let feed = &self.feed;
        let running = self.status == AgentStatus::Running;
        let now = if feed.wall_clock {
            Some(Utc::now())
        } else {
            feed.now
        };

        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(bg);
            }
        }

        // Border: neon cycling (tinted toward the provider) while alive, a
        // faint provider tint once quiet, a pink throb when selected.
        let tint = match feed.provider {
            Some(p) => fx::provider_color(p == Provider::Claude),
            None => palette.subtle,
        };
        let phase = fx::hash_str(&self.title) % 4000;
        let cells = perimeter(area);
        let per = cells.len() as u64;
        // A tool call just started: the border flashes and fades, so busy
        // agents visibly fire even zoomed out.
        let flash = feed
            .calls
            .first()
            .filter(|c| c.state == ToolState::Pending && running && fx::motion())
            .and_then(|c| Some((now? - c.start?).num_milliseconds()))
            .filter(|ms| (0..TOOL_FLASH_MS).contains(ms))
            .map(|ms| 0.75 * (1.0 - ms as f32 / TOOL_FLASH_MS as f32));
        for (i, &(x, y, ch)) in cells.iter().enumerate() {
            let fg = if ctx.selected {
                fx::glow(palette.accent, fx::pulse(1200, 0) * 0.6)
            } else if feed.finished {
                done_blink()
            } else if running && fx::motion() {
                fx::mix(
                    fx::neon_cycle(4000, phase + i as u64 * 4000 / per),
                    tint,
                    0.3,
                )
            } else if running {
                palette.success
            } else {
                fx::mix(tint, palette.surface, 0.55)
            };
            let fg = flash.map_or(fg, |f| fx::glow(fg, f));
            buf[(x, y)].set_char(ch).set_style(bg.fg(fg));
        }
        if running && !ctx.selected {
            fx::render_border_spark(buf, area, fx::neon_cycle(4000, phase), phase);
        }

        // Top border: status glyph, provider emblem, title — model on the right.
        let glyph = if running && (ctx.animation_phase / 4) % 2 == 1 {
            '○'
        } else {
            self.status.glyph()
        };
        let status_color = crate::ui::status_color(self.status, &palette);
        let mut x = put(
            buf,
            area,
            2,
            0,
            &format!(" {glyph} "),
            bg.fg(status_color).add_modifier(bold),
        );
        if let Some(p) = feed.provider {
            let mark = if p == Provider::Claude {
                "❋ "
            } else {
                "❂ "
            };
            x = put(buf, area, x, 0, mark, bg.fg(tint).add_modifier(bold));
        }
        if feed.task {
            x = put(
                buf,
                area,
                x,
                0,
                "◈ ",
                bg.fg(palette.accent).add_modifier(bold),
            );
        }
        let model = feed
            .model
            .as_deref()
            .map(|m| format!(" {} ", m.trim_start_matches("claude-")))
            .unwrap_or_default();
        // The project's tag, right of the model, while the title keeps room.
        let project = feed
            .project
            .as_deref()
            .map(|p| format!(" {} ", truncate(p, 14)))
            .filter(|p| w > 40 + width(p) + width(&model))
            .unwrap_or_default();
        let tag_w = if project.is_empty() {
            0
        } else {
            width(&project) + 1
        };
        // A task's owner says so after its name, while the name keeps room.
        let tag = if feed.owner { "· owner " } else { "" };
        let room = w.saturating_sub(x + 4 + width(&model) + tag_w) as usize;
        let tag = if room >= 8 + width(tag) as usize {
            tag
        } else {
            ""
        };
        let title_room = room - width(tag) as usize;
        let tx = put(
            buf,
            area,
            x,
            0,
            &format!("{} ", truncate(&self.title, title_room)),
            bg.fg(palette.text).add_modifier(bold),
        );
        if !tag.is_empty() {
            put(buf, area, tx, 0, tag, bg.fg(palette.accent));
        }
        if let Some(name) = feed.project.as_deref().filter(|_| !project.is_empty()) {
            let look = Style::default()
                .bg(fx::project_color(name))
                .fg(palette.canvas_bg)
                .add_modifier(bold);
            put_right(buf, area, 2, 0, &project, look);
        }
        if !model.is_empty() {
            put_right(buf, area, 2 + tag_w, 0, &model, bg.fg(palette.subtle));
        }

        // Description, and how long the agent has worked: the clock runs
        // while a turn is open and stops while it waits for you.
        let ticking = feed.open_since.is_some();
        let clock = match (feed.first_ts, feed.open_since, now) {
            (None, _, _) => String::new(),
            (Some(_), Some(since), Some(now)) => fmt_clock(
                (feed.busy + crate::state::session::working_gap(now - since)).num_seconds(),
            ),
            _ => fmt_clock(feed.busy.num_seconds()),
        };
        let desc = self.description.as_deref().unwrap_or("");
        let desc_room = w.saturating_sub(5 + width(&clock)) as usize;
        put(
            buf,
            area,
            2,
            1,
            &truncate(desc, desc_room),
            bg.fg(palette.subtle),
        );
        let clock_color = if running && ticking {
            fx::cyan()
        } else {
            palette.muted
        };
        put_right(buf, area, 2, 1, &clock, bg.fg(clock_color));

        // Feed: the call in flight (or "thinking" between calls), then history.
        let dim = fx::mix(palette.text, palette.surface, 0.25);
        let last_row = 3 + h.saturating_sub(7);
        let mut y = 3;
        let in_flight = feed
            .calls
            .first()
            .is_some_and(|c| c.state == ToolState::Pending);
        if running && !in_flight && y < last_row {
            put(
                buf,
                area,
                2,
                y,
                &fx::spinner().to_string(),
                bg.fg(fx::violet()),
            );
            let dots = ".".repeat(1 + (fx::now_ms() / 333 % 3) as usize);
            put(
                buf,
                area,
                4,
                y,
                &format!("thinking{dots}"),
                bg.fg(fx::violet()),
            );
            y += 1;
        }
        for call in &feed.calls {
            if y >= last_row {
                break;
            }
            let live_call = call.state == ToolState::Pending && running;
            let (mark, mark_style) = match call.state {
                ToolState::Ok => ("✓".to_string(), bg.fg(palette.success).add_modifier(bold)),
                ToolState::Err => ("✗".to_string(), bg.fg(palette.error).add_modifier(bold)),
                ToolState::Pending if live_call => (fx::spinner().to_string(), bg.fg(fx::cyan())),
                ToolState::Pending => ("…".to_string(), bg.fg(palette.muted)),
            };
            let end = if call.state == ToolState::Pending {
                now
            } else {
                call.end
            };
            let dur = match (call.start, end) {
                (Some(s), Some(e)) => fmt_secs((e - s).num_milliseconds().max(0) as f64 / 1000.0),
                _ => String::new(),
            };
            put(buf, area, 2, y, &mark, mark_style);
            let name_style = if live_call {
                bg.fg(palette.text).add_modifier(bold)
            } else {
                bg.fg(palette.subtle)
            };
            let nx = put(buf, area, 4, y, &truncate(&call.name, 14), name_style);
            let room = w.saturating_sub(nx + 1 + width(&dur) + 3) as usize;
            if let Some(summary) = &call.summary {
                let style = if live_call {
                    bg.fg(palette.text)
                } else {
                    bg.fg(dim)
                };
                let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
                put(buf, area, nx + 1, y, &truncate(&summary, room), style);
            }
            let dur_style = if live_call {
                bg.fg(fx::yellow())
            } else {
                bg.fg(palette.muted)
            };
            put_right(buf, area, 2, y, &dur, dur_style);
            y += 1;
        }

        // Latest reasoning, two lines.
        let sep_color = fx::mix(palette.muted, palette.surface, 0.4);
        put(
            buf,
            area,
            2,
            h - 4,
            &"┄".repeat(w.saturating_sub(4) as usize),
            bg.fg(sep_color),
        );
        put(
            buf,
            area,
            2,
            h - 3,
            "»",
            bg.fg(palette.accent).add_modifier(bold),
        );
        let text_style = if running {
            bg.fg(palette.text)
        } else {
            bg.fg(palette.subtle)
        };
        match &feed.reasoning {
            Some(r) => {
                // Reasoning is markdown; emphasis and code marks only cost cells here.
                let plain: String = r
                    .chars()
                    .filter(|c| !matches!(c, '*' | '`' | '#'))
                    .collect();
                let lines = wrap(plain.trim(), w.saturating_sub(7) as usize, 2);
                for (i, line) in lines.iter().enumerate() {
                    put(buf, area, 4, h - 3 + i as u16, line, text_style);
                }
                if running && (fx::now_ms() / 333).is_multiple_of(2) {
                    let last = lines.last().map(|l| width(l)).unwrap_or(0);
                    let ly = h - 4 + lines.len() as u16;
                    if 4 + last < w - 2 {
                        put(buf, area, 4 + last, ly, "▌", bg.fg(palette.accent));
                    }
                }
            }
            None => {
                let word = crate::state::session::status_word(self.status, self.interactive);
                put(buf, area, 4, h - 3, word, text_style);
            }
        }

        // Bottom border: activity sparkline, call count, then the context
        // gauge on a session's main card (the token meter elsewhere). The
        // gauge is fitted first; the sparkline takes what is left, so it
        // gives up columns first as the card narrows (zoom).
        if w >= 26 {
            let count = format!(" ⚒ {} ", self.tool_count);
            let room = w.saturating_sub(3 + width(&count));
            let gauge = match feed.context {
                Some(ctx) => context_gauge(ctx, feed.cost, room, &palette, running),
                None => token_meter(self.output_tokens, running, &palette),
            };
            let gauge_w: u16 = gauge.iter().map(|(t, _)| width(t)).sum();
            let spark_len = (room.saturating_sub(gauge_w) as usize).min(feed.activity.len());
            let max = feed.activity.iter().copied().max().unwrap_or(0).max(2) as f32;
            let skip = feed.activity.len() - spark_len;
            for (i, &n) in feed.activity.iter().skip(skip).enumerate() {
                let bar = BARS[((n as f32 / max) * 7.0).round().min(7.0) as usize];
                let color = if running && fx::motion() {
                    fx::neon_cycle(3000, i as u64 * 190)
                } else {
                    palette.muted
                };
                put(
                    buf,
                    area,
                    2 + i as u16,
                    h - 1,
                    &bar.to_string(),
                    bg.fg(color),
                );
            }
            let count_style = if running {
                bg.fg(palette.accent).add_modifier(bold)
            } else {
                bg.fg(palette.subtle)
            };
            put(buf, area, 2 + spark_len as u16, h - 1, &count, count_style);
            let mut x = w.saturating_sub(1 + gauge_w);
            for (text, color) in &gauge {
                x = put(buf, area, x, h - 1, text, bg.fg(*color));
            }
        }
    }
}

/// Output tokens on a gauge whose segments grow with the order of
/// magnitude: 1k → 2, 10k → 3 …
fn token_meter(tokens: u64, running: bool, palette: &rataflow::Palette) -> Vec<(String, Color)> {
    let on = if tokens == 0 {
        0
    } else {
        ((tokens as f64).log10().floor() as usize)
            .saturating_sub(1)
            .min(5)
    };
    let meter = format!(
        " {}{} {} ",
        "▰".repeat(on),
        "▱".repeat(5 - on),
        fmt_tokens(tokens)
    );
    let color = if running {
        crate::ui::fx::cyan()
    } else {
        palette.muted
    };
    vec![(meter, color)]
}

/// How full the context window is, `ctx ▰▰▰▱▱ 61%`, plus the session's cost
/// when known: green below 60%, yellow below 85%, red past it. The widest
/// form that fits `room` wins; the cost goes first, then the label, then
/// the bar.
fn context_gauge(
    (used, window): (u64, u64),
    cost: Option<f64>,
    room: u16,
    palette: &rataflow::Palette,
    running: bool,
) -> Vec<(String, Color)> {
    use crate::ui::fx;
    let fill = if window == 0 {
        0.0
    } else {
        (used as f64 / window as f64).clamp(0.0, 1.0)
    };
    let color = if fill < 0.60 {
        palette.success
    } else if fill < 0.85 {
        fx::yellow()
    } else {
        palette.error
    };
    let on = ((fill * 5.0).round() as usize).min(5);
    let bar = format!("{}{}", "▰".repeat(on), "▱".repeat(5 - on));
    let pct = format!("{:.0}%", fill * 100.0);
    let label = if running {
        palette.subtle
    } else {
        palette.muted
    };
    let usd = cost.map(crate::state::usage::fmt_usd);
    let mut forms = Vec::new();
    if let Some(usd) = &usd {
        forms.push(vec![
            (" ctx ".to_string(), label),
            (bar.clone(), color),
            (format!(" {pct}"), color),
            (format!(" {usd} "), palette.text),
        ]);
    }
    forms.push(vec![
        (" ctx ".to_string(), label),
        (bar.clone(), color),
        (format!(" {pct} "), color),
    ]);
    forms.push(vec![(format!(" {bar} {pct} "), color)]);
    forms.push(vec![(format!(" {pct} "), color)]);
    forms
        .into_iter()
        .find(|f| f.iter().map(|(t, _)| width(t)).sum::<u16>() <= room)
        .unwrap_or_default()
}

/// A plane hub: a banner naming the project (or the universe), its branch,
/// and how much is going on under it. Quiet hubs go dim; busy ones cycle neon.
fn render_hub(hub: &HubInfo, ctx: &NodeRenderContext, buf: &mut Buffer) {
    use crate::ui::fx;
    let palette = ctx.theme.palette();
    let area = ctx.area;
    let alive = hub.live > 0;
    let universe = hub.projects.is_some();
    // Too short for a frame (zoomed out): the name alone, on a tinted strip,
    // so a hub never reads as a blank bar.
    if area.height < 3 && area.width >= 8 {
        let strip = Style::default().bg(fx::mix(palette.accent, palette.surface, 0.7));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(strip);
            }
        }
        let glyph = if universe {
            "✦"
        } else if alive {
            "◈"
        } else {
            "◇"
        };
        let label = format!(" {glyph} {} ", hub.project);
        put(
            buf,
            area,
            0,
            area.height / 2,
            &label,
            strip.fg(palette.text).add_modifier(Modifier::BOLD),
        );
        return;
    }
    if area.width < 12 || area.height < 3 {
        let fill = Style::default().bg(if alive { palette.accent } else { palette.muted });
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(fill);
            }
        }
        return;
    }
    let bg = Style::default().bg(palette.surface);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(bg);
        }
    }
    let cells = perimeter(area);
    let per = cells.len() as u64;
    for (i, &(x, y, ch)) in cells.iter().enumerate() {
        let fg = if !(alive || universe) {
            fx::mix(palette.subtle, palette.surface, 0.4)
        } else if fx::motion() {
            fx::neon_cycle(6000, 6000 - i as u64 * 6000 / per)
        } else {
            palette.accent
        };
        buf[(x, y)].set_char(ch).set_style(bg.fg(fg));
    }

    let bold = Modifier::BOLD;
    let row = area.height / 2;
    let (glyph, name_color) = match (universe, alive) {
        (true, _) => ("✦ ", palette.text),
        (false, true) => ("◈ ", palette.text),
        (false, false) => ("◇ ", palette.subtle),
    };
    let mark_color = if alive || universe {
        fx::neon_cycle(2000, 0)
    } else {
        palette.subtle
    };
    let mut x = put(
        buf,
        area,
        2,
        row,
        glyph,
        bg.fg(mark_color).add_modifier(bold),
    );
    x = put(
        buf,
        area,
        x,
        row,
        &hub.project,
        bg.fg(name_color).add_modifier(bold),
    );

    let plural =
        |n: usize, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
    let info = match hub.projects {
        Some(p) => format!(
            "{} · {} · {} live · Σ {} tok",
            plural(p, "project", "projects"),
            plural(hub.sessions, "session", "sessions"),
            hub.live,
            fmt_tokens(hub.tokens)
        ),
        None if hub.collapsed => format!("{} · quiet", plural(hub.sessions, "session", "sessions")),
        None => format!(
            "{} · {} live · Σ {} tok",
            plural(hub.agents, "agent", "agents"),
            hub.live,
            fmt_tokens(hub.tokens)
        ),
    };
    // The stats outrank the branch: a long branch name gets cut, not them.
    if let Some(branch) = &hub.branch {
        let room = area.width.saturating_sub(x + 2 + 2 + width(&info) + 3) as usize;
        if room > 4 {
            let label = format!("⎇ {}", truncate(branch, room - 2));
            x = put(buf, area, x + 2, row, &label, bg.fg(fx::violet()));
        }
    }
    if x + 2 + width(&info) + 2 <= area.width {
        put_right(buf, area, 2, row, &info, bg.fg(palette.subtle));
    }
}

/// "3m", "2h", "4d" — how long ago, coarsely.
fn ago(ts: Option<DateTime<Utc>>) -> String {
    let Some(ts) = ts else {
        return String::new();
    };
    let secs = (Utc::now() - ts).num_seconds().max(0);
    match secs {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", secs / 60),
        3600..86_400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// A lit project's outline round its sessions: a dashed frame in the
/// project's colour, and its label line along the top — name and branch on
/// the left, who and how much on the right. Nothing else is written; the
/// node is see-through, so its cards and the canvas show.
fn render_zone(hub: &HubInfo, ctx: &NodeRenderContext, buf: &mut Buffer) {
    use crate::ui::fx;
    let palette = ctx.theme.palette();
    let area = ctx.area;
    let alive = hub.live > 0;
    let tint = fx::project_color(&hub.project);
    if area.width >= 4 && area.height >= 3 {
        let line = Style::default().fg(fx::mix(tint, palette.canvas_bg, 0.35));
        for &(x, y, ch) in &perimeter(area) {
            let ch = match ch {
                '─' => '┄',
                '│' => '┆',
                c => c,
            };
            if buf.area.contains(ratatui::layout::Position::new(x, y)) {
                buf[(x, y)].set_char(ch).set_style(line);
            }
        }
    }
    // Too small for words: a tinted strip where the label would be.
    if area.width < 4 || area.height < 2 {
        let fill = Style::default().bg(fx::mix(tint, palette.canvas_bg, 0.5));
        for x in area.left()..area.right() {
            buf[(x, area.top())].set_char(' ').set_style(fill);
        }
        return;
    }
    // A faint band under the label: it reads as the project's header, and
    // marks the row as content so the canvas rain never runs through it.
    let bg = Style::default().bg(fx::mix(palette.surface, palette.canvas_bg, 0.45));
    for x in area.left()..area.right() {
        buf[(x, area.top())].set_char(' ').set_style(bg);
    }
    let bold = Modifier::BOLD;
    let mark = if alive { " ◈ " } else { " ◇ " };
    let mark_color = tint;
    let mut x = put(buf, area, 0, 0, mark, bg.fg(mark_color).add_modifier(bold));
    x = put(
        buf,
        area,
        x,
        0,
        &format!("{} ", hub.project),
        bg.fg(palette.text).add_modifier(bold),
    );
    // The task's agents working here, beside your own sessions.
    if hub.on_task > 0 {
        let chip = format!(" ◈ {} on task ", hub.on_task);
        if x + width(&chip) + 2 < area.width {
            let look = Style::default()
                .bg(palette.accent)
                .fg(palette.canvas_bg)
                .add_modifier(bold);
            x = put(buf, area, x, 0, &chip, look) + 1;
        }
    }
    let dot = if (fx::now_ms() / 500).is_multiple_of(2) {
        "●"
    } else {
        "○"
    };
    let spent = hub
        .cost_today
        .map(|c| format!("· {} today ", crate::state::usage::fmt_usd(c)))
        .unwrap_or_default();
    let right = if alive {
        format!(
            " {} · {} agents · {dot} {} live {spent}",
            hub.marks, hub.agents, hub.live
        )
    } else {
        format!(
            " {} · {} agents · quiet {} {spent}",
            hub.marks,
            hub.agents,
            ago(hub.last)
        )
    };
    let right_x = area.width.saturating_sub(2 + width(&right));
    if let Some(branch) = &hub.branch {
        let room = right_x.saturating_sub(x + 4) as usize;
        if room > 4 {
            x = put(
                buf,
                area,
                x + 1,
                0,
                &format!("⎇ {} ", truncate(branch, room - 3)),
                bg.fg(fx::violet()),
            );
        }
    }
    if right_x > x {
        let color = if alive {
            palette.success
        } else {
            palette.subtle
        };
        put(buf, area, right_x, 0, &right, bg.fg(color));
    }
}

/// A universe tile: a quiet project, one line — click or `⏎` to open it.
/// A phase lane's header: `◇ CHECK 6 ⠹ 2`, the spine leaving its left end.
fn render_lane(hub: &HubInfo, ctx: &NodeRenderContext, buf: &mut Buffer) {
    let palette = ctx.theme.palette();
    let area = ctx.area;
    let mut spans = vec![
        Span::styled("◇ ", Style::default().fg(palette.muted)),
        Span::styled(
            hub.project.clone(),
            Style::default()
                .fg(palette.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", hub.agents),
            Style::default().fg(palette.subtle),
        ),
    ];
    if hub.live > 0 {
        spans.push(Span::styled(
            format!("  ● {}", hub.live),
            Style::default().fg(palette.success),
        ));
    }
    let rect = Rect::new(area.x, area.y, area.width, 1);
    Paragraph::new(Line::from(spans)).render(rect, buf);
}

fn render_tile(hub: &HubInfo, ctx: &NodeRenderContext, buf: &mut Buffer) {
    use crate::ui::fx;
    let palette = ctx.theme.palette();
    let area = ctx.area;
    let bg = Style::default().bg(palette.surface);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(bg);
        }
    }
    if area.width < 6 || area.height < 3 {
        put(
            buf,
            area,
            0,
            0,
            &format!("◇ {}", hub.project),
            bg.fg(palette.subtle),
        );
        return;
    }
    let border = if ctx.selected {
        palette.accent
    } else {
        palette.muted
    };
    for &(x, y, ch) in &perimeter(area) {
        buf[(x, y)].set_char(ch).set_style(bg.fg(border));
    }
    let row = area.height / 2;
    let x = put(buf, area, 2, row, "◇ ", bg.fg(palette.subtle));
    let x = put(
        buf,
        area,
        x,
        row,
        &hub.project,
        bg.fg(palette.subtle).add_modifier(Modifier::BOLD),
    );
    let info = format!("{} {} · {}", hub.marks, hub.sessions, ago(hub.last));
    let info_x = area.width.saturating_sub(2 + width(&info));
    if let Some(branch) = &hub.branch {
        let room = info_x.saturating_sub(x + 4) as usize;
        if room > 4 {
            let dim = fx::mix(fx::violet(), palette.surface, 0.4);
            put(
                buf,
                area,
                x + 1,
                row,
                &format!("⎇ {}", truncate(branch, room - 2)),
                bg.fg(dim),
            );
        }
    }
    if info_x > x {
        put(buf, area, info_x, row, &info, bg.fg(palette.muted));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_budget() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("abcde", 4), "abc…");
        assert_eq!(truncate("hello", 1), "…");
        assert_eq!(truncate("hello", 0), "");
    }

    #[test]
    fn truncate_handles_multibyte() {
        // Must not panic on a char boundary; counts columns not bytes.
        let s = "café crème brûlée";
        let out = truncate(s, 5);
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn truncate_measures_display_columns_not_chars() {
        use unicode_width::UnicodeWidthStr;
        // CJK chars are 2 columns wide: 6 chars = 12 columns must truncate to
        // fit a 10-column budget (chars-based counting would pass it through
        // and overflow the card).
        let s = "修复解析错误";
        let out = truncate(s, 10);
        assert!(out.width() <= 10, "{out:?} is {} columns", out.width());
        assert!(out.ends_with('…'));
        // And an exact fit is left alone.
        assert_eq!(truncate(s, 12), s);
    }

    #[test]
    fn token_formatting() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_500), "1.5k");
        assert_eq!(fmt_tokens(2_000_000), "2.0M");
    }

    #[test]
    fn glyphs_per_status() {
        assert_eq!(AgentStatus::Running.glyph(), '●');
        assert_eq!(AgentStatus::Done.glyph(), '✓');
        assert_eq!(AgentStatus::Failed.glyph(), '✗');
    }

    fn render_into(area: ratatui::layout::Rect) -> Buffer {
        use rataflow::Theme;
        use rataflow::types::Position;

        let node = AgentNode {
            title: "claude".into(),
            description: None,
            status: AgentStatus::Done,
            tool_count: 3,
            last_tool: Some("Bash".into()),
            output_tokens: 1200,
            interactive: false,
            feed: Feed::default(),
        };
        let ctx = NodeRenderContext {
            id: "main",
            area,
            selected: false,
            dragging: false,
            position_absolute: Position::new(0.0, 0.0),
            theme: Theme::default(),
            animation_phase: 0,
        };
        let mut buf = Buffer::empty(area);
        node.render(&ctx, &mut buf);
        buf
    }

    #[test]
    fn card_level_renders_text() {
        let area = ratatui::layout::Rect::new(0, 0, 24, 6);
        let buf = render_into(area);
        let text: String = (0..area.width).map(|x| buf[(x, 1)].symbol()).collect();
        assert!(
            text.contains("claude"),
            "card level must show title: {text}"
        );
    }

    #[test]
    fn cell_level_renders_status_fill() {
        use rataflow::Theme;
        use ratatui::style::Color;

        // Below the text threshold: solid status-colored fill, no border.
        let area = ratatui::layout::Rect::new(0, 0, 6, 2);
        let buf = render_into(area);
        let palette = Theme::default().palette();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &buf[(x, y)];
                assert_eq!(cell.symbol(), " ", "cell level draws no text/border");
                assert_eq!(cell.style().bg, Some(palette.accent)); // Done = calm accent
                assert_ne!(cell.style().bg, Some(Color::Reset));
            }
        }
    }
}
