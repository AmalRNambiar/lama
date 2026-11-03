//! A scripted mouse pointer, for recordings only.
//!
//! VHS (which records the demo GIFs) is keyboard-only, and it films a headless
//! terminal where no OS pointer exists to film. So lama's mouse-driven
//! features — drag-to-pan, wheel-zoom around the cursor, and dragging the
//! scrubber to seek — cannot be captured directly at all. The keyboard demo in
//! `assets/demo.tape` shows everything reachable by key; this closes the rest.
//!
//! The app owns every cell of the frame, so it can move a pointer and draw one.
//! An autopilot runs a script of waypoints, synthesizes real
//! [`crossterm::event::Event::Mouse`] events, and paints a cursor glyph where
//! that pointer is.
//!
//! **The events are the point.** The synthesized events go through
//! [`crate::handler::handle_event`], the same entry the real terminal uses — so
//! the scrubber-row intercept, the flow's hit testing, and the drag threshold
//! all run for real. Animating the camera or the playhead directly would look
//! superficially similar and demonstrate nothing.
//!
//! Demo scaffolding, gated by the caller behind `LAMA_DEMO=1`. Nothing here
//! runs for a real user.

use std::time::Duration;

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

/// The pointer glyph. A plain arrow reads as a cursor at terminal resolution
/// without the double-width surprises an emoji brings.
const POINTER: &str = "➤";

/// One instruction in a pointer script.
#[derive(Clone, Debug)]
pub enum Step {
    /// Glide to a terminal cell over `secs`.
    MoveTo { col: u16, row: u16, secs: f64 },
    /// Press the left button where the pointer currently is.
    Press,
    /// Release the left button.
    Release,
    /// Hold still. People pause before clicking and after dropping, and those
    /// beats are what separate a pointer from an interpolation. Keep them
    /// short — long ones make the recording feel like a screensaver.
    Dwell(f64),
    /// Wheel clicks at the current position. The flow zooms around the cursor,
    /// so this reads as "zoom into where I am pointing".
    Scroll { up: bool, clicks: u8 },
    /// Press a key. The script needs this to reach state the mouse cannot —
    /// notably `Esc` to dismiss the detail panel, since clicking empty canvas
    /// does not deselect.
    Key(KeyCode),
}

/// Runs a [`Step`] script, emitting mouse events and tracking a cursor.
pub struct Autopilot {
    steps: Vec<Step>,
    index: usize,
    /// Seconds elapsed inside the current step.
    t: f64,
    from: (f64, f64),
    pos: (f64, f64),
    pressed: bool,
}

impl Autopilot {
    /// Build a pilot starting at `start`, running `steps` in order.
    pub fn new(start: (u16, u16), steps: Vec<Step>) -> Self {
        let pos = (start.0 as f64, start.1 as f64);
        Autopilot {
            steps,
            index: 0,
            t: 0.0,
            from: pos,
            pos,
            pressed: false,
        }
    }

    /// Whether the script has run to completion.
    pub fn finished(&self) -> bool {
        self.index >= self.steps.len()
    }

    /// The pointer's current cell.
    pub fn cell(&self) -> (u16, u16) {
        (self.pos.0.round() as u16, self.pos.1.round() as u16)
    }

    /// Advance by `dt` and return the events to feed the app this frame.
    ///
    /// A move emits a `Drag` while the button is held and a `Moved` otherwise —
    /// the same distinction a real terminal reports, which is what lets a drag
    /// pan the canvas instead of merely hovering.
    pub fn tick(&mut self, dt: Duration) -> Vec<Event> {
        let mut out = Vec::new();
        if self.finished() {
            return out;
        }
        self.t += dt.as_secs_f64();

        while let Some(step) = self.steps.get(self.index).cloned() {
            match step {
                Step::MoveTo { col, row, secs } => {
                    let target = (col as f64, row as f64);
                    let secs = secs.max(0.001);
                    let k = (self.t / secs).min(1.0);
                    // Ease in/out: constant-velocity glides read as mechanical.
                    let e = if k < 0.5 {
                        2.0 * k * k
                    } else {
                        1.0 - (-2.0 * k + 2.0).powi(2) / 2.0
                    };
                    self.pos = (
                        self.from.0 + (target.0 - self.from.0) * e,
                        self.from.1 + (target.1 - self.from.1) * e,
                    );
                    let (c, r) = self.cell();
                    out.push(mouse(
                        if self.pressed {
                            MouseEventKind::Drag(MouseButton::Left)
                        } else {
                            MouseEventKind::Moved
                        },
                        c,
                        r,
                    ));
                    if k < 1.0 {
                        break;
                    }
                    self.advance(target);
                }
                Step::Dwell(secs) => {
                    if self.t < secs {
                        break;
                    }
                    let pos = self.pos;
                    self.advance(pos);
                }
                Step::Press => {
                    let (c, r) = self.cell();
                    self.pressed = true;
                    out.push(mouse(MouseEventKind::Down(MouseButton::Left), c, r));
                    let pos = self.pos;
                    self.advance(pos);
                }
                Step::Release => {
                    let (c, r) = self.cell();
                    self.pressed = false;
                    out.push(mouse(MouseEventKind::Up(MouseButton::Left), c, r));
                    let pos = self.pos;
                    self.advance(pos);
                }
                Step::Key(code) => {
                    out.push(Event::Key(KeyEvent {
                        code,
                        modifiers: KeyModifiers::NONE,
                        kind: KeyEventKind::Press,
                        state: KeyEventState::NONE,
                    }));
                    let pos = self.pos;
                    self.advance(pos);
                }
                Step::Scroll { up, clicks } => {
                    let (c, r) = self.cell();
                    let kind = if up {
                        MouseEventKind::ScrollUp
                    } else {
                        MouseEventKind::ScrollDown
                    };
                    for _ in 0..clicks {
                        out.push(mouse(kind, c, r));
                    }
                    let pos = self.pos;
                    self.advance(pos);
                }
            }
        }
        out
    }

    /// Finish the current step: reset the clock and anchor the next glide.
    fn advance(&mut self, from: (f64, f64)) {
        self.index += 1;
        self.t = 0.0;
        self.from = from;
    }

    /// Paint the cursor over the finished frame. Called after `ui::draw`, so it
    /// sits above whatever it is pointing at.
    pub fn draw(&self, buf: &mut Buffer) {
        let (c, r) = self.cell();
        let area = buf.area;
        if c >= area.width || r >= area.height {
            return;
        }
        // Pressed reads brighter, so a click is visible even in a still frame.
        let style = Style::default()
            .fg(if self.pressed {
                Color::White
            } else {
                Color::Indexed(220)
            })
            .add_modifier(Modifier::BOLD);
        if let Some(cell) = buf.cell_mut((c, r)) {
            cell.set_symbol(POINTER).set_style(style);
        }
    }
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}

/// The recorded tour, with waypoints **found from the live layout** rather than
/// hardcoded cells.
///
/// Guessing coordinates is how this goes wrong quietly: a press one cell off a
/// node still hits the node (so the "pan" beat drags a card instead), and a
/// waypoint on empty canvas demonstrates nothing at all. So every point is
/// derived from `node_terminal_rect` / `canvas_size` / `App::scrubber_area`.
///
/// Pacing follows rataflow's overview demo: the pointer leads, beats are short,
/// and each one lands a fact rather than adding motion. Total ≈ 10.5s — keep
/// `assets/tour.tape`'s `Sleep` equal to that sum, since trailing still frames
/// are the worst thing in a looping recording.
/// How far the pan beat throws the pointer, in cells.
///
/// Module scope because two functions need to agree on it: the search in
/// [`tour`] has to prove the RELEASE point is also on empty canvas, and
/// [`tour_steps`] does the throwing. When these were local to one of them, the
/// other could not see them.
const THROW_X: i32 = 22;
const THROW_Y: i32 = -4;

/// The points a tour aims at, read from the live app by [`tour`].
///
/// A struct rather than seven positional `u16`s: they are all the same type, so
/// a transposed pair would compile and simply aim the tour somewhere else.
#[derive(Clone, Copy, Default)]
pub struct TourMarks {
    /// Centre of the agent card worth opening.
    pub click_col: u16,
    pub click_row: u16,
    /// Empty canvas for the pan beat, found by search.
    pub pan_col: u16,
    pub pan_row: u16,
    /// The scrubber: its row and the two ends worth dragging between.
    pub bar_row: u16,
    pub bar_l: u16,
    pub bar_r: u16,
}

/// The tour's choreography. THE source for both what it does and how long it
/// takes — [`tour_secs`] sums this same list, so a tape's `Sleep` can no longer
/// silently disagree with it.
pub fn tour_steps(m: TourMarks) -> Vec<Step> {
    use Step::*;
    let TourMarks {
        click_col,
        click_row,
        pan_col,
        pan_row,
        bar_row,
        bar_l,
        bar_r,
    } = m;

    vec![
        // 1. Click an agent → its provenance panel opens. The pointer leads:
        //    someone clicking a node is the part that reads as a person.
        MoveTo {
            col: click_col,
            row: click_row,
            secs: 0.5,
        },
        Dwell(0.15),
        Press,
        Dwell(0.1),
        Release,
        Dwell(1.5),
        // Esc, not a click — clicking empty canvas does not deselect.
        Key(KeyCode::Esc),
        Dwell(0.25),
        // 2. Press empty canvas and throw → the viewport pans.
        MoveTo {
            col: pan_col,
            row: pan_row,
            secs: 0.5,
        },
        Dwell(0.15),
        Press,
        Dwell(0.1),
        MoveTo {
            col: (pan_col as i32 + THROW_X) as u16,
            row: (pan_row as i32 + THROW_Y).max(1) as u16,
            secs: 0.6,
        },
        Dwell(0.2),
        Release,
        Dwell(0.25),
        // 3. Wheel over a card → zoom anchored on the pointer, not the centre.
        //    This is what `+`/`-` cannot do.
        MoveTo {
            col: click_col,
            row: click_row,
            secs: 0.45,
        },
        Dwell(0.15),
        Scroll {
            up: true,
            clicks: 3,
        },
        Dwell(0.9),
        Scroll {
            up: false,
            clicks: 3,
        },
        Dwell(0.3),
        // 4. Drag the scrubber → the one interaction with no keyboard
        //    equivalent. The graph rewinds continuously under the playhead.
        MoveTo {
            col: bar_r,
            row: bar_row,
            secs: 0.5,
        },
        Dwell(0.15),
        Press,
        Dwell(0.1),
        MoveTo {
            col: bar_l,
            row: bar_row,
            secs: 1.1,
        },
        Dwell(0.35),
        MoveTo {
            col: bar_r,
            row: bar_row,
            secs: 0.9,
        },
        Release,
        Dwell(0.3),
        // 5. Park the pointer clear, then fit — the last frame of a loop should
        //    be the whole graph, not a half-finished gesture.
        MoveTo {
            col: pan_col,
            row: pan_row,
            secs: 0.4,
        },
        Key(KeyCode::Char('o')),
        Dwell(0.9),
    ]
}

/// How long [`tour_steps`] runs, in seconds.
///
/// The marks do not affect timing — only the `secs` and `Dwell` values do — so
/// this answers without an app, a layout or a terminal. That is what lets
/// `LAMA_DEMO=duration` print it before the TUI starts.
pub fn tour_secs() -> f64 {
    duration(&tour_steps(TourMarks::default()))
}

/// How long a script takes, in seconds.
///
/// The single source of truth for that number. It was written by hand in the
/// tape as well, and "Sleep MUST equal the script's total" is a rule nothing
/// enforced — a tape that sleeps short cuts the recording mid-gesture.
///
/// Only the timed steps count; `Press`, `Release`, `Scroll` and `Key` are
/// instantaneous and the pauses around them are `Dwell`s.
pub fn duration(steps: &[Step]) -> f64 {
    steps
        .iter()
        .map(|s| match s {
            Step::MoveTo { secs, .. } => *secs,
            Step::Dwell(secs) => *secs,
            Step::Press | Step::Release | Step::Scroll { .. } | Step::Key(_) => 0.0,
        })
        .sum()
}

pub fn tour(app: &crate::state::App) -> Vec<Step> {
    let size = app.flow.canvas_size();
    let (cw, ch) = (size.width as i32, size.height as i32);
    let rects: Vec<(i32, i32, i32, i32)> = app
        .flow
        .nodes()
        .filter_map(|n| app.flow.node_terminal_rect(&n.id))
        .collect();

    // Which card to click is a MEANING question, not a geometry one: pick the
    // subagent with the most tool calls, so the panel that opens is the richest
    // one (spawning prompt, reasoning, a real tool list). Choosing by rect size
    // picked the workflow group — widest card, but it has no prompt, no thought
    // and no tools, so the panel opened on three empty sections.
    let richest = app
        .session
        .agents
        .iter()
        .filter(|(_, a)| a.kind == crate::state::session::AgentKind::Subagent)
        .max_by_key(|(_, a)| a.tool_calls.len())
        .map(|(id, _)| id.clone());
    let target = richest
        .and_then(|id| app.flow.node_terminal_rect(&id))
        .or_else(|| rects.iter().filter(|(_, t, _, _)| *t > 2).copied().next())
        .unwrap_or((10, 10, 30, 14));
    let (click_col, click_row) = (
        ((target.0 + target.2) / 2).clamp(1, cw - 2) as u16,
        ((target.1 + target.3) / 2).clamp(1, ch - 2) as u16,
    );

    // Genuinely free canvas for the pan beat, and a clear release point — a
    // press only pans when it hits nothing, and ending on a node misfires the
    // next press. Margins because a press just outside a border still hits.
    const INSET: i32 = 4;
    let free = |x: i32, y: i32| {
        !rects
            .iter()
            .any(|(l, t, r, b)| x >= l - 2 && x <= r + 2 && y >= t - 1 && y <= b + 1)
    };
    let mut spot = None;
    'search: for y in (INSET..ch - INSET).rev() {
        for x in INSET..(cw - INSET - THROW_X).max(INSET + 1) {
            if free(x, y) && free(x + THROW_X, y + THROW_Y) {
                spot = Some((x as u16, y as u16));
                break 'search;
            }
        }
    }
    let (pan_col, pan_row) = spot.unwrap_or((INSET as u16, (ch - INSET) as u16));

    // The scrubber's real row — the UI records its rect every frame, so this
    // tracks any layout change for free.
    let bar = app.scrubber_area;
    let (bar_row, bar_l, bar_r) = bar.map_or((ch as u16 - 3, 8, cw as u16 - 8), |b| {
        (b.y + b.height / 2, b.x + 3, b.x + b.width.saturating_sub(4))
    });

    tour_steps(TourMarks {
        click_col,
        click_row,
        pan_col,
        pan_row,
        bar_row,
        bar_l,
        bar_r,
    })
}

/// The key that fires the tour (demo builds only). Firing on a key rather than
/// at startup means the tape controls the moment, so the recording has no dead
/// opening dwell while the replay catches up.
pub fn is_trigger(event: &Event) -> bool {
    matches!(event, Event::Key(k)
        if k.kind == KeyEventKind::Press && k.code == KeyCode::Char('t'))
}

/// Whether a recording run was requested (`LAMA_DEMO=1`).
pub fn requested() -> bool {
    std::env::var("LAMA_DEMO").is_ok_and(|v| v == "1")
}

/// Key events are ignored while the pilot drives, so a stray keypress in the
/// recording terminal cannot desync the script.
pub fn is_key_press(event: &Event) -> bool {
    matches!(event, Event::Key(k) if k.kind == KeyEventKind::Press)
}
