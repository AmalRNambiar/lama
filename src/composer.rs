//! The new-session composer: pick Claude or Codex (whichever you have
//! installed), type a first prompt, and
//! a new session starts in the project, in a new kitty tab (or tmux
//! window, when lama runs in tmux).
//!
//! The first prompt is what makes the session appear on the canvas right
//! away: neither agent writes a transcript until it has something to answer.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::provider::Provider;
use crate::ui::fx;

pub struct Composer {
    /// The project (plane group) the session starts in.
    pub group: usize,
    pub project: String,
    pub provider: Provider,
    /// The agents there are to pick from.
    installed: crate::agents::Installed,
    input: String,
    cursor: usize,
    pub error: Option<String>,
}

/// What a key press asks of the composer's owner.
pub enum Action {
    None,
    Cancel,
    /// Start the session with this prompt.
    Launch(String),
}

impl Composer {
    pub fn new(group: usize, project: String, installed: crate::agents::Installed) -> Composer {
        Composer {
            group,
            project,
            provider: installed
                .or_other(Provider::Claude)
                .unwrap_or(Provider::Claude),
            installed,
            input: String::new(),
            cursor: 0,
            error: None,
        }
    }

    pub fn key(&mut self, key: &KeyEvent) -> Action {
        if key.kind == KeyEventKind::Release {
            return Action::None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let len = self.input.chars().count();
        let byte = |s: &str, ci: usize| s.char_indices().nth(ci).map(|(b, _)| b).unwrap_or(s.len());
        match key.code {
            KeyCode::Esc => return Action::Cancel,
            KeyCode::Tab | KeyCode::BackTab if self.installed.both() => {
                self.provider = crate::agents::other(self.provider);
            }
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if !self.installed.has(self.provider) {
                    self.error = Some("neither claude nor codex is installed".into());
                } else if text.is_empty() {
                    self.error =
                        Some("type a first prompt — it's what the session starts on".into());
                } else {
                    return Action::Launch(text);
                }
            }
            KeyCode::Char('u') if ctrl => {
                self.input.clear();
                self.cursor = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                let at = byte(&self.input, self.cursor);
                self.input.insert(at, c);
                self.cursor += 1;
                self.error = None;
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
            _ => {}
        }
        Action::None
    }

    /// Draw the composer centred over `canvas`.
    pub fn render(&self, buf: &mut Buffer, canvas: Rect) {
        let palette = fx::palette();
        let w = canvas.width.min(76);
        let h = 8;
        if w < 30 || canvas.height < h {
            return;
        }
        let area = Rect::new(
            canvas.x + (canvas.width - w) / 2,
            canvas.y + (canvas.height - h) / 3,
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
        frame(buf, area, bg.fg(palette.accent));
        let put = |buf: &mut Buffer, x: u16, y: u16, s: &str, st: Style| {
            if x < area.right() - 1 {
                buf.set_stringn(x, y, s, (area.right() - 1 - x) as usize, st);
            }
        };
        put(
            buf,
            area.x + 2,
            area.y,
            &format!(" ＋ new session in ◈ {} ", self.project),
            bg.fg(palette.text).add_modifier(bold),
        );

        // Provider choice.
        let (claude, codex) = (
            self.provider == Provider::Claude,
            self.provider == Provider::Codex,
        );
        let have = self.installed;
        let pick = |on: bool, there: bool, tint| {
            if !there {
                bg.fg(palette.muted).add_modifier(Modifier::CROSSED_OUT)
            } else if on {
                Style::default()
                    .bg(tint)
                    .fg(palette.canvas_bg)
                    .add_modifier(bold)
            } else {
                bg.fg(palette.subtle)
            }
        };
        put(
            buf,
            area.x + 3,
            area.y + 2,
            " ❋ claude ",
            pick(claude, have.claude, fx::provider_color(true)),
        );
        put(
            buf,
            area.x + 15,
            area.y + 2,
            " ❂ codex ",
            pick(codex, have.codex, fx::provider_color(false)),
        );
        let hint = match (have.claude, have.codex) {
            (true, true) => "tab switches",
            (true, false) => "codex isn't installed",
            (false, true) => "claude isn't installed",
            (false, false) => "install claude or codex",
        };
        put(buf, area.x + 27, area.y + 2, hint, bg.fg(palette.muted));

        // The first prompt.
        let field = (w as usize).saturating_sub(10);
        let chars: Vec<char> = self.input.chars().collect();
        let start = self.cursor.saturating_sub(field.saturating_sub(1));
        let shown: String = chars.iter().skip(start).take(field).collect();
        let py = area.y + 4;
        put(
            buf,
            area.x + 3,
            py,
            ">",
            bg.fg(palette.accent).add_modifier(bold),
        );
        if self.input.is_empty() {
            put(
                buf,
                area.x + 5,
                py,
                "first prompt — what should it work on?",
                bg.fg(palette.muted),
            );
        } else {
            put(buf, area.x + 5, py, &shown, bg.fg(palette.text));
        }
        let cx = area.x + 5 + (self.cursor - start) as u16;
        if cx < area.right() - 1 && (fx::now_ms() / 500).is_multiple_of(2) {
            buf[(cx, py)].set_style(bg.bg(palette.accent).fg(palette.canvas_bg));
        }
        match &self.error {
            Some(e) => put(buf, area.x + 3, area.y + 5, e, bg.fg(palette.error)),
            None => put(
                buf,
                area.x + 3,
                area.y + 5,
                "opens in a new tab, in the project folder",
                bg.fg(palette.muted),
            ),
        }
        put(
            buf,
            area.x + 2,
            area.bottom() - 1,
            " ⏎ start · tab claude/codex · esc cancel ",
            bg.fg(palette.subtle),
        );
    }
}

pub(crate) fn frame(buf: &mut Buffer, area: Rect, style: Style) {
    let (l, t, r, b) = (area.left(), area.top(), area.right() - 1, area.bottom() - 1);
    for x in l..=r {
        buf[(x, t)].set_char('─').set_style(style);
        buf[(x, b)].set_char('─').set_style(style);
    }
    for y in t..=b {
        buf[(l, y)].set_char('│').set_style(style);
        buf[(r, y)].set_char('│').set_style(style);
    }
    buf[(l, t)].set_char('╭').set_style(style);
    buf[(r, t)].set_char('╮').set_style(style);
    buf[(l, b)].set_char('╰').set_style(style);
    buf[(r, b)].set_char('╯').set_style(style);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(c: &mut Composer, code: KeyCode) -> Action {
        c.key(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    const BOTH: crate::agents::Installed = crate::agents::Installed {
        claude: true,
        codex: true,
    };

    #[test]
    fn only_an_installed_agent_can_be_picked() {
        let codex = crate::agents::Installed {
            claude: false,
            codex: true,
        };
        let mut c = Composer::new(0, "p".into(), codex);
        assert_eq!(c.provider, Provider::Codex, "starts on the one you have");
        press(&mut c, KeyCode::Tab);
        assert_eq!(c.provider, Provider::Codex, "tab has nothing to switch to");
    }

    #[test]
    fn tab_switches_provider_and_enter_needs_a_prompt() {
        let mut c = Composer::new(0, "p".into(), BOTH);
        assert_eq!(c.provider, Provider::Claude);
        press(&mut c, KeyCode::Tab);
        assert_eq!(c.provider, Provider::Codex);
        assert!(matches!(press(&mut c, KeyCode::Enter), Action::None));
        assert!(c.error.is_some(), "an empty prompt explains itself");
        for ch in "fix it".chars() {
            press(&mut c, KeyCode::Char(ch));
        }
        match press(&mut c, KeyCode::Enter) {
            Action::Launch(p) => assert_eq!(p, "fix it"),
            _ => panic!("expected a launch"),
        }
    }
}
