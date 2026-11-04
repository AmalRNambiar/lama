//! Tasks on the lamaverse: the left panel (the intake that takes what you
//! hand over, the conductor that dispatches each task to its projects'
//! owners, the TASKS box listing how each goes), the planned cards waiting
//! beside their projects' sessions, each new agent's arrival at its card, the
//! messages flying between the agents' cards and their tasks' rows, and
//! the message log.
//!
//! Everything is drawn from [`TaskView`], plain data the frontend refills
//! each tick. Message flights and arrivals run on the task's own clock
//! ([`TaskView::now`]); the ambient motion (radar sweep, pulsing prompts)
//! on [`fx::now_ms`], so `LAMA_FX=off` stills it like every other effect.
//! Without motion a message skips its flight and sits by its receiver,
//! readable, for as long as it would have been on screen, an arrival is a
//! still highlight on the new card, and a planned card stands whole and
//! still from the start.

// The lamaverse is drawn only by the native frontend; the browser's core
// build compiles this without calling it.
#![cfg_attr(not(feature = "native"), allow(dead_code))]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use super::{fx, nodes, truncate};
use crate::provider::Provider;
use crate::state::App;
use crate::state::plane::Plane;
use crate::state::session::AgentStatus;
use crate::state::task::{
    ARRIVE_SECS, CONDUCTOR, ChoiceView, EVERYONE, FLASH_SECS, LINGER_SECS, MessageView, MsgKind,
    PlannedView, RowStatus, TASK, TRAVEL_SECS, TaskHits, TaskRow, TaskView,
};

/// Width of the task panel on the lamaverse's left.
pub(crate) const PANEL_W: u16 = 38;
/// The canvas keeps at least this many columns beside the panel; a narrower
/// terminal goes without the panel.
const MIN_CANVAS_W: u16 = 40;
/// Rows the intake keeps before a task: the drop zone over the prompt.
const INTAKE_H: u16 = 9;
/// Rows it keeps once a task runs — the source, its status, the prompt.
const INTAKE_H_STARTED: u16 = 8;
/// The most rows the intake grows to.
const INTAKE_H_MAX: u16 = 16;
/// The most rows of the draft, of the source or of the conductor's question,
/// and of owners, that the intake grows for; more scroll.
const PROMPT_MAX: usize = 6;
const BODY_MAX: usize = 8;
const OWNERS_MAX: usize = 8;
/// The owner chooser's keys, wrapped under its rows.
const CHOOSER_HINT: &str =
    "↑↓ owner · ←→ switch · 1 all claude · 2 all codex · 3 as planned · ⏎ launch";
/// Rows of the conductor's box: header, radar beside its vitals, a rule,
/// three lines of its log.
const COND_H: u16 = 11;
/// The fewest rows the TASKS box keeps before the conductor gives up its own.
const TASKS_MIN_H: u16 = 6;
/// Widest a message bubble's text runs before it is cut.
const BUBBLE_TEXT: usize = 36;
/// Cells of fading trail behind a travelling packet.
const TRAIL: usize = 6;
/// The part of an arrival the beam takes to reach the card; the burst
/// plays the rest of [`ARRIVE_SECS`].
const BEAM_SECS: f64 = 0.4;
/// How long a planned card takes to draw its dashed border round, and its
/// text to resolve out of noise.
const GHOST_DRAW_SECS: f64 = 0.8;
/// Each planned card starts drawing this long after the one before it.
const GHOST_STAGGER_SECS: f64 = 0.15;
/// What text resolving out of noise flickers through: one column each.
const NOISE: &[char] = &[
    '#', '%', '&', '*', '+', '=', '<', '>', '/', '\\', '|', '0', '1', 'A', 'X', 'Z', '$', ':',
];

/// A screen rectangle in fractional cells, as the canvas reports cards: they
/// may hang off its edges.
type Box4 = (f64, f64, f64, f64);

/// Where a task agent stands, as the intake's status line counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    /// Planned, or launched without a transcript yet: no card to show.
    Planned,
    /// Its main agent is in a turn.
    Working,
    /// Between turns: its part is done until a message wakes it.
    Done,
}

/// One agent of the task's team.
#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub name: String,
    pub standing: Standing,
}

/// The task's team: the launched agents in launch order, then the plan's
/// agents not launched yet.
pub(crate) fn roster(m: &TaskView, apps: &[App]) -> Vec<Member> {
    let mut out: Vec<Member> = Vec::new();
    for a in &m.agents {
        let i = a
            .session
            .as_deref()
            .and_then(|sid| apps.iter().position(|x| x.current_session_id == sid));
        let main = i.and_then(|i| {
            let s = &apps[i].session;
            s.spawn_order().next().and_then(|r| s.agent(r))
        });
        let standing = match (i, main) {
            (None, _) => Standing::Planned,
            (_, Some(x)) if x.status == AgentStatus::Running => Standing::Working,
            _ => Standing::Done,
        };
        out.push(Member {
            name: a.name.clone(),
            standing,
        });
    }
    for p in m.plan.iter().flat_map(|p| &p.agents) {
        if out.iter().any(|x| x.name == p.name) {
            continue;
        }
        out.push(Member {
            name: p.name.clone(),
            standing: Standing::Planned,
        });
    }
    out
}

/// How many of the team stand as `s`.
fn count(team: &[Member], s: Standing) -> usize {
    team.iter().filter(|x| x.standing == s).count()
}

/// Split the lamaverse's canvas row into the task panel and what is left,
/// or leave it whole when there is no room for both.
pub(crate) fn split(area: Rect) -> (Option<Rect>, Rect) {
    if area.width < PANEL_W + MIN_CANVAS_W || area.height < 12 {
        return (None, area);
    }
    let panel = Rect::new(area.x, area.y, PANEL_W, area.height);
    let rest = Rect::new(area.x + PANEL_W, area.y, area.width - PANEL_W, area.height);
    (Some(panel), rest)
}

/// Whether the intake has something to show or take in: it then grows to
/// fit it instead of staying compact.
fn grows(m: &TaskView) -> bool {
    m.intake.focused || !m.intake.draft.is_empty() || m.choosing || m.asking.is_some()
}

/// The text the intake's body shows above its status: the conductor's
/// question while it waits on you, else what was handed over.
fn body_text(m: &TaskView) -> &str {
    if let Some(q) = &m.asking {
        q
    } else if m.source.is_empty() {
        m.chat.first().map_or("", |c| c.text.as_str())
    } else {
        &m.source
    }
}

/// Rows the owner chooser's hint takes at `tw` columns.
fn chooser_hint(tw: usize) -> Vec<String> {
    super::wrap(CHOOSER_HINT, tw, 3)
}

/// Rows the intake wants for its box `w` columns wide: compact when idle
/// and empty, else its content, up to [`INTAKE_H_MAX`].
fn intake_h(m: &TaskView, w: u16) -> u16 {
    let compact = if started(m) {
        INTAKE_H_STARTED
    } else {
        INTAKE_H
    };
    if !grows(m) {
        return compact;
    }
    let tw = w.saturating_sub(4) as usize;
    // While you type, the intake is all yours: just the text.
    if !m.intake.draft.is_empty() && !m.choosing {
        let lines = wrap_lines(&m.intake.draft, tw.saturating_sub(2)).len();
        let notice = usize::from(m.intake.notice.is_some());
        return ((2 + notice + lines) as u16).clamp(compact, INTAKE_H_MAX);
    }
    let prompt = if m.intake.draft.is_empty() {
        1
    } else {
        wrap_lines(&m.intake.draft, tw.saturating_sub(2))
            .len()
            .min(PROMPT_MAX)
    };
    let body = if !started(m) {
        5
    } else if m.choosing {
        match &m.choice {
            Some(c) => 1 + c.owners.len().min(OWNERS_MAX) + chooser_hint(tw).len(),
            None => 1,
        }
    } else {
        wrap_lines(body_text(m), tw).len().min(BODY_MAX) + 2
    };
    let notice = usize::from(m.intake.notice.is_some());
    ((2 + body + notice + prompt) as u16).clamp(compact, INTAKE_H_MAX)
}

/// The panel's three boxes: the intake on top, the conductor under it, the
/// TASKS box taking the rest of the height. The intake is compact until it
/// has something to hold, and never takes more than half the panel. Short
/// of rows the conductor and the tasks share what the intake leaves.
pub(crate) fn panel_boxes(panel: Rect, m: &TaskView) -> (Rect, Rect, Rect) {
    let intake_h = intake_h(m, panel.width).min(panel.height / 2);
    let rest = panel.height - intake_h;
    let cond_h = if rest >= COND_H + TASKS_MIN_H {
        COND_H
    } else {
        rest / 2
    };
    let intake = Rect::new(panel.x, panel.y, panel.width, intake_h);
    let cond = Rect::new(panel.x, intake.bottom(), panel.width, cond_h);
    let tasks = Rect::new(panel.x, cond.bottom(), panel.width, rest - cond_h);
    (intake, cond, tasks)
}

/// Whether the user has handed anything over yet.
fn started(m: &TaskView) -> bool {
    !m.source.is_empty() || !m.chat.is_empty()
}

/// Whether the conductor has called the task done.
fn finished(phase: &str) -> bool {
    let phase = phase.to_ascii_uppercase();
    phase.contains("COMPLETE") || phase.contains("DONE")
}

/// A message kind's colour and glyph.
fn kind_look(kind: MsgKind) -> (Color, &'static str) {
    let palette = fx::palette();
    match kind {
        MsgKind::Assign => (palette.accent, "◆"),
        MsgKind::Contract => (fx::cyan(), "◇"),
        MsgKind::Question => (fx::violet(), "?"),
        MsgKind::Answer => (palette.success, "✓"),
        MsgKind::Blocker => (palette.error, "!"),
        MsgKind::Report => (fx::yellow(), "▪"),
    }
}

/// The panel's backdrop: a shade between the cards' surface and the canvas.
fn panel_bg() -> Color {
    let palette = fx::palette();
    fx::mix(palette.surface, palette.canvas_bg, 0.35)
}

/// Write `text` at `(x, y)`, cut short of column `end`. Returns the column
/// after the last cell written.
fn put(buf: &mut Buffer, x: u16, y: u16, end: u16, text: &str, style: Style) -> u16 {
    if x >= end || !buf.area.contains((x, y).into()) {
        return x;
    }
    let room = end.min(buf.area.right()) - x;
    buf.set_stringn(x, y, text, room as usize, style).0
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            buf[(x, y)].set_char(' ').set_style(style);
        }
    }
}

/// A box's border, each cell coloured by its place along the walk, so a
/// neon gradient can run round it.
fn frame(buf: &mut Buffer, area: Rect, bg: Color, color_at: impl Fn(usize, usize) -> Color) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let cells = nodes::perimeter(area);
    let n = cells.len();
    for (i, &(x, y, ch)) in cells.iter().enumerate() {
        buf[(x, y)]
            .set_char(ch)
            .set_style(Style::default().bg(bg).fg(color_at(i, n)));
    }
}

/// A dashed box: the drop zone, a place for something not there yet.
fn dashed(buf: &mut Buffer, area: Rect, style: Style) {
    if area.width < 2 || area.height < 2 {
        return;
    }
    let (l, t, r, b) = (area.left(), area.top(), area.right() - 1, area.bottom() - 1);
    for x in l..=r {
        let (top, bottom) = match x {
            _ if x == l => ('┌', '└'),
            _ if x == r => ('┐', '┘'),
            _ => ('┄', '┄'),
        };
        buf[(x, t)].set_char(top).set_style(style);
        buf[(x, b)].set_char(bottom).set_style(style);
    }
    for y in t + 1..b {
        buf[(l, y)].set_char('┆').set_style(style);
        buf[(r, y)].set_char('┆').set_style(style);
    }
}

/// The colour a border walks through: the neon stops sliding round it with
/// motion, a still `rest` colour without.
fn neon_walk(i: usize, n: usize, period: u64, rest: Color) -> Color {
    if !fx::motion() {
        return rest;
    }
    let off = period - (i as u64 * period / n.max(1) as u64) % period;
    fx::neon_cycle(period, off)
}

/// The most recent arrival at `who` still flashing its border, by kind colour.
fn flash_on(m: &TaskView, who: &str) -> Option<Color> {
    if !fx::motion() {
        return None;
    }
    m.messages
        .iter()
        .rev()
        .filter(|msg| msg.to == who || (who == CONDUCTOR && msg.to == EVERYONE))
        .find(|msg| (TRAVEL_SECS..TRAVEL_SECS + FLASH_SECS).contains(&msg.age(m.now)))
        .map(|msg| kind_look(msg.kind).0)
}

/// The left panel: the intake over the conductor over the tasks. Records
/// where each part went in `hits`, each task's block too.
pub(crate) fn render_panel(
    buf: &mut Buffer,
    panel: Rect,
    m: &TaskView,
    team: &[Member],
    hits: &mut TaskHits,
) {
    let (intake, cond, tasks) = panel_boxes(panel, m);
    hits.panel = Some(panel);
    hits.intake = Some(intake);
    hits.conductor = Some(cond);
    hits.prompt = render_intake(buf, intake, m, team);
    render_conductor(buf, cond, m);
    render_tasks(buf, tasks, m, &mut hits.rows);
}

/// The intake's one line about the task: what the conductor is up to in
/// the background, read off the phase and the team. Its text, its colour,
/// and whether it is still busy (a spinner rides along).
/// The three ways to run a planned team, with the planned mix spelled out.
fn choice_text(m: &TaskView) -> String {
    let planned = m.plan.as_ref().map_or(String::new(), |p| {
        let claude = p
            .agents
            .iter()
            .filter(|a| a.provider == Provider::Claude)
            .count();
        let codex = p.agents.len() - claude;
        format!(" ({claude} claude, {codex} codex)")
    });
    format!("1 ❋ claude  ·  2 ❂ codex  ·  3 as planned{planned}")
}

fn status_line(m: &TaskView, team: &[Member]) -> (String, Color, bool) {
    let palette = fx::palette();
    let msgs = m.delivered();
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let working = count(team, Standing::Working);
    let launched = team.len() - count(team, Standing::Planned);
    if finished(&m.phase) {
        (format!("✓ done · {msgs} msgs"), palette.success, false)
    } else if m.choosing {
        ("◈ who runs this team?".to_string(), fx::yellow(), false)
    } else if m.approving {
        (
            "◈ conductor asks to use a tool".to_string(),
            fx::yellow(),
            false,
        )
    } else if m.asking.is_some() {
        ("◈ conductor needs you".to_string(), fx::yellow(), false)
    } else if working > 0 {
        let text = format!("◈ {working} agent{} working · {msgs} msgs", plural(working));
        (text, palette.accent, true)
    } else if launched > 0 {
        let text = format!(
            "◈ {launched} agent{} waiting · {msgs} msgs",
            plural(launched)
        );
        (text, palette.subtle, false)
    } else if !team.is_empty() {
        let text = format!("◈ launching {} agent{}…", team.len(), plural(team.len()));
        (text, palette.accent, true)
    } else {
        (
            "◈ conductor investigating…".to_string(),
            palette.accent,
            true,
        )
    }
}

/// Wrap `text` to `width` columns, keeping its spaces (the cursor sits after
/// a typed one) and honouring `\n`. Each line says whether it opens a
/// logical line rather than continuing a wrapped one.
fn wrap_lines(text: &str, width: usize) -> Vec<(String, bool)> {
    use unicode_width::UnicodeWidthStr;
    let width = width.max(1);
    let mut out = Vec::new();
    for seg in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0;
        // Byte offset just after the last space in `cur`: where to break.
        let mut brk = None;
        let mut first = true;
        for ch in seg.chars() {
            let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch != ' ' && w + cw > width && !cur.is_empty() {
                match brk {
                    Some(b) if b < cur.len() => {
                        let rest = cur.split_off(b);
                        out.push((std::mem::replace(&mut cur, rest), first));
                    }
                    _ => out.push((std::mem::take(&mut cur), first)),
                }
                first = false;
                w = cur.width();
                brk = None;
            }
            cur.push(ch);
            w += cw;
            if ch == ' ' {
                brk = Some(cur.len());
            }
        }
        out.push((cur, first));
    }
    out
}

/// The slice of `len` lines that shows in `rows` rows, scrolled `scroll`
/// lines up from the end, and how many lie above it. When some do, one row
/// goes to the `↑ n more` marker instead of a line.
fn window(len: usize, rows: usize, scroll: usize) -> (usize, usize, usize) {
    if len <= rows {
        return (0, len, 0);
    }
    let end = len - scroll.min(len - rows);
    let start = end.saturating_sub(rows);
    if start > 0 {
        (start + 1, end, start)
    } else {
        (0, end, 0)
    }
}

/// The owner chooser: who runs each owner of the planned team.
fn render_chooser(
    buf: &mut Buffer,
    r: Rect,
    top: u16,
    body_end: u16,
    m: &TaskView,
    c: &ChoiceView,
) {
    use unicode_width::UnicodeWidthStr;
    let palette = fx::palette();
    let bg = Style::default().bg(panel_bg());
    let end = r.right() - 2;
    let tw = (end - r.x - 2) as usize;
    let name = m
        .tasks
        .iter()
        .find(|t| t.id == c.task)
        .map_or(c.task.as_str(), |t| t.title.as_str());
    put(
        buf,
        r.x + 2,
        top,
        end,
        &truncate(&format!("who runs {name}?"), tw),
        bg.fg(palette.text).add_modifier(Modifier::BOLD),
    );
    let hint = chooser_hint(tw);
    let rows = (body_end.saturating_sub(top + 1) as usize).saturating_sub(hint.len());
    let first = (c.cursor + 1)
        .saturating_sub(rows)
        .min(c.owners.len().saturating_sub(rows));
    for (y, (i, o)) in (top + 1..).zip(c.owners.iter().enumerate().skip(first).take(rows)) {
        let on = i == c.cursor;
        let marker = if on { "▸ " } else { "  " };
        let claude = o.provider == Provider::Claude;
        // The chosen provider is lit; the other waits dim.
        let lit = Style::default().bg(fx::yellow()).fg(Color::Black);
        let seg = |text: &str, chosen: bool| {
            if chosen {
                (format!(" {text} "), lit.add_modifier(Modifier::BOLD))
            } else {
                (format!(" {text} "), bg.fg(palette.subtle))
            }
        };
        let (a, sa) = seg("❋ claude", claude);
        let (b, sb) = seg("❂ codex", !claude);
        let tx = end.saturating_sub((a.width() + b.width() + 1) as u16);
        let x = put(buf, r.x + 2, y, tx, marker, bg.fg(fx::yellow()));
        let label = truncate(
            &format!("{} · {}", o.name, o.project),
            tx.saturating_sub(x + 1) as usize,
        );
        let ls = if on {
            bg.fg(palette.text).add_modifier(Modifier::BOLD)
        } else {
            bg.fg(palette.subtle)
        };
        put(buf, x, y, tx, &label, ls);
        let x = put(buf, tx, y, end, &a, sa);
        put(buf, x + 1, y, end, &b, sb);
    }
    for (k, line) in hint.iter().enumerate() {
        let hy = top + 1 + rows.min(c.owners.len()) as u16 + k as u16;
        put(buf, r.x + 2, hy, end, line, bg.fg(palette.muted));
    }
}

/// The intake: a place to hand things over, nothing more. Before a task the
/// drop zone; after, what was handed over and a line on how it goes. The
/// prompt, a text area of its own, sits at the bottom either way. Returns
/// its rect.
fn render_intake(buf: &mut Buffer, r: Rect, m: &TaskView, team: &[Member]) -> Option<Rect> {
    let palette = fx::palette();
    let bgc = panel_bg();
    let bg = Style::default().bg(bgc);
    let bold = Modifier::BOLD;
    fill(buf, r, bg);
    let started = started(m);
    frame(buf, r, bgc, |i, n| {
        if started {
            fx::mix(neon_walk(i, n, 5000, palette.accent), palette.accent, 0.4)
        } else {
            palette.muted
        }
    });
    if r.width < 12 || r.height < 4 {
        return None;
    }
    let end = r.right() - 2;
    let head_c = if started {
        palette.accent
    } else {
        palette.subtle
    };
    put(
        buf,
        r.x + 2,
        r.y,
        end,
        " ◈ INTAKE ",
        bg.fg(head_c).add_modifier(bold),
    );
    if m.intake.focused {
        // Shift+Enter is not obvious; say it where the border is free.
        let hint = " ⇧⏎ new line ";
        let w = hint.chars().count() as u16;
        put(
            buf,
            end.saturating_sub(w),
            r.bottom() - 1,
            end,
            hint,
            bg.fg(palette.muted),
        );
    }

    // The prompt: the draft wrapped, its last lines in view.
    let draft = &m.intake.draft;
    let tw = (end - r.x - 2) as usize;
    let room = tw.saturating_sub(2);
    let dlines = wrap_lines(draft, room);
    let typing = !draft.is_empty() && !m.choosing;
    let ph = if draft.is_empty() {
        1
    } else if typing {
        dlines.len()
    } else {
        dlines.len().min(PROMPT_MAX)
    }
    .min(r.height as usize - 3) as u16;
    let prompt_y = r.bottom() - 1 - ph;
    let pbg = Style::default().bg(fx::mix(palette.accent, bgc, 0.88));
    let prompt = Rect::new(r.x + 1, prompt_y, r.width - 2, ph);
    fill(buf, prompt, pbg);
    let marker = |buf: &mut Buffer, y: u16, ch: &str| {
        put(
            buf,
            r.x + 2,
            y,
            end,
            ch,
            pbg.fg(palette.accent).add_modifier(bold),
        );
    };
    let blink = !fx::motion() || (fx::now_ms() / 500).is_multiple_of(2);
    if m.intake.focused || !draft.is_empty() {
        let (start, stop, above) = window(dlines.len(), ph as usize, m.intake.scroll);
        let mut y = prompt_y;
        if above > 0 {
            put(
                buf,
                r.x + 4,
                y,
                end,
                &format!("↑ {above} more"),
                pbg.fg(palette.muted),
            );
            y += 1;
        }
        for (i, (line, opens)) in dlines.iter().enumerate().take(stop).skip(start) {
            let mark = if i == 0 {
                "›"
            } else if *opens {
                "·"
            } else {
                " "
            };
            marker(buf, y, mark);
            let x = put(buf, r.x + 4, y, end, line, pbg.fg(palette.text));
            if i + 1 == dlines.len() && m.intake.focused && blink {
                put(buf, x, y, end + 1, "▌", pbg.fg(palette.accent));
            }
            y += 1;
        }
    } else {
        marker(buf, prompt_y, "›");
        let hint = if m.choosing {
            "pick the owners above"
        } else if m.approving {
            "y allow · n refuse · or type why not…"
        } else if m.asking.is_some() {
            "answer the conductor…"
        } else if started {
            "paste the next task…"
        } else {
            "click here: paste, drop or type"
        };
        put(buf, r.x + 4, prompt_y, end, hint, pbg.fg(palette.muted));
    }
    let mut body_end = prompt_y;
    if let Some(note) = &m.intake.notice {
        body_end -= 1;
        put(
            buf,
            r.x + 2,
            body_end,
            end,
            &truncate(note, tw),
            bg.fg(fx::yellow()),
        );
    }
    let top = r.y + 1;
    let body_h = body_end.saturating_sub(top);
    if typing {
        return Some(prompt);
    }

    if !started {
        // Nothing yet: the drop zone, breathing.
        let h = body_h.min(5);
        if h == 5 {
            let dz = Rect::new(r.x + 2, top, r.width - 4, h);
            let look = fx::mix(palette.muted, palette.accent, fx::pulse(1600, 0));
            dashed(buf, dz, bg.fg(look));
            let lines: [(&str, Style); 3] = [
                (
                    "⇣ drop · paste · type",
                    bg.fg(palette.text).add_modifier(bold),
                ),
                ("a sentry issue, a jira ticket,", bg.fg(palette.subtle)),
                ("a teams thread — or just say it", bg.fg(palette.subtle)),
            ];
            for (k, (text, style)) in lines.iter().enumerate() {
                let w = unicode_width::UnicodeWidthStr::width(*text) as u16;
                let x = dz.x + dz.width.saturating_sub(w) / 2;
                put(buf, x, dz.y + 1 + k as u16, dz.right() - 1, text, *style);
            }
        }
        return Some(prompt);
    }

    if let (true, Some(c)) = (m.choosing, &m.choice) {
        render_chooser(buf, r, top, body_end, m, c);
        return Some(prompt);
    }

    // What was handed over (or the conductor's question), then a blank and
    // the status under it. While the draft is empty the wheel scrolls it.
    let choices = choice_text(m);
    let source = if m.choosing {
        choices.as_str()
    } else {
        body_text(m)
    };
    let rows = body_h.saturating_sub(2) as usize;
    let rows = if grows(m) { rows } else { rows.min(2) };
    let all: Vec<String> = wrap_lines(source, tw).into_iter().map(|l| l.0).collect();
    let scroll = if draft.is_empty() { m.intake.scroll } else { 0 };
    let (start, stop, above) = window(all.len(), rows, scroll);
    let dim = if m.asking.is_some() || m.choosing {
        palette.text
    } else {
        fx::mix(palette.subtle, bgc, 0.25)
    };
    let mut y = top;
    if above > 0 {
        put(
            buf,
            r.x + 2,
            y,
            end,
            &format!("↑ {above} more"),
            bg.fg(palette.muted),
        );
        y += 1;
    }
    for line in &all[start..stop] {
        put(buf, r.x + 2, y, end, line, bg.fg(dim));
        y += 1;
    }
    if body_h > 0 {
        let sy = (y + 1).min(body_end - 1);
        let (text, color, busy) = status_line(m, team);
        let x = put(
            buf,
            r.x + 2,
            sy,
            end,
            &text,
            bg.fg(color).add_modifier(bold),
        );
        if busy {
            put(
                buf,
                x + 1,
                sy,
                end,
                &fx::spinner().to_string(),
                bg.fg(color),
            );
        }
    }
    Some(prompt)
}

/// The radar's sweep speed in radians a second: fast while the conductor
/// dispatches a task, a slow idle turn between tasks.
fn sweep_speed(dispatching: bool) -> f64 {
    if dispatching { 4.0 } else { 0.8 }
}

/// The conductor: a dispatcher, busy only while it routes a task. Its
/// model, a radar, its vitals — mode, tasks routed, owners created, spent —
/// then the last of its log under a rule, what it reads right now newest.
fn render_conductor(buf: &mut Buffer, r: Rect, m: &TaskView) {
    let palette = fx::palette();
    let bgc = panel_bg();
    let bg = Style::default().bg(bgc);
    let bold = Modifier::BOLD;
    let c = &m.conductor;
    fill(buf, r, bg);
    let flash = flash_on(m, CONDUCTOR);
    frame(buf, r, bgc, |i, n| match flash {
        Some(f) => fx::glow(f, 0.2),
        None if c.dispatching => neon_walk(i, n, 2000, palette.accent),
        None => fx::mix(neon_walk(i, n, 8000, palette.muted), palette.muted, 0.6),
    });
    if r.width < 20 || r.height < 4 {
        return;
    }
    let end = r.right() - 2;
    let head_c = if c.dispatching {
        palette.accent
    } else {
        palette.subtle
    };
    let x = put(
        buf,
        r.x + 2,
        r.y,
        end,
        " ◈ CONDUCTOR ",
        bg.fg(head_c).add_modifier(bold),
    );
    let claude = m.conductor_provider != Some(Provider::Codex);
    let model = m
        .conductor_model
        .as_deref()
        .map(|s| s.trim_start_matches("claude-"))
        .unwrap_or("");
    if !model.is_empty() {
        let head = format!("{} {model} ", if claude { "❋" } else { "❂" });
        put(buf, x, r.y, end, &head, bg.fg(fx::provider_color(claude)));
    }

    // The radar, when there is room for it; the vitals sit beside it.
    let radar = r.width >= 34 && r.height >= 7;
    let sx = if radar {
        let (cx, cy) = (r.x as f64 + 7.0, r.y as f64 + 3.0);
        let (rx, ry) = (5.0, 2.0);
        let ring = bg.fg(palette.muted);
        let mut a = 0.0_f64;
        while a < std::f64::consts::TAU {
            let (x, y) = (cx + a.cos() * rx, cy + a.sin() * ry);
            put(buf, x.round() as u16, y.round() as u16, end, "·", ring);
            a += 0.45;
        }
        let ang = fx::now_ms() as f64 / 1000.0 * sweep_speed(c.dispatching);
        // Idle, the sweep fades toward the panel.
        let rest = if c.dispatching { 0.0 } else { 0.45 };
        for tr in 0..3 {
            let aa = ang - tr as f64 * 0.25;
            let col = fx::mix(palette.accent, bgc, tr as f32 * 0.3 + rest);
            let glyph = if tr == 0 { "•" } else { "·" };
            for d in 1..=5 {
                let d = d as f64;
                let (x, y) = (cx + aa.cos() * d, cy + aa.sin() * d * ry / rx);
                put(
                    buf,
                    x.round() as u16,
                    y.round() as u16,
                    end,
                    glyph,
                    bg.fg(col).add_modifier(bold),
                );
            }
        }
        let centre = if c.dispatching {
            palette.accent
        } else {
            palette.muted
        };
        put(
            buf,
            cx as u16,
            cy as u16,
            end,
            "◉",
            bg.fg(centre).add_modifier(bold),
        );
        r.x + 15
    } else {
        r.x + 2
    };

    let (mode, mode_c) = if c.dispatching {
        ("DISPATCHING", palette.accent)
    } else {
        ("IDLE · ready", palette.success)
    };
    let plural = if c.routed == 1 { "" } else { "s" };
    let spent = c
        .spent
        .map(crate::state::usage::fmt_usd)
        .unwrap_or_else(|| "—".to_string());
    let vitals: [(&str, String, Color); 4] = [
        ("mode", mode.to_string(), mode_c),
        ("routed", format!("{} task{plural}", c.routed), palette.text),
        ("owners", c.owners.to_string(), palette.text),
        ("spent", spent, fx::yellow()),
    ];
    let bottom = r.bottom() - 1;
    for (k, (label, value, color)) in vitals.iter().enumerate() {
        let y = r.y + 1 + k as u16;
        if y >= bottom {
            break;
        }
        put(buf, sx, y, end, label, bg.fg(palette.muted));
        put(buf, sx + 7, y, end, value, bg.fg(*color).add_modifier(bold));
    }

    // The log under a rule: a line per routed task, then what it reads
    // right now while it dispatches — the newest last and brightest.
    let sep_y = r.y + 6;
    if sep_y + 1 >= bottom {
        return;
    }
    let hair = fx::mix(palette.muted, bgc, 0.4);
    for x in r.x + 2..end {
        buf[(x, sep_y)].set_char('─').set_style(bg.fg(hair));
    }
    let mut lines: Vec<(&str, &str, Color)> = c
        .log
        .iter()
        .map(|l| ("›", l.as_str(), palette.subtle))
        .collect();
    if let Some(now) = c.reading.as_deref().filter(|_| c.dispatching) {
        lines.push(("◉", now, palette.text));
    }
    if lines.is_empty() {
        lines.push(("·", "ready for a task", palette.muted));
    }
    let rows = (bottom - sep_y - 1) as usize;
    let skip = lines.len().saturating_sub(rows);
    let shown = lines.len() - skip;
    let tw = (end - r.x - 4) as usize;
    for (k, (mark, line, color)) in lines.iter().skip(skip).enumerate() {
        let y = sep_y + 1 + k as u16;
        let last = k + 1 == shown;
        let mark_c = if last && c.dispatching {
            palette.accent
        } else {
            palette.muted
        };
        let mut text = bg.fg(*color);
        if last {
            text = text.add_modifier(bold);
        }
        put(buf, r.x + 2, y, end, mark, bg.fg(mark_c).add_modifier(bold));
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        put(buf, r.x + 4, y, end, &truncate(&line, tw), text);
    }
}

/// Rows a task's block takes in the TASKS box: its title and status, and
/// for the picked one a blank and its goals under them.
fn block_h(t: &TaskRow, picked: bool) -> u16 {
    if picked && !t.goals.is_empty() {
        3 + t.goals.len() as u16
    } else {
        2
    }
}

/// A task's status line, after its projects: its text and colour.
fn row_status(t: &TaskRow) -> (String, Color) {
    let palette = fx::palette();
    match t.status {
        RowStatus::Dispatching => (
            format!("◉ conductor dispatching {}", fx::spinner()),
            palette.accent,
        ),
        RowStatus::Asking => ("conductor needs you".to_string(), fx::yellow()),
        RowStatus::Choosing => ("who runs it? 1 ❋ · 2 ❂ · 3".to_string(), fx::yellow()),
        RowStatus::Launching => (format!("launching… {}", fx::spinner()), palette.accent),
        RowStatus::Working(n) => (format!("● {n} working · {} msgs", t.msgs), palette.success),
        RowStatus::Waiting => ("⚠ waiting on you".to_string(), palette.error),
        RowStatus::Done => (
            t.note
                .as_deref()
                .map_or_else(|| "✓ done".to_string(), |n| format!("✓ done · {n}")),
            palette.success,
        ),
    }
}

/// TASKS: every task and how it goes, a block each, newest last; the
/// picked one opens up with its goals behind a neon bar. Short of rows the
/// oldest go first, never the picked one. Records each block in `rows`.
fn render_tasks(buf: &mut Buffer, r: Rect, m: &TaskView, rows: &mut Vec<(String, Rect)>) {
    let palette = fx::palette();
    let bgc = panel_bg();
    let bg = Style::default().bg(bgc);
    let bold = Modifier::BOLD;
    fill(buf, r, bg);
    frame(buf, r, bgc, |i, n| {
        fx::mix(neon_walk(i, n, 6000, fx::violet()), fx::violet(), 0.5)
    });
    if r.width < 16 || r.height < 3 {
        return;
    }
    let end = r.right() - 2;
    let running = m
        .tasks
        .iter()
        .filter(|t| t.status != RowStatus::Done)
        .count();
    let waiting = m
        .tasks
        .iter()
        .filter(|t| t.status == RowStatus::Waiting)
        .count();
    let x = put(
        buf,
        r.x + 2,
        r.y,
        end,
        " ◈ TASKS ",
        bg.fg(palette.accent).add_modifier(bold),
    );
    let x = put(
        buf,
        x,
        r.y,
        end,
        &format!("{running} running "),
        bg.fg(palette.subtle),
    );
    if waiting > 0 {
        let on = !fx::motion() || (fx::now_ms() / 333).is_multiple_of(2);
        let look = if on {
            Style::default().bg(palette.error).fg(palette.canvas_bg)
        } else {
            bg.fg(palette.error)
        };
        put(
            buf,
            x,
            r.y,
            end,
            &format!(" ⚠ {waiting} waiting "),
            look.add_modifier(bold),
        );
    }
    if r.height >= 4 {
        put(
            buf,
            r.x + 2,
            r.bottom() - 1,
            end,
            " ↑↓ select · ⏎ open owner ",
            bg.fg(palette.muted),
        );
    }
    let (top, bottom) = (r.y + 2, r.bottom() - 1);
    if m.tasks.is_empty() {
        for (k, line) in ["no tasks yet", "paste one above ↑"].iter().enumerate() {
            let y = top + k as u16;
            if y < bottom {
                put(buf, r.x + 3, y, end, line, bg.fg(palette.muted));
            }
        }
        return;
    }
    let picked = |t: &TaskRow| m.selected.as_deref() == Some(t.id.as_str());
    let heights: Vec<u16> = m.tasks.iter().map(|t| block_h(t, picked(t))).collect();
    // Blocks with a blank row between them: drop the oldest until the rest
    // fit, but keep the picked one even if the newest then fall short.
    let room = bottom.saturating_sub(top) + 1;
    let fits = |hs: &[u16]| hs.iter().map(|h| h + 1).sum::<u16>() <= room;
    let mut start = 0;
    while start + 1 < heights.len() && !fits(&heights[start..]) {
        start += 1;
    }
    if let Some(sel) = m.tasks.iter().position(picked) {
        start = start.min(sel);
    }
    let mut y = top;
    for (t, &h) in m.tasks.iter().zip(&heights).skip(start) {
        if y >= bottom {
            break;
        }
        let b = Rect::new(r.x + 1, y, r.width - 2, h.min(bottom - y));
        render_task_row(buf, b, m, t, picked(t));
        rows.push((t.id.clone(), b));
        y += h + 1;
    }
}

/// One task's block at `b`: `◈ title` over `→ projects` and its status,
/// then its goals when picked. Its backdrop flashes a message's colour as
/// the message lands on it.
fn render_task_row(buf: &mut Buffer, b: Rect, m: &TaskView, t: &TaskRow, picked: bool) {
    let palette = fx::palette();
    let bgc = panel_bg();
    let bold = Modifier::BOLD;
    let back = match flash_on(m, &format!("{TASK}{}", t.id)) {
        Some(c) => fx::mix(c, palette.canvas_bg, 0.7),
        None if picked => fx::mix(bgc, palette.accent, 0.12),
        None => bgc,
    };
    let rb = Style::default().bg(back);
    fill(buf, b, rb);
    let end = b.right() - 1;
    if picked {
        let bar = if fx::motion() {
            fx::neon_cycle(4000, 0)
        } else {
            palette.accent
        };
        for y in b.top()..b.bottom() {
            put(buf, b.x, y, b.x + 1, "▌", rb.fg(bar).add_modifier(bold));
        }
    }
    let done = t.status == RowStatus::Done;
    let (mark, mark_c) = if done {
        ("✓ ", palette.success)
    } else {
        ("◈ ", palette.accent)
    };
    let x = put(
        buf,
        b.x + 2,
        b.y,
        end,
        mark,
        rb.fg(mark_c).add_modifier(bold),
    );
    let title = if t.title.is_empty() { &t.id } else { &t.title };
    let title = title.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut look = rb.fg(palette.text);
    if picked {
        look = look.add_modifier(bold);
    }
    put(
        buf,
        x,
        b.y,
        end,
        &truncate(&title, (end - x) as usize),
        look,
    );
    if b.height < 2 {
        return;
    }
    let y = b.y + 1;
    let (text, color) = row_status(t);
    let mut x = b.x + 2;
    if t.status != RowStatus::Dispatching && !t.projects.is_empty() {
        let to = truncate(&format!("→ {}", t.projects.join(" + ")), 20);
        x = put(buf, x, y, end, &format!("{to} "), rb.fg(palette.subtle));
    }
    let room = end.saturating_sub(x) as usize;
    put(
        buf,
        x,
        y,
        end,
        &truncate(&text, room),
        rb.fg(color).add_modifier(bold),
    );
    if !picked {
        return;
    }
    for (k, (goal, met)) in t.goals.iter().enumerate() {
        let gy = b.y + 3 + k as u16;
        if gy >= b.bottom() {
            break;
        }
        let (tick, tick_c, text_c) = if *met {
            ("☑", palette.success, palette.success)
        } else {
            ("☐", palette.muted, fx::mix(palette.text, back, 0.2))
        };
        put(
            buf,
            b.x + 3,
            gy,
            end,
            tick,
            rb.fg(tick_c).add_modifier(bold),
        );
        let room = end.saturating_sub(b.x + 5) as usize;
        put(buf, b.x + 5, gy, end, &truncate(goal, room), rb.fg(text_c));
    }
}

/// `s` resolving out of noise, left to right, as `p` goes 0 → 1: settled
/// characters, then a few flickering ahead of them, then nothing.
fn decrypt(s: &str, p: f64, seed: u64) -> String {
    if p >= 1.0 {
        return s.to_string();
    }
    if p <= 0.0 {
        return String::new();
    }
    let n = s.chars().count();
    let settled = (p * n as f64) as usize;
    let shown = n.min((p * n as f64 * 1.6) as usize + 2);
    let tick = fx::now_ms() / 42;
    s.chars()
        .take(shown)
        .enumerate()
        .map(|(i, c)| {
            if i < settled || c == ' ' {
                c
            } else {
                let h = fx::hash(seed ^ tick.wrapping_mul(31) ^ ((i as u64) << 20));
                NOISE[(h % NOISE.len() as u64) as usize]
            }
        })
        .collect()
}

/// Write `text` from cell `(x, y)`, cell by cell, keeping only what lands
/// on the canvas below its top line: a card may hang off any edge.
fn put_on(buf: &mut Buffer, canvas: Rect, x: i32, y: i32, end: i32, text: &str, style: Style) {
    let mut cx = x;
    for ch in text.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) as i32;
        if w == 0 {
            continue;
        }
        if cx + w > end {
            break;
        }
        if on_canvas(canvas, cx, y) && on_canvas(canvas, cx + w - 1, y) {
            buf[(cx as u16, y as u16)].set_char(ch).set_style(style);
        }
        cx += w;
    }
}

/// The task's planned agents, each a dashed card beside its project's sessions
/// until its session arrives: the border draws itself round, `◈ PLANNED
/// OWNER` (or a peer's `◈ REQUESTED`) and the agent resolve out of noise, then the border breathes pink while
/// it waits to launch. Drawn over the placeholder the plane keeps for it.
pub(crate) fn render_ghosts(buf: &mut Buffer, canvas: Rect, plane: &Plane) {
    let Some(m) = &plane.task else { return };
    let Some(plan) = &m.plan else { return };
    let motion = fx::motion();
    for (k, p) in plan.agents.iter().enumerate() {
        let Some(card) = plane.ghost_card(p).and_then(|id| plane.screen_rect(&id)) else {
            continue;
        };
        if !visible(card, canvas) {
            continue;
        }
        let t = m.now - plan.planned_at - k as f64 * GHOST_STAGGER_SECS;
        if motion && t < 0.0 {
            continue;
        }
        let draw = if motion {
            (t / GHOST_DRAW_SECS).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let read = if motion {
            ((t - 0.2) / GHOST_DRAW_SECS).clamp(0.0, 1.0)
        } else {
            1.0
        };
        render_ghost(buf, canvas, card, m, p, draw, read);
    }
}

/// One planned card at screen box `card`: its border `draw` (0..1) of the
/// way round, its text `read` (0..1) resolved.
fn render_ghost(
    buf: &mut Buffer,
    canvas: Rect,
    (x, y, w, h): Box4,
    m: &TaskView,
    p: &PlannedView,
    draw: f64,
    read: f64,
) {
    let palette = fx::palette();
    let (x, y) = (x.round() as i32, y.round() as i32);
    let (w, h) = (w.round() as i32, h.round() as i32);
    if w < 2 || h < 2 {
        return;
    }
    let seed = fx::hash_str(&p.name);
    let border = if fx::motion() {
        let dim = fx::mix(fx::violet(), palette.canvas_bg, 0.45);
        fx::mix(dim, palette.accent, fx::pulse(1100, seed % 1100))
    } else {
        fx::mix(palette.muted, palette.accent, 0.5)
    };
    // The picked task's planned cards light up like its live ones.
    let back = if m.selected.as_deref() == Some(p.task.as_str()) {
        fx::mix(palette.canvas_bg, palette.accent, 0.1)
    } else {
        palette.canvas_bg
    };
    let bg = Style::default().bg(back);
    for cy in y + 1..y + h - 1 {
        put_on(
            buf,
            canvas,
            x + 1,
            cy,
            x + w - 1,
            &" ".repeat((w - 2) as usize),
            bg,
        );
    }
    // The dashed border, walked clockwise from the top-left corner: as far
    // round as it has drawn.
    let mut walk: Vec<(i32, i32, char)> = Vec::with_capacity(2 * (w + h) as usize);
    let (r, b) = (x + w - 1, y + h - 1);
    for cx in x..=r {
        walk.push((
            cx,
            y,
            if cx == x {
                '┌'
            } else if cx == r {
                '┐'
            } else {
                '┄'
            },
        ));
    }
    for cy in y + 1..b {
        walk.push((r, cy, '┆'));
    }
    for cx in (x..=r).rev() {
        walk.push((
            cx,
            b,
            if cx == r {
                '┘'
            } else if cx == x {
                '└'
            } else {
                '┄'
            },
        ));
    }
    for cy in (y + 1..b).rev() {
        walk.push((x, cy, '┆'));
    }
    let n = ((draw * walk.len() as f64).ceil() as usize).min(walk.len());
    let edge = bg.fg(border).add_modifier(Modifier::BOLD);
    for (k, &(cx, cy, ch)) in walk[..n].iter().enumerate() {
        if !on_canvas(canvas, cx, cy) {
            continue;
        }
        // The pen's tip glows while it draws.
        let style = if k + 1 == n && n < walk.len() {
            bg.fg(fx::glow(palette.accent, 0.6))
                .add_modifier(Modifier::BOLD)
        } else {
            edge
        };
        buf[(cx as u16, cy as u16)].set_char(ch).set_style(style);
    }
    if w < 8 || h < 3 || read <= 0.0 {
        return;
    }
    let (tx, end) = (x + 2, x + w - 2);
    let room = (end - tx).max(0) as usize;
    let claude = p.provider == Provider::Claude;
    let mark = if claude { "❋" } else { "❂" };
    let bold = Modifier::BOLD;
    if w < 12 || h < 4 {
        // Zoomed out: who it is, on its one row.
        let who = format!("◈ {}", truncate(&p.name, room.saturating_sub(2)));
        put_on(
            buf,
            canvas,
            tx,
            y + h / 2,
            end,
            &decrypt(&who, read, seed),
            bg.fg(palette.accent).add_modifier(bold),
        );
        return;
    }
    put_on(
        buf,
        canvas,
        tx,
        y + 1,
        end,
        &decrypt(
            if p.parent.is_some() {
                "◈ REQUESTED"
            } else {
                "◈ PLANNED OWNER"
            },
            read,
            seed,
        ),
        bg.fg(palette.accent).add_modifier(bold),
    );
    let who = format!("{mark} {}", truncate(&p.name, room.saturating_sub(2)));
    put_on(
        buf,
        canvas,
        tx,
        y + 2,
        end,
        &decrypt(&who, read, seed >> 8),
        bg.fg(palette.text).add_modifier(bold),
    );
    if read >= 1.0 {
        put_on(
            buf,
            canvas,
            tx,
            y + 2,
            end,
            mark,
            bg.fg(fx::provider_color(claude)).add_modifier(bold),
        );
    }
    let mut row = y + 3;
    let model = p.model.trim_start_matches("claude-");
    if !model.is_empty() && row < y + h - 1 {
        put_on(
            buf,
            canvas,
            tx,
            row,
            end,
            &decrypt(&truncate(model, room), read, seed >> 16),
            bg.fg(fx::mix(fx::provider_color(claude), palette.canvas_bg, 0.3)),
        );
        row += 1;
    }
    // A peer names the owner that asked for it.
    if let Some(owner) = &p.parent
        && row < y + h - 2
    {
        put_on(
            buf,
            canvas,
            tx,
            row,
            end,
            &decrypt(&format!("asked for by {owner}"), read, seed >> 12),
            bg.fg(palette.muted),
        );
        row += 1;
    }
    // The role in what is left, the last row kept for where it stands.
    let role_rows = (y + h - 2 - row).max(0) as usize;
    let role = p.role.split_whitespace().collect::<Vec<_>>().join(" ");
    if !role.is_empty() && role_rows > 0 {
        for line in super::wrap(&role, room, role_rows) {
            put_on(
                buf,
                canvas,
                tx,
                row,
                end,
                &decrypt(&line, read, seed >> 24),
                bg.fg(palette.subtle),
            );
            row += 1;
        }
    }
    let launching = m.agents.iter().any(|a| a.name == p.name);
    let state = if launching {
        format!("◌ launching {}", fx::spinner())
    } else if p.start_later {
        "○ starts later".to_string()
    } else {
        "○ awaiting launch".to_string()
    };
    if h >= 5 {
        put_on(
            buf,
            canvas,
            tx,
            y + h - 2,
            end,
            &decrypt(&state, read, seed >> 32),
            bg.fg(palette.muted),
        );
    }
}

/// Where a new agent's entrance stands `t` seconds after it joined.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Arrival {
    /// The conductor's beam, this far (0..1) on its way to the card.
    Beam(f64),
    /// The burst round the card, this far (0..1) through: rings of `✦`
    /// widening and fading.
    Burst(f64),
    /// The card's border flashing, this far (0..1) through.
    Flash(f64),
}

/// The part of an arrival playing at `t`; `None` before and after.
fn arrival(t: f64) -> Option<Arrival> {
    if t < 0.0 {
        None
    } else if t < BEAM_SECS {
        Some(Arrival::Beam(t / BEAM_SECS))
    } else if t < ARRIVE_SECS {
        Some(Arrival::Burst((t - BEAM_SECS) / (ARRIVE_SECS - BEAM_SECS)))
    } else if t < ARRIVE_SECS + FLASH_SECS {
        Some(Arrival::Flash((t - ARRIVE_SECS) / FLASH_SECS))
    } else {
        None
    }
}

/// New agents arriving: a beam to the agent's card — an owner's from its
/// task's row, a peer's from the owner that asked for it — a burst of `✦`
/// round the card, then its border flashing. Without a task row (a lone
/// task) the beam leaves the conductor's box. Without motion the card's
/// border just holds the colour as long.
pub(crate) fn render_arrivals(buf: &mut Buffer, canvas: Rect, plane: &Plane, apps: &[App]) {
    let Some(m) = &plane.task else { return };
    let pink = fx::palette().accent;
    for a in &m.agents {
        let Some(phase) = a.joined.and_then(|j| arrival(m.now - j)) else {
            continue;
        };
        let Some(card) = endpoint(&a.name, plane, apps, canvas) else {
            continue;
        };
        if !fx::motion() {
            flash_border(buf, card, canvas, pink);
            continue;
        }
        match phase {
            Arrival::Beam(p) => {
                let from = a
                    .parent
                    .as_deref()
                    .and_then(|owner| endpoint(owner, plane, apps, canvas))
                    .or_else(|| task_row(plane, &a.task))
                    .or_else(|| plane.task_hits.conductor.map(to_box));
                let Some(from) = from else {
                    continue;
                };
                let cells = path(from, card);
                let n = ((p * cells.len() as f64).ceil() as usize).min(cells.len());
                for (k, &(x, y)) in cells[..n].iter().enumerate() {
                    if !on_canvas(canvas, x, y) {
                        continue;
                    }
                    let (glyph, c) = if k + 1 == n {
                        ("✸", fx::glow(pink, 0.6))
                    } else {
                        ("━", pink)
                    };
                    buf[(x as u16, y as u16)]
                        .set_symbol(glyph)
                        .set_style(Style::default().fg(c).add_modifier(Modifier::BOLD));
                }
            }
            Arrival::Burst(k) => burst(buf, card, canvas, pink, k),
            Arrival::Flash(k) => {
                flash_border(buf, card, canvas, fx::glow(pink, 0.3 * (1.0 - k as f32)));
            }
        }
    }
}

/// A ring of `✦` round a card, `k` (0..1) through: it steps outward and
/// fades into the canvas.
fn burst(buf: &mut Buffer, (x, y, w, h): Box4, canvas: Rect, color: Color, k: f64) {
    let d = 1 + (k * 3.0).floor() as i32;
    let (x0, y0) = (x.round() as i32 - d * 2, y.round() as i32 - d);
    let (w, h) = (w.round() as i32 + d * 4, h.round() as i32 + d * 2);
    let style = Style::default()
        .fg(fx::mix(color, fx::palette().canvas_bg, k as f32))
        .add_modifier(Modifier::BOLD);
    let mut star = |cx: i32, cy: i32| {
        if on_canvas(canvas, cx, cy) {
            buf[(cx as u16, cy as u16)].set_symbol("✦").set_style(style);
        }
    };
    for cx in (x0..x0 + w).step_by(3) {
        star(cx, y0);
        star(cx, y0 + h - 1);
    }
    for cy in (y0..y0 + h).step_by(2) {
        star(x0, cy);
        star(x0 + w - 1, cy);
    }
}

/// Whether cell `(x, y)` is on the canvas, below its top line (the
/// breadcrumb's).
fn on_canvas(canvas: Rect, x: i32, y: i32) -> bool {
    x >= canvas.x as i32
        && y > canvas.y as i32
        && x < canvas.right() as i32
        && y < canvas.bottom() as i32
}

/// Smooth in-and-out: a packet leaves gently and slows into its receiver.
fn ease(k: f64) -> f64 {
    let k = k.clamp(0.0, 1.0);
    if k < 0.5 {
        4.0 * k * k * k
    } else {
        1.0 - (-2.0 * k + 2.0).powi(3) / 2.0
    }
}

fn inside((x, y, w, h): Box4, cx: i32, cy: i32) -> bool {
    let (cx, cy) = (cx as f64 + 0.5, cy as f64 + 0.5);
    cx >= x && cy >= y && cx < x + w && cy < y + h
}

fn centre((x, y, w, h): Box4) -> (i32, i32) {
    ((x + w / 2.0).floor() as i32, (y + h / 2.0).floor() as i32)
}

/// The cells of a straight line from `a`'s centre to `b`'s, leaving out the
/// cells under either box: the gap a message crosses.
pub(crate) fn path(a: Box4, b: Box4) -> Vec<(i32, i32)> {
    let (mut x0, mut y0) = centre(a);
    let (x1, y1) = centre(b);
    let (dx, dy) = ((x1 - x0).abs(), -(y1 - y0).abs());
    let (sx, sy) = (if x0 < x1 { 1 } else { -1 }, if y0 < y1 { 1 } else { -1 });
    let mut err = dx + dy;
    let mut out = Vec::with_capacity((dx - dy) as usize + 1);
    loop {
        if !inside(a, x0, y0) && !inside(b, x0, y0) {
            out.push((x0, y0));
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
    out
}

/// Where a `(w, h)` bubble anchored at `at` (its top-left) lands, pulled back
/// inside `area` — never under the top line, which the breadcrumb holds.
pub(crate) fn place_bubble(at: (i32, i32), (w, h): (u16, u16), area: Rect) -> (u16, u16) {
    let max_x = area.right().saturating_sub(w).max(area.x) as i32;
    let max_y = area.bottom().saturating_sub(h).max(area.y) as i32;
    let min_y = (area.y as i32 + 1).min(max_y);
    (
        at.0.clamp(area.x as i32, max_x) as u16,
        at.1.clamp(min_y, max_y) as u16,
    )
}

/// A landed bubble's anchor: over the receiver's top-left, or under it when
/// there is no room above.
pub(crate) fn landing((x, y, _, h): Box4, bubble_h: u16, area: Rect) -> (i32, i32) {
    let above = y.floor() as i32 - bubble_h as i32 - 1;
    if above > area.y as i32 {
        (x.floor() as i32 + 1, above)
    } else {
        (x.floor() as i32 + 1, (y + h).ceil() as i32 + 1)
    }
}

fn to_box(r: Rect) -> Box4 {
    (r.x as f64, r.y as f64, r.width as f64, r.height as f64)
}

/// Whether a box is at least partly inside `area`.
fn visible((x, y, w, h): Box4, area: Rect) -> bool {
    x < area.right() as f64
        && y < area.bottom() as f64
        && x + w > area.x as f64
        && y + h > area.y as f64
}

/// Task `id`'s row in the TASKS box as an endpoint: its right edge, so a
/// packet enters the canvas beside the row rather than crossing the panel.
fn task_row(plane: &Plane, id: &str) -> Option<Box4> {
    let (_, r) = plane.task_hits.rows.iter().find(|(t, _)| t == id)?;
    Some((r.right() as f64 - 1.0, r.y as f64, 1.0, r.height as f64))
}

/// How a message's end reads on its bubble and in the log: a task by the
/// start of its title, anyone else by name.
fn label(m: &TaskView, name: &str) -> String {
    match name.strip_prefix(TASK) {
        Some(id) => m
            .tasks
            .iter()
            .find(|t| t.id == id && !t.title.trim().is_empty())
            .map(|t| truncate(&t.title, 24))
            .unwrap_or_else(|| id.to_string()),
        None => name.to_string(),
    }
}

/// Where a message's end is on screen: an agent's main card (its planned
/// card until then), a task's row, the conductor's panel box, or the
/// intake for you. `None` when it isn't on screen.
fn endpoint(name: &str, plane: &Plane, apps: &[App], canvas: Rect) -> Option<Box4> {
    if let Some(id) = name.strip_prefix(TASK) {
        return task_row(plane, id);
    }
    match name {
        CONDUCTOR | EVERYONE => plane.task_hits.conductor.map(to_box),
        "you" | "user" => plane.task_hits.intake.map(to_box),
        _ => {
            // Before its own card is on the canvas, its planned one stands
            // in: an arrival or a first message lands there.
            let card = plane
                .task_card(apps, name)
                .or_else(|| plane.task_ghost(name))?;
            plane.screen_rect(&card).filter(|b| visible(*b, canvas))
        }
    }
}

/// One message on screen this frame, resolved.
struct Flight<'a> {
    msg: &'a MessageView,
    path: Vec<(i32, i32)>,
    to: Box4,
    /// Travelled fraction, 0..1; 1 once landed.
    p: f64,
    /// How far it has faded out, 0..1.
    fade: f32,
    color: Color,
    glyph: &'static str,
    /// `sender ▸ receiver`, as its bubble reads.
    who: String,
}

/// Messages in flight: a dotted line between sender and receiver, a packet
/// with a fading trail travelling along it, and a bubble with the message
/// riding above the packet — then lingering by the receiver, whose border
/// flashes the message's colour as it lands.
pub(crate) fn render_messages(buf: &mut Buffer, canvas: Rect, plane: &Plane, apps: &[App]) {
    let Some(m) = &plane.task else { return };
    let motion = fx::motion();
    let mut flights = Vec::new();
    for msg in m.messages.iter().filter(|x| x.in_flight(m.now)) {
        // The conductor talking to everyone has nowhere to fly.
        if msg.to == EVERYONE && msg.from == CONDUCTOR {
            continue;
        }
        let (Some(from), Some(to)) = (
            endpoint(&msg.from, plane, apps, canvas),
            endpoint(&msg.to, plane, apps, canvas),
        ) else {
            continue;
        };
        let t = msg.age(m.now);
        let (p, fade) = if motion {
            let end = TRAVEL_SECS + LINGER_SECS;
            let fade = ((t - (end - 0.6)) / 0.6).clamp(0.0, 1.0) as f32;
            ((t / TRAVEL_SECS).min(1.0), fade)
        } else {
            (1.0, 0.0)
        };
        let (color, glyph) = kind_look(msg.kind);
        flights.push(Flight {
            msg,
            path: path(from, to),
            to,
            p,
            fade,
            color,
            glyph,
            who: format!("{} ▸ {}", label(m, &msg.from), label(m, &msg.to)),
        });
    }
    if flights.is_empty() {
        return;
    }
    let palette = fx::palette();
    let canvas_bg = palette.canvas_bg;
    let on_canvas = |x: i32, y: i32| on_canvas(canvas, x, y);

    // Dotted lines, on bare canvas only: they pass behind other cards.
    for f in &flights {
        let c = fx::mix(f.color, canvas_bg, 0.5 + 0.5 * f.fade);
        for &(x, y) in f.path.iter().step_by(2) {
            if !on_canvas(x, y) {
                continue;
            }
            let cell = &mut buf[(x as u16, y as u16)];
            if cell.bg == canvas_bg {
                cell.set_char('·').set_fg(c);
            }
        }
    }

    // Packets and their trails; landed ones flash their receiver.
    for f in &flights {
        let t = f.msg.age(m.now);
        if motion && f.p < 1.0 && !f.path.is_empty() {
            let n = f.path.len();
            let head = (ease(f.p) * (n - 1) as f64).round() as usize;
            for k in (0..=TRAIL.min(head)).rev() {
                let (x, y) = f.path[head - k];
                if !on_canvas(x, y) {
                    continue;
                }
                let glyph = match k {
                    0 => f.glyph,
                    1 | 2 => "•",
                    _ => "·",
                };
                let mut style = Style::default()
                    .fg(fx::mix(f.color, canvas_bg, k as f32 * 0.12))
                    .add_modifier(Modifier::BOLD);
                if k == 0 {
                    style = style.bg(fx::mix(f.color, canvas_bg, 0.7));
                }
                buf[(x as u16, y as u16)].set_symbol(glyph).set_style(style);
            }
        } else if motion && t < TRAVEL_SECS + FLASH_SECS {
            let k = ((t - TRAVEL_SECS) / FLASH_SECS).clamp(0.0, 1.0) as f32;
            flash_border(buf, f.to, canvas, fx::glow(f.color, 0.3 * (1.0 - k)));
        }
    }

    // Bubbles last, over everything on the canvas.
    for f in &flights {
        let head = if f.p < 1.0 && !f.path.is_empty() {
            let n = f.path.len();
            Some(f.path[(ease(f.p) * (n - 1) as f64).round() as usize])
        } else {
            None
        };
        render_bubble(buf, canvas, f, head);
    }
}

/// The receiver's border, recoloured for a moment.
fn flash_border(buf: &mut Buffer, (x, y, w, h): Box4, canvas: Rect, color: Color) {
    let r = Rect::new(
        x.round().max(0.0) as u16,
        y.round().max(0.0) as u16,
        w.round().max(0.0) as u16,
        h.round().max(0.0) as u16,
    );
    if r.width < 2 || r.height < 2 {
        return;
    }
    for (cx, cy, _) in nodes::perimeter(r) {
        if canvas.contains((cx, cy).into()) {
            buf[(cx, cy)].set_fg(color);
        }
    }
}

/// A message's bubble: `sender ▸ receiver` in its kind's colour over the
/// text. Rides above the packet's head while travelling, then by the
/// receiver.
fn render_bubble(buf: &mut Buffer, canvas: Rect, f: &Flight, head: Option<(i32, i32)>) {
    use unicode_width::UnicodeWidthStr;
    let palette = fx::palette();
    let canvas_bg = palette.canvas_bg;
    let who = f.who.clone();
    let text = truncate(
        &f.msg.text.split_whitespace().collect::<Vec<_>>().join(" "),
        BUBBLE_TEXT,
    );
    let w = (who.width().max(text.width()) as u16 + 4).min(canvas.width);
    if w < 6 || canvas.height < 4 {
        return;
    }
    let at = match head {
        Some((x, y)) => (x - w as i32 / 2, y - 3),
        // Landed on a task's row: beside it, at the canvas's left edge.
        None if f.msg.to.starts_with(TASK) => (canvas.x as i32, f.to.1 as i32),
        None => landing(f.to, 2, canvas),
    };
    let (bx, by) = place_bubble(at, (w, 2), canvas);
    let fg = fx::mix(f.color, canvas_bg, f.fade);
    let bb = match f.color {
        Color::Rgb(..) => fx::mix(fx::mix(f.color, canvas_bg, 0.84), canvas_bg, f.fade),
        _ => palette.surface,
    };
    let style = Style::default().bg(bb);
    fill(buf, Rect::new(bx, by, w, 2), style);
    let end = bx + w;
    for y in [by, by + 1] {
        put(
            buf,
            bx,
            y,
            end,
            "▌",
            style.fg(fg).add_modifier(Modifier::BOLD),
        );
    }
    put(
        buf,
        bx + 2,
        by,
        end,
        &who,
        style.fg(fg).add_modifier(Modifier::BOLD),
    );
    let text_c = fx::mix(palette.text, canvas_bg, f.fade);
    put(buf, bx + 2, by + 1, end, &text, style.fg(text_c));
}

/// `+mm:ss` since the task started.
fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("+{:02}:{:02}", s / 60, s % 60)
}

/// The message log: a drawer down the canvas's right side listing every
/// delivered message, newest at the bottom.
pub(crate) fn render_log(buf: &mut Buffer, canvas: Rect, m: &TaskView, hits: &mut TaskHits) {
    let palette = fx::palette();
    if canvas.width < 30 || canvas.height < 6 {
        return;
    }
    let w = (canvas.width / 2).clamp(30, 48);
    let r = Rect::new(canvas.right() - w, canvas.y + 1, w, canvas.height - 1);
    hits.log = Some(r);
    let bgc = panel_bg();
    let bg = Style::default().bg(bgc);
    let bold = Modifier::BOLD;
    fill(buf, r, bg);
    frame(buf, r, bgc, |i, n| {
        fx::mix(neon_walk(i, n, 6000, fx::cyan()), fx::cyan(), 0.5)
    });
    let end = r.right() - 2;
    let got: Vec<&MessageView> = m.messages.iter().filter(|x| x.delivered(m.now)).collect();
    let x = put(
        buf,
        r.x + 2,
        r.y,
        end,
        " ⇄ MESSAGES ",
        bg.fg(fx::cyan()).add_modifier(bold),
    );
    put(
        buf,
        x,
        r.y,
        end,
        &format!("{} · ⇄ closes ", got.len()),
        bg.fg(palette.subtle),
    );
    if got.is_empty() {
        put(
            buf,
            r.x + 3,
            r.y + 2,
            end,
            "no messages yet",
            bg.fg(palette.muted),
        );
        return;
    }
    let max = ((r.height - 2) / 2) as usize;
    let skip = got.len().saturating_sub(max);
    let dim = fx::mix(palette.text, bgc, 0.2);
    for (k, msg) in got.iter().skip(skip).enumerate() {
        let y = r.y + 1 + k as u16 * 2;
        let (color, glyph) = kind_look(msg.kind);
        let mut x = put(
            buf,
            r.x + 2,
            y,
            end,
            &clock(msg.shown_at + TRAVEL_SECS),
            bg.fg(palette.muted),
        );
        x = put(
            buf,
            x + 1,
            y,
            end,
            &label(m, &msg.from),
            bg.fg(color).add_modifier(bold),
        );
        x = put(buf, x, y, end, " ▸ ", bg.fg(palette.muted));
        put(
            buf,
            x,
            y,
            end,
            &label(m, &msg.to),
            bg.fg(palette.text).add_modifier(bold),
        );
        put(
            buf,
            r.x + 3,
            y + 1,
            end,
            glyph,
            bg.fg(color).add_modifier(bold),
        );
        let text = msg.text.split_whitespace().collect::<Vec<_>>().join(" ");
        put(
            buf,
            r.x + 5,
            y + 1,
            end,
            &truncate(&text, (end - r.x - 5) as usize),
            bg.fg(dim),
        );
    }
}

/// The task's part of the lamaverse's top line, from column `x`:
/// `◈ TASK`, what it is about, its phase, and the `⇄ N msgs` chip that
/// opens the log. Returns the chip's rect.
pub(crate) fn render_crumb(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    end: u16,
    m: &TaskView,
) -> Option<Rect> {
    use unicode_width::UnicodeWidthStr;
    let palette = fx::palette();
    let bar = fx::mix(palette.surface, palette.canvas_bg, 0.3);
    let chip = format!(" ⇄ {} msgs ", m.delivered());
    let chip_w = chip.width() as u16;
    let phase = format!(" {} ", m.phase);
    let lead = " ◈ TASK ";
    // Room after the source for the phase, the chip and the gaps.
    let rest = phase.width() as u16 + chip_w + 3;
    if x + lead.width() as u16 + rest + 6 > end {
        return None;
    }
    let mut cx = put(
        buf,
        x,
        y,
        end,
        lead,
        Style::default()
            .bg(palette.accent)
            .fg(palette.canvas_bg)
            .add_modifier(Modifier::BOLD),
    );
    let about = if m.source.is_empty() {
        "awaiting intake".to_string()
    } else {
        m.source.split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let room = end.saturating_sub(cx + rest).min(42) as usize;
    cx = put(
        buf,
        cx,
        y,
        end,
        &format!(" {} ", truncate(&about, room.saturating_sub(2))),
        Style::default()
            .bg(bar)
            .fg(fx::cyan())
            .add_modifier(Modifier::BOLD),
    );
    cx = put(
        buf,
        cx,
        y,
        end,
        &phase,
        Style::default()
            .bg(bar)
            .fg(palette.subtle)
            .add_modifier(Modifier::BOLD),
    );
    let cx = cx + 1;
    let look = if m.log_open {
        Style::default().bg(fx::cyan()).fg(palette.canvas_bg)
    } else {
        Style::default()
            .bg(fx::mix(fx::cyan(), palette.canvas_bg, 0.85))
            .fg(fx::cyan())
    };
    let after = put(buf, cx, y, end, &chip, look.add_modifier(Modifier::BOLD));
    Some(Rect::new(cx, y, after - cx, 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_runs_between_the_boxes_not_through_them() {
        let a = (0.0, 0.0, 4.0, 3.0);
        let b = (20.0, 0.0, 4.0, 3.0);
        let p = path(a, b);
        assert!(!p.is_empty());
        assert!(p.iter().all(|&(x, y)| !inside(a, x, y) && !inside(b, x, y)));
        assert_eq!(p.first(), Some(&(4, 1)), "leaves at the sender's edge");
        assert_eq!(p.last(), Some(&(19, 1)), "ends at the receiver's edge");
        // Contiguous: every step moves one cell.
        for w in p.windows(2) {
            assert!((w[1].0 - w[0].0).abs() <= 1 && (w[1].1 - w[0].1).abs() <= 1);
        }
    }

    #[test]
    fn overlapping_boxes_have_no_path() {
        let a = (0.0, 0.0, 10.0, 5.0);
        assert!(path(a, (2.0, 1.0, 4.0, 2.0)).is_empty());
    }

    #[test]
    fn bubbles_stay_on_the_canvas() {
        let area = Rect::new(40, 0, 80, 30);
        assert_eq!(place_bubble((60, 10), (20, 2), area), (60, 10));
        assert_eq!(
            place_bubble((-5, -3), (20, 2), area),
            (40, 1),
            "below the top line"
        );
        assert_eq!(place_bubble((200, 100), (20, 2), area), (100, 28));
        // Wider than the canvas: pinned to its left edge.
        assert_eq!(place_bubble((50, 5), (100, 2), area), (40, 5));
    }

    #[test]
    fn a_landed_bubble_sits_over_its_receiver_or_under_it() {
        let area = Rect::new(0, 0, 100, 40);
        assert_eq!(landing((10.0, 20.0, 30.0, 9.0), 2, area), (11, 17));
        assert_eq!(landing((10.0, 1.0, 30.0, 9.0), 2, area), (11, 11));
    }

    #[test]
    fn packets_ease_from_rest_to_rest() {
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(1.0), 1.0);
        assert!((ease(0.5) - 0.5).abs() < 1e-9);
        assert!(ease(0.1) < 0.1 && ease(0.9) > 0.9);
    }

    #[test]
    fn the_panel_leaves_the_canvas_room_or_steps_aside() {
        let (panel, rest) = split(Rect::new(0, 0, 160, 40));
        assert_eq!(panel, Some(Rect::new(0, 0, PANEL_W, 40)));
        assert_eq!(rest, Rect::new(PANEL_W, 0, 160 - PANEL_W, 40));
        let narrow = Rect::new(0, 0, 60, 40);
        assert_eq!(split(narrow), (None, narrow));
        for started in [false, true] {
            let m = TaskView {
                source: if started {
                    "a task".into()
                } else {
                    String::new()
                },
                ..TaskView::default()
            };
            let (intake, cond, tasks) = panel_boxes(Rect::new(0, 0, PANEL_W, 40), &m);
            assert_eq!(intake.height + cond.height + tasks.height, 40);
            assert_eq!(cond.y, intake.bottom());
            assert_eq!(tasks.y, cond.bottom());
            assert!(intake.height <= 9, "the intake stays compact");
            assert_eq!(cond.height, COND_H);
            assert!(tasks.height >= 20, "the tasks get the rest");
        }
        // A short panel shares what the intake leaves.
        let (intake, cond, tasks) = panel_boxes(Rect::new(0, 0, PANEL_W, 14), &TaskView::default());
        assert_eq!(intake.height + cond.height + tasks.height, 14);
        assert!(cond.height >= 3 && tasks.height >= 3);
    }

    /// Draw the lamaverse twice — the first frame tells the canvas its
    /// size, the fit lands on the second — and return the screen.
    fn draw_buf(plane: &mut Plane, apps: &mut [App]) -> Buffer {
        let backend = ratatui::backend::TestBackend::new(180, 50);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        for _ in 0..2 {
            terminal
                .draw(|f| crate::ui::plane::draw(f, plane, apps, None))
                .unwrap();
        }
        terminal.backend().buffer().clone()
    }

    /// [`draw_buf`]'s screen as text, a line per row.
    fn draw_text(plane: &mut Plane, apps: &mut [App]) -> String {
        let buf = draw_buf(plane, apps);
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn the_lamaverse_draws_the_task_panel_and_a_message() {
        use crate::state::task::{ChatLine, PlanView, PlannedView, Who};
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = "https://acme.sentry.io/issues/4821".into();
        m.phase = "RUNNING".into();
        m.chat.push(ChatLine {
            who: Who::Conductor,
            text: "fix the api, guard the ui".into(),
        });
        m.plan = Some(PlanView {
            agents: vec![
                PlannedView {
                    name: "api-forge".into(),
                    project_name: "proj".into(),
                    model: "claude-opus-4-7".into(),
                    ..PlannedView::default()
                },
                PlannedView {
                    name: "ui-guard".into(),
                    project_name: "web".into(),
                    model: "claude-sonnet-4-6".into(),
                    ..PlannedView::default()
                },
            ],
            ..PlanView::default()
        });
        m.now = 100.0;
        // Landed a second ago: its bubble lingers by the receiver.
        m.messages.push(MessageView {
            from: CONDUCTOR.into(),
            to: "api-forge".into(),
            kind: MsgKind::Assign,
            text: "always send meta".into(),
            shown_at: 100.0 - TRAVEL_SECS - 1.0,
        });
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("◈ INTAKE"), "{text}");
        assert!(text.contains("acme.sentry.io/issues/4821"), "{text}");
        assert!(
            text.contains("1 msgs") && text.contains("◈ 1 agent "),
            "the intake's status line: {text}"
        );
        assert!(text.contains("paste the next task"), "{text}");
        // A conductor waiting on you shows its question and takes the answer.
        plane.task.as_mut().unwrap().asking =
            Some("can't read the Jira link: paste SHOP-42".into());
        let text = draw_text(&mut plane, &mut apps);
        assert!(
            text.contains("conductor needs you") && text.contains("SHOP-42"),
            "{text}"
        );
        assert!(text.contains("answer the conductor"), "{text}");
        plane.task.as_mut().unwrap().asking = None;
        let text = draw_text(&mut plane, &mut apps);
        assert!(
            !text.contains("fix the api, guard the ui"),
            "the chat isn't drawn: {text}"
        );
        assert!(!text.contains("approve"), "no plan approval: {text}");
        assert!(text.contains("◈ CONDUCTOR"), "{text}");
        assert!(!text.contains("TEAM"), "no team overview: {text}");
        assert!(text.contains("◈ TASK"), "{text}");
        assert!(text.contains("⇄ 1 msgs"), "{text}");
        assert!(text.contains("conductor ▸ api-forge"), "{text}");
        assert!(text.contains("always send meta"), "{text}");
        assert!(
            text.contains("◈ proj") && text.contains("◈ 1 on task"),
            "the strip: {text}"
        );
        let hits = plane.task_hits;
        assert_eq!(hits.panel.map(|r| r.width), Some(PANEL_W));
        assert!(hits.intake.is_some_and(|r| r.height <= 9));
        assert!(hits.prompt.is_some() && hits.log_chip.is_some());
        assert!(hits.log.is_none(), "the log is closed by default");
    }

    #[test]
    fn a_long_draft_grows_the_intake_and_scrolls() {
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = "https://example.com/issue/1".into();
        m.intake.focused = true;
        m.intake.draft = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("line20"), "{text}");
        assert!(!text.contains("line3 "), "{text}");
        assert!(text.contains("↑ 7"), "{text}");
        assert!(text.contains("more"), "{text}");
        assert!(text.contains("⇧⏎ new line"), "{text}");
        let h = plane.task_hits.intake.unwrap().height;
        assert!(h > INTAKE_H_STARTED && h <= INTAKE_H_MAX, "{h}");

        plane.task.as_mut().unwrap().intake.scroll = 100;
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("line1"), "{text}");
        assert!(!text.contains("line20"), "{text}");
    }

    #[test]
    fn a_long_source_scrolls_when_the_draft_is_empty() {
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = (1..=12)
            .map(|i| format!("src{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        m.intake.focused = true;
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("src12"), "{text}");
        assert!(text.contains("↑ 4 more"), "{text}");
        plane.task.as_mut().unwrap().intake.scroll = 100;
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("│ src1 "), "{text}");
        assert!(!text.contains("│ src12 "), "{text}");
    }

    #[test]
    fn the_chooser_lists_owners_with_the_provider_lit() {
        use crate::state::task::ChoiceOwner;
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = "fix it".into();
        m.choosing = true;
        m.tasks = vec![row("t1", "the api drops meta", RowStatus::Waiting)];
        let owner = |name: &str, provider| ChoiceOwner {
            name: name.into(),
            project: "proj".into(),
            provider,
            model: "m".into(),
        };
        m.choice = Some(ChoiceView {
            task: "t1".into(),
            owners: vec![
                owner("api", Provider::Claude),
                owner("web", Provider::Codex),
            ],
            cursor: 1,
        });
        let buf = draw_buf(&mut plane, &mut apps);
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("who runs the api drops meta?"), "{text}");
        assert!(text.contains("▸ web"), "{text}");
        assert!(
            text.contains("❋ claude") && text.contains("❂ codex"),
            "{text}"
        );
        assert!(text.contains("⏎ launch"), "{text}");
        let lit = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].bg == fx::yellow())
            .count();
        assert!(lit >= 2 * 8, "one lit provider per owner: {lit}");
    }

    #[test]
    fn before_a_task_the_intake_is_a_drop_zone() {
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        plane.task.as_mut().unwrap().agents.clear();
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("⇣ drop · paste · type"), "{text}");
        assert!(text.contains("a teams thread — or just say it"), "{text}");
        assert!(text.contains("IDLE · ready"), "{text}");
        assert!(text.contains("ready for a task"), "{text}");
        assert!(text.contains("no tasks yet"), "{text}");
    }

    #[test]
    fn an_arrival_beams_then_bursts_then_flashes() {
        assert_eq!(arrival(-0.1), None);
        assert!(matches!(arrival(0.1), Some(Arrival::Beam(p)) if p > 0.0 && p < 1.0));
        assert!(matches!(arrival(0.6), Some(Arrival::Burst(_))));
        assert!(matches!(
            arrival(ARRIVE_SECS + 0.1),
            Some(Arrival::Flash(_))
        ));
        assert_eq!(arrival(ARRIVE_SECS + FLASH_SECS + 0.1), None);
    }

    #[test]
    fn a_lone_agent_arrives_on_a_beam_from_the_conductor() {
        let (mut plane, mut apps) = crate::state::plane::tests::task_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = "the api drops meta".into();
        m.now = 50.0;
        m.agents[0].joined = Some(50.0 - BEAM_SECS * 0.9);
        let text = draw_text(&mut plane, &mut apps);
        if fx::motion() {
            assert!(text.contains('✸'), "the beam's head: {text}");
            assert!(text.contains('━'), "the beam: {text}");
        }
        // Long after: nothing left of it.
        plane.task.as_mut().unwrap().now = 60.0;
        let text = draw_text(&mut plane, &mut apps);
        assert!(!text.contains('✸') && !text.contains('━'), "{text}");
    }

    #[test]
    fn a_planned_agent_draws_as_a_dashed_card_in_its_place() {
        let (mut plane, mut apps) = crate::state::plane::tests::planned_universe();
        let m = plane.task.as_mut().unwrap();
        m.source = "guard the ui".into();
        m.agents.clear();
        let p = &mut m.plan.as_mut().unwrap().agents[0];
        p.model = "claude-sonnet-4-6".into();
        p.role = "guards a missing meta".into();
        // Long after the plan came: drawn whole.
        m.now = 30.0;
        draw_text(&mut plane, &mut apps);
        for _ in 0..200 {
            plane.tick_motion(std::time::Duration::from_millis(16));
        }
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("◈ PLANNED OWNER"), "{text}");
        assert!(text.contains("❋ ui-guard"), "{text}");
        assert!(text.contains("sonnet-4-6"), "{text}");
        assert!(text.contains("guards a missing meta"), "{text}");
        assert!(text.contains("○ awaiting launch"), "{text}");
        assert!(text.contains('┆') && text.contains('┌'), "dashed: {text}");

        // A peer an owner asked for says so.
        let m = plane.task.as_mut().unwrap();
        m.plan.as_mut().unwrap().agents[0].parent = Some("api-owner".into());
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("◈ REQUESTED"), "{text}");
        assert!(text.contains("asked for by api-owner"), "{text}");
    }

    #[test]
    fn noise_resolves_into_the_text() {
        assert_eq!(decrypt("◈ PLANNED", 1.0, 7), "◈ PLANNED");
        assert_eq!(decrypt("◈ PLANNED", 0.0, 7), "");
        let half = decrypt("◈ PLANNED", 0.5, 7);
        assert!(half.starts_with("◈ PL"), "{half}");
        assert!(half.chars().count() <= "◈ PLANNED".chars().count());
    }

    #[test]
    fn the_radar_sweeps_fast_only_while_dispatching() {
        assert!(sweep_speed(true) > sweep_speed(false));
        assert!(sweep_speed(false) > 0.0, "idle, it still turns");
    }

    /// A task row with these parts.
    fn row(id: &str, title: &str, status: RowStatus) -> TaskRow {
        TaskRow {
            id: id.into(),
            title: title.into(),
            projects: vec!["proj".into()],
            status,
            ..TaskRow::default()
        }
    }

    #[test]
    fn the_panel_shows_the_conductor_dispatching_and_every_task() {
        use crate::state::task::ConductorView;
        let (mut plane, mut apps) = crate::state::plane::tests::owner_universe();
        let m = plane.task.as_mut().unwrap();
        m.conductor_model = Some("claude-sonnet-4-6".into());
        m.conductor = ConductorView {
            routed: 3,
            owners: 3,
            spent: Some(0.16),
            log: vec![
                "t1 → proj · 1 owner".into(),
                "t2 → proj · 1 owner".into(),
                "t4 → proj · 1 owner".into(),
            ],
            ..ConductorView::default()
        };
        let mut t1 = row("t1", "the api drops meta", RowStatus::Working(2));
        t1.msgs = 4;
        t1.goals = vec![
            ("api always sends meta".into(), true),
            ("ui survives missing meta".into(), false),
        ];
        let mut t4 = row("t4", "offline conversions", RowStatus::Done);
        t4.note = Some("MR !812".into());
        m.tasks = vec![
            t1,
            row("t2", "guard the shortlist", RowStatus::Waiting),
            row("t3", "flaky expiry job", RowStatus::Dispatching),
            t4,
        ];
        m.selected = Some("t1".into());
        let text = draw_text(&mut plane, &mut apps);
        assert!(text.contains("◈ CONDUCTOR ❋ sonnet-4-6"), "{text}");
        assert!(text.contains("IDLE · ready"), "{text}");
        assert!(text.contains("routed 3 tasks"), "{text}");
        assert!(text.contains("owners 3"), "{text}");
        assert!(text.contains("t4 → proj · 1 owner"), "the log: {text}");
        assert!(text.contains("◈ TASKS 3 running"), "{text}");
        assert!(text.contains("⚠ 1 waiting"), "{text}");
        assert!(text.contains("◈ the api drops meta"), "{text}");
        assert!(text.contains("→ proj ● 2 working · 4 msgs"), "{text}");
        assert!(text.contains("→ proj ⚠ waiting on you"), "{text}");
        assert!(text.contains("◉ conductor dispatching"), "{text}");
        assert!(text.contains("✓ offline conversions"), "{text}");
        assert!(text.contains("✓ done · MR !812"), "{text}");
        assert!(text.contains("☑ api always sends meta"), "picked: {text}");
        assert!(text.contains("☐ ui survives missing meta"), "{text}");
        assert!(text.contains("↑↓ select · ⏎ open owner"), "{text}");
        let rows: Vec<&str> = plane
            .task_hits
            .rows
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(rows, ["t1", "t2", "t3", "t4"]);
        let (_, r1) = &plane.task_hits.rows[0];
        assert_eq!(r1.height, 5, "the picked block opens up with its goals");

        // Unpicked, its goals fold away. Dispatching, the conductor says
        // what it reads.
        let m = plane.task.as_mut().unwrap();
        m.selected = None;
        m.conductor.dispatching = true;
        m.conductor.reading = Some("jira issue view SHOP-42".into());
        let text = draw_text(&mut plane, &mut apps);
        assert!(!text.contains("api always sends meta"), "{text}");
        assert!(text.contains("DISPATCHING"), "{text}");
        assert!(text.contains("◉ jira issue view SHOP-42"), "{text}");
    }

    #[test]
    fn a_report_to_a_task_lands_on_its_row() {
        let (mut plane, mut apps) = crate::state::plane::tests::owner_universe();
        let m = plane.task.as_mut().unwrap();
        m.now = 100.0;
        m.messages.push(MessageView {
            from: "api-owner".into(),
            to: format!("{TASK}t1"),
            kind: MsgKind::Report,
            text: "fix ready · !818".into(),
            shown_at: 100.0 - TRAVEL_SECS - 1.0,
        });
        let text = draw_text(&mut plane, &mut apps);
        let (_, r) = plane.task_hits.rows[0];
        let line = text
            .lines()
            .position(|l| l.contains("api-owner ▸ the api drops meta"))
            .unwrap_or_else(|| panic!("the bubble: {text}"));
        assert_eq!(line as u16, r.y, "beside its row: {text}");
        assert!(text.contains("fix ready · !818"), "{text}");
    }

    /// The cells of an arrival's beam on screen.
    fn beam_cells(buf: &Buffer) -> Vec<(u16, u16)> {
        let area = buf.area;
        (area.top()..area.bottom())
            .flat_map(|y| (area.left()..area.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| matches!(buf[(x, y)].symbol(), "━" | "✸"))
            .collect()
    }

    #[test]
    fn an_owner_beams_in_from_its_row_and_a_peer_from_its_owner() {
        if !fx::motion() {
            return;
        }
        let (mut plane, mut apps) = crate::state::plane::tests::owner_universe();
        let m = plane.task.as_mut().unwrap();
        m.now = 50.0;
        m.agents[1].joined = Some(50.0 - BEAM_SECS * 0.5);
        let buf = draw_buf(&mut plane, &mut apps);
        let owner = plane.task_card(&apps, "api-owner").unwrap();
        let (x, y, w, h) = plane.screen_rect(&owner).unwrap();
        let near = |&(cx, cy): &(u16, u16)| {
            let (cx, cy) = (cx as f64 + 0.5, cy as f64 + 0.5);
            cx >= x - 1.0 && cx <= x + w + 1.0 && cy >= y - 1.0 && cy <= y + h + 1.0
        };
        let cells = beam_cells(&buf);
        assert!(!cells.is_empty(), "the peer's beam");
        assert!(cells.iter().any(near), "it leaves its owner's card");

        // An owner's beam leaves its task's row: it enters the canvas at
        // its left edge.
        let m = plane.task.as_mut().unwrap();
        m.agents[1].joined = None;
        m.agents[0].joined = Some(50.0 - BEAM_SECS * 0.5);
        let buf = draw_buf(&mut plane, &mut apps);
        let left = plane.strip_area.unwrap().x;
        let cells = beam_cells(&buf);
        assert!(cells.iter().any(|&(cx, _)| cx == left), "{cells:?}");
    }
}
