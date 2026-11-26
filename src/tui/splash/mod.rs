//! The splash: the moment between starting the dashboard and its first
//! frame, while the first polls go out (any key skips it). `ui.splash`:
//!
//! * `cockpit` (the default, about 2 s) — the view from an alien ship's
//!   bridge as it drops out of warp at a black hole:
//!   - the jump: stars stream past, rings of a warp tunnel rush by, meteors
//!     streak; dropping out sends a shockwave and debris outward;
//!   - the hole: the stars bend round it (gravitational lensing) and ripple
//!     as its gravitational waves pass (a chirp, stretching one way while
//!     squeezing the other); its disc turns, brighter on the side coming
//!     toward you (Doppler beaming) and hotter inside, under a halo that is
//!     the disc's far side lensed over the top; matter spirals in, faster
//!     as it nears, and sparks are flung up its jets, which flicker with
//!     knots running out along them like a quasar's;
//!   - the sky: a pulsar sweeping two beams and flashing once a turn, an
//!     eclipsing binary whose star throws off a flare, a comet whose ion
//!     tail points away from the light, a nebula, diffraction spikes;
//!   - the bridge: its frame and alien script power up with a holographic
//!     flicker, read-outs type in, the reticle locks on, a radar sweep
//!     lights the stars it passes, and your fleet comes up as a
//!     constellation — machines as stars, their sessions orbiting them in
//!     their states' colours;
//!   - the end (`ui.splash_end`): the particles form the ℵ, or the ship
//!     falls into the ℵ, or it warps out to the fleet — or one at random;
//! * `classic` — the aleph under a band of light, its wordmark below;
//! * `off`.
//!
//! `ui.splash_fx` switches the effects one by one. Each cockpit frame is a
//! pure function of the time since it began, drawn in the foreground only:
//! the terminal's own background is never painted. A terminal too small
//! for it gets `classic`, or nothing.

use super::Dash;
use crate::config::Config;
use crate::ui::{self, c};
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use std::collections::HashMap;
use std::f64::consts::{PI, TAU};
use std::time::{Duration, Instant};

mod convergence;
mod cyberdeck;
mod horizon;
mod light;
mod matrix;
mod neon;
mod pulsar;
mod scene;
mod singularity;

/// How long each plays (any key ends it sooner).
const COCKPIT_MS: u64 = 2000;
const CLASSIC_MS: u64 = 1100;

/// The cockpit's moments (ms).
const WARP: (f64, f64) = (60.0, 950.0);
const DROP: f64 = 880.0;
const GROW: (f64, f64) = (700.0, 1150.0);
const LOCK: (f64, f64) = (850.0, 1200.0);
const UPLINK: f64 = 1150.0;
const FINALE: f64 = 1650.0;
const END: f64 = COCKPIT_MS as f64;

/// The smallest screens each needs.
const COCKPIT_MIN: (u16, u16) = (60, 18);
const CLASSIC_MIN: (u16, u16) = (20, 8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Cockpit,
    Classic,
    /// the cosmos (`scene.rs`): each a module of its own
    Singularity,
    Pulsar,
    Horizon,
    Matrix,
    Convergence,
    /// cyberpunk's (`cyberdeck.rs`, `neon.rs`)
    Cyberdeck,
    Neon,
    Off,
}

impl Kind {
    /// Every splash with a name (`ui.splash`, `aleph splash NAME`), the
    /// built ones only: random picks among the animated ones.
    pub const ALL: [Kind; 9] = [
        Kind::Cockpit,
        Kind::Classic,
        Kind::Singularity,
        Kind::Pulsar,
        Kind::Horizon,
        Kind::Matrix,
        Kind::Convergence,
        Kind::Cyberdeck,
        Kind::Neon,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Cockpit => "cockpit",
            Kind::Classic => "classic",
            Kind::Singularity => "singularity",
            Kind::Pulsar => "pulsar",
            Kind::Horizon => "event-horizon",
            Kind::Matrix => "matrix",
            Kind::Convergence => "convergence",
            Kind::Cyberdeck => "cyberdeck",
            Kind::Neon => "neon",
            Kind::Off => "off",
        }
    }

    /// One by its name (`event-horizon` or `horizon`).
    pub fn named(s: &str) -> Option<Kind> {
        let s = s.trim().to_lowercase();
        if s == "horizon" || s == "black-hole" {
            return Some(Kind::Horizon);
        }
        Kind::ALL
            .into_iter()
            .chain([Kind::Off])
            .find(|k| k.name() == s)
    }

    /// Is its scene there yet (a name nothing draws falls back to cockpit)?
    pub fn built(self) -> bool {
        // every one is, now: a new name stays out of here until it's drawn
        let _ = self;
        true
    }

    /// `ui.splash` (`random` without a seed: cockpit).
    pub fn of(cfg: &Config) -> Kind {
        Kind::pick(cfg, 0)
    }

    /// The splash a theme pairs with (`crate::look`), as far as it's built
    /// (else the cockpit).
    pub fn of_theme(theme: &str) -> Kind {
        match Kind::named(crate::look::of(theme).splash) {
            Some(k) if k.built() => k,
            _ => Kind::Cockpit,
        }
    }

    /// `ui.splash`, with `random` picking one of the animated ones by `seed`
    /// and `theme` the theme's own.
    pub fn pick(cfg: &Config, seed: u64) -> Kind {
        let v = cfg.get_or("ui.splash");
        if v == "theme" {
            return Kind::of_theme(&cfg.get_or("theme"));
        }
        if v == "random" {
            let all: Vec<Kind> = Kind::ALL
                .into_iter()
                .filter(|k| *k != Kind::Classic && k.built())
                .collect();
            return all[(seed % all.len() as u64) as usize];
        }
        match Kind::named(&v) {
            Some(k) if k.built() => k,
            _ => Kind::Cockpit,
        }
    }

    /// The one that fits a `w`×`h` screen (pure — tested).
    pub fn fit(self, w: u16, h: u16) -> Kind {
        let fits = |(mw, mh): (u16, u16)| w >= mw && h >= mh;
        match self {
            Kind::Off => Kind::Off,
            Kind::Cockpit if fits(COCKPIT_MIN) => Kind::Cockpit,
            k if k != Kind::Cockpit && k != Kind::Classic && fits(scene::MIN) => k,
            _ if fits(CLASSIC_MIN) => Kind::Classic,
            _ => Kind::Off,
        }
    }
}

/// `ui.splash_ms`: how long a splash plays (auto: each its own), 1–8 s.
pub(crate) fn len_of(cfg: &Config) -> Option<u64> {
    let v = cfg.get_or("ui.splash_ms");
    if v.is_empty() || v == "auto" {
        return None;
    }
    crate::log::parse_duration(&v).map(|d| (d.as_millis() as u64).clamp(1000, 8000))
}

/// `auto` plays every splash brisker than its moments are written: 15%
/// quicker, and never longer than `AUTO_MS` on screen, the logo and all (the
/// cosmos' 3 s story plays in 2). A length you set (`ui.splash_ms`, `--ms`)
/// is kept as it is.
const PACE: f64 = 0.85;
const AUTO_MS: f64 = 2000.0;

/// Screen time per moment of the story (1: as written).
fn pace(kind: Kind, opts: Opts, len: Option<u64>) -> f64 {
    if len.is_some() {
        return 1.0;
    }
    let t = total(kind, opts, None) as f64;
    if t <= 0.0 {
        return PACE;
    }
    PACE.min(AUTO_MS / t)
}

/// How long `kind` is on screen (ms): its story's length at `auto`'s pace.
pub(crate) fn played(kind: Kind, opts: Opts, len: Option<u64>) -> u64 {
    (total(kind, opts, len) as f64 * pace(kind, opts, len)).round() as u64
}

/// The moment of the story `ms` of screen time in (what `frame` draws).
pub(crate) fn story(kind: Kind, opts: Opts, ms: u64, len: Option<u64>) -> u64 {
    (ms as f64 / pace(kind, opts, len)).round() as u64
}

/// How long `kind`'s story is with `opts` (`len`: `ui.splash_ms`), in ms.
pub(crate) fn total(kind: Kind, opts: Opts, len: Option<u64>) -> u64 {
    match kind {
        Kind::Cockpit => {
            let scale = len.map_or(1.0, |l| l as f64 / COCKPIT_MS as f64);
            (opts.total() as f64 * scale) as u64
        }
        Kind::Classic => len.unwrap_or(CLASSIC_MS),
        Kind::Off => 0,
        _ => scene::total(len.unwrap_or(scene::SCENE_MS) as f64, opts),
    }
}

/// One frame of `kind` at `ms` (pure: tests, the dashboard, `aleph splash`,
/// the splash gallery all draw through it).
#[allow(clippy::too_many_arguments)]
pub(crate) fn frame(
    kind: Kind,
    buf: &mut Buffer,
    area: Rect,
    ms: u64,
    seed: u64,
    info: &Info,
    spin: &str,
    opts: Opts,
    len: Option<u64>,
) {
    match kind {
        Kind::Cockpit => {
            // ui.splash_ms stretches (or squeezes) the whole run
            let scale = len.map_or(1.0, |l| l as f64 / COCKPIT_MS as f64);
            cockpit(
                buf,
                area,
                (ms as f64 / scale) as u64,
                seed,
                info,
                spin,
                opts,
            )
        }
        Kind::Classic => {
            let total = len.unwrap_or(CLASSIC_MS);
            let art = classic_art(area);
            classic(buf, area, &art, ms * CLASSIC_MS / total.max(1))
        }
        Kind::Off => {}
        k => scene::draw(k, buf, area, ms, seed, info, spin, opts, len),
    }
}

/// How the cockpit ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum End {
    /// the particles gather into the ℵ, it flares, and bursts
    Form,
    /// the shadow takes the view, the sky squeezed into a ring round the ℵ
    Fall,
    /// the stars streak out again and the fleet rushes in
    Warp,
}

/// The effects `ui.splash_fx` switches, one bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fx(u16);

impl Fx {
    pub const NAMES: [&'static str; 12] = [
        "tunnel",
        "shock",
        "waves",
        "spirals",
        "comet",
        "flares",
        "meteors",
        "constellation",
        "radar",
        "scanlines",
        "glitch",
        "glow",
    ];
    const ALL: Fx = Fx((1 << 12) - 1);

    /// `all` (the default), `off`, a list (`comet, radar`), or all but some
    /// (`all, -glitch`). Pure — tested.
    pub fn parse(v: &str) -> Fx {
        let mut bits = 0u16;
        let mut any = false;
        for w in v.split(',').map(|w| w.trim().to_lowercase()) {
            match w.as_str() {
                "" => {}
                "all" | "on" => {
                    bits = Fx::ALL.0;
                    any = true;
                }
                "off" | "none" => any = true,
                w => {
                    let (minus, name) = match w.strip_prefix('-') {
                        Some(n) => (true, n),
                        None => (false, w),
                    };
                    if let Some(i) = Fx::NAMES.iter().position(|n| *n == name) {
                        any = true;
                        if minus {
                            if bits == 0 {
                                bits = Fx::ALL.0;
                            }
                            bits &= !(1 << i);
                        } else {
                            bits |= 1 << i;
                        }
                    }
                }
            }
        }
        if any {
            Fx(bits)
        } else {
            Fx::ALL
        }
    }

    fn has(self, name: &str) -> bool {
        Fx::NAMES
            .iter()
            .position(|n| *n == name)
            .is_some_and(|i| self.0 & (1 << i) != 0)
    }
}

/// What a cockpit run looks like.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Opts {
    pub end: End,
    pub fx: Fx,
    /// how long the Aleph logo stays up at the end (ms)
    pub hold: f64,
    /// does the run end on the logo, gathered from particles?
    pub logo: bool,
}

impl Opts {
    /// From `ui.splash_end` and `ui.splash_fx`; `random` picks an end by `seed`.
    pub fn of(cfg: &Config, seed: u64) -> Opts {
        let logo = cfg.get_or("ui.splash_logo") != "off";
        let end = match cfg.get_or("ui.splash_end").as_str() {
            // forming builds the logo: without it, the ship falls in
            "form" if logo => End::Form,
            "form" | "fall" => End::Fall,
            "warp" => End::Warp,
            _ if logo => [End::Form, End::Fall, End::Warp][(seed % 3) as usize],
            _ => [End::Fall, End::Warp][(seed % 2) as usize],
        };
        let hold = crate::log::parse_duration(&cfg.get_or("ui.splash_hold"))
            .map_or(HOLD, |d| d.as_millis() as f64)
            .clamp(0.0, 3000.0);
        Opts {
            end,
            fx: Fx::parse(&cfg.get_or("ui.splash_fx")),
            hold,
            logo,
        }
    }

    /// How long the whole run takes (ms): the story, the logo held, and
    /// (forming) its burst.
    pub fn total(self) -> u64 {
        if !self.logo {
            return END as u64;
        }
        let (start, dur) = self.gather();
        (start + dur + self.hold + if self.end == End::Form { BURST } else { 0.0 }) as u64
    }

    /// When the particles start gathering into the logo, and how long they
    /// take: forming, as the scene goes; falling in and warping out, as their
    /// own ends play out (a share of the hold, so a short hold still ends on it).
    fn gather(self) -> (f64, f64) {
        match self.end {
            End::Form => (FINALE, GATHER),
            _ => (FINALE + 100.0, GATHER),
        }
    }
}

/// The pulsar coming apart into the logo: shattering, the cloud sheared into
/// a vortex, the stream settling into the mark (shares of `GATHER`).
const GATHER: f64 = 600.0;
const SHATTER: f64 = 0.3;
const SHEAR: f64 = 0.65;

/// The logo's time on screen by default, and the burst after it (forming).
const HOLD: f64 = 700.0;
const BURST: f64 = 150.0;

/// A machine as the bridge sees it: its name (as it should read on screen),
/// colour, whether it answers, and its sessions' colours by state.
#[derive(Clone, Debug, Default)]
pub(crate) struct Mach {
    pub name: String,
    pub rgb: (u8, u8, u8),
    pub up: bool,
    pub sessions: Vec<(u8, u8, u8)>,
}

/// What the bridge reads out: the fleet as the last snapshot knew it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Info {
    pub machines: usize,
    pub sessions: usize,
    pub needs: usize,
    pub fleet: Vec<Mach>,
}

fn rgb_of(col: Color, or: (u8, u8, u8)) -> (u8, u8, u8) {
    match col {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => or,
    }
}

impl Info {
    pub fn of(d: &Dash) -> Info {
        Info::from_fleet(&d.fleet)
    }

    /// From a fleet (the dashboard's, or the cached snapshot's for `aleph splash`).
    pub fn from_fleet(fl: &crate::fleet::Fleet) -> Info {
        let all = || fl.hosts.iter().flat_map(|h| &h.sessions);
        let fleet = fl
            .hosts
            .iter()
            .map(|h| Mach {
                name: crate::showcase::shown(&h.alias).into_owned(),
                rgb: rgb_of(h.color, (226, 180, 81)),
                up: matches!(h.conn, crate::fleet::Conn::Up),
                sessions: h
                    .sessions
                    .iter()
                    .map(|s| {
                        let col = if s.blocked() {
                            c::NEEDS()
                        } else {
                            match s.status.as_str() {
                                "working" => c::WORKING(),
                                "done" => c::DONE(),
                                _ => c::IDLE(),
                            }
                        };
                        rgb_of(col, (150, 150, 160))
                    })
                    .collect(),
            })
            .collect();
        Info {
            machines: fl.hosts.len(),
            sessions: all().count(),
            needs: all().filter(|s| s.blocked()).count(),
            fleet,
        }
    }
}

/// Play it on the dashboard's screen before its first frame.
pub(crate) fn run(tui: &mut crate::term::Tui, cfg: &Config, info: &Info) {
    // started again on the same screen (the demo world switched): once is enough
    if std::env::var_os("ALEPH_NO_SPLASH").is_some() || !crate::showcase::splash_wanted(cfg) {
        return;
    }
    let seed = seed_now();
    let kind = Kind::pick(cfg, seed);
    play(
        tui,
        kind,
        Opts::of(cfg, seed),
        len_of(cfg),
        info,
        seed,
        false,
    );
}

/// A seed from the clock: every run its own.
pub(crate) fn seed_now() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(7);
    spread(t ^ (u64::from(std::process::id()) << 32))
}

/// A seed's bits spread (splitmix64): macOS's clock ticks in microseconds,
/// so a raw nanosecond count is always a multiple of 1000 — and `seed % 8`
/// always 0: `ui.splash = random` played the same splash every time.
pub(crate) fn spread(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Play `kind` full screen (any key ends it; `looped`: again and again,
/// each its own seed, until a key). True when a key ended it.
pub(crate) fn play(
    tui: &mut crate::term::Tui,
    kind: Kind,
    opts: Opts,
    len: Option<u64>,
    info: &Info,
    seed: u64,
    looped: bool,
) -> bool {
    use crossterm::event::{self, Event, KeyEventKind};
    let size = tui.term.size().unwrap_or_default();
    let kind = kind.fit(size.width, size.height);
    let total = played(kind, opts, len);
    if total == 0 {
        return false;
    }
    let mut seed = seed;
    let mut t0 = Instant::now();
    // a steady clock: each frame starts on its own 60 Hz tick, whatever the
    // last one took to draw (a wait after every draw made frames uneven)
    const TICK: Duration = Duration::from_micros(16_667);
    let mut next = Instant::now();
    loop {
        next += TICK;
        let mut ms = t0.elapsed().as_millis() as u64;
        if ms >= total {
            if !looped {
                return false;
            }
            t0 = Instant::now();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            ms = 0;
        }
        let spin = ui::spinner(t0);
        let drawn = tui.term.draw(|f| {
            let area = f.area();
            let at = story(kind, opts, ms, len);
            frame(kind, f.buffer_mut(), area, at, seed, info, spin, opts, len);
            ui::ground(f.buffer_mut());
            crate::showcase::buffer(f.buffer_mut());
        });
        if drawn.is_err() {
            return false;
        }
        // a frame running late starts the next at once and drops the clock
        // back in step (no burst of catch-up frames)
        let now = Instant::now();
        if next < now {
            next = now;
        }
        if event::poll(next - now).unwrap_or(false) {
            if let Ok(Event::Key(k)) = event::read() {
                if k.kind != KeyEventKind::Release {
                    // (showcase: the panic key pauses or resumes here too,
                    // and ends the splash like any key)
                    crate::showcase::panic_key(&super::key_str(&k));
                    return true;
                }
            }
        }
    }
}

// ------------------------------------------------------------------ helpers

/// 0 before `a`, 1 after `b`, smoothly between.
fn smooth(t: f64, a: f64, b: f64) -> f64 {
    let x = ((t - a) / (b - a)).clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// Up over `a`–`b`, down again over `c`–`d`.
fn window(t: f64, a: f64, b: f64, c: f64, d: f64) -> f64 {
    smooth(t, a, b) * (1.0 - smooth(t, c, d))
}

fn mix(a: (u8, u8, u8), b: (u8, u8, u8), k: f64) -> (u8, u8, u8) {
    let k = k.clamp(0.0, 1.0);
    let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * k).round() as u8;
    (m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
}

/// Along a ramp of colours, 0 to 1.
fn ramp(stops: &[(u8, u8, u8)], f: f64) -> (u8, u8, u8) {
    let f = f.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
    let i = (f.floor() as usize).min(stops.len() - 2);
    mix(stops[i], stops[i + 1], f - i as f64)
}

fn hash(a: u64, b: u64) -> u64 {
    let mut x = a ^ b.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// A number in 0..1 from a hash and a lane.
fn unit(h: u64, lane: u64) -> f64 {
    (hash(h, lane) >> 11) as f64 / (1u64 << 53) as f64
}

/// A braille cell with `dots` of its eight dots up, which ones from `h`.
fn braille(dots: u32, h: u64) -> char {
    let mut order = [0u32, 1, 2, 3, 4, 5, 6, 7];
    for i in (1..8).rev() {
        order.swap(i, (hash(h, i as u64) % (i as u64 + 1)) as usize);
    }
    let bits = order[..dots.min(8) as usize]
        .iter()
        .fold(0u32, |m, b| m | 1 << b);
    char::from_u32(0x2800 + bits).unwrap_or(' ')
}

/// A braille cell drawn from `f` (a brightness at a point, in cells):
/// each of its eight dots is up when `f` there beats its threshold — a
/// fixed half for crisp lines, or a Bayer order (`dither`) so a gradient
/// comes out even. The glyph, and the brightest dot that's up.
fn braille_at(x: i32, y: i32, dither: bool, f: impl Fn(f64, f64) -> f64) -> Option<(char, f64)> {
    const BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    const ORDER: [[f64; 4]; 2] = [[0.0, 6.0, 1.0, 7.0], [4.0, 2.0, 5.0, 3.0]];
    let (mut bits, mut top) = (0u32, 0f64);
    for i in 0..2 {
        for j in 0..4 {
            let val = f(
                x as f64 - 0.25 + i as f64 * 0.5,
                y as f64 - 0.375 + j as f64 * 0.25,
            );
            let th = if dither {
                (ORDER[i][j] + 0.5) / 8.0
            } else {
                0.5
            };
            if val > th {
                bits |= BITS[i][j];
                top = top.max(val);
            }
        }
    }
    (bits != 0).then(|| (char::from_u32(0x2800 + bits).unwrap_or(' '), top))
}

/// Thin curves in braille, gathered dot by dot: a point in cells lands on
/// one of its cell's eight dots.
#[derive(Default)]
struct Dots(HashMap<(i32, i32), (u32, f64)>);

impl Dots {
    fn add(&mut self, x: f64, y: f64, k: f64) {
        const BITS: [[u32; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
        let (cx, cy) = (x.round(), y.round());
        let i = (((x - cx + 0.5) * 2.0).floor() as usize).min(1);
        let j = (((y - cy + 0.5) * 4.0).floor() as usize).min(3);
        let e = self.0.entry((cx as i32, cy as i32)).or_insert((0, 0.0));
        e.0 |= BITS[i][j];
        e.1 = e.1.max(k);
    }

    /// A circle of `r` rows round `(cx, cy)` (cells), squashed by `flat`.
    fn ring(&mut self, cx: f64, cy: f64, r: f64, flat: f64, k: f64) {
        if r <= 0.0 {
            return;
        }
        let n = ((TAU * r * 4.0) as usize).clamp(12, 4000);
        for s in 0..n {
            let a = TAU * s as f64 / n as f64;
            self.add(cx + a.cos() * r * 2.0, cy + a.sin() * r * flat, k);
        }
    }

    fn draw(self, cv: &mut Canvas, rgb: impl Fn(i32, i32, f64) -> (u8, u8, u8)) {
        for ((x, y), (bits, k)) in self.0 {
            let ch = char::from_u32(0x2800 + bits).unwrap_or(' ');
            let mut b = [0u8; 4];
            cv.put(x, y, ch.encode_utf8(&mut b), rgb(x, y, k), k);
        }
    }
}

/// A line's glyph for a direction on screen (x, y in rows: y grows down).
fn stroke(dx: f64, dy: f64) -> &'static str {
    let a = dy.atan2(dx).rem_euclid(PI);
    if !(PI / 8.0..7.0 * PI / 8.0).contains(&a) {
        "─"
    } else if a < 3.0 * PI / 8.0 {
        "╲"
    } else if a < 5.0 * PI / 8.0 {
        "│"
    } else {
        "╱"
    }
}

/// What the instruments are written in.
const ALIEN: [&str; 20] = [
    "⊹", "⋔", "⌖", "⟡", "◬", "◭", "⧗", "⌬", "⊶", "⊷", "⋈", "⍟", "⎔", "⟐", "⊿", "⋇", "⌁", "⏚", "⍜",
    "⟟",
];

/// Star colours by spectral class (O/B blue-white to M orange), and how
/// common each is in this sky.
const CLASSES: [((u8, u8, u8), f64); 7] = [
    ((140, 165, 255), 0.08),
    ((175, 198, 255), 0.12),
    ((214, 226, 255), 0.16),
    ((250, 248, 255), 0.22),
    ((255, 238, 200), 0.18),
    ((255, 206, 150), 0.14),
    ((255, 168, 110), 0.10),
];

fn class(h: u64) -> (u8, u8, u8) {
    let mut x = unit(h, 9);
    for (rgb, p) in CLASSES {
        if x < p {
            return rgb;
        }
        x -= p;
    }
    CLASSES[3].0
}

/// The disc, hottest inside: white, gold, orange, ember, deep red.
const DISC: [(u8, u8, u8); 6] = [
    (255, 250, 232),
    (255, 222, 150),
    (236, 182, 80),
    (232, 124, 46),
    (176, 62, 34),
    (110, 30, 40),
];

const TEAL: (u8, u8, u8) = (110, 200, 190);
const CYAN: (u8, u8, u8) = (120, 220, 255);
const VIOLET: (u8, u8, u8) = (175, 135, 255);
const ROSE: (u8, u8, u8) = (230, 120, 170);

/// Drawing into the frame: the foreground only, inside its area.
struct Canvas<'a> {
    buf: &'a mut Buffer,
    area: Rect,
    light: bool,
    mono: bool,
    /// a scanline's row while the bridge's instruments draw (they brighten
    /// as it passes)
    scan: Option<f64>,
}

impl Canvas<'_> {
    fn colour(&self, rgb: (u8, u8, u8), k: f64) -> Color {
        if self.mono {
            return Color::Reset;
        }
        // from the background's own darkness (or paper) toward the colour
        let (from, to) = if self.light {
            ((228, 224, 214), mix(rgb, (40, 30, 60), 0.45))
        } else {
            ((16, 16, 22), rgb)
        };
        let (r, g, b) = mix(from, to, k);
        Color::Rgb(r, g, b)
    }

    fn put(&mut self, x: i32, y: i32, sym: &str, rgb: (u8, u8, u8), k: f64) {
        self.put_with(x, y, sym, rgb, k, k > 0.85);
    }

    fn put_with(&mut self, x: i32, y: i32, sym: &str, rgb: (u8, u8, u8), k: f64, bold: bool) {
        if k < 0.05 || x < 0 || y < 0 || x >= self.area.width as i32 || y >= self.area.height as i32
        {
            return;
        }
        let k = match self.scan {
            Some(s) => (k * (1.0 + 0.45 * (-(y as f64 - s).powi(2) / 1.2).exp())).min(1.0),
            None => k,
        };
        let fg = self.colour(rgb, k);
        let pos = (self.area.x + x as u16, self.area.y + y as u16);
        if let Some(cell) = self.buf.cell_mut(pos) {
            // whatever background it had stays (a popup\'s surface, the ground)
            let bg = cell.bg;
            cell.reset();
            cell.bg = bg;
            cell.set_symbol(sym).set_fg(fg);
            if bold {
                cell.modifier.insert(Modifier::BOLD);
            }
        }
    }

    fn text(&mut self, x: i32, y: i32, s: &str, rgb: (u8, u8, u8), k: f64) {
        for (x, ch) in (x..).zip(s.chars()) {
            let mut b = [0u8; 4];
            self.put_with(x, y, ch.encode_utf8(&mut b), rgb, k, false);
        }
    }

    fn blank(&self, x: i32, y: i32) -> bool {
        x >= 0
            && y >= 0
            && x < self.area.width as i32
            && y < self.area.height as i32
            && self.buf[(self.area.x + x as u16, self.area.y + y as u16)].symbol() == " "
    }

    fn clear(&mut self, x: i32, y: i32) {
        if x < 0 || y < 0 || x >= self.area.width as i32 || y >= self.area.height as i32 {
            return;
        }
        let pos = (self.area.x + x as u16, self.area.y + y as u16);
        if let Some(cell) = self.buf.cell_mut(pos) {
            // whatever background it had stays (a popup\'s surface, the ground)
            let bg = cell.bg;
            cell.reset();
            cell.bg = bg;
        }
    }
}

/// Where everything is, at one moment.
struct View {
    t: f64,
    w: f64,
    h: f64,
    /// the hole's centre (cells)
    cx: f64,
    cy: f64,
    /// its shadow's radius now, and once formed (rows; a cell is half as wide)
    rs: f64,
    full: f64,
    /// how far it has formed (0–1)
    grow: f64,
    /// how the run ends, how far into its end it is (0–1), how far the ship
    /// has fallen in (falling only), and how much of the scene is left
    end: End,
    out: f64,
    fall: f64,
    dim: f64,
    fx: Fx,
    /// the Aleph logo's presence (0–1), held to the end, and (forming) how
    /// far it has burst
    logo: f64,
    burst: f64,
    hold: f64,
    /// the logo's gathering: whether there is one, its start and length
    logo_on: bool,
    gather: (f64, f64),
}

impl View {
    fn at(area: Rect, ms: u64, opts: Opts) -> View {
        let (w, h) = (area.width as f64, area.height as f64);
        let t = ms as f64;
        let full = (h * 0.13).min(w * 0.065).clamp(2.0, 9.0);
        let grow = smooth(t, GROW.0, GROW.1);
        let out = smooth(t, FINALE, END);
        let fall = if opts.end == End::Fall {
            out.powi(2)
        } else {
            0.0
        };
        let dim = match opts.end {
            End::Fall => 1.0 - fall,
            End::Warp => 1.0 - smooth(t, FINALE, FINALE + 220.0),
            End::Form => 1.0 - smooth(t, FINALE, FINALE + 160.0),
        };
        let reach = (w / 4.0).hypot(h / 2.0) + 2.0;
        let gather = opts.gather();
        let logo = if opts.logo {
            smooth(t, gather.0, gather.0 + gather.1)
        } else {
            0.0
        };
        let burst = if opts.end == End::Form && opts.logo {
            let done = gather.0 + gather.1 + opts.hold;
            smooth(t, done, done + BURST)
        } else {
            0.0
        };
        View {
            t,
            w,
            h,
            cx: (w / 2.0).floor(),
            cy: ((h - 3.0) / 2.0).floor().max(1.0),
            rs: full * grow + reach * fall,
            full,
            grow,
            end: opts.end,
            out,
            fall,
            dim,
            fx: opts.fx,
            logo,
            burst,
            hold: opts.hold,
            logo_on: opts.logo,
            gather,
        }
    }

    /// A cell's offset from the centre, in rows.
    fn rel(&self, x: f64, y: f64) -> (f64, f64) {
        ((x - self.cx) / 2.0, y - self.cy)
    }

    /// Where light from `(x, y)` is seen with the hole in the way: pushed
    /// out from it (the lens equation's outer image), and rippled by its
    /// gravitational waves as they pass.
    fn lensed(&self, x: f64, y: f64) -> (f64, f64) {
        let re = self.full * 1.5 * self.grow;
        let (u, v) = self.rel(x, y);
        let r = u.hypot(v);
        if r < 1e-6 {
            return (x, y);
        }
        let k = if re > 0.0 {
            (r + (r * r + 4.0 * re * re).sqrt()) / 2.0 / r
        } else {
            1.0
        };
        let (mut u, mut v) = (u * k, v * k);
        let h = self.strain(r * k);
        // a plus-polarised wave: stretched along one axis, squeezed along the other
        u *= 1.0 + h / 2.0;
        v *= 1.0 - h / 2.0;
        (self.cx + u * 2.0, self.cy + v)
    }

    /// The gravitational wave's strain at `r` rows out (a chirp: it quickens).
    fn strain(&self, r: f64) -> f64 {
        if !self.fx.has("waves") {
            return 0.0;
        }
        let a = window(self.t, 950.0, 1030.0, 1380.0, 1520.0);
        if a <= 0.0 {
            return 0.0;
        }
        let s = (self.t - 950.0) / 1000.0;
        let phase = TAU * (s * (3.5 + 9.0 * s) - r / 7.0);
        0.22 * a * phase.sin() / (1.0 + r / 24.0)
    }
}

/// How far the ship has travelled by `t` (depth units): a crawl, the jump
/// to warp and out of it, and (warping out) the jump on to the fleet.
fn travel(t: f64, end: End) -> f64 {
    let on = if end == End::Warp {
        3.2 * smooth(t, FINALE, END).powi(2) + (t - END).max(0.0) * 0.0035
    } else {
        0.0
    };
    0.05 * t / 1000.0 + 2.2 * smooth(t, WARP.0, WARP.1) + on
}

/// The fastest 50 ms of the jump, for the velocity gauge.
const WARP_STEP: f64 = 2.2 * 1.5 / (WARP.1 - WARP.0) * 50.0;

/// How fast the ship goes at `t`, 0 to 1 (the gauge, the tunnel).
fn speed(t: f64, end: End) -> f64 {
    ((travel(t, end) - travel((t - 50.0).max(0.0), end)) / WARP_STEP).clamp(0.0, 1.0)
}

// ------------------------------------------------------------------ cockpit

/// One frame of the cockpit, `ms` in (pure).
pub(crate) fn cockpit(
    buf: &mut Buffer,
    area: Rect,
    ms: u64,
    seed: u64,
    info: &Info,
    spin: &str,
    opts: Opts,
) {
    let pal = c::pal();
    let mut cv = Canvas {
        buf,
        area,
        light: pal.light,
        mono: pal.mono,
        scan: None,
    };
    let v = View::at(area, ms, opts);
    nebula(&mut cv, &v, seed);
    tunnel(&mut cv, &v);
    stars(&mut cv, &v, seed);
    meteors(&mut cv, &v, seed);
    comet(&mut cv, &v);
    binary(&mut cv, &v);
    pulsar(&mut cv, &v);
    hole(&mut cv, &v);
    spirals(&mut cv, &v, seed);
    jets(&mut cv, &v, seed);
    shock(&mut cv, &v, seed);
    spikes(&mut cv, &v, seed);
    radar(&mut cv, &v);
    constellation(&mut cv, &v, info);
    hud(&mut cv, &v, info);
    swallow(&mut cv, &v);
    warp_out(&mut cv, &v);
    form(&mut cv, &v, seed);
    console(&mut cv, &v, info, spin);
}

/// A stellar nursery in the lower right: braille cloud, teal through
/// violet into rose.
fn nebula(cv: &mut Canvas, v: &View, seed: u64) {
    let a = smooth(v.t, 500.0, 1000.0) * v.dim;
    if a <= 0.0 {
        return;
    }
    let (nx, ny) = (v.w * 0.8, v.h * 0.66);
    let (rx, ry) = (v.w * 0.17, v.h * 0.24);
    let tt = v.t * 0.0009;
    for y in (ny - ry) as i32..=(ny + ry) as i32 {
        for x in (nx - rx) as i32..=(nx + rx) as i32 {
            let (fx, fy) = (x as f64, y as f64);
            let d = ((fx - nx) / rx).hypot((fy - ny) / ry);
            if d >= 1.0 {
                continue;
            }
            let n = (fx * 0.23 + (fy * 0.31).sin() * 1.7 + tt).sin()
                * (fy * 0.41 + fx * 0.07 - tt).sin()
                + 0.5 * ((fx + fy) * 0.13 + 2.0).sin();
            let k = ((1.0 - d).powf(1.3) * (0.45 + 0.4 * n)).clamp(0.0, 1.0) * a;
            let dots = (k * 6.0) as u32;
            if dots == 0 {
                continue;
            }
            let f = (fx - nx + rx) / (2.0 * rx);
            let hue = ramp(&[TEAL, VIOLET, ROSE], f);
            let mut b = [0u8; 4];
            let ch = braille(dots, hash(seed, (x as u64) << 20 | y as u64));
            cv.put(x, y, ch.encode_utf8(&mut b), hue, 0.2 + 0.32 * k);
        }
    }
}

/// The warp tunnel: rings rushing past while the ship is at speed.
fn tunnel(cv: &mut Canvas, v: &View) {
    if !v.fx.has("tunnel") {
        return;
    }
    let sp = speed(v.t, v.end);
    if sp < 0.08 {
        return;
    }
    let reach = (v.w / 4.0).hypot(v.h / 2.0);
    let mut dots = Dots::default();
    let phase = travel(v.t, v.end) * 2.2;
    for j in 0..7 {
        let p = (phase + j as f64 / 7.0).fract();
        // a ring far off is small; it grows faster as it comes past
        let r = p.powf(2.2) * reach;
        let k = sp * (0.18 + 0.5 * p) * (1.0 - p).powf(0.4);
        dots.ring(v.cx, v.cy, r, 0.55, k);
    }
    dots.draw(cv, |_, y, _| {
        let f = ((y as f64 - v.cy).abs() / (v.h / 2.0)).min(1.0);
        mix(CYAN, VIOLET, f)
    });
}

/// The starfield: each star a point in depth, streaming past through the
/// jump, lensed by the hole once it's there, rippled by its waves, lit by
/// the radar's sweep, twinkling all along.
fn stars(cv: &mut Canvas, v: &View, seed: u64) {
    let n = ((v.w * v.h) / 11.0).clamp(60.0, 1100.0) as u64;
    let fade = smooth(v.t, 0.0, 250.0);
    let (now, before) = (travel(v.t, v.end), travel((v.t - 45.0).max(0.0), v.end));
    let fov = (v.w * 0.3, v.h * 0.3);
    let project = |x: f64, y: f64, z: f64| (v.cx + x / z * fov.0, v.cy + y / z * fov.1);
    let glow = v.fx.has("glow");
    for i in 0..n {
        let h0 = hash(seed, i);
        let d = unit(h0, 0) + now;
        let lap = d.floor();
        let z = 1.0 - (d - lap);
        if z < 0.035 {
            continue;
        }
        let hk = hash(h0, lap as u64);
        let (x, y) = (unit(hk, 1) * 2.0 - 1.0, unit(hk, 2) * 2.0 - 1.0);
        let (sx, sy) = v.lensed_pt(project(x, y, z));
        let twinkle =
            0.76 + 0.24 * (v.t * 0.005 * (1.0 + 2.0 * unit(h0, 5)) + unit(h0, 6) * TAU).sin();
        let lit = 1.0 + 0.8 * sweep_glow(v, sx, sy);
        let k = ((0.22 + 0.78 * (1.0 - z).powf(1.4)) * twinkle * fade * lit).min(1.0);
        let rgb = class(h0);
        // where it was 45 ms ago (the same lap): a streak when it moved far
        let dp = unit(h0, 0) + before;
        let streak = (dp.floor() == lap).then(|| {
            let zp = 1.0 - (dp - lap);
            v.lensed_pt(project(x, y, zp.max(0.035)))
        });
        match streak {
            Some((px, py)) if (sx - px).abs() / 2.0 + (sy - py).abs() > 1.6 => {
                let (dx, dy) = (sx - px, sy - py);
                let glyph = stroke(dx / 2.0, dy);
                let steps = dx.abs().max(dy.abs()).ceil().min(60.0) as i32;
                for j in 0..=steps {
                    let f = j as f64 / steps as f64;
                    // hotter at the head: blue-shifted toward us
                    let col = mix(rgb, (215, 230, 255), f * 0.5);
                    cv.put(
                        (px + dx * f).round() as i32,
                        (py + dy * f).round() as i32,
                        glyph,
                        col,
                        k * (0.25 + 0.75 * f),
                    );
                }
            }
            _ => {
                let glyph = match k {
                    k if k < 0.32 => "·",
                    k if k < 0.52 => "∙",
                    k if k < 0.78 => "•",
                    _ => "✦",
                };
                let (gx, gy) = (sx.round() as i32, sy.round() as i32);
                // a bright star's glow round it
                if glow && k > 0.7 {
                    for (ox, oy) in [(-1, 0), (1, 0)] {
                        if cv.blank(gx + ox, gy + oy) {
                            cv.put(gx + ox, gy + oy, "·", rgb, k * 0.28);
                        }
                    }
                }
                cv.put(gx, gy, glyph, rgb, k);
            }
        }
    }
}

impl View {
    fn lensed_pt(&self, p: (f64, f64)) -> (f64, f64) {
        self.lensed(p.0, p.1)
    }
}

/// A short shower of meteors through the jump: bright heads, fading trails.
fn meteors(cv: &mut Canvas, v: &View, seed: u64) {
    if !v.fx.has("meteors") {
        return;
    }
    for i in 0..5u64 {
        let t0 = 260.0 + i as f64 * 130.0 + unit(seed, 40 + i) * 60.0;
        let p = (v.t - t0) / 240.0;
        if !(0.0..1.0).contains(&p) {
            continue;
        }
        let (x0, y0) = (
            v.w * (0.25 + 0.6 * unit(seed, 50 + i)),
            v.h * 0.1 * unit(seed, 60 + i),
        );
        let (dx, dy) = (-1.6, 0.7);
        let travel = v.h * 0.55;
        let (hx, hy) = (x0 + dx * p * travel, y0 + dy * p * travel);
        let tail = 7;
        for j in 0..tail {
            let f = j as f64 / tail as f64;
            let (x, y) = (hx - dx * f * 4.0, hy - dy * f * 4.0);
            let col = mix((255, 250, 235), (255, 170, 80), f);
            let k = (1.0 - f).powf(1.4) * (1.0 - p * 0.6);
            let g = if j == 0 { "•" } else { stroke(dx / 2.0, dy) };
            cv.put(x.round() as i32, y.round() as i32, g, col, k);
        }
    }
}

/// A comet crossing the upper sky: its ion tail straight, pointing away
/// from the light (the hole's blazing disc), its dust tail curving back
/// along the way it came.
fn comet(cv: &mut Canvas, v: &View) {
    if !v.fx.has("comet") {
        return;
    }
    let a = window(v.t, 450.0, 650.0, FINALE - 100.0, FINALE + 100.0);
    if a <= 0.0 {
        return;
    }
    let p = smooth(v.t, 400.0, 1900.0);
    let (hx, hy) = (
        v.w * (0.06 + 0.5 * p),
        v.h * (0.3 - 0.16 * p) + (p * PI).sin() * -1.5,
    );
    let (u, w) = v.rel(hx, hy);
    let r = u.hypot(w).max(1e-3);
    let away = (u / r, w / r);
    // the way it's going (rows)
    let vel = (0.25, -0.08);
    for (dir, len, col, g) in [
        (away, 7.0_f64, (150, 205, 255), 0.8_f64),
        (
            (
                (away.0 * 0.7 - vel.0 * 2.0).clamp(-1.0, 1.0),
                (away.1 * 0.7 - vel.1 * 2.0).clamp(-1.0, 1.0),
            ),
            5.0,
            (245, 225, 170),
            0.6,
        ),
    ] {
        let n = (dir.0.hypot(dir.1)).max(1e-3);
        let dir = (dir.0 / n, dir.1 / n);
        let glyph = stroke(dir.0, dir.1);
        let mut s = 0.6;
        while s <= len {
            let k = a * g * (1.0 - s / len).powf(1.2);
            let (x, y) = (hx + dir.0 * s * 2.0, hy + dir.1 * s);
            cv.put(x.round() as i32, y.round() as i32, glyph, col, k);
            s += 0.5;
        }
    }
    cv.put(
        hx.round() as i32 - 1,
        hy.round() as i32,
        "·",
        (200, 235, 255),
        a * 0.4,
    );
    cv.put(
        hx.round() as i32 + 1,
        hy.round() as i32,
        "·",
        (200, 235, 255),
        a * 0.4,
    );
    cv.put_with(
        hx.round() as i32,
        hy.round() as i32,
        "✦",
        (225, 245, 255),
        a,
        true,
    );
}

/// An eclipsing binary in the lower left: two stars round their common
/// centre, the heavier closer in, on an orbit seen nearly edge-on; the
/// cooler one throws off a flare, a loop of plasma lifting off it.
fn binary(cv: &mut Canvas, v: &View) {
    let a = smooth(v.t, 750.0, 1000.0) * v.dim;
    if a <= 0.0 {
        return;
    }
    let (bx, by) = ((v.w * 0.19).round(), (v.cy + v.h * 0.2).round());
    let phi = TAU * v.t / 1100.0;
    // the lighter one's orbit, faintly
    for i in 0..18 {
        let p = TAU * i as f64 / 18.0;
        let (x, y) = (bx + p.cos() * 3.8, by + p.sin() * 0.9);
        cv.put(
            x.round() as i32,
            y.round() as i32,
            "·",
            (150, 150, 170),
            0.16 * a,
        );
    }
    let blue = (bx - phi.cos() * 1.8, by - phi.sin() * 0.42);
    let gold = (bx + phi.cos() * 3.8, by + phi.sin() * 0.9);
    if v.fx.has("flares") {
        let f = window(v.t, 1200.0, 1320.0, 1500.0, 1650.0);
        if f > 0.0 {
            // the loop's height grows as it lifts off
            let lift = 0.8 + 2.2 * smooth(v.t, 1200.0, 1600.0);
            let mut dots = Dots::default();
            for s in 0..40 {
                let q = PI * s as f64 / 39.0;
                dots.add(gold.0 + q.cos() * 2.4, gold.1 - q.sin() * lift, f);
            }
            dots.draw(cv, |_, _, k| mix((255, 200, 120), (255, 90, 60), 1.0 - k));
        }
    }
    // the one behind first: sin > 0 is the near side
    let mut pair = [
        (blue, phi.sin() < 0.0, "✦", (175, 195, 255)),
        (gold, phi.sin() > 0.0, "•", (255, 190, 120)),
    ];
    pair.sort_by_key(|p| p.1);
    for ((x, y), _, g, rgb) in pair {
        cv.put(x.round() as i32, y.round() as i32, g, rgb, 0.95 * a);
    }
}

/// A pulsar in the upper right: two beams sweeping round, a flash each time
/// one points our way.
fn pulsar(cv: &mut Canvas, v: &View) {
    // with the logo to come, it stays lit until it comes apart (`form`)
    let a = smooth(v.t, 850.0, 1100.0) * if v.logo_on { 1.0 } else { v.dim };
    if a <= 0.0 || (v.logo_on && v.t >= v.gather.0) {
        return;
    }
    let (px, py) = pulsar_at(v);
    // a glitch before it goes: it spins up, its pulses quickening
    let spin = if v.logo_on {
        (v.t - (v.gather.0 - 220.0)).max(0.0).powi(2) * 0.00045
    } else {
        0.0
    };
    let th = TAU * v.t / PULSE_MS + 0.7 + spin;
    let len = (v.h * 0.16).clamp(3.0, 8.0);
    for dir in [th, th + PI] {
        let (dx, dy) = (dir.cos(), dir.sin());
        let glyph = stroke(dx, dy);
        let mut s = 1.0;
        while s <= len {
            let k = a * 0.85 * (1.0 - s / len).powf(1.5);
            let (x, y) = (px + dx * s * 2.0, py + dy * s);
            cv.put(
                x.round() as i32,
                y.round() as i32,
                glyph,
                mix((200, 220, 255), CYAN, s / len),
                k,
            );
            s += 0.5;
        }
    }
    let (g, k) = if flashing(v.t) || spin > 0.6 {
        ("✸", 1.0)
    } else {
        ("✦", 0.7)
    };
    cv.put_with(px as i32, py as i32, g, (215, 230, 255), k * a, true);
}

/// Where the pulsar sits (cells): the upper right.
fn pulsar_at(v: &View) -> (f64, f64) {
    (
        (v.cx + v.w * 0.31).round(),
        (v.cy - v.h * 0.24).round().max(3.0),
    )
}

/// The pulsar's period (and its readout's: PSR B1919+21 turns every 1.337 s).
const PULSE_MS: f64 = 480.0;

/// Is a beam pointing our way at `t`?
fn flashing(t: f64) -> bool {
    (TAU * t / PULSE_MS + 0.7).rem_euclid(PI) < 0.42
}

/// The hole: its halo (the disc's far side, lensed over the top and under
/// the bottom), its shadow, the photon ring, and the disc across the front.
fn hole(cv: &mut Canvas, v: &View) {
    if v.grow <= 0.0 {
        return;
    }
    let rs = v.full * v.grow;
    let ro = rs * 3.4;
    let fade = v.dim;
    let (bw, bh) = ((ro * 2.0 + 2.0) as i32, (rs * 2.1 + 1.0) as i32);
    let (cx, cy) = (v.cx as i32, v.cy as i32);
    let boil = v.t * 0.014;
    for y in cy - bh..=cy + bh {
        for x in cx - bw..=cx + bw {
            let (u, w) = v.rel(x as f64, y as f64);
            if u.hypot(w) < rs * 0.9 {
                cv.clear(x, y);
                continue;
            }
            // the photon ring: a thin, sharp circle hugging the shadow
            let ring = braille_at(x, y, false, |fx, fy| {
                let (u, w) = v.rel(fx, fy);
                (-((u.hypot(w) - rs * 1.08) / 0.18).powi(2)).exp()
            });
            // the halo: the disc's far side, lensed over the top and under
            // the bottom, glowing in a dithered gradient that boils a little
            let halo = braille_at(x, y, true, |fx, fy| {
                let (u, w) = v.rel(fx, fy);
                let (r, a) = (u.hypot(w), w.atan2(u));
                (-((r - rs * 1.42) / (rs * 0.26)).powi(2)).exp()
                    * (0.25 + 0.75 * a.sin().abs().powf(0.7))
                    * (0.88 + 0.12 * (boil + a * 5.0).sin())
                    * (1.0 - 0.3 * (u / ro).clamp(-1.0, 1.0))
            });
            // brighter on the side coming toward us (the left)
            let dopp = 1.0 - 0.38 * (u / ro).clamp(-1.0, 1.0);
            let mut b = [0u8; 4];
            match (ring, halo) {
                (Some((rc, _)), _) => {
                    let flick = 0.92 + 0.08 * (v.t * 0.02 + w.atan2(u) * 3.0).sin();
                    let k = (0.9 * dopp * flick).min(1.0) * fade;
                    // the near side blue-white, the far side warmer
                    let col = mix(DISC[0], (215, 228, 255), (-u / ro).clamp(0.0, 1.0) * 0.6);
                    cv.put_with(x, y, rc.encode_utf8(&mut b), col, k, true);
                }
                (None, Some((hc, top))) => {
                    let rgb = ramp(&DISC[1..], (u.hypot(w) - rs) / rs);
                    cv.put(x, y, hc.encode_utf8(&mut b), rgb, (0.3 + 0.6 * top) * fade);
                }
                (None, None) => {}
            }
        }
    }
    // the disc, edge on, passing in front of the shadow
    let disc = fade * (1.0 - v.fall * 1.5).max(0.0);
    for x in cx - (ro * 2.0) as i32..=cx + (ro * 2.0) as i32 {
        let u = (x as f64 - v.cx) / 2.0;
        let au = u.abs();
        if au > ro || (x - cx).abs() <= 1 {
            continue;
        }
        // matter goes round faster further in (Kepler), seen as moving knots
        let spin = 1.4 * u + v.t * 0.015 / (au / rs).max(1.0).powf(1.5);
        let tex = 0.72 + 0.28 * spin.cos();
        let dopp = 1.0 - 0.38 * (u / ro);
        let edge = 1.0 - (au / ro).powi(2);
        let k = (0.95 * tex * dopp * edge).min(1.0) * disc;
        let rgb = ramp(&DISC, ((au - rs) / (ro - rs)).max(0.0));
        cv.put(x, cy, if k > 0.55 { "━" } else { "─" }, rgb, k);
        // thicker outside the shadow, thinner and dimmer
        if au > rs * 1.25 && au < ro * 0.8 {
            for y in [cy - 1, cy + 1] {
                cv.put(x, y, "╌", rgb, k * 0.42);
            }
        }
    }
    aleph_point(cv, v);
}

/// The ℵ at the hole's centre: the point that holds every other.
fn aleph_point(cv: &mut Canvas, v: &View) {
    let a = smooth(v.t, 1350.0, 1550.0);
    if a <= 0.0 || (v.end == End::Form && v.out > 0.0) || v.logo >= 1.0 {
        return;
    }
    let beat = 0.82 + 0.18 * (v.t * 0.016).sin();
    cv.put_with(v.cx as i32, v.cy as i32, "ℵ", accent(), a * beat, true);
}

fn accent() -> (u8, u8, u8) {
    rgb_of(c::ACCENT(), (226, 180, 81))
}

/// Matter spiralling in: each clump on its orbit, quicker as it nears
/// (Kepler), hotter as it falls, trailing light; the far side's hidden by
/// the shadow.
fn spirals(cv: &mut Canvas, v: &View, seed: u64) {
    if !v.fx.has("spirals") || v.grow <= 0.0 {
        return;
    }
    let a = smooth(v.t, 950.0, 1150.0) * v.dim;
    if a <= 0.0 {
        return;
    }
    let rs = v.full * v.grow;
    let r0 = rs * 4.2;
    let life = 650.0;
    for i in 0..40u64 {
        let off = unit(seed, 100 + i) * life;
        let phi0 = unit(seed, 200 + i) * TAU;
        let at = |t: f64| {
            let u = ((t - 950.0 + off).rem_euclid(life)) / life;
            let r = rs * 1.05 + (r0 - rs * 1.05) * (1.0 - u).powf(1.3);
            // the angle swept grows fast as the radius shrinks
            let phi = phi0 + 1.6 * TAU * u.powf(1.6) + v.t * 0.0015;
            (u, r, phi)
        };
        for (j, dt) in [0.0, 18.0, 36.0].iter().enumerate() {
            let (u, r, phi) = at(v.t - dt);
            let (x, y) = (v.cx + r * phi.cos() * 2.0, v.cy + r * phi.sin() * 0.24);
            // behind the shadow: hidden
            let (ru, rv) = v.rel(x, y);
            if phi.sin() < 0.0 && ru.hypot(rv) < rs * 1.05 {
                continue;
            }
            let col = ramp(&DISC, 1.0 - u);
            let k = a * (0.45 + 0.55 * u) * [1.0, 0.45, 0.2][j];
            let g = if j > 0 {
                "·"
            } else if u > 0.7 {
                "•"
            } else {
                "∙"
            };
            cv.put(x.round() as i32, y.round() as i32, g, col, k);
        }
    }
}

/// Jets from the poles, knots of plasma running out along them, sparks
/// flung up them from the disc's inner edge.
fn jets(cv: &mut Canvas, v: &View, seed: u64) {
    let on = smooth(v.t, 1000.0, 1250.0) * v.dim;
    if on <= 0.0 {
        return;
    }
    let j = on * (0.8 + 0.2 * (v.t * 0.034).sin() * (v.t * 0.017 + 1.0).sin());
    let base = v.full * 1.95;
    let x = v.cx as i32;
    let up = (v.cy - base - 2.0).max(0.0);
    let down = (v.h - 4.0 - (v.cy + base)).max(0.0);
    for (len, sign) in [(up, -1.0), (down, 1.0)] {
        let mut d = 0.0;
        while d <= len {
            let fall = (-d / (len * 0.8 + 1.0)).exp();
            let knot = (TAU * (d / 5.0 - v.t / 240.0)).cos().max(0.0).powi(6);
            let k = j * (0.45 * fall + 0.55 * knot * fall.sqrt()).min(1.0);
            let g = match k {
                k if k > 0.55 => "┃",
                k if k > 0.3 => "│",
                _ => "╎",
            };
            // a hot blue core in a violet sheath of synchrotron light
            let rgb = mix((180, 205, 255), VIOLET, d / len.max(1.0));
            let y = (v.cy + sign * (base + d)).round() as i32;
            cv.put(x, y, g, rgb, k);
            if knot > 0.7 && k > 0.4 {
                cv.put(x - 1, y, "·", VIOLET, k * 0.55);
                cv.put(x + 1, y, "·", VIOLET, k * 0.55);
            }
            d += 1.0;
        }
        // sparks: flung out along the jet, fast, fading as they go
        if v.fx.has("spirals") {
            for i in 0..6u64 {
                let life = 380.0;
                let p = ((v.t - 1000.0 + unit(seed, 300 + i) * life).rem_euclid(life)) / life;
                let d = p * len;
                let wob = ((p * 9.0 + i as f64).sin() * 1.5).round() as i32;
                let y = (v.cy + sign * (base + d)).round() as i32;
                cv.put(x + wob, y, "∙", (230, 240, 255), on * (1.0 - p) * 0.9);
            }
        }
    }
}

/// Dropping out of warp: a shockwave runs outward, and debris with it,
/// cooling from white to red as it goes.
fn shock(cv: &mut Canvas, v: &View, seed: u64) {
    if !v.fx.has("shock") {
        return;
    }
    let p = (v.t - DROP) / 480.0;
    if !(0.0..1.0).contains(&p) {
        return;
    }
    let reach = (v.w / 4.0).hypot(v.h / 2.0);
    let e = 1.0 - (1.0 - p).powi(3);
    let mut dots = Dots::default();
    dots.ring(v.cx, v.cy, e * reach, 0.6, (1.0 - p) * 0.95);
    dots.ring(v.cx, v.cy, e * reach * 0.96, 0.6, (1.0 - p) * 0.5);
    dots.draw(cv, |_, _, k| mix((255, 120, 70), (255, 245, 225), k));
    for i in 0..48u64 {
        let a = unit(seed, 400 + i) * TAU;
        let sp = 0.35 + 0.65 * unit(seed, 500 + i);
        let r = e * reach * sp;
        let (x, y) = (v.cx + a.cos() * r * 2.0, v.cy + a.sin() * r * 0.6);
        let col = ramp(&[(255, 250, 235), (255, 190, 90), (220, 80, 50)], p);
        let g = if sp > 0.8 { "•" } else { "∙" };
        cv.put(
            x.round() as i32,
            y.round() as i32,
            g,
            col,
            (1.0 - p) * (0.5 + 0.5 * sp),
        );
    }
}

/// A few bright stars close by, with the spikes a telescope's struts make.
fn spikes(cv: &mut Canvas, v: &View, seed: u64) {
    let a = smooth(v.t, 1000.0, 1250.0) * v.dim;
    if a <= 0.0 {
        return;
    }
    for (i, (fx, fy)) in [(0.14, 0.62), (0.88, 0.5), (0.6, 0.14)].iter().enumerate() {
        let (x, y) = ((v.w * fx).round() as i32, (v.h * fy).round() as i32);
        let tw = 0.8 + 0.2 * (v.t * 0.006 + unit(seed, i as u64) * TAU).sin();
        let rgb = (225, 235, 255);
        cv.put_with(x, y, "✦", rgb, a * tw, true);
        for (dx, k) in [(1, 0.6), (2, 0.32), (3, 0.14)] {
            cv.put(x - dx, y, "─", rgb, a * tw * k);
            cv.put(x + dx, y, "─", rgb, a * tw * k);
        }
        cv.put(x, y - 1, "│", rgb, a * tw * 0.45);
        cv.put(x, y + 1, "│", rgb, a * tw * 0.45);
    }
}

/// The radar's sweep: its angle at `t`, and how lit a point is by the wake
/// behind it (0–1).
fn sweep(v: &View) -> Option<f64> {
    if !v.fx.has("radar") {
        return None;
    }
    let a = window(v.t, LOCK.1, LOCK.1 + 60.0, FINALE - 40.0, FINALE + 60.0) * v.dim;
    (a > 0.0).then(|| TAU * (v.t - LOCK.1) / 620.0)
}

fn sweep_glow(v: &View, x: f64, y: f64) -> f64 {
    let Some(th) = sweep(v) else {
        return 0.0;
    };
    let (u, w) = v.rel(x, y);
    let behind = (th - w.atan2(u)).rem_euclid(TAU);
    if behind > 1.1 {
        0.0
    } else {
        1.0 - behind / 1.1
    }
}

/// The sweep itself: a line from the hole out to the reticle, the wake
/// glowing faintly behind it.
fn radar(cv: &mut Canvas, v: &View) {
    let Some(th) = sweep(v) else {
        return;
    };
    let reach = v.full * 4.2;
    let green = (110, 240, 170);
    let mut dots = Dots::default();
    let mut s = v.full * 1.3;
    while s <= reach {
        for (lag, k) in [(0.0, 0.8), (0.09, 0.25)] {
            let a = th - lag;
            dots.add(v.cx + a.cos() * s * 2.0, v.cy + a.sin() * s * 0.62, k);
        }
        s += 0.22;
    }
    dots.draw(cv, |_, _, _| green);
}

/// Your fleet as a constellation round the hole: each machine a star (a
/// flash as it comes online), linked to the next, its name beside it, its
/// sessions orbiting it in their states' colours.
fn constellation(cv: &mut Canvas, v: &View, info: &Info) {
    if !v.fx.has("constellation") || info.fleet.is_empty() {
        return;
    }
    let a = smooth(v.t, UPLINK, UPLINK + 120.0) * v.dim;
    if a <= 0.0 {
        return;
    }
    // places round the hole, off the disc's line and the jets'
    const DEG: [f64; 12] = [
        30.0, 150.0, 210.0, 330.0, 62.0, 118.0, 242.0, 298.0, 12.0, 168.0, 192.0, 348.0,
    ];
    let rx = (v.full * 3.4 * 2.0 + 8.0).min(v.w / 2.0 - 14.0);
    let ry = (v.full * 2.4 + 2.0).min(v.h / 2.0 - 4.0);
    let at: Vec<(f64, f64)> = (0..info.fleet.len().min(DEG.len()))
        .map(|i| {
            let d = DEG[i].to_radians();
            (
                (v.cx + d.cos() * rx).clamp(2.0, v.w - 3.0),
                (v.cy + d.sin() * ry).clamp(6.0, v.h - 5.0),
            )
        })
        .collect();
    // the links, drawn out one after another
    let mut order: Vec<usize> = (0..at.len()).collect();
    order.sort_by(|&i, &j| {
        let ang = |k: usize| (at[k].1 - v.cy).atan2((at[k].0 - v.cx) / 2.0);
        ang(i).total_cmp(&ang(j))
    });
    let mut dots = Dots::default();
    for (n, w) in order
        .windows(2)
        .chain(std::iter::once(&[order[order.len() - 1], order[0]][..]))
        .enumerate()
    {
        if at.len() < 2 || (w[0] == w[1]) {
            continue;
        }
        let p = smooth(
            v.t,
            UPLINK + 80.0 + n as f64 * 40.0,
            UPLINK + 220.0 + n as f64 * 40.0,
        );
        let (x0, y0) = at[w[0]];
        let (x1, y1) = at[w[1]];
        let steps = ((x1 - x0).abs() + (y1 - y0).abs() * 2.0).ceil().max(1.0) as usize * 2;
        for s in 0..(steps as f64 * p) as usize {
            if s % 3 != 0 {
                continue;
            }
            let f = s as f64 / steps as f64;
            dots.add(x0 + (x1 - x0) * f, y0 + (y1 - y0) * f, 0.3 * a);
        }
    }
    dots.draw(cv, |_, _, _| (150, 170, 210));
    for (i, m) in info.fleet.iter().enumerate().take(at.len()) {
        let born = UPLINK + i as f64 * 55.0;
        if v.t < born {
            continue;
        }
        let (x, y) = at[i];
        let (xi, yi) = (x.round() as i32, y.round() as i32);
        let flash = v.t - born < 90.0;
        let (g, k) = match (m.up, flash) {
            (_, true) => ("✸", 1.0),
            (true, _) => ("✦", 0.95),
            (false, _) => ("○", 0.5),
        };
        cv.put_with(xi, yi, g, m.rgb, k * a, true);
        // its sessions, satellites on little orbits
        let n = m.sessions.len().min(8);
        for (j, col) in m.sessions.iter().take(n).enumerate() {
            let ang = TAU * j as f64 / n.max(1) as f64 + v.t * 0.004 * (1.0 + i as f64 * 0.1);
            let (sx, sy) = (x + ang.cos() * 2.4, y + ang.sin() * 0.9);
            cv.put(sx.round() as i32, sy.round() as i32, "•", *col, 0.9 * a);
        }
        // its name, beside it on the outside
        let name: String = m.name.chars().take(16).collect();
        let nw = name.chars().count() as i32;
        let nx = if x >= v.cx { xi + 4 } else { xi - 3 - nw };
        let shown: String = name
            .chars()
            .take(((v.t - born) / 12.0).max(0.0) as usize)
            .collect();
        cv.text(nx, yi, &shown, m.rgb, 0.85 * a);
    }
}

/// `s` as typed so far: from `start`, about 100 characters a second.
fn typed(s: &str, t: f64, start: f64) -> String {
    let n = ((t - start) / 10.0).max(0.0) as usize;
    s.chars().take(n).collect()
}

fn bar(f: f64, n: usize) -> String {
    let full = (f.clamp(0.0, 1.0) * n as f64).round() as usize;
    "▰".repeat(full) + &"▱".repeat(n - full)
}

/// The holo glitch: while the instruments power up, and in two short bursts
/// after, letters flip to alien glyphs and rows slip a column, fringed.
fn glitching(v: &View) -> f64 {
    if !v.fx.has("glitch") {
        return 0.0;
    }
    let t = v.t;
    let burst = |a: f64, b: f64| if (a..b).contains(&t) { 1.0 } else { 0.0 };
    (1.0 - smooth(t, 120.0, 380.0))
        .max(burst(1180.0, 1215.0))
        .max(burst(1560.0, 1585.0))
}

/// A line of the instruments, through the glitch when it's on.
fn hud_text(cv: &mut Canvas, v: &View, x: i32, y: i32, s: &str, rgb: (u8, u8, u8), k: f64) {
    let g = glitching(v);
    if g <= 0.0 {
        cv.text(x, y, s, rgb, k);
        return;
    }
    let frame = (v.t / 40.0) as u64;
    let slip = if hash(y as u64, frame).is_multiple_of(3) {
        1
    } else {
        0
    };
    for (i, ch) in s.chars().enumerate() {
        let hh = hash((y as u64) << 16 | i as u64, frame);
        let x = x + i as i32 + slip;
        if ch != ' ' && unit(hh, 1) < 0.35 * g {
            let a = ALIEN[(hh >> 8) as usize % ALIEN.len()];
            cv.put_with(
                x,
                y,
                a,
                if hh.is_multiple_of(2) { CYAN } else { ROSE },
                k,
                false,
            );
        } else {
            let mut b = [0u8; 4];
            cv.put_with(x, y, ch.encode_utf8(&mut b), rgb, k, false);
        }
    }
    // the colour fringe at the line's ends
    let n = s.chars().count() as i32;
    if slip == 1 && n > 0 {
        cv.put_with(x, y, "▏", CYAN, k * 0.5 * g, false);
        cv.put_with(x + n + 1, y, "▏", ROSE, k * 0.5 * g, false);
    }
}

/// The bridge: the canopy's frame, its alien script, the read-outs either
/// side and the reticle locking on.
fn hud(cv: &mut Canvas, v: &View, info: &Info) {
    let t = v.t;
    let (w, h) = (v.w as i32, v.h as i32);
    let gold = accent();
    let text = (205, 208, 220);
    // scanlines drifting down the instruments
    if v.fx.has("scanlines") {
        cv.scan = Some((t / 22.0).rem_euclid(v.h + 8.0) - 4.0);
    }
    let hud_a = v.dim.max(if v.end == End::Fall { 0.0 } else { 0.25 });
    // the frame's corners, drawn out from each
    let e = smooth(t, 0.0, 260.0);
    let la = ((v.w * 0.16).min(22.0) * e) as i32;
    let lv = ((v.h * 0.22).min(7.0) * e) as i32;
    if e > 0.0 {
        let k = 0.6 * (1.0 - glitching(v) * 0.4 * unit((t / 40.0) as u64, 3));
        for (x0, y0, sx, sy, corner) in [
            (0, 0, 1, 1, "┏"),
            (w - 1, 0, -1, 1, "┓"),
            (0, h - 1, 1, -1, "┗"),
            (w - 1, h - 1, -1, -1, "┛"),
        ] {
            cv.put(x0, y0, corner, gold, k);
            for i in 1..=la {
                let g = if i == la {
                    if sx > 0 {
                        "╸"
                    } else {
                        "╺"
                    }
                } else {
                    "━"
                };
                cv.put(x0 + sx * i, y0, g, gold, k);
            }
            for i in 1..=lv {
                let g = if i == lv {
                    if sy > 0 {
                        "╹"
                    } else {
                        "╻"
                    }
                } else {
                    "┃"
                };
                cv.put(x0, y0 + sy * i, g, gold, k);
            }
        }
    }
    // the canopy's script, decoding glyph by glyph
    let band = 9;
    let x0 = v.cx as i32 - band;
    for i in 0..=band * 2 {
        let settle = 160.0 + i as f64 * 18.0;
        if t < settle - 150.0 {
            continue;
        }
        let g = if i == band {
            "ℵ"
        } else if t < settle {
            ALIEN[(hash(i as u64, (t / 40.0) as u64) % ALIEN.len() as u64) as usize]
        } else {
            ALIEN[(hash(i as u64, 77) % ALIEN.len() as u64) as usize]
        };
        let k = if i == band { 0.9 } else { 0.5 };
        cv.put(x0 + i, 0, g, if i == band { gold } else { TEAL }, k);
    }
    // data running up the canopy's struts
    let sa = smooth(t, 100.0, 350.0) * hud_a;
    if sa > 0.0 {
        for x in [1, w - 2] {
            for y in lv + 2..h - lv - 3 {
                let hh = hash(x as u64 * 131 + y as u64, (t / 60.0) as u64 + y as u64);
                if hh.is_multiple_of(3) {
                    continue;
                }
                let g = ALIEN[(hh >> 8) as usize % ALIEN.len()];
                cv.put(x, y, g, TEAL, sa * (0.15 + 0.3 * unit(hh, 1)));
            }
        }
    }
    // read-outs: the ship's on the left, the sky's on the right
    let room = (v.cx as i32 - 6).max(0) as usize;
    let drive = match t {
        t if t < 300.0 => "spooling",
        t if t < 850.0 => "warp",
        t if t < 1150.0 => "braking",
        t if t >= FINALE && v.end == End::Warp => "warp",
        _ => "holding",
    };
    let fleet = match info.machines {
        0 => "this machine".to_string(),
        1 => "1 machine".to_string(),
        n => format!("{n} machines"),
    };
    let left = [
        ("⌬ ℵ-drive ", drive.to_string()),
        ("⊹ velocity", bar(speed(t, v.end), 8)),
        ("⋔ gravity ", bar(v.grow, 8)),
        ("⟡ uplink  ", fleet),
    ];
    for (i, (label, val)) in left.iter().enumerate() {
        let start = 260.0 + i as f64 * 90.0;
        let line = typed(&format!("{label}  {val}"), t, start);
        let line: String = line.chars().take(room).collect();
        let n = label.chars().count().min(line.chars().count());
        let (l, r) = line.split_at(line.char_indices().nth(n).map_or(line.len(), |x| x.0));
        hud_text(cv, v, 3, 2 + i as i32, l, TEAL, 0.6 * hud_a);
        hud_text(cv, v, 3 + n as i32, 2 + i as i32, r, text, 0.85 * hud_a);
    }
    // the pulse profile, newest on the right
    let trace: String = (0..14)
        .map(|j| {
            let at = (t - (13 - j) as f64 * PULSE_MS / 10.0).max(0.0);
            let k = (TAU * at / PULSE_MS + 0.7).rem_euclid(PI);
            let p = (-((k - 0.2) / 0.28).powi(2)).exp();
            ["▁", "▂", "▃", "▅", "▇", "█"][(p * 5.99) as usize]
        })
        .collect();
    // the gravitational wave's chirp, as a detector draws it
    let chirp: String = (0..12)
        .map(|j| {
            let r = (11 - j) as f64 * 2.0;
            let s = v.strain(r) / 0.22;
            ["▁", "▂", "▃", "▄", "▅", "▆", "▇"]
                [((s + 1.0) / 2.0 * 6.0).round().clamp(0.0, 6.0) as usize]
        })
        .collect();
    let mut right = vec![
        ("psr ", format!("{trace} 1.337 s"), 850.0),
        ("jets ", "0.99 c".to_string(), 1000.0),
        ("quasar z ", "0.158".to_string(), 1080.0),
    ];
    if v.fx.has("waves") {
        right.push(("gw ", format!("{chirp} chirp"), 950.0));
    } else {
        right.push(("binary ", "1.5 s".to_string(), 1160.0));
    }
    for (i, (label, val, start)) in right.iter().enumerate() {
        let full = format!("{label}{val}");
        let n = full.chars().count() as i32;
        if n as usize > room {
            continue;
        }
        let line = typed(&full, t, *start);
        let x = w - 3 - n;
        let ln = label.chars().count();
        let (l, r): (String, String) = (
            line.chars().take(ln).collect(),
            line.chars().skip(ln).collect(),
        );
        hud_text(cv, v, x, 2 + i as i32, &l, TEAL, 0.6 * hud_a);
        hud_text(cv, v, x + ln as i32, 2 + i as i32, &r, text, 0.85 * hud_a);
    }
    // the reticle, closing on the hole and locking
    let ra = smooth(t, LOCK.0, LOCK.0 + 100.0) * v.dim;
    if ra > 0.0 {
        let e = 1.0 - (1.0 - smooth(t, LOCK.0, LOCK.1)).powi(3);
        let tw = v.full * 3.4 * 2.0 + 3.0;
        let th = v.full * 1.5 + 1.0;
        let bw = (v.w * 0.45 + (tw - v.w * 0.45) * e).round() as i32;
        let bh = (v.h * 0.4 + (th - v.h * 0.4) * e).round() as i32;
        let blink = (LOCK.1..LOCK.1 + 220.0).contains(&t) && ((t - LOCK.1) / 60.0) as i64 % 2 == 0;
        let k = ra * if blink { 1.0 } else { 0.55 };
        let (cx, cy) = (v.cx as i32, v.cy as i32);
        for (sx, sy, g) in [(-1, -1, "┌"), (1, -1, "┐"), (-1, 1, "└"), (1, 1, "┘")] {
            let (x, y) = (cx + sx * bw, cy + sy * bh);
            cv.put(x, y, g, gold, k);
            cv.put(x - sx, y, "─", gold, k * 0.8);
            cv.put(x, y - sy, "│", gold, k * 0.8);
        }
        if t >= LOCK.1 {
            let tag = typed("⊹ locked", t, LOCK.1);
            hud_text(cv, v, cx + bw - 8, cy - bh - 1, &tag, gold, ra * 0.8);
        }
    }
    cv.scan = None;
}

/// Falling in: the shadow takes the view, the sky closing into a ring of
/// light at its edge.
fn swallow(cv: &mut Canvas, v: &View) {
    if v.fall <= 0.0 {
        return;
    }
    let (w, h) = (v.w as i32, v.h as i32);
    for y in 0..h - 3 {
        for x in 0..w {
            let (u, wv) = v.rel(x as f64, y as f64);
            if u.hypot(wv) < v.rs * 0.95 {
                cv.clear(x, y);
                continue;
            }
            // the whole sky, squeezed into a band of light at the edge
            let edge = braille_at(x, y, true, |fx, fy| {
                let (u, w) = v.rel(fx, fy);
                (-((u.hypot(w) - v.rs * 1.04) / 0.3).powi(2)).exp()
            });
            if let Some((ch, top)) = edge {
                let a = wv.atan2(u);
                // white where the light piles up, gold further round
                let rgb = mix(DISC[0], DISC[2], (a.sin() + 1.0) / 2.0 * (1.0 - top) + 0.2);
                let mut b = [0u8; 4];
                cv.put_with(x, y, ch.encode_utf8(&mut b), rgb, 0.55 + 0.45 * top, true);
            }
        }
    }
    aleph_point(cv, v);
}

/// Where the Aleph logo sits, as big as fits: its box (cells) and its
/// cells with their braille dots (`logo::mark_dots`).
/// The logo's box (left, top, columns, rows) and its cells (x, y, dots).
type LogoCells = (i32, i32, i32, i32, Vec<(i32, i32, u8)>);

fn logo_cells(v: &View) -> LogoCells {
    logo_box(v.w, v.h, v.cx, v.cy)
}

/// The logo's box and cells for a `w`×`h` screen, centred on (`cx`, `cy`).
fn logo_box(w: f64, h: f64, cx: f64, cy: f64) -> LogoCells {
    let rows = ((h * 0.5) as usize).clamp(6, 18);
    let cols = (rows * 2).min((w as usize).saturating_sub(10)).max(12);
    let rows = rows.min(cols / 2);
    let (x0, y0) = (
        (cx - cols as f64 / 2.0).round() as i32,
        (cy - rows as f64 / 2.0).round() as i32,
    );
    let cells = crate::logo::mark_dots(cols, rows)
        .into_iter()
        .map(|(c, r, b)| (x0 + c as i32, y0 + r as i32, b))
        .collect();
    (x0, y0, cols as i32, rows as i32, cells)
}

/// The app icon's gold, lighter top left to deeper bottom right.
const GOLD: [(u8, u8, u8); 3] = [(243, 205, 115), (226, 180, 81), (185, 138, 46)];

/// One of the logo's cells, arrived: solid where the mark is whole, its
/// braille dots at the edges, in the app icon's gold (`f` = how far along
/// its diagonal, 0 top left) with a glint (`shine`) and a flare.
fn logo_cell(cv: &mut Canvas, x: i32, y: i32, bits: u8, f: f64, shine: f64, flare: f64) {
    let col = mix(
        ramp(&GOLD, f),
        (255, 246, 214),
        (shine * 0.8 + flare).min(1.0),
    );
    let ch = if bits == 0xFF {
        '█'
    } else {
        char::from_u32(0x2800 + bits as u32).unwrap_or(' ')
    };
    let mut b = [0u8; 4];
    cv.put_with(x, y, ch.encode_utf8(&mut b), col, 1.0, true);
}

/// One fragment of the pulsar on its way to the logo, at `t`: where it is
/// and its colour. It shatters out from the pulsar (slowing as it goes),
/// is sheared into the vortex round the logo (turning as it contracts, to
/// just outside its own place), then settles into its cell by `land`.
#[allow(clippy::too_many_arguments)]
fn fragment(
    seed: u64,
    k: u64,
    from: (f64, f64),
    centre: (f64, f64),
    to: (f64, f64),
    gather: (f64, f64),
    land: f64,
    t: f64,
) -> ((f64, f64), (u8, u8, u8)) {
    let (start, dur) = gather;
    let u = ((t - start) / dur).clamp(0.0, 1.0);
    let lerp =
        |a: (f64, f64), b: (f64, f64), q: f64| (a.0 + (b.0 - a.0) * q, a.1 + (b.1 - a.1) * q);
    // destruction: thrown out, slowing as it goes
    let a = unit(seed, 900 + k) * TAU;
    let d = 3.0 + 10.0 * unit(seed, 1900 + k).powf(0.7);
    let e = (from.0 + a.cos() * d * 2.0, from.1 + a.sin() * d * 0.8);
    if u < SHATTER {
        let q = 1.0 - (1.0 - u / SHATTER).powi(3);
        return (lerp(from, e, q), mix((250, 252, 255), CYAN, q));
    }
    // deformation: sheared into a vortex round the logo, contracting as it turns
    let (eu, ev) = ((e.0 - centre.0) / 2.0, e.1 - centre.1);
    let (re, ae) = (eu.hypot(ev), ev.atan2(eu));
    let (tu, tv) = ((to.0 - centre.0) / 2.0, to.1 - centre.1);
    let (rt, at) = (tu.hypot(tv), tv.atan2(tu));
    // round to its own angle, at least half a turn
    let mut turn = (at - ae).rem_euclid(TAU);
    if turn < PI {
        turn += TAU;
    }
    let vortex = |q: f64| {
        let r = re + (rt + 2.0 - re) * q;
        let a = ae + turn * q;
        (centre.0 + a.cos() * r * 2.0, centre.1 + a.sin() * r)
    };
    let shear_end = start + dur * SHEAR;
    if t < shear_end {
        let q = smooth(t, start + dur * SHATTER, shear_end);
        return (vortex(q), mix(CYAN, VIOLET, q));
    }
    // reformation: in from the vortex to its place
    let q = smooth(t, shear_end, land);
    (lerp(vortex(1.0), to, q), mix(VIOLET, (243, 205, 115), q))
}

/// The end on the Aleph logo, made from the pulsar. It glitches and spins
/// up (`pulsar`), then comes apart in three moves:
/// * destruction — it shatters: a flash, a shockwave ring, its fragments
///   flying out and slowing;
/// * deformation — the debris is caught by the logo's pull and sheared into
///   a vortex round it, contracting as it turns, cooling to violet;
/// * reformation — the stream settles into the mark cell by cell, top left
///   to bottom right, turning gold as it lands.
///
/// It flares as the last arrives, a glint crosses it while it's held, and
/// (forming) it bursts into gold sparks as the dashboard comes.
fn form(cv: &mut Canvas, v: &View, seed: u64) {
    if !v.logo_on {
        return;
    }
    let (start, dur) = v.gather;
    let t = v.t;
    if t < start {
        return;
    }
    let done = start + dur;
    let flare = window(t, done, done + 50.0, done + 70.0, done + 140.0);
    let glint = ((t - done) / v.hold.max(300.0)).clamp(0.0, 1.0) * 1.6 - 0.3;
    let (x0, y0, cols, rows, cells) = logo_cells(v);
    let centre = (x0 as f64 + cols as f64 / 2.0, y0 as f64 + rows as f64 / 2.0);
    let gold = accent();
    let p0 = pulsar_at(v);
    // the shattering: a flash, and a shockwave running out from where it was
    let u = ((t - start) / dur).clamp(0.0, 1.0);
    if u < SHATTER + 0.12 && v.burst <= 0.0 {
        let q = u / (SHATTER + 0.12);
        let mut dots = Dots::default();
        dots.ring(p0.0, p0.1, q.sqrt() * 10.0, 0.75, (1.0 - q) * 0.95);
        dots.ring(p0.0, p0.1, q.sqrt() * 9.3, 0.75, (1.0 - q) * 0.45);
        dots.draw(cv, |_, _, k| mix(CYAN, (250, 252, 255), k));
        if u < 0.12 {
            let k = 1.0 - u / 0.12;
            cv.put_with(p0.0 as i32, p0.1 as i32, "✸", (255, 255, 255), k, true);
            for (dx, dy) in [(-2, 0), (2, 0), (0, -1), (0, 1)] {
                let g = if dy == 0 { "─" } else { "│" };
                cv.put(
                    p0.0 as i32 + dx,
                    p0.1 as i32 + dy,
                    g,
                    (230, 240, 255),
                    k * 0.7,
                );
            }
        }
    }
    for (k, (tx, ty, bits)) in cells.iter().enumerate() {
        let k = k as u64;
        let (txf, tyf) = (*tx as f64, *ty as f64);
        if v.burst > 0.0 {
            // bursting: gold sparks flying off
            let (du, dv) = v.rel(txf, tyf);
            let n = du.hypot(dv).max(0.5);
            let (x, y) = (txf + du / n * v.burst * 28.0, tyf + dv / n * v.burst * 14.0);
            let g = if k.is_multiple_of(7) { "✦" } else { "∙" };
            let col = mix((255, 246, 214), gold, v.burst);
            cv.put(
                x.round() as i32,
                y.round() as i32,
                g,
                col,
                1.0 - v.burst * 0.85,
            );
            continue;
        }
        let f = ((tx - x0) as f64 / cols as f64 + (ty - y0) as f64 / rows as f64) / 2.0;
        // each lands in turn: the mark builds top left to bottom right
        let order = 0.75 * f + 0.25 * unit(seed, 2900 + k);
        let land = start + dur * (SHEAR + 0.08) + dur * (1.0 - SHEAR - 0.08) * order;
        if t >= land {
            let shine = (1.0 - ((f - glint).abs() / 0.12)).max(0.0);
            logo_cell(cv, *tx, *ty, *bits, f, shine, flare);
            continue;
        }
        let at = |t: f64| fragment(seed, k, p0, centre, (txf, tyf), v.gather, land, t);
        // a short trail behind it, so the stream reads as motion
        for (lag, g, kk) in [(28.0, "·", 0.3), (14.0, "∙", 0.55)] {
            if t - lag > start {
                let ((x, y), col) = at(t - lag);
                cv.put(x.round() as i32, y.round() as i32, g, col, kk);
            }
        }
        let ((x, y), col) = at(t);
        cv.put(x.round() as i32, y.round() as i32, "•", col, 0.95);
    }
}

/// Warping out: the stars streak again (the travel picks up), a flash runs
/// out from the centre and the fleet rushes in.
fn warp_out(cv: &mut Canvas, v: &View) {
    if v.end != End::Warp || v.out <= 0.0 {
        return;
    }
    let p = smooth(v.t, FINALE + 150.0, END);
    if p <= 0.0 {
        return;
    }
    let reach = (v.w / 4.0).hypot(v.h / 2.0);
    let mut dots = Dots::default();
    dots.ring(v.cx, v.cy, p.powf(1.5) * reach, 0.55, 1.0 - p * 0.5);
    dots.ring(
        v.cx,
        v.cy,
        p.powf(1.5) * reach * 0.93,
        0.55,
        (1.0 - p) * 0.6,
    );
    dots.draw(cv, |_, _, _| (235, 245, 255));
}

/// The wordmark, and what the ship is doing with a spinner by it.
fn console(cv: &mut Canvas, v: &View, info: &Info, spin: &str) {
    let t = v.t;
    let (w, h) = (v.w as i32, v.h as i32);
    let status = match t {
        t if t < 300.0 => "igniting the ℵ-drive".to_string(),
        t if t < 850.0 => "at warp".to_string(),
        t if t < UPLINK => "dropping out at a singularity".to_string(),
        t if t < FINALE => {
            let mut s = match info.machines {
                0 => "uplink · this machine".to_string(),
                1 => "uplink · 1 machine".to_string(),
                n => format!("uplink · {n} machines"),
            };
            if info.sessions > 0 {
                s += &format!(
                    " · {} session{}",
                    info.sessions,
                    if info.sessions == 1 { "" } else { "s" }
                );
            }
            if info.needs > 0 {
                s += &format!(
                    " · {} need{} you",
                    info.needs,
                    if info.needs == 1 { "s" } else { "" }
                );
            }
            s
        }
        _ => match v.end {
            End::Form => "the point that holds every other".to_string(),
            End::Fall => "entering the aleph".to_string(),
            End::Warp => "jumping to the fleet".to_string(),
        },
    };
    let line = format!("{spin} {status}");
    let n = line.chars().count() as i32;
    let muted = rgb_of(c::MUTED(), (150, 150, 160));
    cv.text((w - n) / 2, h - 2, &line, muted, 0.9);
    // the wordmark, shimmering in
    let a = smooth(t, UPLINK, UPLINK + 200.0);
    if a > 0.0 {
        let word = format!("{} aleph", crate::brand::MARK);
        let base = Style::new().fg(c::ACCENT()).add_modifier(Modifier::BOLD);
        let mut spans = ui::shimmer(&word, base, t as u128 * 2 + 300);
        if let Some((chip, _)) = crate::showcase::splash_extras(t as u128) {
            spans.push(Span::raw("  "));
            spans.push(chip);
        }
        let wn: i32 = spans.iter().map(|s| ui::width(&s.content) as i32).sum();
        let mut x = (w - wn) / 2;
        let y = h - 3;
        for s in spans {
            let st = if a < 1.0 {
                // fading in: toward the colour from nothing
                match s.style.fg {
                    Some(Color::Rgb(r, g, b)) => {
                        let (r2, g2, b2) = mix((16, 16, 22), (r, g, b), a);
                        s.style.fg(Color::Rgb(r2, g2, b2))
                    }
                    _ => s.style,
                }
            } else {
                s.style
            };
            for ch in s.content.chars() {
                let pos = (cv.area.x + x.max(0) as u16, cv.area.y + y as u16);
                if x >= 0 && x < w {
                    if let Some(cell) = cv.buf.cell_mut(pos) {
                        // whatever background it had stays (a popup\'s surface, the ground)
                        let bg = cell.bg;
                        cell.reset();
                        cell.bg = bg;
                        cell.set_char(ch).set_style(st);
                    }
                }
                x += 1;
            }
        }
    }
}

// ------------------------------------------------------------------ classic

/// The classic splash's picture: the aleph as large as fits (twice as many
/// columns as rows), or none on a tiny screen.
fn classic_art(area: Rect) -> Vec<Line<'static>> {
    let rows = (area.height.saturating_sub(8) as usize).min(12);
    let cols = (rows * 2).min(area.width.saturating_sub(4) as usize);
    if rows < 4 || cols < 8 {
        return Vec::new();
    }
    crate::logo::mark(cols, cols / 2, c::ACCENT())
}

/// The classic splash: the aleph under a band of light drifting down it,
/// the wordmark below (and in showcase mode its chip and a tagline).
fn classic(buf: &mut Buffer, area: Rect, art: &[Line<'static>], ms: u64) {
    use ratatui::widgets::{Paragraph, Widget};
    let ms = ms as u128;
    let rows = art.len().max(1) as f64;
    let pos = (ms as f64 / CLASSIC_MS as f64) * (rows + 4.0) - 2.0;
    let mut lines: Vec<Line> = art
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let mut l = l.clone();
            if let Some(s) = l.spans.first_mut() {
                s.style = ui::shimmer_one(s.style, (1.0 - (i as f64 - pos).abs() / 2.5).max(0.0));
            }
            l
        })
        .collect();
    if !lines.is_empty() {
        lines.push(Line::raw(""));
    }
    let mut word = ui::shimmer(
        &format!("{} aleph", crate::brand::MARK),
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ms + 300,
    );
    let extras = crate::showcase::splash_extras(ms);
    if let Some((chip, _)) = &extras {
        word.push(Span::raw("  "));
        word.push(chip.clone());
    }
    lines.push(Line::from(word));
    if let Some((_, tagline)) = extras {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            tagline,
            ui::muted().add_modifier(Modifier::ITALIC),
        )));
    }
    let h = (lines.len() as u16).min(area.height);
    let r = Rect {
        y: area.y + (area.height - h) / 2,
        height: h,
        ..area
    };
    Paragraph::new(lines)
        .alignment(Alignment::Center)
        .render(r, buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> Info {
        let m = |name: &str, n: usize| Mach {
            name: name.into(),
            rgb: (120, 200, 255),
            up: true,
            sessions: vec![(110, 160, 250); n],
        };
        Info {
            machines: 4,
            sessions: 11,
            needs: 2,
            fleet: vec![
                m("normandy", 3),
                m("serenity", 4),
                m("tycho", 2),
                m("ceres", 2),
            ],
        }
    }

    fn frame_with(w: u16, h: u16, ms: u64, end: End, fx: &str) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut buf = Buffer::empty(area);
        let opts = Opts {
            end,
            fx: Fx::parse(fx),
            hold: HOLD,
            logo: true,
        };
        cockpit(&mut buf, area, ms, 42, &info(), "ℵ", opts);
        buf
    }

    fn frame(w: u16, h: u16, ms: u64) -> Buffer {
        frame_with(w, h, ms, End::Fall, "all")
    }

    fn row(b: &Buffer, y: u16) -> String {
        (0..b.area.width).map(|x| b[(x, y)].symbol()).collect()
    }

    fn screen(b: &Buffer) -> String {
        (0..b.area.height).map(|y| row(b, y) + "\n").collect()
    }

    #[test]
    fn the_setting_picks_one_and_small_screens_fall_back() {
        let cfg = |v: &str| Config::in_memory(&[&format!("ui.splash={v}")]);
        assert_eq!(Kind::of(&Config::in_memory(&[])), Kind::Cockpit);
        assert_eq!(Kind::of(&cfg("classic")), Kind::Classic);
        assert_eq!(Kind::of(&cfg("off")), Kind::Off);
        assert_eq!(Kind::Cockpit.fit(140, 36), Kind::Cockpit);
        assert_eq!(Kind::Cockpit.fit(50, 16), Kind::Classic);
        assert_eq!(Kind::Classic.fit(140, 36), Kind::Classic);
        assert_eq!(Kind::Cockpit.fit(16, 6), Kind::Off);
        assert_eq!(Kind::Off.fit(140, 36), Kind::Off);
    }

    #[test]
    fn effects_and_endings_follow_the_settings() {
        assert_eq!(Fx::parse(""), Fx::ALL);
        assert_eq!(Fx::parse("all"), Fx::ALL);
        assert_eq!(Fx::parse("off").0, 0);
        let some = Fx::parse("comet, radar");
        assert!(some.has("comet") && some.has("radar") && !some.has("glitch"));
        let most = Fx::parse("all, -glitch, -scanlines");
        assert!(most.has("comet") && !most.has("glitch") && !most.has("scanlines"));
        assert_eq!(Fx::parse("-glow"), Fx(Fx::ALL.0 & !(1 << 11)));
        let cfg = |v: &str| Config::in_memory(&[&format!("ui.splash_end={v}")]);
        assert_eq!(Opts::of(&cfg("form"), 1).end, End::Form);
        assert_eq!(Opts::of(&cfg("warp"), 1).end, End::Warp);
        // random: each of the three, by the seed
        let ends: Vec<End> = (0..3).map(|s| Opts::of(&cfg("random"), s).end).collect();
        assert!(
            ends.contains(&End::Form) && ends.contains(&End::Fall) && ends.contains(&End::Warp)
        );
    }

    #[test]
    fn every_glyph_is_one_column() {
        let mut all: Vec<&str> = ALIEN.to_vec();
        all.extend([
            "·", "∙", "•", "✦", "✸", "○", "─", "│", "╱", "╲", "━", "╌", "┃", "╎", "ℵ", "┏", "┓",
            "┗", "┛", "╸", "╺", "╹", "╻", "┌", "┐", "└", "┘", "▰", "▱", "█", "▁", "▂", "▃", "▄",
            "▅", "▆", "▇", "▏",
        ]);
        for g in all {
            assert_eq!(ui::width(g), 1, "{g}");
        }
        for d in 0..=8 {
            assert_eq!(ui::width(&braille(d, 5).to_string()), 1);
        }
    }

    #[test]
    fn frames_paint_no_background_and_stay_in_bounds() {
        for end in [End::Form, End::Fall, End::Warp] {
            for (w, h) in [(60, 18), (61, 19), (140, 36), (220, 60)] {
                for ms in (0..3200).step_by(83) {
                    let b = frame_with(w, h, ms, end, "all");
                    for y in 0..h {
                        for x in 0..w {
                            let cell = &b[(x, y)];
                            assert_eq!(cell.bg, Color::Reset, "{w}x{h} at {ms} ms: ({x},{y})");
                            assert!(ui::width(cell.symbol()) <= 1, "{:?}", cell.symbol());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_frame_is_a_function_of_its_moment() {
        assert_eq!(frame(140, 36, 1234), frame(140, 36, 1234));
        assert_ne!(frame(140, 36, 1234), frame(140, 36, 1300));
    }

    #[test]
    fn the_hole_forms_with_the_aleph_at_its_heart() {
        let b = frame(140, 36, 200);
        let v = View::at(
            b.area,
            200,
            Opts {
                end: End::Fall,
                fx: Fx::ALL,
                hold: HOLD,
                logo: true,
            },
        );
        let (cx, cy) = (v.cx as u16, v.cy as u16);
        assert_ne!(b[(cx, cy)].symbol(), "ℵ", "not yet");
        let b = frame(140, 36, 1580);
        assert_eq!(b[(cx, cy)].symbol(), "ℵ");
        // the shadow round it is dark, the disc crosses it on the centre row
        assert_eq!(b[(cx, cy - 2)].symbol(), " ");
        assert!(row(&b, cy).contains('━'), "{}", row(&b, cy));
        // the instruments and the wordmark are up
        assert!(row(&b, 33).contains("aleph"), "{}", row(&b, 33));
        assert!(row(&b, 34).contains("uplink · 4 machines · 11 sessions · 2 need you"));
        // the fleet's constellation, names and all
        let all = screen(&b);
        assert!(
            all.contains("normandy") && all.contains("serenity"),
            "{all}"
        );
    }

    #[test]
    fn the_jump_draws_the_tunnel_and_dropping_out_a_shockwave() {
        let braille = |b: &Buffer| {
            screen(b)
                .chars()
                .filter(|c| ('\u{2801}'..='\u{28FF}').contains(c))
                .count()
        };
        let on = frame_with(140, 36, 500, End::Fall, "all");
        let off = frame_with(140, 36, 500, End::Fall, "all, -tunnel");
        assert!(braille(&on) > braille(&off) + 20, "{}", screen(&on));
        let on = frame_with(140, 36, 1000, End::Fall, "all");
        let off = frame_with(140, 36, 1000, End::Fall, "all, -shock");
        assert!(braille(&on) > braille(&off) + 20);
    }

    #[test]
    fn every_ending_ends_its_own_way() {
        // falling in (without the logo): the view swallowed
        let area = Rect::new(0, 0, 140, 36);
        let mut b = Buffer::empty(area);
        let bare = Opts {
            end: End::Fall,
            fx: Fx::ALL,
            hold: HOLD,
            logo: false,
        };
        cockpit(&mut b, area, COCKPIT_MS - 70, 42, &info(), "ℵ", bare);
        let lit = (0..33)
            .flat_map(|y| (0..140).map(move |x| (x, y)))
            .filter(|&(x, y)| b[(x, y)].symbol() != " ")
            .count();
        assert!(lit < 140 * 33 / 8, "{lit} cells still lit");
        assert!(row(&b, 34).contains("entering the aleph"));
        // forming: the logo gathered at the centre
        let b = frame_with(140, 36, (FINALE + GATHER + 60.0) as u64, End::Form, "all");
        let blocks = screen(&b).chars().filter(|c| *c == '█').count();
        assert!(blocks > 30, "{}", screen(&b));
        assert!(row(&b, 34).contains("the point that holds every other"));
        // warping out: the stars streak again
        let b = frame_with(140, 36, COCKPIT_MS - 60, End::Warp, "all");
        let streaks = screen(&b).chars().filter(|c| "─│╱╲".contains(*c)).count();
        assert!(streaks > 40, "{}", screen(&b));
        assert!(row(&b, 34).contains("jumping to the fleet"));
    }

    #[test]
    fn the_logo_holds_at_the_end_of_every_ending() {
        let solid = |b: &Buffer| screen(b).chars().filter(|c| *c == '█').count();
        let dots = |b: &Buffer| screen(b).chars().filter(|c| *c == '•').count();
        for end in [End::Form, End::Fall, End::Warp] {
            let opts = Opts {
                end,
                fx: Fx::ALL,
                hold: HOLD,
                logo: true,
            };
            let (start, dur) = opts.gather();
            let frame = |ms: f64| frame_with(140, 36, ms as u64, end, "all");
            // the pulsar spins up, then shatters: fragments round where it was
            let b = frame(start - 30.0);
            assert!(screen(&b).contains('✸'), "{end:?} spun up: {}", screen(&b));
            let b = frame(start + dur * SHATTER * 0.6);
            assert!(
                dots(&b) > 30 && solid(&b) == 0,
                "{end:?} shattered: {}",
                screen(&b)
            );
            // sheared into the vortex, then reforming: the mark builds
            let b = frame(start + dur * (SHATTER + SHEAR) / 2.0);
            assert!(dots(&b) > 30, "{end:?} vortex: {}", screen(&b));
            let b = frame(start + dur * 0.9);
            assert!(
                dots(&b) > 5 && solid(&b) > 10,
                "{end:?} reforming: {}",
                screen(&b)
            );
            // whole, and held to the end: no tile round it
            let b = frame(start + dur + HOLD - 40.0);
            assert!(solid(&b) > 40, "{end:?} held: {}", screen(&b));
            assert!(!screen(&b).contains('╭'), "no tile");
            assert_eq!(
                opts.total(),
                (start + dur + HOLD + if end == End::Form { BURST } else { 0.0 }) as u64
            );
        }
        // forming ends in a burst: the logo's gone to sparks
        let b = frame_with(
            140,
            36,
            (FINALE + GATHER + HOLD + BURST - 15.0) as u64,
            End::Form,
            "all",
        );
        assert!(solid(&b) < 5, "{}", screen(&b));
        // the hold is a setting
        let cfg = Config::in_memory(&["ui.splash_hold=1500ms"]);
        assert_eq!(Opts::of(&cfg, 0).hold, 1500.0);
        let cfg = Config::in_memory(&["ui.splash_hold=0"]);
        assert_eq!(Opts::of(&cfg, 0).hold, 0.0);
    }

    #[test]
    fn without_the_logo_it_ends_on_the_fall_or_the_warp() {
        let cfg =
            |end: &str| Config::in_memory(&["ui.splash_logo=off", &format!("ui.splash_end={end}")]);
        // forming means the logo: without it, the ship falls in
        assert_eq!(Opts::of(&cfg("form"), 0).end, End::Fall);
        assert_eq!(Opts::of(&cfg("warp"), 0).end, End::Warp);
        for seed in 0..6 {
            let o = Opts::of(&cfg("random"), seed);
            assert_ne!(o.end, End::Form);
            assert!(!o.logo);
            assert_eq!(o.total(), COCKPIT_MS);
        }
        // and no logo is drawn, at any moment
        let area = Rect::new(0, 0, 140, 36);
        for ms in [END - 30.0, END - 1.0] {
            let mut buf = Buffer::empty(area);
            let o = Opts::of(&cfg("fall"), 0);
            cockpit(&mut buf, area, ms as u64, 42, &info(), "ℵ", o);
            assert!(!screen(&buf).contains('█'), "{}", screen(&buf));
        }
    }

    #[test]
    fn the_braille_mark_reads_as_the_aleph() {
        // its two strokes and both yods: a filled cell in each corner region
        let cells = crate::logo::mark_dots(36, 18);
        let at = |c0: usize, c1: usize, r0: usize, r1: usize| {
            cells
                .iter()
                .any(|(c, r, b)| *b == 0xFF && (c0..c1).contains(c) && (r0..r1).contains(r))
        };
        assert!(at(0, 12, 0, 6), "the stroke's top left");
        assert!(at(24, 36, 0, 6), "the upper yod");
        assert!(at(0, 12, 12, 18), "the lower yod's foot");
        assert!(at(24, 36, 12, 18), "the stroke's foot");
        assert!(!at(14, 22, 0, 3), "open between the top two");
    }

    #[test]
    fn classic_still_draws_its_wordmark() {
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        classic(&mut buf, area, &classic_art(area), 500);
        assert!((0..24).any(|y| row(&buf, y).contains("aleph")));
    }

    #[cfg(feature = "showcase")]
    #[test]
    fn showcase_mode_adds_its_chip_and_a_tagline() {
        crate::showcase::force("masked");
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        classic(&mut buf, area, &classic_art(area), 300);
        let all: String = (0..30).map(|y| row(&buf, y)).collect();
        assert!(all.contains("showcase"), "{all}");
        let b = frame(140, 36, 1500);
        assert!(row(&b, 33).contains("aleph") && row(&b, 33).contains("showcase"));
        crate::showcase::force("off");
    }

    /// Frames as HTML pages, to look at (`SPLASH_HTML=dir cargo test
    /// splash_pages -- --ignored`).
    #[test]
    #[ignore]
    fn splash_pages() {
        let Some(dir) = std::env::var_os("SPLASH_HTML") else {
            return;
        };
        // SPLASH_SIZE=50x14: another screen
        let (w, h) = std::env::var("SPLASH_SIZE")
            .ok()
            .and_then(|v| {
                let (a, b) = v.split_once('x')?;
                Some((a.parse().ok()?, b.parse().ok()?))
            })
            .unwrap_or((150, 40));
        // a cosmos scene: 24 moments across the whole run, logo and all
        if let Some(k) = std::env::var("SPLASH_KIND")
            .ok()
            .and_then(|n| Kind::named(&n))
            .filter(|k| !matches!(k, Kind::Cockpit))
        {
            let opts = Opts {
                end: End::Fall,
                fx: Fx::parse("all"),
                hold: HOLD,
                logo: std::env::var_os("SPLASH_NO_LOGO").is_none(),
            };
            let tot = total(k, opts, None);
            // SPLASH_MS=1200,1300: just those moments
            let moments: Vec<u64> = match std::env::var("SPLASH_MS") {
                Ok(v) => v.split(',').filter_map(|m| m.trim().parse().ok()).collect(),
                Err(_) => (0..24u64).map(|i| tot * i / 23).collect(),
            };
            // SPLASH_THEME=convergence: in that theme's colours
            if let Ok(t) = std::env::var("SPLASH_THEME") {
                crate::ui::c::init(&t, None);
            }
            // SPLASH_ANSI=1: a terminal's colour text instead of a page
            let as_ansi = std::env::var_os("SPLASH_ANSI").is_some();
            for ms in moments {
                let area = Rect::new(0, 0, w, h);
                let mut b = Buffer::empty(area);
                super::frame(k, &mut b, area, ms, 42, &info(), "ℵ", opts, None);
                let (name, body) = if as_ansi {
                    (format!("{}-f{ms:05}.ansi", k.name()), ansi(&b))
                } else {
                    (format!("{}-f{ms:04}.html", k.name()), html(&b))
                };
                std::fs::write(std::path::Path::new(&dir).join(name), body).unwrap();
            }
            return;
        }
        let shots: &[(u64, End)] = &[
            (150, End::Fall),
            (480, End::Fall),
            (960, End::Fall),
            (1100, End::Fall),
            (1300, End::Fall),
            (1520, End::Fall),
            (1880, End::Fall),
            (1700, End::Form),
            (1760, End::Form),
            (1880, End::Form),
            (2000, End::Form),
            (2150, End::Form),
            (2300, End::Form),
            (2400, End::Form),
            (2800, End::Form),
            (1900, End::Warp),
            (2120, End::Warp),
            (2400, End::Warp),
            (2120, End::Fall),
            (2400, End::Fall),
        ];
        for (ms, end) in shots {
            let b = frame_with(w, h, *ms, *end, "all");
            let name = format!("f{ms:04}-{end:?}.html").to_lowercase();
            std::fs::write(std::path::Path::new(&dir).join(name), html(&b)).unwrap();
        }
    }

    /// A frame as a page (for a browser's screenshot).
    /// A buffer as a terminal would get it: SGR colours, one line a row.
    fn ansi(b: &Buffer) -> String {
        fn sgr(c: Color, fg: bool) -> String {
            let base = if fg { 38 } else { 48 };
            match c {
                Color::Rgb(r, g, b) => format!("{base};2;{r};{g};{b}"),
                Color::Indexed(n) => format!("{base};5;{n}"),
                Color::Reset => format!("{}", if fg { 39 } else { 49 }),
                other => {
                    let named = [
                        Color::Black,
                        Color::Red,
                        Color::Green,
                        Color::Yellow,
                        Color::Blue,
                        Color::Magenta,
                        Color::Cyan,
                        Color::Gray,
                        Color::DarkGray,
                        Color::LightRed,
                        Color::LightGreen,
                        Color::LightYellow,
                        Color::LightBlue,
                        Color::LightMagenta,
                        Color::LightCyan,
                        Color::White,
                    ];
                    let i = named.iter().position(|x| *x == other).unwrap_or(7) as u8;
                    format!("{base};5;{i}")
                }
            }
        }
        let mut out = String::new();
        for y in 0..b.area.height {
            for x in 0..b.area.width {
                let c = &b[(x, y)];
                let bold = if c.modifier.contains(Modifier::BOLD) {
                    "1;"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\x1b[0;{bold}{};{}m{}",
                    sgr(c.fg, true),
                    sgr(c.bg, false),
                    c.symbol()
                ));
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }

    fn html(b: &Buffer) -> String {
        let (w, h) = (b.area.width, b.area.height);
        {
            let mut s = String::from(
                "<html><body style='background:#0b0b0d;margin:12px'><pre style='font:15px/19px Menlo,\"Apple Braille\",monospace;color:#ccc'>",
            );
            for y in 0..h {
                for x in 0..w {
                    let cell = &b[(x, y)];
                    let sym = match cell.symbol() {
                        "<" => "&lt;",
                        ">" => "&gt;",
                        "&" => "&amp;",
                        o => o,
                    };
                    // one column, as a terminal draws it (a browser draws
                    // halfwidth katakana from a CJK font, wider)
                    let pinned;
                    let sym = if sym.is_ascii() {
                        sym
                    } else {
                        pinned = format!(
                            "<span style='display:inline-block;width:1ch;overflow:visible'>{sym}</span>"
                        );
                        pinned.as_str()
                    };
                    match cell.fg {
                        Color::Rgb(r, g, bl) => {
                            s += &format!(
                                "<span style='color:#{r:02x}{g:02x}{bl:02x}{}'>{sym}</span>",
                                if cell.modifier.contains(Modifier::BOLD) {
                                    ";font-weight:bold"
                                } else {
                                    ""
                                }
                            )
                        }
                        _ => s += sym,
                    }
                }
                s.push('\n');
            }
            s += "</pre></body></html>";
            s
        }
    }

    /// Every cosmos scene that's built.
    fn scenes() -> Vec<Kind> {
        Kind::ALL
            .into_iter()
            .filter(|k| k.built() && !matches!(k, Kind::Cockpit | Kind::Classic))
            .collect()
    }

    fn scene_frame(k: Kind, w: u16, h: u16, ms: u64, logo: bool, len: Option<u64>) -> Buffer {
        let area = Rect::new(0, 0, w, h);
        let mut b = Buffer::empty(area);
        let opts = Opts {
            end: End::Fall,
            fx: Fx::parse("all"),
            hold: HOLD,
            logo,
        };
        super::frame(k, &mut b, area, ms, 42, &info(), "ℵ", opts, len);
        b
    }

    #[test]
    fn a_splash_keeps_the_background_it_is_drawn_on() {
        // the splash gallery's preview sits on a popup's surface: every kind
        // draws over it without wiping it back to the terminal's
        let info = info();
        let surface = Color::Rgb(30, 28, 44);
        for k in Kind::ALL {
            for ms in [300, 1300, 2600] {
                let area = Rect::new(0, 0, 90, 28);
                let mut buf = Buffer::empty(area);
                for cell in buf.content.iter_mut() {
                    cell.set_bg(surface);
                }
                let opts = Opts::of(&Config::in_memory(&[]), 3);
                super::frame(k, &mut buf, area, ms, 3, &info, "ℵ", opts, None);
                let wiped = buf.content.iter().filter(|c| c.bg != surface).count();
                assert_eq!(wiped, 0, "{k:?} at {ms} ms wiped {wiped} cells");
            }
        }
    }

    #[test]
    fn every_scene_paints_no_background_stays_in_bounds_and_keeps_one_column() {
        assert!(!scenes().is_empty());
        for k in scenes() {
            for (w, h) in [(50, 14), (120, 32), (200, 60)] {
                let opts = Opts {
                    end: End::Fall,
                    fx: Fx::parse("all"),
                    hold: HOLD,
                    logo: true,
                };
                let tot = total(k, opts, None);
                for ms in (0..tot).step_by(83) {
                    let b = scene_frame(k, w, h, ms, true, None);
                    for y in 0..h {
                        for x in 0..w {
                            let c = &b[(x, y)];
                            assert_eq!(c.bg, Color::Reset, "{k:?} {w}×{h} at {ms}: ({x},{y})");
                            assert!(
                                ui::width(c.symbol()) <= 1,
                                "{k:?}: {:?} is wide",
                                c.symbol()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_scene_is_a_function_of_its_moment_and_draws_something() {
        for k in scenes() {
            for ms in [200, 1200, 2400, 3300] {
                let (a, b) = (
                    scene_frame(k, 120, 32, ms, true, None),
                    scene_frame(k, 120, 32, ms, true, None),
                );
                assert_eq!(a, b, "{k:?} at {ms}");
                let lit = (0..32)
                    .flat_map(|y| (0..120).map(move |x| (x, y)))
                    .filter(|p| a[*p].symbol() != " ")
                    .count();
                assert!(lit > 10, "{k:?} at {ms}: only {lit} cells");
            }
        }
    }

    #[test]
    fn a_scene_ends_on_the_logo_with_it_and_its_own_way_without() {
        for k in scenes() {
            let with = Opts {
                end: End::Fall,
                fx: Fx::parse("all"),
                hold: HOLD,
                logo: true,
            };
            let without = Opts {
                logo: false,
                ..with
            };
            assert_eq!(total(k, without, None), scene::SCENE_MS);
            assert!(total(k, with, None) > scene::SCENE_MS + scene::GATHER_S as u64);
            // while it's held: solid logo cells in the middle
            let at = scene::SCENE_MS + scene::GATHER_S as u64 + 200;
            let b = scene_frame(k, 120, 32, at, true, None);
            let solid = (0..32)
                .flat_map(|y| (0..120).map(move |x| (x, y)))
                .filter(|p| b[*p].symbol() == "█")
                .count();
            assert!(
                solid > 20,
                "{k:?}: the ℵ isn't there ({solid})\n{}",
                screen(&b)
            );
            // without: no logo at its last moment
            let b = scene_frame(k, 120, 32, scene::SCENE_MS - 1, false, None);
            let solid = (0..32)
                .flat_map(|y| (0..120).map(move |x| (x, y)))
                .filter(|p| b[*p].symbol() == "█")
                .count();
            assert!(solid < 10, "{k:?}: a logo without the logo");
        }
    }

    #[test]
    fn random_picks_differ_even_on_a_microsecond_clock() {
        let cfg = Config::in_memory(&["ui.splash=random"]);
        let mut seen: Vec<Kind> = Vec::new();
        // a clock that only ticks in microseconds (macOS): multiples of 1000
        for us in 0..64u64 {
            let k = Kind::pick(&cfg, spread(1_792_800_000_000_000 + us * 1000));
            if !seen.contains(&k) {
                seen.push(k);
            }
        }
        assert!(seen.len() >= 5, "{seen:?}");
    }

    /// `auto` plays each 15% quicker than its story is written and in 2 s at
    /// most, ending on its last moment; a length you set is kept.
    #[test]
    fn auto_plays_every_splash_quicker_and_in_two_seconds_at_most() {
        let opts = Opts {
            end: End::Form,
            fx: Fx::parse("all"),
            hold: HOLD,
            logo: true,
        };
        let bare = Opts {
            logo: false,
            end: End::Fall,
            ..opts
        };
        for k in [Kind::Cockpit, Kind::Classic].into_iter().chain(scenes()) {
            for o in [opts, bare] {
                let t = total(k, o, None);
                let p = played(k, o, None);
                let want = (t as f64 * 0.85).min(2000.0).round() as u64;
                assert_eq!(p, want, "{k:?}");
                assert!(p <= 2000, "{k:?}: {p}");
                assert!(story(k, o, p, None).abs_diff(t) <= 1, "{k:?}");
                assert_eq!(played(k, o, Some(4000)), total(k, o, Some(4000)));
                assert_eq!(story(k, o, 1234, Some(4000)), 1234);
            }
        }
        // the cosmos: 3 s of story in 2
        let k = scenes()[0];
        assert_eq!(played(k, bare, None), 2000);
    }

    #[test]
    fn splash_ms_stretches_every_kind_and_random_picks_a_built_one() {
        let opts = Opts {
            end: End::Fall,
            fx: Fx::parse("all"),
            hold: HOLD,
            logo: false,
        };
        assert_eq!(total(Kind::Cockpit, opts, Some(4000)), 4000);
        assert_eq!(total(Kind::Classic, opts, Some(4000)), 4000);
        for k in scenes() {
            assert_eq!(total(k, opts, Some(5000)), 5000);
            // a frame late in a long run is the scene's late frame
            let b = scene_frame(k, 120, 32, 4900, false, Some(5000));
            assert!((0..32).any(|y| row(&b, y).trim() != ""));
        }
        let cfg = Config::in_memory(&["ui.splash=random"]);
        for seed in 0..40 {
            let k = Kind::pick(&cfg, seed);
            assert!(k.built() && k != Kind::Classic && k != Kind::Off, "{k:?}");
        }
        assert_eq!(Kind::named("event-horizon"), Some(Kind::Horizon));
        assert!(crate::config::check("ui.splash_ms", "3s").is_ok());
        assert!(crate::config::check("ui.splash_ms", "20s").is_err());
        assert!(crate::config::check("ui.splash_ms", "auto").is_ok());
    }
}
