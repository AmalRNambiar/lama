//! The sessions drawer (`h`): every session lama's record keeps
//! ([`crate::registry`]), open or closed, wherever it was started. Pick a
//! closed one to pick it back up in a new tab — resumed, or started
//! again from its first prompt if its agent never wrote anything.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::registry::{Closed, Entry};
use crate::ui::fx;

pub struct Drawer {
    entries: Vec<Entry>,
    cursor: usize,
    note: Option<(String, bool)>,
}

/// What a key press asks of the drawer's owner.
pub enum Action {
    None,
    Close,
    /// Open this session's console, if it is on the canvas.
    Show(String),
}

impl Default for Drawer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drawer {
    pub fn new() -> Drawer {
        Drawer {
            entries: listed(),
            cursor: 0,
            note: None,
        }
    }

    /// Catch up with the record (the observer writes it every few seconds),
    /// keeping the same entry picked.
    pub fn refresh(&mut self) {
        let picked = self.entries.get(self.cursor).map(key);
        self.entries = listed();
        if let Some(k) = picked
            && let Some(i) = self.entries.iter().position(|e| key(e) == k)
        {
            self.cursor = i;
        }
        self.cursor = self.cursor.min(self.entries.len().saturating_sub(1));
    }

    pub fn key(&mut self, key: &KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('q') => return Action::Close,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.cursor = (self.cursor + 1).min(self.entries.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let Some(e) = self.entries.get(self.cursor).cloned() else {
                    return Action::None;
                };
                if e.is_open() {
                    return match &e.session {
                        Some(s) => Action::Show(s.clone()),
                        None => {
                            self.note = Some(("it hasn't started a session yet".into(), false));
                            Action::None
                        }
                    };
                }
                self.note = Some(match crate::registry::resume(&e) {
                    Ok(w) => (
                        if e.closed_as == Some(Closed::Unstarted) {
                            format!("✧ started again in {}", w.label())
                        } else {
                            format!("▶ resumed in {}", w.label())
                        },
                        true,
                    ),
                    Err(err) => (format!("✗ {err}"), false),
                });
            }
            KeyCode::Char('x') | KeyCode::Delete => {
                if let Some(e) = self.entries.get(self.cursor).filter(|e| !e.is_open()) {
                    crate::registry::forget(e);
                    self.refresh();
                }
            }
            _ => {}
        }
        Action::None
    }

    pub fn render(&self, buf: &mut Buffer, canvas: Rect) {
        let palette = fx::palette();
        let w = canvas.width.saturating_sub(4).min(110);
        let rows = self.entries.len().clamp(1, 18) as u16;
        let h = (rows + 6).min(canvas.height);
        if w < 40 || h < 7 {
            return;
        }
        let area = Rect::new(
            canvas.x + (canvas.width - w) / 2,
            canvas.y + (canvas.height - h) / 4,
            w,
            h,
        );
        let bg = Style::default().bg(fx::mix(palette.surface, palette.canvas_bg, 0.2));
        let bold = Modifier::BOLD;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].set_char(' ').set_style(bg);
            }
        }
        crate::composer::frame(buf, area, bg.fg(palette.accent));
        let put = |buf: &mut Buffer, x: u16, y: u16, s: &str, st: Style| {
            if x < area.right() - 1 {
                buf.set_stringn(x, y, s, (area.right() - 1 - x) as usize, st);
            }
        };
        let open = self.entries.iter().filter(|e| e.is_open()).count();
        put(
            buf,
            area.x + 2,
            area.y,
            &format!(
                " ⟲ sessions · {open} open · {} closed ",
                self.entries.len() - open
            ),
            bg.fg(palette.text).add_modifier(bold),
        );
        let list_h = (area.height - 5) as usize;
        let first = self.cursor.saturating_sub(list_h.saturating_sub(1));
        if self.entries.is_empty() {
            put(
                buf,
                area.x + 3,
                area.y + 2,
                "nothing yet — sessions are recorded while lama runs",
                bg.fg(palette.muted),
            );
        }
        let now = chrono::Utc::now();
        let inner = (area.width - 4) as usize;
        for (row, (i, e)) in self
            .entries
            .iter()
            .enumerate()
            .skip(first)
            .take(list_h)
            .enumerate()
        {
            let y = area.y + 2 + row as u16;
            let picked = i == self.cursor;
            let line = if picked {
                bg.bg(fx::mix(palette.accent, palette.surface, 0.75))
            } else {
                bg
            };
            if picked {
                for x in area.x + 1..area.right() - 1 {
                    buf[(x, y)].set_style(line);
                }
            }
            let (glyph, state, tint) = status(e, &palette);
            let when = ago(now - e.closed.unwrap_or(e.last_seen));
            let who = match e.provider {
                crate::provider::Provider::Claude => "❋ claude",
                crate::provider::Provider::Codex => "❂ codex ",
            };
            let project: String = e.project().chars().take(16).collect();
            let fixed = 2 + 9 + 17 + 18 + 5;
            let label: String = e
                .label()
                .chars()
                .take(inner.saturating_sub(fixed))
                .collect();
            let mut x = area.x + 2;
            put(buf, x, y, glyph, line.fg(tint).add_modifier(bold));
            x += 2;
            put(
                buf,
                x,
                y,
                who,
                line.fg(fx::provider_color(
                    e.provider == crate::provider::Provider::Claude,
                )),
            );
            x += 9;
            put(
                buf,
                x,
                y,
                &project,
                line.fg(palette.text).add_modifier(bold),
            );
            x += 17;
            put(buf, x, y, &label, line.fg(palette.text));
            let right = area.right() - 2;
            let tail = format!("{state:<16}{when:>4}");
            let tx = right.saturating_sub(tail.chars().count() as u16);
            put(buf, tx, y, &format!("{state:<16}"), line.fg(tint));
            put(
                buf,
                tx + 16,
                y,
                &format!("{when:>4}"),
                line.fg(palette.muted),
            );
        }
        // What the picked one was last doing.
        if let Some(e) = self.entries.get(self.cursor) {
            let detail = match (&e.last_reply, &e.socket, e.window) {
                (Some(r), _, _) if !e.is_open() => format!("last said: {r}"),
                (_, Some(s), Some(id)) if e.is_open() => {
                    let w = crate::kitty::Window {
                        socket: s.clone(),
                        id,
                        tty: None,
                    };
                    format!(
                        "{} · {} · {}",
                        w.label(),
                        s.trim_start_matches("unix:")
                            .trim_start_matches(crate::tmux::PREFIX),
                        e.cwd.display()
                    )
                }
                _ => e.cwd.display().to_string(),
            };
            put(
                buf,
                area.x + 3,
                area.bottom() - 3,
                &detail,
                bg.fg(palette.muted),
            );
        }
        if let Some((n, ok)) = &self.note {
            put(
                buf,
                area.x + 3,
                area.bottom() - 2,
                n,
                bg.fg(if *ok { palette.success } else { palette.error }),
            );
        }
        put(
            buf,
            area.x + 2,
            area.bottom() - 1,
            " ↑↓ pick · ⏎ resume / show · x forget · esc close ",
            bg.fg(palette.subtle),
        );
    }
}

/// The entries worth a row: a process that never named a session and
/// wasn't started from lama has nothing to show or resume.
fn listed() -> Vec<Entry> {
    crate::registry::all()
        .into_iter()
        .filter(|e| e.session.is_some() || e.prompt.is_some())
        .collect()
}

fn key(
    e: &Entry,
) -> (
    crate::provider::Provider,
    Option<String>,
    chrono::DateTime<chrono::Utc>,
) {
    (e.provider, e.session.clone(), e.first_seen)
}

/// The glyph, words and colour of an entry's state.
fn status(e: &Entry, p: &rataflow::Palette) -> (&'static str, &'static str, ratatui::style::Color) {
    if e.is_open() {
        return if e.asking.is_some() {
            ("⚠", "asking", p.accent)
        } else {
            ("●", "open", p.success)
        };
    }
    match e.closed_as {
        Some(Closed::Asking) => ("⚠", "closed asking", p.error),
        Some(Closed::Working) => ("◐", "closed mid-turn", p.error),
        Some(Closed::Unstarted) => ("✧", "never started", p.accent),
        _ => ("○", "closed", p.subtle),
    }
}

/// `5m`, `3h`, `2d`.
fn ago(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..60 => "now".into(),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}
