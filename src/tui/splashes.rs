//! The splash gallery: every splash beside a live preview of the one
//! highlighted, looping, each run its own seed. Enter keeps it (`ui.splash`,
//! and `ui.splash_logo` when `l` changed it), `p` plays it full screen once,
//! `l` turns the ℵ ending on or off. From the palette, and enter on Settings'
//! `ui.splash`. First comes `theme`: the theme's own splash (each theme
//! has one), which follows the theme as it changes.

use super::splash::{self, End, Info, Kind, Opts};
use super::{Dash, Level, Mode};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};
use std::time::Instant;

/// The quiet between two runs of the preview (ms).
const GAP: u64 = 500;

/// One row of the gallery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    /// the theme's own (`ui.splash = theme`)
    Theme,
    One(Kind),
    Random,
}

impl Pick {
    fn name(self) -> &'static str {
        match self {
            Pick::Theme => "theme",
            Pick::One(k) => k.name(),
            Pick::Random => "random",
        }
    }

    /// Is it what `ui.splash` says?
    fn is(self, v: &str) -> bool {
        match self {
            Pick::Theme => v == "theme" || v.is_empty(),
            Pick::Random => v == "random",
            Pick::One(k) => Kind::named(v) == Some(k),
        }
    }
}

pub(crate) struct Splashes {
    /// the theme's own, every splash there is, random, off
    items: Vec<Pick>,
    /// what the theme's own plays
    theme: Kind,
    i: usize,
    /// does the preview end on the ℵ?
    logo: bool,
    /// `ui.splash` and the logo as they were
    before: (String, bool),
    /// the preview's first run began
    t0: Instant,
    seed: u64,
    /// `p`: play this one full screen (the dashboard's loop does it)
    play: bool,
}

/// What each is, in a line.
fn about(p: Pick) -> &'static str {
    match p {
        Pick::Theme => "the theme's own: each theme plays its splash (cyberpunk its cyberdeck boot, the cosmos theirs, the classics the cockpit), and it follows the theme as it changes",
        Pick::Random => "a different one each time the dashboard starts (never classic)",
        Pick::One(k) => about_kind(k),
    }
}

fn about_kind(k: Kind) -> &'static str {
    match k {
        Kind::Cockpit => "an alien ship's bridge dropping out of warp at a black hole; your fleet comes up as a constellation",
        Kind::Classic => "the ℵ under a band of light: quick and quiet",
        Kind::Singularity => "an emerging universe: a point in the foam, its fireball, the cosmic web, nebulae and spiral galaxies (your machines, named)",
        Kind::Pulsar => "a neutron star's beams sweeping like a lighthouse, a magnetar flare, the spin-up to a disc of light",
        Kind::Horizon => "a black hole close up: lensed stars, its disc and jets, a star torn apart, crossing the horizon",
        Kind::Matrix => "digital rain with your fleet in the code; déjà vu; the code freezing, then decoding",
        Kind::Convergence => "a network learning and converging into one bright core: the AI singularity",
        Kind::Cyberdeck => "a neural interface booting: its HUD drawing itself, diagnostics, your machines coming online, the sync",
        Kind::Neon => "a rain-soaked city at night: towers in the haze, your machines as neon signs, traffic, a wet street mirroring it all",
        Kind::Off => "no splash: the dashboard at once",
    }
}

pub(crate) fn open(d: &mut Dash) {
    let mut items: Vec<Pick> = vec![Pick::Theme];
    items.extend(Kind::ALL.into_iter().filter(|k| k.built()).map(Pick::One));
    items.push(Pick::Random);
    items.push(Pick::One(Kind::Off));
    let now = d.cfg.get_or("ui.splash");
    let logo = d.cfg.get_or("ui.splash_logo") != "off";
    let i = items.iter().position(|p| p.is(&now)).unwrap_or(0);
    d.mode = Mode::Splashes(Splashes {
        items,
        theme: Kind::of_theme(&d.cfg.get_or("theme")),
        i,
        logo,
        before: (now, logo),
        t0: Instant::now(),
        seed: splash::seed_now(),
        play: false,
    });
}

impl Splashes {
    fn chosen(&self) -> Pick {
        self.items.get(self.i).copied().unwrap_or(Pick::Theme)
    }

    fn off(&self) -> bool {
        self.chosen() == Pick::One(Kind::Off)
    }

    fn opts(&self, cfg: &crate::config::Config, seed: u64) -> Opts {
        let mut o = Opts::of(cfg, seed);
        o.logo = self.logo;
        if !o.logo && o.end == End::Form {
            o.end = End::Fall;
        }
        o
    }

    /// The kind its `n`th run plays (random: one of the animated ones).
    fn kind(&self, n: u64) -> Kind {
        match self.chosen() {
            Pick::One(k) => k,
            Pick::Theme => self.theme,
            Pick::Random => {
                let all: Vec<Kind> = Kind::ALL
                    .into_iter()
                    .filter(|k| *k != Kind::Classic && k.built())
                    .collect();
                all[(self.seed.wrapping_add(n) % all.len() as u64) as usize]
            }
        }
    }

    /// Where the preview is now: (the kind, its run's seed, ms into it —
    /// None in the gap between runs).
    fn now(&self, cfg: &crate::config::Config, w: u16, h: u16) -> (Kind, u64, Option<u64>) {
        let len = splash::len_of(cfg);
        let mut t = self.t0.elapsed().as_millis() as u64;
        for n in 0..10_000u64 {
            let seed = self.seed.wrapping_add(n);
            let k = self.kind(n).fit(w, h);
            let total = splash::played(k, self.opts(cfg, seed), len);
            if total == 0 {
                return (k, seed, None);
            }
            if t < total + GAP {
                let opts = self.opts(cfg, seed);
                return (k, seed, (t < total).then(|| splash::story(k, opts, t, len)));
            }
            t -= total + GAP;
        }
        (Kind::Off, self.seed, None)
    }

    fn restart(&mut self) {
        self.t0 = Instant::now();
        self.seed = splash::seed_now();
    }
}

pub(crate) fn key(d: &mut Dash, mut g: Splashes, k: KeyEvent) {
    let n = g.items.len().max(1);
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') => return,
        KeyCode::Enter => {
            let v = g.chosen().name();
            if v != g.before.0 {
                super::settings::save(d, "ui.splash", v);
            }
            let mut said = format!("splash {v}");
            if g.logo != g.before.1 {
                super::settings::save(
                    d,
                    "ui.splash_logo",
                    if g.logo { "particles" } else { "off" },
                );
                said += if g.logo {
                    " · ends on the ℵ"
                } else {
                    " · ends its own way"
                };
            }
            d.toast(Level::Ok, said);
            return;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            g.i = (g.i + 1) % n;
            g.restart();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            g.i = (g.i + n - 1) % n;
            g.restart();
        }
        KeyCode::Home | KeyCode::Char('g') => {
            g.i = 0;
            g.restart();
        }
        KeyCode::End | KeyCode::Char('G') => {
            g.i = n - 1;
            g.restart();
        }
        KeyCode::Char('l') => {
            g.logo = !g.logo;
            g.restart();
        }
        KeyCode::Char('p') if !g.off() => g.play = true,
        _ => {}
    }
    d.mode = Mode::Splashes(g);
}

/// `p`: the chosen one full screen, once (any key ends it), then the
/// gallery again. From the dashboard's loop, which holds the terminal.
pub(crate) fn play_asked(d: &mut Dash, tui: &mut crate::term::Tui) {
    let Mode::Splashes(g) = &mut d.mode else {
        return;
    };
    if !std::mem::take(&mut g.play) {
        return;
    }
    let seed = splash::seed_now();
    let kind = g.kind(seed);
    let opts = g.opts(&d.cfg, seed);
    let info = Info::from_fleet(&d.fleet);
    splash::play(tui, kind, opts, splash::len_of(&d.cfg), &info, seed, false);
    // the dashboard draws again from a blank screen
    let _ = tui.term.clear();
    if let Mode::Splashes(g) = &mut d.mode {
        g.restart();
    }
}

/// Move the preview `ms` on (tests).
#[cfg(test)]
pub(crate) fn skip(d: &mut Dash, ms: u64) {
    if let Mode::Splashes(g) = &mut d.mode {
        g.t0 -= std::time::Duration::from_millis(ms);
    }
}

/// Is a preview running (the dashboard draws faster while it is)?
pub(crate) fn animating(d: &Dash) -> bool {
    matches!(&d.mode, Mode::Splashes(g) if !g.off())
}

pub(crate) fn draw(d: &Dash, f: &mut Frame, area: Rect, g: &Splashes) {
    let w = area.width.saturating_sub(2).clamp(60, 150).min(area.width);
    let h = area.height.saturating_sub(2).clamp(16, 46).min(area.height);
    let r = ui::centered(area, w, h);
    crate::ui::clear(f, r);
    let block = ui::panel("Splash gallery", true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [body, foot] = Layout::vertical([Constraint::Min(4), Constraint::Length(1)]).areas(inner);
    let lw = 26u16.min(body.width / 3);
    let [list, prev] =
        Layout::horizontal([Constraint::Length(lw), Constraint::Min(10)]).areas(body);
    let len = splash::len_of(&d.cfg);
    let opts = g.opts(&d.cfg, g.seed);
    // the list: what's set now marked ●, its length beside it
    let mut lines: Vec<Line> = vec![Line::raw("")];
    for (i, p) in g.items.iter().enumerate() {
        let on = i == g.i;
        let set = p.is(&g.before.0);
        let secs = match p {
            Pick::One(k) if *k != Kind::Off => {
                format!("{:.1}s", splash::played(*k, opts, len) as f64 / 1000.0)
            }
            _ => String::new(),
        };
        let nm = p.name();
        // ┃, the mark, the name, its length; a gap before the preview
        let room = (lw as usize).saturating_sub(2 + 3 + secs.len() + 1);
        lines.push(Line::from(vec![
            Span::styled(if on { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(if set { "● " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(
                ui::pad(nm, room),
                if on { ui::bold() } else { Style::new() },
            ),
            Span::styled(format!(" {secs}"), ui::muted()),
        ]));
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(about(g.chosen()), ui::muted()));
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
        Span::styled("ℵ ending  ", ui::muted()),
        if g.logo {
            Span::styled("on", ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("off", ui::muted())
        },
    ]));
    let list = Rect {
        width: list.width.saturating_sub(2),
        ..list
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), list);
    // the preview, in a frame of its own
    let (kind, seed, at) = g.now(
        &d.cfg,
        prev.width.saturating_sub(2),
        prev.height.saturating_sub(2),
    );
    let title = match (g.chosen(), kind) {
        (Pick::One(Kind::Off), _) => "Preview".to_string(),
        (Pick::Random, k) => format!("Preview · random: {}", k.name()),
        (Pick::Theme, k) => format!("Preview · the theme's: {}", k.name()),
        (Pick::One(want), k) if want != k => {
            format!("Preview · {} (too small here: {})", want.name(), k.name())
        }
        (Pick::One(k), _) => format!("Preview · {}", k.name()),
    };
    let pb = ui::panel(title, false);
    let pa = pb.inner(prev);
    f.render_widget(pb, prev);
    if g.off() {
        let y = pa.y + pa.height / 2;
        f.render_widget(
            Paragraph::new(Line::styled(
                "no splash — the dashboard comes at once",
                ui::muted(),
            ))
            .alignment(Alignment::Center),
            Rect { y, height: 1, ..pa },
        );
    } else if let Some(ms) = at {
        let info = Info::of(d);
        let spin = ui::spinner(g.t0);
        let o = g.opts(&d.cfg, seed);
        splash::frame(kind, f.buffer_mut(), pa, ms, seed, &info, spin, o, len);
    }
    f.render_widget(
        Paragraph::new(ui::hints_fit(
            &[
                ("↑↓", "pick"),
                ("enter", "keep"),
                ("p", "play full screen"),
                ("l", "ℵ ending on/off"),
                ("esc", "close"),
            ],
            foot.width,
        )),
        foot,
    );
}
