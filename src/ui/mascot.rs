//! The lama: a small mascot that lives on the lamaverse's cards. He walks
//! along their top edges, jumps to a card close by, shoots a rope arrow to
//! one farther off and swings over on it, now and then slips — hangs by his
//! hooves off the corner, climbs back or drops — and from the ground ropes
//! his way back up.
//!
//! He minds the sessions too: one waiting on you, he ropes over to its
//! card and waves until you answer; one that finished, he goes and does a
//! little victory bounce on.
//!
//! He lives in screen cells, not on the canvas, so he keeps his size at any
//! zoom; he stands on a card by its node id, so he rides along when the card
//! is dragged or the camera moves. He is never drawn inside a card: only
//! the canvas around them, so he can't cover what a card says. `l` sends
//! him away and brings him back; with motion off he stays away.

// The lamaverse is drawn only by the native frontend; the browser's core
// build compiles this without calling it.
#![cfg_attr(not(feature = "native"), allow(dead_code))]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::ui::fx;

/// A card on screen: its node id and `(x, y, w, h)` in terminal cells.
pub type Card = (String, (f64, f64, f64, f64));

/// His size in cells.
const W: f64 = 7.0;
const H: f64 = 4.0;
/// Walking speed, cells per second; how hard he falls.
const WALK: f64 = 7.0;
const GRAV: f64 = 70.0;
/// The farthest gap he jumps across, and how far up or down.
const JUMP_GAP: f64 = 9.0;
const JUMP_RISE: f64 = 6.0;
/// The farthest card he shoots his rope to.
const ROPE_REACH: f64 = 80.0;

const FUR: Color = Color::Rgb(240, 232, 214);
const HOOF: Color = Color::Rgb(176, 150, 120);
const BLUSH: Color = Color::Rgb(255, 140, 190);

/// Line-art poses, facing right: a big head on a little body, a blush
/// under his eye. The last row is his legs.
const STAND: [&str; 4] = ["  ╭╮╭╮ ", "  │ •╰╮", "╭─╯·╭─╯", "╰┬─┬╯  "];
const fn legs(last: &'static str) -> [&'static str; 4] {
    [STAND[0], STAND[1], STAND[2], last]
}
const WALK1: [&str; 4] = legs("╰┬─┴╯  ");
const WALK2: [&str; 4] = legs("╰┴─┬╯  ");
const HOP: [&str; 4] = legs("╰───╯  ");
const FALL: [&str; 4] = ["  ╭╮╭╮ ", "  │ °╰╮", "╭─╯·╭─╯", "╱┬─┬╲  "];
const BLINK: [&str; 4] = ["  ╭╮╭╮ ", "  │ -╰╮", "╭─╯·╭─╯", "╰┬─┬╯  "];
/// Hanging: front hooves up on the edge, the rest dangling under them.
const HANG1: [&str; 5] = ["  ┃ ┃  ", "  ╭╮╭╮ ", "  │ •╰╮", "╭─╯·╭─╯", "╰╽─╽╯  "];
const HANG2: [&str; 5] = ["  ┃ ┃  ", "  ╭╮╭╮ ", "  │ •╰╮", "╭─╯·╭─╯", "╰┬─┬╯  "];
/// Waving a hoof by his snout, two frames.
const WAVE1: [&str; 4] = ["  ╭╮╭╮╱", "  │ •╰╮", "╭─╯·╭─╯", "╰┬─┬╯  "];
const WAVE2: [&str; 4] = ["  ╭╮╭╮─", "  │ •╰╮", "╭─╯·╭─╯", "╰┬─┬╯  "];
const FLAT: [&str; 4] = ["          ", "          ", "     ╭╮╭╮ ", "╰────╯ ×╰╮"];
/// Where his hooves are on a hanging pose, from his left edge facing right.
const HOOVES: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Walk,
    Look,
    Jump,
    Aim,
    Arrow,
    Zip,
    Slip,
    Hang,
    Climb,
    Fall,
    Dazed,
    /// On the ground with nothing to climb: he wanders.
    Ground,
    /// A victory bounce on a card whose session just finished.
    Cheer,
    /// Waving by a card whose session waits on you.
    Poke,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pose {
    Stand,
    Walk1,
    Walk2,
    Blink,
    Hop,
    Bow,
    Hang1,
    Hang2,
    Fall,
    Flat,
    Wave1,
    Wave2,
}

/// A rope arrow: shot from a point on screen, stuck in a card's top edge at
/// `off` cells from its left.
#[derive(Debug, Clone)]
struct Rope {
    from: (f64, f64),
    card: String,
    off: f64,
    /// Where the arrow is while it flies.
    head: (f64, f64),
    /// When it struck, to flash.
    struck: Option<f64>,
}

pub struct Mascot {
    /// Sent away with `l`.
    pub hidden: bool,
    state: State,
    /// Seconds in the current state.
    t: f64,
    /// Seconds since he appeared: the animation clock.
    clock: f64,
    x: f64,
    y: f64,
    /// 1 facing right, -1 left.
    dir: f64,
    /// The card he stands or hangs on, and how far from its left edge.
    on: Option<String>,
    off: f64,
    from: (f64, f64),
    /// A jump's or climb's landing card and where on it.
    target: Option<(String, f64)>,
    dur: f64,
    apex: f64,
    vy: f64,
    rope: Option<Rope>,
    /// A puff of dust where he landed: where, and when.
    puff: Option<(f64, f64, f64)>,
    pose: Pose,
    say: Option<(String, Color)>,
    last: Option<web_time::Instant>,
    seed: u64,
    /// Sessions that just finished, by id, until the frontend finds their
    /// cards.
    finished: Vec<String>,
    /// Cards to go and cheer on, oldest first.
    cheers: std::collections::VecDeque<String>,
    /// Cards whose session waits on you.
    asks: Vec<String>,
}

impl Default for Mascot {
    fn default() -> Self {
        Self::new()
    }
}

impl Mascot {
    pub fn new() -> Mascot {
        Mascot::seeded(fx::now_ms() | 1)
    }

    fn seeded(seed: u64) -> Mascot {
        Mascot {
            hidden: false,
            state: State::Fall,
            t: 0.0,
            clock: 0.0,
            x: f64::NAN,
            y: f64::NAN,
            dir: 1.0,
            on: None,
            off: 0.0,
            from: (0.0, 0.0),
            target: None,
            dur: 0.0,
            apex: 0.0,
            vy: 0.0,
            rope: None,
            puff: None,
            pose: Pose::Fall,
            say: None,
            last: None,
            seed,
            finished: Vec::new(),
            cheers: Default::default(),
            asks: Vec::new(),
        }
    }

    /// Sessions that just finished their turn, by id: he'll go and cheer.
    pub fn cheer(&mut self, sessions: Vec<String>) {
        if self.hidden {
            return;
        }
        self.finished.extend(sessions);
        let extra = self.finished.len().saturating_sub(8);
        self.finished.drain(..extra);
    }

    /// The finished sessions waiting to be found on the canvas.
    pub fn take_cheers(&mut self) -> Vec<String> {
        std::mem::take(&mut self.finished)
    }

    /// What he should mind this frame: the cards waiting on you, and the
    /// cards of sessions that just finished.
    pub fn errands(&mut self, asking: Vec<String>, cheers: Vec<String>) {
        self.asks = asking;
        for c in cheers {
            if !self.cheers.contains(&c) {
                self.cheers.push_back(c);
            }
        }
        while self.cheers.len() > 4 {
            self.cheers.pop_front();
        }
    }

    /// Where he is needed: a card waiting on you before one to cheer.
    fn errand(&self, perches: &[&Card]) -> Option<(String, State)> {
        let here = |id: &String| perches.iter().any(|(c, _)| c == id);
        if let Some(c) = self.asks.iter().find(|c| here(c)) {
            return Some((c.clone(), State::Poke));
        }
        self.cheers
            .iter()
            .find(|c| here(c))
            .map(|c| (c.clone(), State::Cheer))
    }

    /// Off on an errand from where he stands: there already, he starts on
    /// it; else he ropes over. Returns whether he went.
    fn go(&mut self, perches: &[&Card]) -> bool {
        let Some((card, what)) = self.errand(perches) else {
            return false;
        };
        if self.on.as_deref() == Some(card.as_str()) {
            self.enter(what);
            return true;
        }
        let Some(c) = perches.iter().find(|(id, _)| *id == card) else {
            return false;
        };
        self.dir = if c.1.0 + c.1.2 / 2.0 > self.x + W / 2.0 {
            1.0
        } else {
            -1.0
        };
        self.aim(c, self.on.is_none());
        true
    }

    /// A number in `0..1`.
    fn rand(&mut self) -> f64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        (self.seed >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Advance him by the time since the last frame and draw him over
    /// `area` (the canvas), around `cards`.
    pub fn frame(&mut self, buf: &mut Buffer, area: Rect, cards: &[Card]) {
        let now = web_time::Instant::now();
        let dt = self
            .last
            .map_or(0.0, |l| now.duration_since(l).as_secs_f64())
            .min(0.1);
        self.last = Some(now);
        if self.hidden || !fx::motion() || area.width < 20 || area.height < 10 {
            return;
        }
        // Whole cells, so he stands right on a card's edge and never inside.
        let cards: Vec<Card> = cards
            .iter()
            .map(|(id, (x, y, w, h))| {
                let (x0, y0) = (x.round(), y.round());
                (
                    id.clone(),
                    (x0, y0, (x + w).round() - x0, (y + h).round() - y0),
                )
            })
            .collect();
        self.step(dt, area, &cards);
        self.draw(buf, area, &cards);
    }

    /// The cards he can stand on: wide enough, with room above them.
    fn perches<'a>(&self, area: Rect, cards: &'a [Card]) -> Vec<&'a Card> {
        cards
            .iter()
            .filter(|(_, (x, y, w, _))| {
                *w >= W + 2.0
                    && *y - H >= area.y as f64
                    && *x + *w > area.x as f64
                    && *x < area.right() as f64
                    && *y < area.bottom() as f64
            })
            .collect()
    }

    fn card<'a>(cards: &'a [Card], id: &str) -> Option<&'a (f64, f64, f64, f64)> {
        cards.iter().find(|(c, _)| c == id).map(|(_, r)| r)
    }

    fn enter(&mut self, s: State) {
        self.state = s;
        self.t = 0.0;
    }

    fn ground(area: Rect) -> f64 {
        area.bottom() as f64 - H
    }

    fn step(&mut self, dt: f64, area: Rect, cards: &[Card]) {
        self.clock += dt;
        self.t += dt;
        self.say = None;
        let perches = self.perches(area, cards);
        // First sight: drop onto a card from above it.
        if self.x.is_nan() {
            let i = (self.rand() * perches.len() as f64) as usize;
            match perches.get(i) {
                Some((_, (x, y, w, _))) => {
                    self.x = x + (w - W) / 2.0;
                    self.y = (y - H - 6.0).max(area.y as f64);
                }
                None => {
                    self.x = area.x as f64 + area.width as f64 / 2.0;
                    self.y = area.y as f64;
                }
            }
            self.enter(State::Fall);
        }
        let on = self
            .on
            .as_deref()
            .and_then(|id| Self::card(cards, id))
            .copied();
        match self.state {
            State::Walk => {
                let Some((cx, cy, cw, _)) = on.filter(|r| r.2 >= W + 2.0) else {
                    self.on = None;
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                self.x = cx + self.off;
                self.y = cy - H;
                if self.go(&perches) {
                    return;
                }
                self.off += self.dir * WALK * dt;
                self.x = cx + self.off;
                self.pose = if beat(self.clock, 8.0) {
                    Pose::Walk1
                } else {
                    Pose::Walk2
                };
                if self.rand() < dt * 0.12 {
                    return self.enter(State::Look);
                }
                let right = self.off >= cw - W;
                let left = self.off <= 0.0;
                if !(right && self.dir > 0.0 || left && self.dir < 0.0) {
                    return;
                }
                self.off = self.off.clamp(0.0, cw - W);
                self.x = cx + self.off;
                self.at_edge((cx, cy, cw), &perches);
            }
            State::Look => {
                if let Some((cx, cy, _, _)) = on {
                    self.x = cx + self.off;
                    self.y = cy - H;
                } else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                }
                self.pose = if (self.t * 3.0) as u64 % 4 == 1 {
                    Pose::Blink
                } else {
                    Pose::Stand
                };
                if self.t > 0.6 && self.t < 1.2 && self.rand() < dt * 2.0 {
                    self.dir = -self.dir;
                }
                if self.go(&perches) {
                    return;
                }
                if self.t > 1.6 {
                    self.enter(if self.on.is_some() {
                        State::Walk
                    } else {
                        State::Ground
                    });
                }
            }
            State::Jump => {
                let Some((id, off)) = self.target.clone() else {
                    return self.enter(State::Fall);
                };
                let Some(&(tx, ty, _, _)) = Self::card(cards, &id) else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                let k = (self.t / self.dur).min(1.0);
                let to = (tx + off, ty - H);
                self.x = lerp(self.from.0, to.0, k);
                self.y = lerp(self.from.1, to.1, k) - 4.0 * self.apex * k * (1.0 - k);
                self.pose = Pose::Hop;
                if k >= 1.0 {
                    self.on = Some(id);
                    self.off = off;
                    self.puff = Some((self.x, self.y + H, self.clock));
                    self.enter(State::Walk);
                }
            }
            State::Aim => {
                self.pose = Pose::Bow;
                self.say = Some(("➶".into(), fx::yellow()));
                if self.t > 0.45 {
                    self.enter(State::Arrow);
                }
            }
            State::Arrow => {
                self.pose = Pose::Bow;
                let Some(anchor) = self.anchor(cards) else {
                    self.rope = None;
                    return self.enter(self.rest_state());
                };
                let Some(r) = self.rope.as_mut() else {
                    return self.enter(State::Walk);
                };
                let k = (self.t / 0.45).min(1.0);
                r.head = (
                    lerp(r.from.0, anchor.0, k),
                    lerp(r.from.1, anchor.1, k) - 3.0 * k * (1.0 - k),
                );
                if k >= 1.0 {
                    r.struck = Some(self.clock);
                    let len = ((anchor.0 - r.from.0) / 2.0).hypot(anchor.1 - r.from.1);
                    self.dur = 0.5 + len / 18.0;
                    self.enter(State::Zip);
                }
            }
            State::Zip => {
                let Some(anchor) = self.anchor(cards) else {
                    self.rope = None;
                    self.vy = 0.0;
                    self.on = None;
                    return self.enter(State::Fall);
                };
                let Some(r) = self.rope.clone() else {
                    return self.enter(State::Fall);
                };
                let k = ease((self.t / self.dur).min(1.0));
                let p = rope_at(r.from, anchor, k, k);
                self.x = p.0 - W / 2.0;
                self.y = p.1;
                self.pose = if beat(self.clock, 6.0) {
                    Pose::Hang1
                } else {
                    Pose::Hang2
                };
                if k >= 1.0 {
                    let (cx, _, cw, _) = *Self::card(cards, &r.card).expect("anchored");
                    self.from = (self.x, self.y);
                    self.target =
                        Some((r.card.clone(), (anchor.0 - W / 2.0 - cx).clamp(0.0, cw - W)));
                    self.rope = None;
                    self.enter(State::Climb);
                }
            }
            State::Slip => {
                self.pose = Pose::Hop;
                self.say = Some(("!".into(), fx::palette().error));
                if self.t > 0.35 {
                    self.enter(State::Hang);
                }
            }
            State::Hang => {
                let Some((cx, cy, cw, _)) = on else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                // Off the corner, outside the card: hooves on its edge.
                self.x = if self.dir > 0.0 {
                    cx + cw - 1.0 - HOOVES
                } else {
                    cx - W + HOOVES + 1.0
                };
                self.y = cy - 1.0;
                self.pose = if beat(self.clock, 7.0) {
                    Pose::Hang1
                } else {
                    Pose::Hang2
                };
                if self.t > 0.6 {
                    self.say = Some(("hnng".into(), fx::palette().subtle));
                }
                if self.t > 1.8 {
                    if self.rand() < 0.5 {
                        let id = self.on.clone().unwrap_or_default();
                        let off = if self.dir > 0.0 { cw - W } else { 0.0 };
                        self.from = (self.x, self.y);
                        self.target = Some((id, off));
                        self.dir = -self.dir;
                        self.enter(State::Climb);
                    } else {
                        self.on = None;
                        self.vy = 0.0;
                        self.enter(State::Fall);
                    }
                }
            }
            State::Climb => {
                let Some((id, off)) = self.target.clone() else {
                    return self.enter(State::Fall);
                };
                let Some(&(cx, cy, _, _)) = Self::card(cards, &id) else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                let k = ease((self.t / 0.45).min(1.0));
                self.x = lerp(self.from.0, cx + off, k);
                self.y = lerp(self.from.1, cy - H, k);
                self.pose = if k < 0.6 { Pose::Hang1 } else { Pose::Stand };
                if k >= 1.0 {
                    self.on = Some(id);
                    self.off = off;
                    self.enter(State::Walk);
                }
            }
            State::Fall => {
                self.pose = Pose::Fall;
                if self.t > 0.15 {
                    self.say = Some(("aaa".into(), fx::yellow()));
                }
                let before = self.y + H;
                self.vy += GRAV * dt;
                self.y += self.vy * dt;
                let feet = self.y + H;
                let land = perches
                    .iter()
                    .filter(|(_, (cx, cy, cw, _))| {
                        self.x + W - 1.0 >= *cx
                            && self.x <= cx + cw - 1.0
                            && before <= *cy
                            && feet >= *cy
                    })
                    .min_by(|a, b| a.1.1.total_cmp(&b.1.1));
                if let Some((id, (cx, cy, cw, _))) = land {
                    self.on = Some(id.clone());
                    self.off = (self.x - cx).clamp(0.0, cw - W);
                    self.y = cy - H;
                    self.puff = Some((self.x, self.y + H, self.clock));
                    return self.enter(State::Look);
                }
                if self.y >= Self::ground(area) {
                    self.y = Self::ground(area);
                    self.on = None;
                    self.puff = Some((self.x, self.y + H, self.clock));
                    self.enter(State::Dazed);
                }
            }
            State::Dazed => {
                self.y = Self::ground(area);
                self.pose = if self.t < 0.8 {
                    Pose::Flat
                } else {
                    Pose::Stand
                };
                if self.t < 1.6 {
                    let stars = ["✦ ✧", "✧ ✦", " ✦✧", "✧✦ "][(self.clock * 6.0) as usize % 4];
                    self.say = Some((stars.into(), fx::yellow()));
                }
                if self.t > 1.9 {
                    self.rope_up(&perches);
                }
            }
            State::Ground => {
                self.y = Self::ground(area);
                self.x += self.dir * WALK * 0.6 * dt;
                let (lo, hi) = (area.x as f64, area.right() as f64 - W);
                if self.x <= lo || self.x >= hi {
                    self.x = self.x.clamp(lo, hi);
                    self.dir = -self.dir;
                }
                self.pose = if beat(self.clock, 6.0) {
                    Pose::Walk1
                } else {
                    Pose::Walk2
                };
                if self.go(&perches) {
                    return;
                }
                if self.t > 3.0 && self.rand() < dt * 0.5 {
                    self.rope_up(&perches);
                }
            }
            State::Cheer => {
                let Some((cx, cy, cw, _)) = on else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                // Three little hops in the middle of the card.
                self.off += ((cw - W) / 2.0 - self.off).clamp(-WALK * dt, WALK * dt);
                self.x = cx + self.off;
                let hop = (self.t * std::f64::consts::PI * 2.5).sin().abs() * 1.5;
                self.y = cy - H - hop.round();
                self.pose = if hop > 0.4 { Pose::Hop } else { Pose::Stand };
                let words = if beat(self.clock, 3.0) {
                    "✦ done ✦"
                } else {
                    "♪ done ♪"
                };
                self.say = Some((words.into(), palette_success()));
                if self.t > 2.4 {
                    if let Some(id) = &self.on {
                        self.cheers.retain(|c| c != id);
                    }
                    self.enter(State::Walk);
                }
            }
            State::Poke => {
                let Some((cx, cy, cw, _)) = on else {
                    self.vy = 0.0;
                    return self.enter(State::Fall);
                };
                let here = self.on.clone().unwrap_or_default();
                if !self.asks.contains(&here) {
                    return self.enter(State::Walk);
                }
                // At the front of the card, facing in, waving.
                self.dir = -1.0;
                self.off += ((cw - W) - self.off).clamp(-WALK * dt, WALK * dt);
                self.x = cx + self.off;
                self.y = cy - H;
                self.pose = if beat(self.clock, 3.0) {
                    Pose::Wave1
                } else {
                    Pose::Wave2
                };
                let words = if beat(self.clock, 1.0 / 1.5) {
                    "psst!"
                } else {
                    "needs you"
                };
                self.say = Some((words.into(), fx::palette().accent));
            }
        }
    }

    /// Standing at the edge of the card at `(cx, cy, cw)`: slip, jump to a
    /// card close by, rope over to one farther off, or turn round.
    fn at_edge(&mut self, (cx, cy, cw): (f64, f64, f64), perches: &[&Card]) {
        if self.asks.is_empty() && self.cheers.is_empty() && self.rand() < 0.18 {
            return self.enter(State::Slip);
        }
        let here = self.on.clone().unwrap_or_default();
        let mut jump: Option<(f64, &Card)> = None;
        let mut far: Option<(f64, &Card)> = None;
        for c in perches.iter().filter(|(id, _)| *id != here) {
            let (ox, oy, ow, _) = c.1;
            let gap = if self.dir > 0.0 {
                ox - (cx + cw)
            } else {
                cx - (ox + ow)
            };
            let rise = oy - cy;
            if (-2.0..=JUMP_GAP).contains(&gap) && rise.abs() <= JUMP_RISE {
                if jump.is_none_or(|(g, _)| gap < g) {
                    jump = Some((gap, c));
                }
            } else if gap > JUMP_GAP && gap < ROPE_REACH {
                let d = gap + rise.abs() * 2.0;
                if far.is_none_or(|(f, _)| d < f) {
                    far = Some((d, c));
                }
            }
        }
        if let Some((gap, (id, (_, oy, ow, _)))) = jump {
            let off = if self.dir > 0.0 { 1.0 } else { ow - W - 1.0 };
            self.from = (self.x, self.y);
            self.target = Some((id.clone(), off.max(0.0)));
            self.dur = 0.55 + gap.max(0.0) * 0.03;
            self.apex = 3.0 + (cy - oy).max(0.0);
            return self.enter(State::Jump);
        }
        if let Some((_, c)) = far
            && self.rand() < 0.8
        {
            return self.aim(c, false);
        }
        self.dir = -self.dir;
    }

    /// From the ground: a rope to the nearest card, or a wander if none.
    fn rope_up(&mut self, perches: &[&Card]) {
        let near = perches.iter().min_by(|a, b| {
            let d = |c: &&Card| (c.1.0 + c.1.2 / 2.0 - self.x).abs();
            d(a).total_cmp(&d(b))
        });
        match near {
            Some(c) => {
                self.dir = if c.1.0 + c.1.2 / 2.0 > self.x {
                    1.0
                } else {
                    -1.0
                };
                self.aim(c, true);
            }
            None => self.enter(State::Ground),
        }
    }

    fn aim(&mut self, (id, (ox, _, ow, _)): &Card, up: bool) {
        let want = if up {
            self.x + 1.0
        } else if self.dir > 0.0 {
            self.x + 12.0
        } else {
            self.x - 8.0
        };
        let off = (want - ox).clamp(2.0, (ow - 3.0).max(2.0));
        let from = (
            if self.dir > 0.0 {
                self.x + W
            } else {
                self.x - 1.0
            },
            self.y + 1.0,
        );
        self.rope = Some(Rope {
            from,
            card: id.clone(),
            off,
            head: from,
            struck: None,
        });
        self.enter(State::Aim);
    }

    /// Where the rope's arrow is stuck, while its card is on screen.
    fn anchor(&self, cards: &[Card]) -> Option<(f64, f64)> {
        let r = self.rope.as_ref()?;
        let &(cx, cy, _, _) = Self::card(cards, &r.card)?;
        Some((cx + r.off, cy))
    }

    fn rest_state(&self) -> State {
        if self.on.is_some() {
            State::Walk
        } else {
            State::Ground
        }
    }

    fn rows(&self) -> &'static [&'static str] {
        match self.pose {
            Pose::Stand | Pose::Bow => &STAND,
            Pose::Walk1 => &WALK1,
            Pose::Walk2 => &WALK2,
            Pose::Blink => &BLINK,
            Pose::Hop => &HOP,
            Pose::Hang1 => &HANG1,
            Pose::Hang2 => &HANG2,
            Pose::Fall => &FALL,
            Pose::Flat => &FLAT,
            Pose::Wave1 => &WAVE1,
            Pose::Wave2 => &WAVE2,
        }
    }

    /// Draw him, his rope and his words — only on the canvas around the
    /// cards, never inside one.
    fn draw(&self, buf: &mut Buffer, area: Rect, cards: &[Card]) {
        let free = |x: i64, y: i64| {
            x >= area.x as i64
                && y >= area.y as i64
                && x < area.right() as i64
                && y < area.bottom() as i64
                && !cards.iter().any(|(_, (cx, cy, cw, ch))| {
                    (x as f64) >= *cx
                        && (x as f64) < cx + cw
                        && (y as f64) >= *cy
                        && (y as f64) < cy + ch
                })
        };
        let put = |buf: &mut Buffer, x: i64, y: i64, ch: &str, style: Style| {
            if free(x, y) {
                buf[(x as u16, y as u16)].set_symbol(ch).set_style(style);
            }
        };
        let palette = fx::palette();
        if let (Some(r), Some(anchor)) = (&self.rope, self.anchor(cards)) {
            let rope = Style::default().fg(fx::mix(fx::yellow(), palette.canvas_bg, 0.25));
            let (end, load) = match self.state {
                State::Arrow => (r.head, None),
                State::Zip => (anchor, Some(ease((self.t / self.dur).min(1.0)))),
                _ => (anchor, None),
            };
            if self.state != State::Aim {
                let n =
                    (((end.0 - r.from.0).abs() + (end.1 - r.from.1).abs() * 2.0) as usize).max(2);
                for i in (0..=n).step_by(2) {
                    let k = i as f64 / n as f64;
                    let p = match load {
                        Some(l) => {
                            let p = rope_at(r.from, end, k, l);
                            (p.0, p.1 + 1.0)
                        }
                        None => (lerp(r.from.0, end.0, k), lerp(r.from.1, end.1, k)),
                    };
                    put(buf, p.0.round() as i64, p.1.round() as i64, "·", rope);
                }
                let right = end.0 >= r.from.0;
                let head = Style::default()
                    .fg(fx::yellow())
                    .add_modifier(Modifier::BOLD);
                let (hx, hy) = (end.0.round() as i64, end.1.round() as i64 - 1);
                put(
                    buf,
                    hx - i64::from(right),
                    hy,
                    if right { "➤" } else { "◄" },
                    head,
                );
                if r.struck.is_some_and(|s| self.clock - s < 0.3) {
                    put(buf, hx, hy - 1, "✶", Style::default().fg(palette.accent));
                }
            }
        }
        let rows = self.rows();
        let tall = rows.len() as i64 - H as i64;
        let (x0, y0) = (self.x.round() as i64, self.y.round() as i64 - tall);
        let last = rows.len() - 1;
        let solid = Style::default().bg(palette.canvas_bg);
        for (r, row) in rows.iter().enumerate() {
            let cells: Vec<char> = row.chars().collect();
            let width = cells.len();
            let first = cells.iter().position(|&c| c != ' ').unwrap_or(width);
            let last_ink = cells.iter().rposition(|&c| c != ' ').unwrap_or(0);
            for (k, &c) in cells.iter().enumerate() {
                if c == ' ' {
                    // Inside his outline he is solid: no rope, rain or dots
                    // showing through his face.
                    if k > first && k < last_ink {
                        let col = if self.dir > 0.0 { k } else { width - 1 - k };
                        put(buf, x0 + col as i64, y0 + r as i64, " ", solid);
                    }
                    continue;
                }
                let (col, ch) = if self.dir > 0.0 {
                    (k, c)
                } else {
                    (width - 1 - k, mirror(c))
                };
                let fg = if matches!(c, '•' | '°' | '×' | '-') && r < last {
                    palette.accent
                } else if c == '·' {
                    BLUSH
                } else if r == last && matches!(c, '┬' | '┴' | '╽' | '╱' | '╲') {
                    HOOF
                } else {
                    FUR
                };
                let mut s = [0u8; 4];
                put(
                    buf,
                    x0 + col as i64,
                    y0 + r as i64,
                    ch.encode_utf8(&mut s),
                    Style::default().fg(fg).add_modifier(Modifier::BOLD),
                );
            }
        }
        if self.pose == Pose::Bow {
            let (bx, bow) = if self.dir > 0.0 {
                (x0 + W as i64, ")")
            } else {
                (x0 - 1, "(")
            };
            put(buf, bx, y0 + 2, bow, Style::default().fg(fx::yellow()));
        }
        if let Some((text, color)) = &self.say {
            let n = text.chars().count() as i64;
            let sx = x0 + W as i64 / 2 - n / 2;
            for (k, c) in text.chars().enumerate() {
                let mut s = [0u8; 4];
                put(
                    buf,
                    sx + k as i64,
                    y0 - 1,
                    c.encode_utf8(&mut s),
                    Style::default().fg(*color).add_modifier(Modifier::BOLD),
                );
            }
        }
        if let Some((px, py, at)) = self.puff
            && self.clock - at < 0.5
        {
            let k = (self.clock - at) / 0.5;
            let w = 1 + (k * 3.0).round() as i64;
            let dust = Style::default().fg(fx::mix(palette.subtle, palette.canvas_bg, k as f32));
            let y = py.round() as i64 - 1;
            put(buf, px.round() as i64 - w, y, "˙", dust);
            put(buf, px.round() as i64 + W as i64 - 1 + w, y, "˙", dust);
        }
    }
}

/// A two-frame animation's first frame, at `rate` frames a second.
fn beat(clock: f64, rate: f64) -> bool {
    ((clock * rate) as u64).is_multiple_of(2)
}

fn palette_success() -> Color {
    fx::palette().success
}

fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn ease(k: f64) -> f64 {
    if k < 0.5 {
        2.0 * k * k
    } else {
        1.0 - (-2.0 * k + 2.0).powi(2) / 2.0
    }
}

/// A point `k` of the way along the rope from `from` to `to`, sagging under
/// him while he hangs on it (`load`: how far along he is).
fn rope_at(from: (f64, f64), to: (f64, f64), k: f64, load: f64) -> (f64, f64) {
    let len = ((to.0 - from.0) / 2.0).hypot(to.1 - from.1);
    let sag = (len * 0.18).min(6.0) * (std::f64::consts::PI * (load + 0.15).min(1.0)).sin();
    (
        lerp(from.0, to.0, k),
        lerp(from.1, to.1, k) + sag * 4.0 * k * (1.0 - k) - 1.0,
    )
}

/// The character facing the other way.
fn mirror(c: char) -> char {
    match c {
        '╭' => '╮',
        '╮' => '╭',
        '╰' => '╯',
        '╯' => '╰',
        '╱' => '╲',
        '╲' => '╱',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: Rect = Rect::new(0, 0, 160, 50);

    fn cards() -> Vec<Card> {
        vec![
            ("a".into(), (10.0, 20.0, 40.0, 10.0)),
            ("b".into(), (100.0, 22.0, 40.0, 10.0)),
        ]
    }

    /// Run him for `secs` at 60 frames a second.
    fn run(m: &mut Mascot, cards: &[Card], secs: f64) {
        for _ in 0..(secs * 60.0) as usize {
            m.step(1.0 / 60.0, AREA, cards);
        }
    }

    #[test]
    fn he_drops_onto_a_card_and_walks_its_top_edge() {
        let mut m = Mascot::seeded(7);
        let cards = cards();
        run(&mut m, &cards, 1.5);
        let on = m.on.clone().expect("landed on a card");
        let (_, cy, _, _) = *Mascot::card(&cards, &on).unwrap();
        if matches!(m.state, State::Walk | State::Look) {
            assert_eq!(m.y, cy - H, "standing on its top edge");
        }
    }

    #[test]
    fn he_rides_a_dragged_card() {
        let mut m = Mascot::seeded(7);
        let mut cards = cards();
        run(&mut m, &cards, 1.5);
        m.on = Some("a".into());
        m.off = 10.0;
        m.enter(State::Look);
        cards[0].1.0 += 30.0;
        cards[0].1.1 += 5.0;
        m.step(1.0 / 60.0, AREA, &cards);
        assert_eq!((m.x, m.y), (50.0, 25.0 - H));
    }

    #[test]
    fn he_falls_when_his_card_goes_and_gets_back_up() {
        let mut m = Mascot::seeded(3);
        let cards = cards();
        run(&mut m, &cards, 1.5);
        m.on = Some("gone".into());
        m.enter(State::Walk);
        m.step(1.0 / 60.0, AREA, &cards);
        assert_eq!(m.state, State::Fall);
        // Over neither card: he hits the ground, then ropes back up.
        m.x = 70.0;
        let mut states = Vec::new();
        for _ in 0..600 {
            m.step(1.0 / 60.0, AREA, &cards);
            states.push(m.state);
        }
        assert!(states.contains(&State::Dazed), "{states:?}");
        assert!(states.contains(&State::Zip), "roped up");
        assert!(m.on.is_some(), "back on a card");
    }

    #[test]
    fn a_long_life_never_leaves_the_screen_or_goes_astray() {
        let mut m = Mascot::seeded(11);
        let mut cards = cards();
        // One close enough to jump to.
        cards.push(("c".into(), (56.0, 17.0, 30.0, 8.0)));
        let mut seen = std::collections::HashSet::new();
        for _ in 0..60 * 600 {
            m.step(1.0 / 60.0, AREA, &cards);
            seen.insert(format!("{:?}", m.state));
            assert!(m.x.is_finite() && m.y.is_finite());
            assert!(m.y <= Mascot::ground(AREA) + 0.01, "{:?} {}", m.state, m.y);
        }
        for s in ["Walk", "Jump", "Zip", "Hang", "Fall"] {
            assert!(seen.contains(s), "{s} never happened: {seen:?}");
        }
    }

    #[test]
    fn he_is_never_drawn_inside_a_card() {
        let cards = cards();
        let mut m = Mascot::seeded(5);
        let mut buf = Buffer::empty(AREA);
        for _ in 0..60 * 120 {
            m.step(1.0 / 60.0, AREA, &cards);
            m.draw(&mut buf, AREA, &cards);
        }
        for (_, (x, y, w, h)) in &cards {
            for cy in *y as u16..(y + h) as u16 {
                for cx in *x as u16..(x + w) as u16 {
                    assert_eq!(buf[(cx, cy)].symbol(), " ", "drew at {cx},{cy}");
                }
            }
        }
    }

    #[test]
    fn he_goes_to_a_session_waiting_on_you_and_waits_with_it() {
        let mut m = Mascot::seeded(9);
        let cards = cards();
        run(&mut m, &cards, 2.0);
        let far = if m.on.as_deref() == Some("a") {
            "b"
        } else {
            "a"
        };
        m.errands(vec![far.into()], Vec::new());
        let mut states = Vec::new();
        for _ in 0..60 * 8 {
            m.step(1.0 / 60.0, AREA, &cards);
            states.push(m.state);
        }
        assert_eq!(m.on.as_deref(), Some(far), "over at the asking card");
        assert_eq!(m.state, State::Poke, "{states:?}");
        assert!(
            m.say
                .as_ref()
                .is_some_and(|(w, _)| w == "psst!" || w == "needs you")
        );
        // Answered: he goes back to his walk.
        m.errands(Vec::new(), Vec::new());
        m.step(1.0 / 60.0, AREA, &cards);
        assert_eq!(m.state, State::Walk);
    }

    #[test]
    fn he_cheers_a_finished_session_once() {
        let mut m = Mascot::seeded(4);
        let cards = cards();
        run(&mut m, &cards, 2.0);
        let far = if m.on.as_deref() == Some("a") {
            "b"
        } else {
            "a"
        };
        m.errands(Vec::new(), vec![far.into()]);
        let mut cheered = false;
        for _ in 0..60 * 12 {
            m.step(1.0 / 60.0, AREA, &cards);
            cheered |= m.state == State::Cheer && m.on.as_deref() == Some(far);
        }
        assert!(cheered, "he went over and cheered");
        assert!(m.cheers.is_empty(), "once");
        assert_ne!(m.state, State::Cheer);
    }

    #[test]
    fn his_insides_hide_what_is_behind_him() {
        let mut m = Mascot::seeded(1);
        let cards: Vec<Card> = Vec::new();
        (m.x, m.y, m.dir, m.pose) = (20.0, 10.0, -1.0, Pose::Stand);
        m.state = State::Look;
        let mut buf = Buffer::empty(AREA);
        for y in 0..AREA.height {
            for x in 0..AREA.width {
                buf[(x, y)].set_symbol("·");
            }
        }
        m.draw(&mut buf, AREA, &cards);
        // His head row facing left: `╭╯• │` — the gap by his eye is solid.
        let row: String = (20..27)
            .map(|x| buf[(x, 11)].symbol().to_string())
            .collect();
        assert_eq!(row, "╭╯• │··");
    }

    #[test]
    fn he_turns_round_whole() {
        let legs: String = STAND[3].chars().rev().map(mirror).collect();
        assert_eq!(legs, "  ╰┬─┬╯");
        let head: String = STAND[1].chars().rev().map(mirror).collect();
        assert_eq!(head, "╭╯• │  ");
    }
}
