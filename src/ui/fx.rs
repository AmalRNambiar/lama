//! The skins: the neon cyberpunk palette (the default), a calmer standard
//! one, and the ambient motion layered over either.
//!
//! Everything here is decoration on top of the real UI — it never changes
//! what a card or the timeline *says*, only how it glows. `LAMA_THEME=standard`
//! swaps the neon for quiet slate colours and keeps every animation;
//! `LAMA_THEME=classic` brings back the stock gold-on-dark look with no motion;
//! `LAMA_FX=off` keeps the palette but stills the animation. The lamaverse
//! also switches between cyberpunk and standard live ([`set_skin`]).
//!
//! Motion runs off one wall clock ([`now_ms`]), so the native loop and the
//! browser frontend animate identically without threading a timer through
//! `App`. Every effect is a pure function of (cell, time) — no state, no RNG —
//! so a frame is cheap and reproducible.

use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use web_time::Instant;

type Rgb = (u8, u8, u8);

/// One skin's truecolor set. Green stays "alive" and red stays "failed" in
/// every skin, same as the stock palette's meanings.
struct Colors {
    canvas: Rgb,
    surface: Rgb,
    muted: Rgb,
    subtle: Rgb,
    /// The skin's lead colour: the accent, and the rain's odd drop.
    accent: Rgb,
    text: Rgb,
    green: Rgb,
    red: Rgb,
    violet: Rgb,
    cyan: Rgb,
    yellow: Rgb,
    orange: Rgb,
    grid: Rgb,
}

/// Deep night base, hot pink accent, cyan/violet glow.
const CYBERPUNK: Colors = Colors {
    canvas: (7, 3, 18),
    surface: (16, 7, 34),
    muted: (72, 38, 128),
    subtle: (138, 112, 210),
    accent: (255, 42, 212),
    text: (228, 250, 255),
    green: (0, 255, 159),
    red: (255, 40, 90),
    violet: (155, 48, 255),
    cyan: (0, 229, 255),
    yellow: (252, 238, 10),
    orange: (255, 140, 0),
    grid: (40, 22, 72),
};

/// Slate base, soft blue accent, muted editor-like hues.
const STANDARD: Colors = Colors {
    canvas: (24, 26, 31),
    surface: (34, 37, 44),
    muted: (70, 76, 90),
    subtle: (132, 140, 156),
    accent: (97, 175, 239),
    text: (220, 223, 228),
    green: (152, 195, 121),
    red: (224, 108, 117),
    violet: (198, 120, 221),
    cyan: (86, 182, 194),
    yellow: (229, 192, 123),
    orange: (209, 154, 102),
    grid: (44, 48, 57),
};

/// The truecolor set in use; the classic skin draws its few truecolor
/// effects (it has no motion) from the cyberpunk one.
fn colors() -> &'static Colors {
    colors_of(skin())
}

fn colors_of(s: Skin) -> &'static Colors {
    match s {
        Skin::Standard => &STANDARD,
        Skin::Classic | Skin::Cyberpunk => &CYBERPUNK,
    }
}

/// A skin's lead colour, for its swatch in the theme picker.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) fn swatch(s: Skin) -> Color {
    rgb(colors_of(s).accent)
}

/// The canvas background of the skin in use.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub(crate) fn canvas() -> Color {
    rgb(colors().canvas)
}

/// Stops the cycling glow walks through, wrapping back to the first.
fn neon() -> [Rgb; 4] {
    let c = colors();
    [c.accent, c.violet, c.cyan, c.green]
}

/// Glyphs the background rain and glitch bursts draw from. ASCII and a few
/// box-adjacent symbols only, so the browser's font atlas has every one.
const RAIN: &[char] = &[
    '0', '1', '0', '1', '7', 'A', 'F', 'X', 'Z', '<', '>', '/', '\\', '|', '=', '+', '*', ':', ';',
    '$', '#', '%',
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skin {
    Classic,
    Cyberpunk,
    Standard,
}

impl Skin {
    /// A skin by its name (`cyberpunk`, `standard`, `classic`).
    pub fn named(name: &str) -> Option<Skin> {
        match name {
            "cyberpunk" => Some(Skin::Cyberpunk),
            "standard" => Some(Skin::Standard),
            "classic" => Some(Skin::Classic),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Skin::Cyberpunk => "cyberpunk",
            Skin::Standard => "standard",
            Skin::Classic => "classic",
        }
    }
}

struct Config {
    /// `LAMA_THEME`, when it names a skin: it wins over any other choice.
    forced: Option<Skin>,
    motion_off: bool,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        // No env in the browser: `var` errors and we fall through to defaults.
        let forced = std::env::var("LAMA_THEME")
            .ok()
            .and_then(|v| Skin::named(&v));
        let motion_off = matches!(
            std::env::var("LAMA_FX").as_deref(),
            Ok("off" | "0" | "false")
        );
        Config { forced, motion_off }
    })
}

/// The skin picked at run time (`0` = none yet), under `LAMA_THEME`.
static PICKED: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn skin() -> Skin {
    if let Some(s) = config().forced {
        return s;
    }
    match PICKED.load(std::sync::atomic::Ordering::Relaxed) {
        2 => Skin::Standard,
        3 => Skin::Classic,
        _ => Skin::Cyberpunk,
    }
}

/// Switch skins from here on, unless `LAMA_THEME` pins one.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub fn set_skin(s: Skin) {
    let n = match s {
        Skin::Cyberpunk => 1,
        Skin::Standard => 2,
        Skin::Classic => 3,
    };
    PICKED.store(n, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the skin is pinned by `LAMA_THEME`, so switching does nothing.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
pub fn skin_pinned() -> bool {
    config().forced.is_some()
}

/// Whether ambient motion is on. Off under the classic skin or `LAMA_FX=off`.
pub(crate) fn motion() -> bool {
    skin() != Skin::Classic && !config().motion_off
}

/// Milliseconds since the first frame asked — the clock every effect reads.
/// Frozen at 0 with motion off, so every effect settles to a still frame.
pub(crate) fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    if !motion() {
        return 0;
    }
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

fn rgb((r, g, b): (u8, u8, u8)) -> Color {
    Color::Rgb(r, g, b)
}

/// The flow palette for the active skin.
pub(crate) fn palette() -> rataflow::Palette {
    palette_of(skin())
}

fn palette_of(s: Skin) -> rataflow::Palette {
    let mut p = rataflow::Theme::Dark.palette();
    if s == Skin::Classic {
        p.accent = Color::Indexed(178);
        return p;
    }
    let c = colors_of(s);
    p.canvas_bg = rgb(c.canvas);
    p.surface = rgb(c.surface);
    p.muted = rgb(c.muted);
    p.subtle = rgb(c.subtle);
    p.accent = rgb(c.accent);
    p.text = rgb(c.text);
    p.success = rgb(c.green);
    p.error = rgb(c.red);
    p
}

/// Provider emblem colours: Claude in orange, Codex in cyan.
pub(crate) fn provider_color(claude: bool) -> Color {
    match (skin(), claude) {
        (Skin::Classic, true) => Color::Indexed(173),
        (Skin::Classic, false) => Color::Indexed(36),
        (_, true) => rgb(colors().orange),
        (_, false) => rgb(colors().cyan),
    }
}

/// Dim dot colour for the canvas grid, so the rain reads over it.
pub(crate) fn grid_color() -> Option<Color> {
    (skin() != Skin::Classic).then(|| rgb(colors().grid))
}

fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (mix(a.0, b.0), mix(a.1, b.1), mix(a.2, b.2))
}

/// 0..1 triangle-ish breathing value with the given period.
pub(crate) fn pulse(period_ms: u64, offset_ms: u64) -> f32 {
    let p = ((now_ms() + offset_ms) % period_ms) as f32 / period_ms as f32;
    (1.0 - (p * std::f32::consts::TAU).cos()) / 2.0
}

/// A colour sliding round the neon stops once per `period_ms`.
pub(crate) fn neon_cycle(period_ms: u64, offset_ms: u64) -> Color {
    let p = ((now_ms() + offset_ms) % period_ms) as f32 / period_ms as f32;
    let stops = neon();
    let pos = p * stops.len() as f32;
    let i = pos as usize % stops.len();
    rgb(lerp(stops[i], stops[(i + 1) % stops.len()], pos.fract()))
}

/// `a` brightened toward white by `t` — for a pulsing highlight.
pub(crate) fn glow(base: Color, t: f32) -> Color {
    match base {
        Color::Rgb(r, g, b) => rgb(lerp((r, g, b), (255, 255, 255), t)),
        other => other,
    }
}

/// Blend two colours: `t = 0` is `a`, `t = 1` is `b`. Truecolor only; an
/// indexed colour passes through as `a`.
pub(crate) fn mix(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            rgb(lerp((r1, g1, b1), (r2, g2, b2), t))
        }
        _ => a,
    }
}

/// Named neon accents for card details, falling back to stock colours in
/// the classic skin.
pub(crate) fn cyan() -> Color {
    match skin() {
        Skin::Classic => Color::Indexed(80),
        _ => rgb(colors().cyan),
    }
}

pub(crate) fn violet() -> Color {
    match skin() {
        Skin::Classic => Color::Indexed(141),
        _ => rgb(colors().violet),
    }
}

pub(crate) fn yellow() -> Color {
    match skin() {
        Skin::Classic => Color::Indexed(178),
        _ => rgb(colors().yellow),
    }
}

/// Cheap stable integer hash (splitmix-style) — the only "randomness" here.
pub(crate) fn hash(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A project's own colour, from its name: its chip on the lamaverse's
/// strip, the tag on its cards and the outline round them when lit.
pub(crate) fn project_color(name: &str) -> Color {
    let c = colors();
    let hues = [c.cyan, c.accent, c.green, c.yellow, c.violet, c.orange];
    rgb(hues[(hash_str(name) % hues.len() as u64) as usize])
}

pub(crate) fn hash_str(s: &str) -> u64 {
    s.bytes()
        .fold(0xCBF2_9CE4_8422_2325, |h, b| hash(h ^ b as u64))
}

/// A canvas cell nothing meaningful sits on: blank or a grid dot over the
/// canvas background. Cards, edges, chips and the minimap all fail this.
fn is_empty(buf: &Buffer, x: u16, y: u16, canvas: Color) -> bool {
    let cell = &buf[(x, y)];
    cell.bg == canvas && matches!(cell.symbol(), " " | "·")
}

/// Paint the ambient layer over the empty parts of the canvas: falling code
/// rain and the odd glitch burst. Runs after the graph
/// so it can only fill gaps, never cover content.
pub(crate) fn render_canvas_fx(buf: &mut Buffer, area: Rect) {
    if !motion() || area.width == 0 || area.height == 0 {
        return;
    }
    let c = colors();
    let canvas = rgb(c.canvas);
    let t = now_ms();
    let h = area.height as i64;

    // Code rain: about a third of the columns carry a drop, each with its own
    // speed, trail and gap, so the field never lines up.
    for col in 0..area.width {
        // Seeded by the absolute column, so side-by-side panes don't mirror.
        let seed = hash((area.x + col) as u64 ^ 0xC0DE);
        if seed % 100 >= 34 {
            continue;
        }
        let speed = 5 + (seed >> 8) % 12; // rows per second
        let trail = 3 + ((seed >> 16) % 9) as i64;
        let period = h + trail + ((seed >> 24) % 24) as i64;
        let head = ((t * speed / 1000 + (seed >> 32)) % period as u64) as i64;
        let tint = if (seed >> 40).is_multiple_of(5) {
            c.accent
        } else {
            c.cyan
        };
        for k in 0..=trail {
            let y = head - k;
            if !(0..h).contains(&y) {
                continue;
            }
            let (x, y) = (area.x + col, area.y + y as u16);
            if !is_empty(buf, x, y, canvas) {
                continue;
            }
            // The head flickers through glyphs; the trail holds its own.
            let flick = if k == 0 { t / 90 } else { 0 };
            let g = RAIN[(hash(seed ^ (y as u64) << 16 ^ flick) % RAIN.len() as u64) as usize];
            let fade = k as f32 / (trail + 1) as f32;
            let head_c = lerp(tint, (255, 255, 255), 0.35);
            let fg = if k == 0 {
                lerp(head_c, c.canvas, 0.45)
            } else {
                lerp(tint, c.canvas, 0.55 + fade * 0.4)
            };
            buf[(x, y)].set_char(g).set_fg(rgb(fg));
        }
    }

    // Glitch: in roughly one 160ms slot out of six, a short streak of junk
    // flashes on a random row.
    let slot = t / 160;
    let g = hash(slot ^ 0x611C);
    if g.is_multiple_of(6) {
        let y = area.y + ((g >> 8) % area.height as u64) as u16;
        let len = 4 + ((g >> 16) % 14) as u16;
        let x0 = area.x + ((g >> 24) % area.width as u64) as u16;
        let color = if (g >> 32).is_multiple_of(2) {
            c.accent
        } else {
            c.yellow
        };
        for x in x0..(x0 + len).min(area.right()) {
            if is_empty(buf, x, y, canvas) {
                let ch = RAIN[(hash(g ^ x as u64) % RAIN.len() as u64) as usize];
                buf[(x, y)]
                    .set_char(ch)
                    .set_fg(rgb(lerp(color, c.canvas, 0.3)));
            }
        }
    }
}

/// A bright spark circling a card's border — the "processing" tell on a
/// running agent. `offset` staggers cards so they don't orbit in lockstep.
pub(crate) fn render_border_spark(buf: &mut Buffer, area: Rect, base: Color, offset: u64) {
    if !motion() || area.width < 3 || area.height < 3 {
        return;
    }
    let (w, h) = (area.width as u64, area.height as u64);
    let perimeter = 2 * (w + h) - 4;
    // One lap every ~1.6s regardless of card size.
    let head = ((now_ms() + offset) * perimeter / 1600) % perimeter;
    for k in 0..6u64 {
        let i = (head + perimeter - k) % perimeter;
        // Walk the perimeter clockwise from the top-left corner.
        let (x, y) = if i < w {
            (i, 0)
        } else if i < w + h - 1 {
            (w - 1, i - w + 1)
        } else if i < 2 * w + h - 2 {
            (w - 1 - (i - (w + h - 2)), h - 1)
        } else {
            (0, h - 1 - (i - (2 * w + h - 3)))
        };
        let c = glow(base, 1.0 - k as f32 / 6.0);
        buf[(area.x + x as u16, area.y + y as u16)].set_fg(c);
    }
}

/// Braille spinner frame — the status bar's "still crunching" tick.
pub(crate) fn spinner() -> char {
    const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    FRAMES[(now_ms() / 80 % FRAMES.len() as u64) as usize]
}

/// An 8-bar equalizer whose height tracks how many agents are running: it
/// idles low with nothing going on and jumps when the swarm is busy.
pub(crate) fn equalizer(running: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let t = now_ms() as f32 / 1000.0;
    let energy = (0.25 + running as f32 * 0.25).min(1.0);
    (0..8)
        .map(|i| {
            let f = i as f32;
            let wave = ((t * (3.1 + f * 0.7) + f * 1.3).sin()
                + (t * (5.3 - f * 0.4) + f).sin() * 0.5)
                / 1.5;
            let level = ((wave + 1.0) / 2.0 * energy * 7.99) as usize;
            BARS[level.min(7)]
        })
        .collect()
}

/// The status-bar title, occasionally hit by a 100ms glitch: a couple of its
/// characters swap for junk, then it snaps back.
pub(crate) fn glitch_text(s: &str) -> String {
    let slot = now_ms() / 100;
    let g = hash(slot ^ 0x7177);
    if !motion() || !g.is_multiple_of(28) {
        return s.to_string();
    }
    s.chars()
        .enumerate()
        .map(|(i, c)| {
            if c != ' ' && hash(g ^ i as u64).is_multiple_of(5) {
                RAIN[(hash(g + i as u64) % RAIN.len() as u64) as usize]
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spark_walks_the_whole_border() {
        // Every perimeter index must land on the border of a 5x4 card.
        let (w, h) = (5u64, 4u64);
        let perimeter = 2 * (w + h) - 4;
        for i in 0..perimeter {
            let (x, y) = if i < w {
                (i, 0)
            } else if i < w + h - 1 {
                (w - 1, i - w + 1)
            } else if i < 2 * w + h - 2 {
                (w - 1 - (i - (w + h - 2)), h - 1)
            } else {
                (0, h - 1 - (i - (2 * w + h - 3)))
            };
            assert!(x < w && y < h, "i={i} -> ({x},{y})");
            assert!(x == 0 || y == 0 || x == w - 1 || y == h - 1);
        }
    }

    #[test]
    fn equalizer_is_eight_columns() {
        assert_eq!(equalizer(0).chars().count(), 8);
        assert_eq!(equalizer(9).chars().count(), 8);
    }

    #[test]
    fn standard_changes_the_colours_not_the_meanings() {
        // Pure lookups: the skin in use is global, and other tests draw.
        let (neon, calm) = (palette_of(Skin::Cyberpunk), palette_of(Skin::Standard));
        assert_ne!(calm.accent, neon.accent);
        assert_ne!(calm.canvas_bg, neon.canvas_bg);
        assert_ne!(calm.success, calm.error, "green is alive, red is failed");
        assert_eq!(Skin::named("standard"), Some(Skin::Standard));
        assert_eq!(Skin::named("neon"), None);
        assert_eq!(Skin::named(Skin::Cyberpunk.name()), Some(Skin::Cyberpunk));
    }

    #[test]
    fn glitch_keeps_length() {
        let s = "a session title";
        assert_eq!(glitch_text(s).chars().count(), s.chars().count());
    }
}
