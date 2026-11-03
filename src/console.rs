//! The session console: a side panel with a session's whole conversation and
//! a prompt line that types straight into the session's kitty window or tmux pane.
//!
//! Opened by double-clicking a card on the plane. A subagent's card opens its
//! session's console — a subagent has no terminal of its own; the session
//! can pass work on to it. What is typed here lands in the agent's own
//! terminal exactly as if typed there (see [`crate::kitty`]), so the reply
//! shows up in the transcript, and so here, like any other turn.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::kitty::{self, Reach};
use crate::provider::conversation::{Conversation, Turn};
use crate::provider::{Provider, Target};
use crate::state::App;
use crate::state::plane::Plane;
use crate::ui::fx;

/// How often the agent's screen is checked for a permission dialog.
const ASK_POLL: Duration = Duration::from_millis(1000);
/// How often the conversation is re-read and the window re-found.
const REFRESH: Duration = Duration::from_millis(500);
const RELOCATE: Duration = Duration::from_secs(3);
/// How long a status note stays up.
const NOTE: Duration = Duration::from_secs(3);

pub struct Console {
    /// The session's index among the plane's apps.
    pub session: usize,
    /// The session's id: what the plane remembers the console by.
    pub session_id_key: String,
    provider: Option<Provider>,
    session_id: String,
    project: String,
    cwd: Option<PathBuf>,
    model: Option<String>,
    /// Set when a subagent's card was opened: its name.
    via: Option<String>,
    /// When the session began, for matching it to the process running it.
    born: Option<std::time::SystemTime>,
    conv: Option<Conversation>,
    input: String,
    /// Cursor position in `input`, in chars.
    cursor: usize,
    /// Lines scrolled up from the newest.
    scroll: usize,
    reach: Reach,
    located: Instant,
    refreshed: Instant,
    /// A permission dialog on the agent's screen, waiting for an answer.
    ask: Option<crate::ask::Ask>,
    asked: Instant,
    /// Where the console's top-left sits on screen (may be off it), and the
    /// rows its approval choices were drawn on — for clicks.
    origin: std::cell::Cell<(i32, i32)>,
    ask_rows: std::cell::RefCell<Vec<(u16, usize)>>,
    /// The header's stop/resume button: its columns within the console, and
    /// which it is — for clicks.
    button: std::cell::Cell<Option<(u16, u16, Button)>>,
    note: Option<(String, bool, Instant)>,
    /// Where the console was drawn last frame, so the mouse over it scrolls
    /// it rather than the canvas behind.
    area: std::cell::Cell<Option<Rect>>,
    whole: std::cell::Cell<bool>,
}

/// The header button for what the session's terminal allows.
#[derive(Clone, Copy)]
enum Button {
    Stop,
    Resume,
}

/// What a key press asks of the console's owner.
pub enum Action {
    None,
    /// Hand the keyboard back to the canvas (`esc`); the console stays open.
    Blur,
}

impl Console {
    /// Open the console of the session behind host node `agent` of app `i`.
    pub fn open(i: usize, agent: &str, apps: &[App], plane: &Plane) -> Console {
        let app = &apps[i];
        let provider = app.session.provider();
        let session_id = app.current_session_id.clone();
        let group = plane.groups.get(plane.group_of(i));
        let root = app.session.spawn_order().next().map(str::to_string);
        let model = root
            .as_deref()
            .and_then(|r| app.session.agent(r))
            .and_then(|a| a.model.clone());
        let born = root
            .as_deref()
            .and_then(|r| app.session.agent(r))
            .and_then(|a| a.first_ts)
            .map(std::time::SystemTime::from);
        let via = (root.as_deref() != Some(agent)).then(|| {
            app.session
                .agent(agent)
                .and_then(|a| a.agent_type.clone().or_else(|| a.description.clone()))
                .unwrap_or_else(|| agent.to_string())
        });
        let conv = provider.and_then(|p| {
            let s = crate::provider::open(&Target::Id(session_id.clone()), Some(p)).ok()?;
            Some(Conversation::new(p, s.root.path))
        });
        let mut c = Console {
            session: i,
            session_id_key: app.current_session_id.clone(),
            provider,
            session_id,
            project: group.map(|g| g.name.clone()).unwrap_or_default(),
            cwd: plane.session_cwd(i).map(|p| p.to_path_buf()),
            model,
            via,
            born,
            conv,
            input: String::new(),
            cursor: 0,
            scroll: 0,
            reach: Reach::NoProcess,
            located: Instant::now(),
            refreshed: Instant::now(),
            ask: None,
            asked: Instant::now(),
            origin: std::cell::Cell::new((0, 0)),
            ask_rows: std::cell::RefCell::new(Vec::new()),
            button: std::cell::Cell::new(None),
            note: None,
            area: std::cell::Cell::new(None),
            whole: std::cell::Cell::new(false),
        };
        if let Some(conv) = c.conv.as_mut() {
            conv.refresh();
        }
        c.locate();
        c
    }

    fn locate(&mut self) {
        let Some(p) = self.provider else {
            self.reach = Reach::NoProcess;
            return;
        };
        let hints = self.conv.as_ref().map(|c| c.snippets()).unwrap_or_default();
        self.reach = kitty::locate(p, &self.session_id, self.cwd.as_deref(), &hints, self.born);
        self.located = Instant::now();
    }

    /// Per-frame upkeep: new turns from the transcript, and a fresh look for
    /// the window now and then (it can open, close or change).
    pub fn tick(&mut self) {
        if self.refreshed.elapsed() >= REFRESH {
            if let Some(conv) = self.conv.as_mut()
                && conv.refresh()
                && self.scroll > 0
            {
                // Hold the reader's place while new lines arrive below.
                self.scroll += 1;
            }
            self.refreshed = Instant::now();
        }
        if self.located.elapsed() >= RELOCATE {
            self.locate();
        }
        // The agent's screen, for a permission dialog (it is never in the
        // transcript).
        if self.asked.elapsed() >= ASK_POLL {
            self.asked = Instant::now();
            self.ask = match &self.reach {
                Reach::Ready(w) => kitty::screen(w).and_then(|s| crate::ask::parse(&s)),
                _ => None,
            };
        }
    }

    /// Whether the agent is waiting on a permission answer.
    pub fn needs_approval(&self) -> bool {
        self.ask.is_some()
    }

    /// Answer the dialog with choice number `n` (1-based).
    pub fn answer(&mut self, n: usize) {
        let (Some(ask), Reach::Ready(w)) = (self.ask.clone(), self.reach.clone()) else {
            return;
        };
        let Some(choice) = ask.options.iter().find(|c| c.number as usize == n) else {
            return;
        };
        match kitty::answer(&w, &ask, choice) {
            Ok(()) => {
                self.say(format!("✓ answered: {}", choice.label), true);
                self.ask = None;
                // Look again soon: the next dialog may follow straight away.
                self.asked = Instant::now() - ASK_POLL + Duration::from_millis(600);
            }
            Err(e) => self.say(format!("✗ {e}"), false),
        }
    }

    /// Where the console's top-left corner sits on screen this frame.
    pub fn set_origin(&self, x: i32, y: i32) {
        self.origin.set((x, y));
    }

    /// The approval choice under a terminal cell, if a click hit one.
    pub fn ask_hit(&self, column: u16, row: u16) -> Option<usize> {
        if !self.contains(column, row) {
            return None;
        }
        let (_, oy) = self.origin.get();
        let local = row as i32 - oy;
        self.ask_rows
            .borrow()
            .iter()
            .find(|(r, _)| *r as i32 == local)
            .map(|&(_, n)| n)
    }

    /// Where the console's top-left corner sat on screen last frame.
    pub fn origin(&self) -> (i32, i32) {
        self.origin.get()
    }

    /// Put `text` in the prompt at the cursor. The prompt is one line, as is
    /// what reaches the agent: line breaks become spaces.
    pub fn paste(&mut self, text: &str) {
        let text: String = text
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let at = self
            .input
            .char_indices()
            .nth(self.cursor)
            .map(|(b, _)| b)
            .unwrap_or(self.input.len());
        self.input.insert_str(at, &text);
        self.cursor += text.chars().count();
    }

    pub fn say(&mut self, text: impl Into<String>, ok: bool) {
        self.note = Some((text.into(), ok, Instant::now()));
    }

    fn send(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.locate();
        let Reach::Ready(w) = self.reach.clone() else {
            self.say("not sent — no window to type into", false);
            return;
        };
        match kitty::send(&w, &text) {
            Ok(()) => {
                self.say(format!("✓ typed into {}", w.label()), true);
                self.input.clear();
                self.cursor = 0;
                self.scroll = 0;
            }
            Err(e) => self.say(format!("✗ {e}"), false),
        }
    }

    fn jump(&mut self) {
        self.locate();
        match self.reach.clone() {
            Reach::Ready(w) => match kitty::focus(&w) {
                Ok(()) => self.say(format!("▣ focused {}", w.label()), true),
                Err(e) => self.say(format!("✗ {e}"), false),
            },
            _ => self.say("no window to jump to", false),
        }
    }

    /// Interrupt the agent's turn (Escape in its window).
    fn stop(&mut self) {
        self.locate();
        match self.reach.clone() {
            Reach::Ready(w) => match kitty::interrupt(&w) {
                Ok(()) => self.say(format!("■ sent stop to {}", w.label()), true),
                Err(e) => self.say(format!("✗ {e}"), false),
            },
            _ => self.say("no window to stop", false),
        }
    }

    /// Reopen the closed session in a new tab.
    fn resume(&mut self) {
        self.locate();
        if !matches!(self.reach, Reach::NoProcess) {
            self.say("not resumed — the session is already open", false);
            return;
        }
        let (Some(p), Some(dir)) = (self.provider, self.cwd.clone()) else {
            self.say("can't resume — project folder unknown", false);
            return;
        };
        let title = format!(
            "{} {}",
            self.project,
            self.session_id.chars().take(8).collect::<String>()
        );
        // A task's agent comes back with its bus, a recorded session on its
        // model; any other session as is.
        let resumed = match crate::registry::find(p, &self.session_id) {
            Some(e) => crate::registry::resume(&e),
            None => crate::task::launch::resume_agent(&self.session_id)
                .unwrap_or_else(|| kitty::resume(&dir, p, &self.session_id, &title)),
        };
        match resumed {
            Ok(w) => {
                self.say(format!("▶ resumed in {}", w.label()), true);
                // Look again in a couple of seconds, once the agent is up.
                self.located = Instant::now()
                    .checked_sub(RELOCATE - Duration::from_secs(2))
                    .unwrap_or_else(Instant::now);
            }
            Err(e) => self.say(format!("✗ {e}"), false),
        }
    }

    /// A left click at a terminal cell: the stop or resume button, if hit.
    /// Whether it was consumed.
    pub fn click(&mut self, column: u16, row: u16) -> bool {
        let Some(a) = self.area.get().filter(|_| self.whole.get()) else {
            return false;
        };
        let Some((from, to, button)) = self.button.get() else {
            return false;
        };
        if row != a.y || column < a.x + from || column >= a.x + to {
            return false;
        }
        match button {
            Button::Stop => self.stop(),
            Button::Resume => self.resume(),
        }
        true
    }

    /// Handle a key. The console owns the keyboard while open: everything
    /// typed goes to the prompt line.
    pub fn key(&mut self, key: &KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input.chars().count();
        let byte = |s: &str, ci: usize| s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len());
        match key.code {
            KeyCode::Esc => return Action::Blur,
            KeyCode::Enter => self.send(),
            KeyCode::Char('t') if ctrl => self.jump(),
            KeyCode::Char('x') if ctrl && matches!(self.reach, Reach::Ready(_)) => self.stop(),
            KeyCode::Char('r') if ctrl && matches!(self.reach, Reach::NoProcess) => self.resume(),
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char('v') if ctrl => match crate::clipboard::paste() {
                Some(text) => self.paste(&text),
                None => self.say("✗ nothing to paste (no wl-paste, xclip or pbpaste)", false),
            },
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = len,
            KeyCode::Char(c @ '1'..='9')
                if !ctrl && self.ask.is_some() && self.input.is_empty() =>
            {
                self.answer(c as usize - '0' as usize);
            }
            KeyCode::Char(c) if !ctrl => {
                let at = byte(&self.input, self.cursor);
                self.input.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = byte(&self.input, self.cursor - 1);
                self.input.remove(at);
                self.cursor -= 1;
            }
            KeyCode::Delete if self.cursor < len => {
                let at = byte(&self.input, self.cursor);
                self.input.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up => self.scroll += 1,
            KeyCode::Down => self.scroll = self.scroll.saturating_sub(1),
            _ => {}
        }
        Action::None
    }

    /// Draw the console into `area` (the plane's right side).
    /// Where the console sits on screen this frame (set by whoever draws it),
    /// for the mouse: the visible part, and whether that is all of it (the
    /// close button only counts when its corner is on screen).
    pub fn set_screen(&self, rect: Option<Rect>, whole: bool) {
        self.area.set(rect);
        self.whole.set(whole);
    }

    /// Whether a terminal cell is the console's close button (`✕`, top right).
    pub fn close_hit(&self, column: u16, row: u16) -> bool {
        self.whole.get()
            && self.area.get().is_some_and(|a| {
                a.width >= 8 && row == a.y && column + 5 >= a.right() && column + 1 < a.right()
            })
    }

    /// Whether a terminal cell is the console's resize grip (`◢`, bottom
    /// right). Like the close button, only when that corner is on screen.
    pub fn grip_hit(&self, column: u16, row: u16) -> bool {
        self.whole.get()
            && self
                .area
                .get()
                .is_some_and(|a| a.width > 0 && column + 1 == a.right() && row + 1 == a.bottom())
    }

    /// Whether a terminal cell lies on the console.
    pub fn contains(&self, column: u16, row: u16) -> bool {
        self.area
            .get()
            .is_some_and(|a| column >= a.x && column < a.right() && row >= a.y && row < a.bottom())
    }

    /// Lines scrolled up from the newest.
    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// Whether a terminal cell is the console's header row: a drag there
    /// moves its project, a drag anywhere else scrolls its history.
    pub fn on_header(&self, column: u16, row: u16) -> bool {
        self.whole.get()
            && self
                .area
                .get()
                .is_some_and(|a| row == a.y && column >= a.x && column < a.right())
    }

    /// A mouse-wheel notch over the console: three lines of history.
    pub fn wheel(&mut self, up: bool) {
        self.scroll = if up {
            self.scroll + 3
        } else {
            self.scroll.saturating_sub(3)
        };
    }

    /// Draw the console into `area` of `buf`. `focused`: this console takes
    /// the keyboard. Small (zoomed out), it shrinks to its header and latest
    /// exchange; tiny, to a block.
    pub fn render_into(&self, buf: &mut Buffer, area: Rect, focused: bool, finished: bool) {
        let palette = fx::palette();
        self.button.set(None);
        if area.width < 8 || area.height < 3 {
            let fill = Style::default().bg(if focused {
                palette.accent
            } else {
                palette.muted
            });
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    buf[(x, y)].set_char(' ').set_style(fill);
                }
            }
            return;
        }
        if area.width < 34 || area.height < 12 {
            self.render_compact(buf, area, focused, finished);
        } else {
            self.render_full(buf, area, focused, finished);
        }
        // The resize grip: the canvas draws one on every resizable node, but
        // the console is painted over it, so it carries its own.
        buf[(area.right() - 1, area.bottom() - 1)]
            .set_char('◢')
            .set_style(Style::default().fg(palette.accent).bg(palette.surface));
    }

    fn render_full(&self, buf: &mut Buffer, area: Rect, focused: bool, finished: bool) {
        let palette = fx::palette();
        let bg = Style::default().bg(fx::mix(palette.surface, palette.canvas_bg, 0.35));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(bg);
            }
        }
        let tint = match self.provider {
            Some(Provider::Claude) => fx::provider_color(true),
            Some(Provider::Codex) => fx::provider_color(false),
            None => palette.subtle,
        };
        let asking = self.ask.is_some();
        frame_box(
            buf,
            area,
            |i, n| {
                if asking {
                    alert_blink()
                } else if finished {
                    crate::ui::nodes::done_blink()
                } else {
                    border(i, n, tint, focused)
                }
            },
            bg,
        );
        let bold = Modifier::BOLD;
        let w = area.width;

        // Header on the top border: who, where, and where prompts land.
        let (mark, name) = match self.provider {
            Some(Provider::Claude) => ("❋", "claude"),
            Some(Provider::Codex) => ("❂", "codex"),
            None => ("✦", "session"),
        };
        let mut x = put(
            buf,
            area.x + 2,
            area.y,
            " ● ",
            bg.fg(palette.success).add_modifier(bold),
            area.right(),
        );
        x = put(
            buf,
            x,
            area.y,
            &format!("{mark} {name} "),
            bg.fg(tint).add_modifier(bold),
            area.right(),
        );
        x = put(
            buf,
            x,
            area.y,
            &format!("· {} ", self.project),
            bg.fg(palette.text).add_modifier(bold),
            area.right(),
        );
        let id8: String = self.session_id.chars().take(8).collect();
        let meta = match &self.model {
            Some(m) => format!("· {} · {id8} ", m.trim_start_matches("claude-")),
            None => format!("· {id8} "),
        };
        let (reach, reach_color) = match &self.reach {
            Reach::Ready(win) => (
                format!(
                    " ▣ {}{} ",
                    win.label(),
                    win.tty
                        .as_deref()
                        .map(|t| format!(" · {}", t.trim_start_matches("/dev/")))
                        .unwrap_or_default()
                ),
                palette.success,
            ),
            Reach::RemoteOff => (" ✗ kitty remote control off ".to_string(), palette.error),
            Reach::NoProcess => (" ◇ terminal closed ".to_string(), palette.subtle),
            Reach::NotKitty => (" ✗ not in kitty or tmux ".to_string(), palette.error),
            Reach::Ambiguous(n) => (format!(" ? {n} windows, can't tell "), fx::yellow()),
        };
        // Leave the top-right corner to the close button.
        put(
            buf,
            area.right().saturating_sub(4),
            area.y,
            " ✕ ",
            bg.fg(palette.error).add_modifier(bold),
            area.right(),
        );
        let (reach, reach_color) = if self.ask.is_some() {
            (" ⚠ needs approval ".to_string(), alert())
        } else {
            (reach, reach_color)
        };
        let reach_x = area.right().saturating_sub(5 + reach.width() as u16);
        // A stop / resume button just left of the reach label.
        let button = match &self.reach {
            Reach::Ready(_) => Some((" ■ stop ", Button::Stop, palette.error)),
            Reach::NoProcess => Some((" ▶ resume ", Button::Resume, palette.success)),
            _ => None,
        };
        let mut left_end = reach_x;
        self.button.set(None);
        if let Some((label, kind, color)) = button {
            let bx = reach_x.saturating_sub(label.width() as u16);
            if bx > x + 4 {
                put(
                    buf,
                    bx,
                    area.y,
                    label,
                    bg.fg(color).add_modifier(bold),
                    reach_x,
                );
                self.button.set(Some((bx - area.x, reach_x - area.x, kind)));
                left_end = bx;
            }
        }
        if x + meta.width() as u16 + 1 < left_end {
            put(buf, x, area.y, &meta, bg.fg(palette.subtle), left_end);
        }
        put(
            buf,
            reach_x,
            area.y,
            &reach,
            bg.fg(reach_color).add_modifier(bold),
            area.right().saturating_sub(4),
        );

        let inner_x = area.x + 2;
        let inner_w = w.saturating_sub(4);
        let mut top = area.y + 1;
        if let Some(via) = &self.via {
            let line = format!(
                "{via} is a subagent of this session — prompts go to the session, which can pass them on."
            );
            put(
                buf,
                inner_x,
                top,
                &truncate(&line, inner_w as usize),
                bg.fg(palette.subtle),
                area.right() - 1,
            );
            top += 1;
        }
        // Why prompts can't go anywhere, when they can't.
        let blocked: Vec<(String, Color)> = match &self.reach {
            Reach::Ready(_) => Vec::new(),
            Reach::RemoteOff => {
                let mut v = vec![(
                    "kitty remote control is off — add to ~/.config/kitty/kitty.conf, restart kitty:".to_string(),
                    palette.error,
                )];
                v.extend(
                    kitty::SETUP
                        .iter()
                        .map(|l| (format!("    {l}"), fx::yellow())),
                );
                v
            }
            Reach::NoProcess => vec![(
                "this session's terminal is closed — there is nothing to type into.".to_string(),
                palette.subtle,
            )],
            Reach::NotKitty => vec![(
                "this session runs outside kitty and tmux — prompts can only be typed into kitty windows or tmux panes."
                    .to_string(),
                palette.error,
            )],
            Reach::Ambiguous(n) => vec![(
                format!(
                    "{n} {name} windows are open in {}; none shows this session's latest reply yet, so lama won't guess. It retries every few seconds.",
                    self.project
                ),
                fx::yellow(),
            )],
        };
        for (line, color) in &blocked {
            for l in wrap(line, inner_w as usize) {
                put(buf, inner_x, top, &l, bg.fg(*color), area.right() - 1);
                top += 1;
            }
        }
        if !blocked.is_empty() {
            top += 1;
        }

        // The conversation, newest at the bottom, scrollable.
        let prompt_y = area.bottom().saturating_sub(5);
        self.ask_rows.borrow_mut().clear();
        let history_end = match &self.ask {
            Some(ask) => self.render_ask(buf, area, top, prompt_y, ask),
            None => prompt_y,
        };
        let room = history_end.saturating_sub(top) as usize;
        let lines = self.lines(inner_w.saturating_sub(4) as usize, room + self.scroll);
        let scroll = self.scroll.min(lines.len().saturating_sub(room));
        let end = lines.len() - scroll;
        let start = end.saturating_sub(room);
        // Bottom-anchored: the newest line sits just above the prompt.
        let pad = room - (end - start);
        for (k, line) in lines[start..end].iter().enumerate() {
            let y = top + (pad + k) as u16;
            let (mark, mark_color) = line.mark.clone();
            put(
                buf,
                inner_x,
                y,
                &mark,
                bg.fg(mark_color).add_modifier(bold),
                area.right() - 1,
            );
            put(
                buf,
                inner_x + 3,
                y,
                &line.text,
                bg.fg(line.color).add_modifier(line.style),
                area.right() - 1,
            );
        }
        if start > 0 {
            put(
                buf,
                area.right().saturating_sub(16),
                top,
                " ↑ PgUp earlier ",
                bg.fg(palette.muted),
                area.right() - 1,
            );
        }
        if scroll > 0 {
            put(
                buf,
                area.right().saturating_sub(16),
                prompt_y - 1,
                " ↓ PgDn newer ",
                bg.fg(palette.muted),
                area.right() - 1,
            );
        }
        if self.conv.is_none() {
            put(
                buf,
                inner_x,
                top,
                "transcript not found",
                bg.fg(palette.muted),
                area.right() - 1,
            );
        }

        // The prompt line.
        let ready = matches!(self.reach, Reach::Ready(_));
        let pbg = Style::default().bg(fx::mix(palette.surface, palette.accent, 0.08));
        let pbox = Rect::new(area.x + 1, prompt_y, w - 2, 3);
        for y in pbox.top()..pbox.bottom() {
            for x in pbox.left()..pbox.right() {
                buf[(x, y)].set_char(' ').set_style(pbg);
            }
        }
        let edge = if ready { palette.accent } else { palette.muted };
        frame_box(buf, pbox, |_, _| edge, pbg);
        put(
            buf,
            pbox.x + 2,
            pbox.y,
            if ready {
                " prompt "
            } else {
                " prompt (can't send) "
            },
            pbg.fg(edge).add_modifier(bold),
            pbox.right(),
        );
        put(
            buf,
            pbox.x + 2,
            pbox.y + 1,
            ">",
            pbg.fg(edge).add_modifier(bold),
            pbox.right(),
        );
        let field = pbox.width.saturating_sub(18) as usize;
        let (shown, cur) = window_input(&self.input, self.cursor, field);
        if self.input.is_empty() {
            let hint = if ready && !focused {
                "click here to type".to_string()
            } else if ready {
                format!("ask {name} something…")
            } else {
                "prompts are off for this session".to_string()
            };
            put(
                buf,
                pbox.x + 4,
                pbox.y + 1,
                &hint,
                pbg.fg(palette.muted),
                pbox.right() - 12,
            );
        } else {
            put(
                buf,
                pbox.x + 4,
                pbox.y + 1,
                &shown,
                pbg.fg(palette.text),
                pbox.right() - 12,
            );
        }
        if focused && ready && ((fx::now_ms() / 500).is_multiple_of(2) || !fx::motion()) {
            let cx = pbox.x + 4 + cur as u16;
            if cx < pbox.right() - 12 {
                buf[(cx, pbox.y + 1)].set_style(pbg.bg(palette.accent).fg(palette.canvas_bg));
            }
        }
        put(
            buf,
            pbox.right().saturating_sub(12),
            pbox.y + 1,
            "[ ⏎ send ]",
            pbg.fg(if ready {
                palette.success
            } else {
                palette.muted
            })
            .add_modifier(bold),
            pbox.right() - 1,
        );

        // Footer, and the latest status note on the right.
        let fy = area.bottom() - 2;
        put(
            buf,
            inner_x,
            fy,
            if focused {
                match self.reach {
                    Reach::Ready(_) => {
                        "⏎ send · ⌃x stop · ⌃t jump to kitty · wheel scroll · esc back"
                    }
                    Reach::NoProcess => {
                        "⌃r resume · drag copy · ⌃v paste · wheel scroll · esc back"
                    }
                    _ => "⏎ send · ⌃t jump to kitty · drag copy · ⌃v paste · esc back",
                }
            } else {
                "click to type · drag to copy · wheel scroll · drag ◢ to resize · ✕ close"
            },
            bg.fg(palette.muted),
            area.right() - 1,
        );
        if let Some((text, ok, at)) = &self.note
            && at.elapsed() < NOTE
        {
            let style = Style::default()
                .bg(if *ok { palette.success } else { palette.error })
                .fg(palette.canvas_bg)
                .add_modifier(bold);
            let label = format!(" {text} ");
            put(
                buf,
                area.right().saturating_sub(2 + label.width() as u16),
                area.bottom() - 1,
                &label,
                style,
                area.right() - 1,
            );
        }
    }

    /// The last `want` display lines of the conversation, wrapped to `width`.
    /// Walks back from the newest turn, so a long history costs only what
    /// is on screen.
    fn lines(&self, width: usize, want: usize) -> Vec<Line> {
        let palette = fx::palette();
        let Some(conv) = &self.conv else {
            return Vec::new();
        };
        let width = width.max(10);
        let mut out: Vec<Line> = Vec::new();
        for turn in conv.turns.iter().rev() {
            let mut block: Vec<Line> = Vec::new();
            match turn {
                Turn::User(text) => {
                    for (k, l) in wrap(text, width).into_iter().enumerate() {
                        block.push(Line {
                            mark: (if k == 0 { "◆" } else { "" }.to_string(), palette.accent),
                            text: l,
                            color: palette.text,
                            style: Modifier::BOLD,
                        });
                    }
                    block.push(Line::blank());
                }
                Turn::Agent(text) => {
                    for (k, l) in wrap(&plain(text), width).into_iter().enumerate() {
                        block.push(Line {
                            mark: (if k == 0 { "»" } else { "" }.to_string(), fx::violet()),
                            text: l,
                            color: fx::mix(palette.text, palette.surface, 0.15),
                            style: Modifier::empty(),
                        });
                    }
                    block.push(Line::blank());
                }
                Turn::Tool { name, summary, ok } => {
                    let (m, c) = match ok {
                        Some(true) => ("✓".to_string(), palette.success),
                        Some(false) => ("✗".to_string(), palette.error),
                        None => (fx::spinner().to_string(), fx::cyan()),
                    };
                    let text = truncate(&format!("{name}  {summary}"), width);
                    let color = if ok.is_none() {
                        fx::yellow()
                    } else {
                        palette.subtle
                    };
                    block.push(Line {
                        mark: (m, c),
                        text,
                        color,
                        style: Modifier::empty(),
                    });
                }
            }
            out.extend(block.into_iter().rev());
            if out.len() >= want {
                break;
            }
        }
        out.reverse();
        // No trailing gap under the newest turn.
        while out
            .last()
            .is_some_and(|l| l.text.is_empty() && l.mark.0.is_empty())
        {
            out.pop();
        }
        out
    }
}

/// Bright neon red: an agent is waiting on you.
fn alert() -> Color {
    Color::Rgb(255, 28, 78)
}

/// The waiting-on-you red, blinking hard (on/off, not a soft pulse) so it
/// catches the eye even zoomed out among many cards.
fn alert_blink() -> Color {
    if !fx::motion() || (fx::now_ms() / 350).is_multiple_of(2) {
        alert()
    } else {
        Color::Rgb(96, 0, 28)
    }
}

/// A console's border: neon tinted toward its provider; brighter, and
/// whiter, when it takes the keyboard.
fn border(i: usize, n: usize, tint: Color, focused: bool) -> Color {
    let base = if fx::motion() {
        fx::mix(
            fx::neon_cycle(5000, i as u64 * 5000 / n.max(1) as u64),
            tint,
            0.4,
        )
    } else {
        tint
    };
    if focused { fx::glow(base, 0.35) } else { base }
}

impl Console {
    /// The approval panel, drawn upward from just above the prompt box: the
    /// question, what it is about, and one row per choice. Returns the row
    /// the history must stop above.
    fn render_ask(
        &self,
        buf: &mut Buffer,
        area: Rect,
        top: u16,
        prompt_y: u16,
        ask: &crate::ask::Ask,
    ) -> u16 {
        let palette = fx::palette();
        let rows = (2 + ask.detail.len() + ask.options.len()) as u16 + 1;
        let y0 = prompt_y.saturating_sub(rows).max(top);
        let bg = Style::default().bg(fx::mix(alert(), palette.surface, 0.88));
        let panel = Rect::new(area.x + 1, y0, area.width - 2, prompt_y - y0);
        for y in panel.top()..panel.bottom() {
            for x in panel.left()..panel.right() {
                buf[(x, y)].set_char(' ').set_style(bg);
            }
        }
        frame_box(buf, panel, |_, _| alert_blink(), bg);
        let limit = panel.right() - 1;
        put(
            buf,
            panel.x + 2,
            panel.y,
            " ⚠ the agent asks ",
            bg.fg(alert()).add_modifier(Modifier::BOLD),
            limit,
        );
        let mut y = panel.y + 1;
        for d in &ask.detail {
            if y + 1 >= panel.bottom() {
                break;
            }
            put(
                buf,
                panel.x + 2,
                y,
                &truncate(d, (panel.width - 4) as usize),
                bg.fg(palette.subtle),
                limit,
            );
            y += 1;
        }
        if y + 1 < panel.bottom() {
            put(
                buf,
                panel.x + 2,
                y,
                &truncate(&ask.question, (panel.width - 4) as usize),
                bg.fg(palette.text).add_modifier(Modifier::BOLD),
                limit,
            );
            y += 1;
        }
        for c in &ask.options {
            if y + 1 >= panel.bottom() {
                break;
            }
            let key = format!(" {} ", c.number);
            let x = put(
                buf,
                panel.x + 2,
                y,
                &key,
                Style::default()
                    .bg(alert())
                    .fg(palette.canvas_bg)
                    .add_modifier(Modifier::BOLD),
                limit,
            );
            put(
                buf,
                x + 1,
                y,
                &truncate(&c.label, (limit - x).saturating_sub(2) as usize),
                bg.fg(palette.text),
                limit,
            );
            self.ask_rows
                .borrow_mut()
                .push((y - area.y, c.number as usize));
            y += 1;
        }
        put(
            buf,
            panel.x + 2,
            panel.bottom() - 1,
            " press a number or click a choice ",
            bg.fg(palette.subtle),
            limit,
        );
        y0
    }

    /// Zoomed out: the header and the latest exchange, enough to watch it.
    fn render_compact(&self, buf: &mut Buffer, area: Rect, focused: bool, finished: bool) {
        let palette = fx::palette();
        let bg = Style::default().bg(fx::mix(palette.surface, palette.canvas_bg, 0.35));
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(bg);
            }
        }
        let tint = match self.provider {
            Some(Provider::Claude) => fx::provider_color(true),
            Some(Provider::Codex) => fx::provider_color(false),
            None => palette.subtle,
        };
        let asking = self.ask.is_some();
        frame_box(
            buf,
            area,
            |i, n| {
                if asking {
                    alert_blink()
                } else if finished {
                    crate::ui::nodes::done_blink()
                } else {
                    border(i, n, tint, focused)
                }
            },
            bg,
        );
        let mark = match self.provider {
            Some(Provider::Claude) => "❋",
            Some(Provider::Codex) => "❂",
            None => "✦",
        };
        let limit = area.right().saturating_sub(4);
        let x = put(
            buf,
            area.x + 1,
            area.y,
            &format!(" {mark} "),
            bg.fg(tint).add_modifier(Modifier::BOLD),
            limit,
        );
        put(
            buf,
            x,
            area.y,
            &format!("{} ", self.project),
            bg.fg(palette.text).add_modifier(Modifier::BOLD),
            limit,
        );
        put(
            buf,
            area.right().saturating_sub(4),
            area.y,
            " ✕ ",
            bg.fg(palette.error),
            area.right(),
        );
        let inner = area.width.saturating_sub(4) as usize;
        let rows = area.height.saturating_sub(2) as usize;
        let lines = self.lines(inner.saturating_sub(3), rows);
        let start = lines.len().saturating_sub(rows);
        for (k, line) in lines[start..].iter().enumerate() {
            let y = area.y + 1 + k as u16;
            put(
                buf,
                area.x + 2,
                y,
                &line.mark.0,
                bg.fg(line.mark.1),
                area.right() - 1,
            );
            put(
                buf,
                area.x + 4,
                y,
                &line.text,
                bg.fg(line.color),
                area.right() - 1,
            );
        }
        if self.ask.is_some() {
            put(
                buf,
                area.x + 2,
                area.bottom() - 1,
                " ⚠ needs approval — zoom in to answer ",
                bg.fg(alert_blink()).add_modifier(Modifier::BOLD),
                area.right() - 1,
            );
        } else if focused {
            put(
                buf,
                area.x + 2,
                area.bottom() - 1,
                " ⌨ typing here ",
                bg.fg(palette.accent).add_modifier(Modifier::BOLD),
                area.right() - 1,
            );
        }
    }
}

struct Line {
    mark: (String, Color),
    text: String,
    color: Color,
    style: Modifier,
}

impl Line {
    fn blank() -> Line {
        Line {
            mark: (String::new(), Color::Reset),
            text: String::new(),
            color: Color::Reset,
            style: Modifier::empty(),
        }
    }
}

/// Write `text` from `x` on row `y`, stopping short of `limit`. Returns the
/// column after the last cell written.
fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, limit: u16) -> u16 {
    if x >= limit {
        return x;
    }
    let (end, _) = buf.set_stringn(x, y, text, (limit - x) as usize, style);
    end
}

/// A rounded frame whose colour is a function of the walk index.
fn frame_box(buf: &mut Buffer, area: Rect, color: impl Fn(usize, usize) -> Color, bg: Style) {
    let (l, t, r, b) = (area.left(), area.top(), area.right() - 1, area.bottom() - 1);
    let mut cells = Vec::new();
    for x in l..=r {
        cells.push((
            x,
            t,
            if x == l {
                '╭'
            } else if x == r {
                '╮'
            } else {
                '─'
            },
        ));
    }
    for y in t + 1..b {
        cells.push((r, y, '│'));
    }
    for x in (l..=r).rev() {
        cells.push((
            x,
            b,
            if x == r {
                '╯'
            } else if x == l {
                '╰'
            } else {
                '─'
            },
        ));
    }
    for y in (t + 1..b).rev() {
        cells.push((l, y, '│'));
    }
    let n = cells.len();
    for (i, (x, y, ch)) in cells.into_iter().enumerate() {
        buf[(x, y)].set_char(ch).set_style(bg.fg(color(i, n)));
    }
}

fn truncate(s: &str, max: usize) -> String {
    crate::ui::truncate(s, max)
}

/// Markdown emphasis and code marks only cost cells in a terminal panel;
/// fence lines are dropped, their contents kept.
fn plain(text: &str) -> String {
    text.lines()
        .filter(|l| !l.trim_start().starts_with("```"))
        .map(|l| l.replace("**", "").replace('`', ""))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Word-wrap `text` to `width` display columns; paragraphs stay apart and a
/// word longer than the line is split.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        let mut w = 0;
        for word in para.split_whitespace() {
            let ww = word.width();
            if w > 0 && w + 1 + ww > width {
                out.push(std::mem::take(&mut line));
                w = 0;
            }
            if ww > width {
                for ch in word.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if w + cw > width {
                        out.push(std::mem::take(&mut line));
                        w = 0;
                    }
                    line.push(ch);
                    w += cw;
                }
                continue;
            }
            if w > 0 {
                line.push(' ');
                w += 1;
            }
            line.push_str(word);
            w += ww;
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// The slice of `input` that fits `width` columns with the cursor in view,
/// and the cursor's column within it.
fn window_input(input: &str, cursor: usize, width: usize) -> (String, usize) {
    let chars: Vec<char> = input.chars().collect();
    let width = width.max(1);
    let start = cursor.saturating_sub(width.saturating_sub(1));
    let shown: String = chars.iter().skip(start).take(width).collect();
    (shown, cursor - start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_paragraphs_and_splits_long_words() {
        assert_eq!(wrap("aa bb cc", 5), vec!["aa bb", "cc"]);
        assert_eq!(wrap("one\ntwo", 10), vec!["one", "two"]);
        assert_eq!(wrap("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn markdown_marks_are_dropped() {
        assert_eq!(
            plain("**Catch:** use `x`\n```json\n{}\n```"),
            "Catch: use x\n{}"
        );
    }

    #[test]
    fn the_cursor_stays_in_view() {
        assert_eq!(window_input("hello", 5, 10), ("hello".to_string(), 5));
        let (shown, cur) = window_input("abcdefghij", 10, 4);
        assert_eq!((shown.as_str(), cur), ("hij", 3));
    }
}
