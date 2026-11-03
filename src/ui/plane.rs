//! Rendering for the shared plane: every live session on one canvas under a
//! project hub, one timeline lane per session, and one status bar.
//!
//! The canvas is the host app's flow, drawn by the same `render_canvas` a lone
//! session uses. The detail panel and the info overlay borrow the session
//! that owns the selected node.

// The lamaverse is drawn only by the native frontend; the browser's core
// build compiles this without calling it.
#![cfg_attr(not(feature = "native"), allow(dead_code))]

use chrono::Utc;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use super::{fx, panel, render_canvas, render_help, render_info, task, truncate};
use crate::provider::Provider;
use crate::state::plane::Plane;
use crate::state::session::{AgentStatus, ToolState};
use crate::state::{App, Transport};

const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Seconds of activity per lane column.
const LANE_BUCKET_SECS: i64 = 5;

fn mark(p: Option<Provider>) -> (&'static str, ratatui::style::Color) {
    match p {
        Some(Provider::Claude) => ("❋", fx::provider_color(true)),
        Some(Provider::Codex) => ("❂", fx::provider_color(false)),
        None => ("✦", fx::palette().subtle),
    }
}

/// Most lanes the timeline shows; the universe keeps its busiest projects.
const MAX_LANES: usize = 6;

/// One timeline lane: a session (project plane) or a project (universe).
struct Lane {
    label: String,
    mark: &'static str,
    color: ratatui::style::Color,
    members: Vec<usize>,
}

fn lanes(plane: &Plane, apps: &[App]) -> Vec<Lane> {
    // A task's conductor lives in its panel, not on the timeline.
    let shown = plane.on_canvas(apps);
    if plane.window.is_none() {
        return shown
            .into_iter()
            .map(|i| {
                let provider = apps[i].session.provider();
                let (mark, color) = mark(provider);
                Lane {
                    label: provider
                        .map(Provider::name)
                        .unwrap_or("session")
                        .to_string(),
                    mark,
                    color,
                    members: vec![i],
                }
            })
            .collect();
    }
    let mut by_group: Vec<(Option<chrono::DateTime<Utc>>, Lane)> = Vec::new();
    for (g, group) in plane.groups.iter().enumerate() {
        let members: Vec<usize> = shown
            .iter()
            .copied()
            .filter(|&i| plane.group_of(i) == g)
            .collect();
        if members.is_empty() {
            continue;
        }
        let last = members
            .iter()
            .filter_map(|&i| apps[i].session.last_activity)
            .max();
        // One provider's colour when the project is all theirs, violet when mixed.
        let first = apps[members[0]].session.provider();
        let color = if members.iter().all(|&i| apps[i].session.provider() == first) {
            mark(first).1
        } else {
            fx::violet()
        };
        let lane = Lane {
            label: group.name.clone(),
            mark: "◈",
            color,
            members,
        };
        by_group.push((last, lane));
    }
    by_group.sort_by_key(|(last, _)| std::cmp::Reverse(*last));
    by_group
        .into_iter()
        .take(MAX_LANES)
        .map(|(_, l)| l)
        .collect()
}

/// What the frontend draws over the canvas once it has rendered — the
/// session consoles, which hold their place on the canvas as nodes but are
/// drawn by the frontend. Called with the canvas area.
pub type Overlay<'a> = &'a mut dyn FnMut(&mut Frame, Rect, &Plane);

/// Render one frame of the plane.
pub fn draw(frame: &mut Frame, plane: &mut Plane, apps: &mut [App], overlay: Option<Overlay>) {
    let area = frame.area();
    let lanes = lanes(plane, apps);
    let [canvas_area, lanes_area, status_area] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(lanes.len().max(1) as u16 + 5),
        Constraint::Length(1),
    ])
    .areas(area);

    // A task takes the far left: its intake, conductor and tasks beside the
    // lamaverse.
    plane.task_hits = Default::default();
    let canvas_area = match plane.task.as_ref().filter(|_| plane.window.is_some()) {
        Some(m) => match task::split(canvas_area) {
            (Some(panel), rest) => {
                let team = task::roster(m, apps);
                let mut hits = Default::default();
                task::render_panel(frame.buffer_mut(), panel, m, &team, &mut hits);
                plane.task_hits = hits;
                rest
            }
            (None, rest) => rest,
        },
        None => canvas_area,
    };

    // The universe's projects ride a strip over the canvas.
    let canvas_area = if plane.window.is_some() {
        let [strip, rest] =
            Layout::vertical([Constraint::Length(STRIP_H), Constraint::Fill(1)]).areas(canvas_area);
        render_strip(frame.buffer_mut(), strip, plane);
        plane.strip_area = Some(strip);
        rest
    } else {
        plane.strip_area = None;
        plane.strip_hits.clear();
        canvas_area
    };

    // Same split as a lone session: the panel is what you read, the canvas
    // keeps the selected card in view.
    let selection = plane.selection();
    let (flow_area, panel_area) = if selection.is_some() {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(30), Constraint::Percentage(70)])
                .areas(canvas_area);
        (left, Some(right))
    } else {
        (canvas_area, None)
    };

    render_canvas(frame, flow_area, &mut plane.host, selection.is_none());
    if let Some(id) = plane.host.pending_center.take() {
        plane.host.center_node(&id, false);
    }
    if plane.window.is_some() {
        // The task's planned cards, over the places the canvas keeps for them.
        task::render_ghosts(frame.buffer_mut(), flow_area, plane);
    }
    if let Some(overlay) = overlay {
        overlay(frame, flow_area, plane);
    }
    if plane.window.is_some() {
        let cards = plane.card_rects();
        let asking: Vec<String> = plane
            .asking
            .iter()
            .filter_map(|sid| plane.main_card(apps, sid))
            .collect();
        let cheers: Vec<String> = plane
            .mascot
            .take_cheers()
            .iter()
            .filter_map(|sid| plane.main_card(apps, sid))
            .collect();
        plane.mascot.errands(asking, cheers);
        plane.mascot.frame(frame.buffer_mut(), flow_area, &cards);
    }
    if plane.window.is_some() {
        // New agents arrive and messages fly over the canvas and its
        // consoles; the top line and the log drawer go over them.
        task::render_arrivals(frame.buffer_mut(), flow_area, plane, apps);
        task::render_messages(frame.buffer_mut(), flow_area, plane, apps);
        render_breadcrumb(frame, flow_area, plane);
        if let Some(m) = plane.task.as_ref().filter(|m| m.log_open) {
            let mut hits = std::mem::take(&mut plane.task_hits);
            task::render_log(frame.buffer_mut(), flow_area, m, &mut hits);
            plane.task_hits = hits;
        }
    }

    // The panel reads the owning session; its scroll lives on the host, where
    // the keys land, so hand it over and take it back.
    if let (Some(panel_area), Some((i, id))) = (panel_area, selection.as_ref())
        && let Some(app) = apps.get_mut(*i)
    {
        app.detail_scroll = plane.host.detail_scroll;
        app.detail_follow = plane.host.detail_follow;
        panel::render(frame, panel_area, app, id);
        plane.host.detail_scroll = app.detail_scroll;
        plane.host.detail_follow = app.detail_follow;
    }

    render_lanes(frame, lanes_area, apps, &lanes);
    render_status(frame, status_area, plane, apps);

    if plane.host.show_help {
        render_help(frame, area, &plane.host.flow.theme.palette());
    }
    if plane.host.show_info {
        let owner = selection
            .map(|(i, _)| i)
            .or_else(|| plane.shown(apps).first().copied());
        if let Some(app) = owner.and_then(|i| apps.get(i)) {
            render_info(frame, area, app);
        }
    }
}

/// Rows the project strip takes: one boxed chip high.
const STRIP_H: u16 = 3;

/// The project strip: a boxed chip per project with sessions on the
/// canvas, live first — its name on the top border in its own colour, how
/// many agents and how many live inside — then one `◇ N quiet` chip for the
/// rest. The lit project's chip is filled with its colour; a click lights a
/// project up or puts it out.
fn render_strip(buf: &mut ratatui::buffer::Buffer, area: Rect, plane: &mut Plane) {
    use crate::state::plane::Chip;
    use unicode_width::UnicodeWidthStr;
    let palette = fx::palette();
    let back = fx::mix(palette.surface, palette.canvas_bg, 0.55);
    let bg = Style::default().bg(back);
    let bold = Modifier::BOLD;
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(bg);
        }
    }
    plane.strip_hits.clear();
    if area.height < STRIP_H {
        return;
    }
    let (top, mid) = (area.y, area.y + 1);
    let w_of = |t: &str| UnicodeWidthStr::width(t) as u16;
    // A rounded box, its border in `line`, its inside filled with `fill`.
    let frame = |buf: &mut ratatui::buffer::Buffer, x: u16, w: u16, line: Color, fill: Color| {
        let r = Rect::new(x, area.y, w, STRIP_H);
        for y in r.top()..r.bottom() {
            for cx in r.left()..r.right() {
                buf[(cx, y)].set_char(' ').set_bg(fill);
            }
        }
        for (cx, cy, ch) in crate::ui::nodes::perimeter(r) {
            buf[(cx, cy)]
                .set_char(ch)
                .set_style(Style::default().fg(line).bg(fill));
        }
    };

    let quiet = plane.rail.iter().filter(|r| !r.open).count();
    let quiet_chip = if plane.expand_all {
        " ◇ fold quiet ".to_string()
    } else {
        format!(" ◇ {quiet} quiet ")
    };
    let show_quiet = quiet > 0 || plane.expand_all;
    // The theme picker, boxed at the strip's right end: a swatch per theme,
    // the one in use lit. A theme pinned by LAMA_THEME shows no picker.
    const THEMES: [fx::Skin; 2] = [fx::Skin::Cyberpunk, fx::Skin::Standard];
    let picker_w = if fx::skin_pinned() {
        0
    } else {
        THEMES.iter().map(|s| w_of(s.name()) + 4).sum::<u16>() + 3
    };
    let reserve = picker_w + if show_quiet { w_of(&quiet_chip) + 4 } else { 3 };
    let mut x = area.x + 1;
    let open: Vec<_> = plane.rail.iter().filter(|r| r.open).cloned().collect();
    for (k, item) in open.iter().enumerate() {
        let tint = fx::project_color(&item.name);
        let lit = plane.spot == Some(item.group);
        let title = format!(" ◈ {} ", truncate(&item.name, 18));
        let task = if item.on_task > 0 {
            format!(" ◈ {} on task", item.on_task)
        } else {
            String::new()
        };
        let count = format!("{} agents · ", item.agents);
        let live = if item.live > 0 {
            format!("● {} live", item.live)
        } else {
            "○ idle".to_string()
        };
        let inner = w_of(&item.marks) + 1 + w_of(&count) + w_of(&live) + w_of(&task);
        let w = inner.max(w_of(&title) + 2) + 4;
        if x + w + reserve > area.right() {
            let more = format!("+{}", open.len() - k);
            buf.set_stringn(x, mid, &more, 4, bg.fg(palette.subtle).add_modifier(bold));
            x += w_of(&more) + 1;
            break;
        }
        let (line, fill) = if lit {
            (tint, fx::mix(palette.canvas_bg, tint, 0.22))
        } else {
            (fx::mix(tint, palette.canvas_bg, 0.45), back)
        };
        frame(buf, x, w, line, fill);
        let inside = Style::default().bg(fill);
        let title_style = if lit {
            Style::default()
                .bg(tint)
                .fg(palette.canvas_bg)
                .add_modifier(bold)
        } else {
            inside.fg(tint).add_modifier(bold)
        };
        buf.set_stringn(x + 1, top, &title, (w - 2) as usize, title_style);
        let mut cx = x + 2;
        let marks_color = match item.marks.as_str() {
            "❋" => fx::provider_color(true),
            "❂" => fx::provider_color(false),
            _ => fx::violet(),
        };
        cx = buf
            .set_stringn(
                cx,
                mid,
                format!("{} ", item.marks),
                4,
                inside.fg(marks_color).add_modifier(bold),
            )
            .0;
        cx = buf
            .set_stringn(cx, mid, &count, 20, inside.fg(palette.text))
            .0;
        let live_style = if item.live > 0 {
            inside.fg(palette.success).add_modifier(bold)
        } else {
            inside.fg(palette.muted)
        };
        cx = buf.set_stringn(cx, mid, &live, 12, live_style).0;
        buf.set_stringn(
            cx,
            mid,
            &task,
            16,
            inside.fg(palette.accent).add_modifier(bold),
        );
        plane
            .strip_hits
            .push((Rect::new(x, area.y, w, STRIP_H), Chip::Project(item.group)));
        x += w + 1;
    }
    if show_quiet {
        let w = w_of(&quiet_chip);
        if x + 1 + w < area.right() {
            let look = Style::default()
                .bg(fx::mix(palette.muted, palette.canvas_bg, 0.55))
                .fg(palette.subtle);
            buf.set_stringn(x + 1, mid, &quiet_chip, w as usize, look);
            plane
                .strip_hits
                .push((Rect::new(x + 1, area.y, w, STRIP_H), Chip::Quiet));
            x += w + 2;
        }
    }
    let mut right = area.right();
    if picker_w > 0 && x + picker_w < right {
        let px = right - picker_w;
        frame(
            buf,
            px,
            picker_w - 1,
            fx::mix(palette.subtle, back, 0.4),
            back,
        );
        buf.set_stringn(
            px + 1,
            top,
            " ◐ theme ",
            (picker_w - 3) as usize,
            bg.fg(palette.subtle).add_modifier(bold),
        );
        let mut cx = px + 2;
        for skin in THEMES {
            let on = fx::skin() == skin;
            let w = w_of(skin.name()) + 3;
            let name = if on {
                bg.fg(palette.text).add_modifier(bold)
            } else {
                bg.fg(palette.muted)
            };
            buf.set_stringn(
                cx,
                mid,
                if on { "●" } else { "○" },
                1,
                bg.fg(fx::swatch(skin)),
            );
            buf.set_stringn(cx + 2, mid, skin.name(), w as usize, name);
            plane
                .strip_hits
                .push((Rect::new(cx, area.y, w, STRIP_H), Chip::Theme(skin)));
            cx += w + 1;
        }
        right = px;
    }
    let hint = "click lights · drag a card anywhere · r tidy ";
    let hw = w_of(hint);
    if x + hw + 2 < right {
        buf.set_stringn(right - hw, mid, hint, hw as usize, bg.fg(palette.muted));
    }
}

/// Where the camera is, over the canvas's top-left corner.
/// Zoomed into a project, it also carries the `＋ new session` button.
fn render_breadcrumb(frame: &mut Frame, area: Rect, plane: &mut Plane) {
    let palette = fx::palette();
    let style = Style::default()
        .bg(fx::mix(palette.accent, palette.surface, 0.82))
        .fg(palette.text)
        .add_modifier(Modifier::BOLD);
    let project = plane.focus.and_then(|g| plane.groups.get(g));
    let label = match project {
        Some(group) => format!(" ✦ lamaverse › ◈ {} ", group.name),
        None => " ✦ lamaverse · home ".to_string(),
    };
    let has_project = project.is_some();
    let room = area.width.saturating_sub(2);
    let (mut end, _) =
        frame
            .buffer_mut()
            .set_stringn(area.x + 1, area.y, &label, room as usize, style);
    plane.new_button = None;
    let button = " ＋ new session ";
    let width = unicode_width::UnicodeWidthStr::width(button) as u16;
    if has_project && end + 1 + width < area.right() {
        let x = end + 1;
        let look = Style::default()
            .bg(palette.accent)
            .fg(palette.canvas_bg)
            .add_modifier(Modifier::BOLD);
        frame
            .buffer_mut()
            .set_stringn(x, area.y, button, width as usize, look);
        plane.new_button = Some(Rect::new(x, area.y, width, 1));
        end = x + width;
    }
    if let Some(m) = &plane.task {
        // Short of the minimap's corner, which sits on the same row.
        let stop = area.right().saturating_sub(2);
        plane.task_hits.log_chip = task::render_crumb(frame.buffer_mut(), end + 2, area.y, stop, m);
    }
}

/// The newest tool call in a session: (start, agent title, call name, summary, in flight).
fn latest_call(app: &App) -> Option<(chrono::DateTime<Utc>, String, String, String, bool)> {
    let mut best: Option<(chrono::DateTime<Utc>, String, String, String, bool)> = None;
    for id in app.session.spawn_order() {
        let Some(a) = app.session.agent(id) else {
            continue;
        };
        let Some(call) = a.tool_calls().last() else {
            continue;
        };
        let Some(ts) = call.ts else { continue };
        if best.as_ref().is_none_or(|b| ts > b.0) {
            let title = a
                .agent_type
                .clone()
                .unwrap_or_else(|| a.kind.default_label().to_string());
            let summary = call.summary.clone().unwrap_or_default();
            let live = call.state == ToolState::Pending && a.status == AgentStatus::Running;
            best = Some((ts, title, call.name.clone(), summary, live));
        }
    }
    best
}

/// Shared timeline: the newest call from each lane on one line, then one
/// activity lane per session (or project) over the same window, ending now.
fn render_lanes(frame: &mut Frame, area: Rect, apps: &[App], lanes: &[Lane]) {
    let palette = fx::palette();
    let bg = Style::default().bg(palette.surface);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(bg.fg(palette.subtle))
        .style(bg);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width < 20 || inner.height < 3 {
        return;
    }
    // Log line: the newest call per lane, newest first.
    let mut latest: Vec<_> = lanes
        .iter()
        .filter_map(|lane| {
            lane.members
                .iter()
                .filter_map(|&i| latest_call(&apps[i]).map(|c| (apps[i].session.provider(), c)))
                .max_by_key(|(_, c)| c.0)
        })
        .collect();
    latest.sort_by_key(|(_, c)| std::cmp::Reverse(c.0));
    let mut spans = Vec::new();
    for (k, (provider, (ts, title, name, summary, live))) in latest.iter().enumerate() {
        if k > 0 {
            spans.push(Span::styled("  ·  ", bg.fg(palette.muted)));
        }
        let (m, color) = mark(*provider);
        let time = ts
            .with_timezone(&chrono::Local)
            .format("%H:%M:%S")
            .to_string();
        spans.push(Span::styled(format!("{time} "), bg.fg(palette.subtle)));
        spans.push(Span::styled(
            format!("{m} "),
            bg.fg(color).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::styled(
            format!("{} ", truncate(title, 16)),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
        ));
        let name_color = if *live { fx::yellow() } else { palette.subtle };
        spans.push(Span::styled(format!("{name} "), bg.fg(name_color)));
        let summary = summary.split_whitespace().collect::<Vec<_>>().join(" ");
        spans.push(Span::styled(truncate(&summary, 32), bg.fg(palette.text)));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(bg),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    let buf = frame.buffer_mut();
    for x in inner.left()..inner.right() {
        buf[(x, inner.y + 1)].set_char('─').set_style(bg.fg(fx::mix(
            palette.subtle,
            palette.surface,
            0.5,
        )));
    }

    // Lanes: tool-call starts per column, coloured by provider.
    const LABEL: u16 = 10;
    let track = inner.width.saturating_sub(LABEL + 1);
    let now = Utc::now();
    let hair = fx::mix(palette.muted, palette.surface, 0.5);
    for (row, lane) in lanes.iter().enumerate() {
        let y = inner.y + 2 + row as u16;
        if y >= inner.bottom() - 1 || track == 0 {
            break;
        }
        let color = lane.color;
        buf.set_stringn(
            inner.x + 1,
            y,
            format!(
                "{} {}",
                lane.mark,
                truncate(&lane.label, LABEL as usize - 3)
            ),
            LABEL as usize - 1,
            bg.fg(color).add_modifier(Modifier::BOLD),
        );
        let mut counts = vec![0u16; track as usize];
        let mut failed = vec![false; track as usize];
        for app in lane.members.iter().map(|&i| &apps[i]) {
            for id in app.session.spawn_order() {
                let Some(a) = app.session.agent(id) else {
                    continue;
                };
                for call in a.tool_calls.iter().rev() {
                    let Some(ts) = call.ts else { continue };
                    let ago = (now - ts).num_seconds() / LANE_BUCKET_SECS;
                    if ago >= track as i64 {
                        break;
                    }
                    let col = (track as i64 - 1 - ago.max(0)) as usize;
                    counts[col] += 1;
                    failed[col] |= call.state == ToolState::Err;
                }
            }
        }
        for (i, &n) in counts.iter().enumerate() {
            let cell = &mut buf[(inner.x + LABEL + i as u16, y)];
            if n == 0 {
                cell.set_char('▁').set_style(bg.fg(hair));
                continue;
            }
            let level = (n as usize * 2).min(7);
            let c = if failed[i] {
                palette.error
            } else {
                fx::mix(
                    palette.surface,
                    color,
                    0.45 + 0.55 * (n as f32 / 3.0).min(1.0),
                )
            };
            cell.set_char(BARS[level]).set_style(bg.fg(c));
        }
        // Playhead at the live edge.
        buf[(inner.x + LABEL + track - 1, y)]
            .set_char('┃')
            .set_style(bg.fg(palette.accent).add_modifier(Modifier::BOLD));
    }

    // Axis: the window's span, then the transport on the right.
    let y = inner.bottom() - 1;
    let mins = track as i64 * LANE_BUCKET_SECS / 60;
    let span_label = if mins > 0 {
        format!("-{mins}m")
    } else {
        format!("-{}s", track as i64 * LANE_BUCKET_SECS)
    };
    buf.set_stringn(inner.x + LABEL, y, &span_label, 8, bg.fg(palette.muted));
    let live = lanes
        .iter()
        .flat_map(|l| &l.members)
        .any(|&i| apps[i].transport() == Transport::Live);
    let (tag, tag_color) = if live {
        let dot = if (fx::now_ms() / 500).is_multiple_of(2) {
            "●"
        } else {
            "○"
        };
        (format!("{dot} LIVE"), palette.success)
    } else {
        ("■ quiet".to_string(), palette.subtle)
    };
    let fill_from = inner.x + LABEL + span_label.len() as u16 + 1;
    let tag_x = inner.right().saturating_sub(tag.chars().count() as u16 + 1);
    for x in fill_from..tag_x.saturating_sub(1) {
        buf[(x, y)].set_char('┈').set_style(bg.fg(hair));
    }
    buf.set_stringn(
        tag_x,
        y,
        &tag,
        8,
        bg.fg(tag_color).add_modifier(Modifier::BOLD),
    );
}

/// One status bar for the whole plane: wordmark, transport, project, each
/// session's provider and size, how many agents are live.
fn render_status(frame: &mut Frame, area: Rect, plane: &Plane, apps: &[App]) {
    let palette = fx::palette();
    let bg = Style::default().bg(palette.surface);
    let shown = plane.shown(apps);
    let visible: Vec<(usize, &App)> = shown.iter().map(|&i| (i, &apps[i])).collect();
    let live_now = visible
        .iter()
        .any(|(_, a)| a.transport() == Transport::Live);
    let running: usize = visible
        .iter()
        .map(|(_, a)| {
            a.session
                .spawn_order()
                .filter_map(|id| a.session.agent(id))
                .filter(|x| x.status == AgentStatus::Running)
                .count()
        })
        .sum();

    let wordmark_bg = match fx::skin() {
        fx::Skin::Classic => palette.accent,
        fx::Skin::Cyberpunk | fx::Skin::Standard => fx::neon_cycle(6000, 0),
    };
    let (badge, badge_color) = if live_now {
        let dot = if (fx::now_ms() / 500).is_multiple_of(2) {
            "●"
        } else {
            "○"
        };
        (
            format!(" {dot} LIVE "),
            fx::glow(palette.success, fx::pulse(1400, 0) * 0.5),
        )
    } else {
        (" ■ IDLE ".to_string(), palette.subtle)
    };
    let mut left = vec![
        Span::styled(
            " lama ",
            Style::default()
                .bg(wordmark_bg)
                .fg(palette.canvas_bg)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ", bg),
        Span::styled(
            badge,
            bg.fg(palette.canvas_bg)
                .bg(badge_color)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if plane.window.is_some() {
        let projects: std::collections::HashSet<usize> =
            shown.iter().map(|&i| plane.group_of(i)).collect();
        left.push(Span::styled(
            format!("  ✦ {}", fx::glitch_text("lamaverse")),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            format!("   {} projects · {} sessions", projects.len(), shown.len()),
            bg.fg(palette.subtle),
        ));
    } else if let Some(group) = plane.groups.first() {
        left.push(Span::styled(
            format!("  {}", fx::glitch_text(&group.name)),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
        ));
    }
    for (_, app) in visible.iter().filter(|_| plane.window.is_none()) {
        let provider = app.session.provider();
        let (m, color) = mark(provider);
        left.push(Span::styled(
            format!("   {m} "),
            bg.fg(color).add_modifier(Modifier::BOLD),
        ));
        left.push(Span::styled(
            format!(
                "{} {} agents",
                provider.map(Provider::name).unwrap_or("session"),
                app.session.agent_count()
            ),
            bg.fg(palette.subtle),
        ));
    }
    if visible.is_empty() {
        left.push(Span::styled(
            "   waiting for a session…",
            bg.fg(palette.subtle),
        ));
    }
    left.push(Span::styled(
        format!("   {running} live"),
        bg.fg(palette.success),
    ));
    if let Some(usd) = plane.cost_today() {
        left.push(Span::styled(
            format!("   Σ {} today", crate::state::usage::fmt_usd(usd)),
            bg.fg(fx::yellow()),
        ));
    }
    if fx::motion() {
        left.push(Span::styled(
            format!("  {} ", fx::spinner()),
            bg.fg(fx::neon_cycle(2400, 0)),
        ));
        left.push(Span::styled(
            fx::equalizer(running),
            bg.fg(fx::neon_cycle(3000, 900)),
        ));
    }

    let quit = if cfg!(target_arch = "wasm32") {
        ""
    } else {
        " · q quit"
    };
    let sessions = if cfg!(target_arch = "wasm32") {
        ""
    } else {
        "h sessions · "
    };
    let hints = if plane.window.is_some() {
        format!(
            "t task · {sessions}p theme · [ ] light project · ⏎ zoom · o home · e quiet · r tidy · l lama · ? help{quit}"
        )
    } else {
        format!("tab agents · f follow · o overview · ? help{quit}")
    };
    let hints_cols = hints.chars().count() as u16;
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(hints_cols + 1)]).areas(area);
    frame.render_widget(Paragraph::new(Line::from(left)).style(bg), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, bg.fg(palette.muted))))
            .style(bg)
            .alignment(Alignment::Right),
        right_area,
    );
}
