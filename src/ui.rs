//! The visual language every screen shares: palette, pills, key hints,
//! spinners, text cards (for printed output) and small text helpers.

use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Padding};
use std::time::Instant;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Palette: Aleph's "Graphite + gold". A gold brand accent; neutral greys
/// (Radix-style, tuned for dark terminals and a "paper" set for light ones);
/// state colours that always mean the same thing: working blue, needs-you
/// orange, done green, idle teal, at-limit violet, down red. The terminal's own
/// background is never painted. With NO_COLOR every colour is the terminal
/// default and meaning is carried by glyphs, bold and reverse video. `init`
/// runs once at startup; `accent=claude` brings back Claude's orange.
#[allow(non_snake_case)]
pub mod c {
    use ratatui::style::Color;
    use std::sync::OnceLock;

    #[derive(Clone)]
    pub struct Pal {
        pub accent: Color,
        pub blue: Color,
        pub mauve: Color,
        pub green: Color,
        pub yellow: Color,
        pub red: Color,
        pub peach: Color,
        pub teal: Color,
        pub pink: Color,
        pub sky: Color,
        pub muted: Color,
        pub faint: Color,
        pub line: Color,
        pub sel: Color,
        pub text: Color,
        pub base: Color,
        /// a focused panel's frame and title, and other panels' titles
        pub edge: Color,
        pub edge_dim: Color,
        /// the session states: working, needs you, done, idle, at a limit, down
        pub working: Color,
        pub needs: Color,
        pub done: Color,
        pub idle: Color,
        pub limit: Color,
        pub down: Color,
        /// a light theme's cards: panels a shade under its ground (`readable`)
        pub panel: Color,
        /// popups: a light theme's lighter than its ground, a dark one's a
        /// shade above it
        pub pop: Color,
        pub light: bool,
        pub mono: bool,
    }

    const fn rgb(x: u32) -> Color {
        Color::Rgb((x >> 16) as u8, (x >> 8 & 0xff) as u8, (x & 0xff) as u8)
    }

    impl Pal {
        /// The theme's own gold, whatever `accent` says.
        pub fn accent_default(&self) -> Color {
            if self.light {
                rgb(0x8f6400)
            } else {
                rgb(0xe2b451)
            }
        }
        pub fn dark() -> Pal {
            Pal {
                accent: rgb(0xe2b451),
                blue: rgb(0x3b9eff),
                mauve: rgb(0xa875ff),
                green: rgb(0x46a758),
                yellow: rgb(0xf0d84a),
                red: rgb(0xe5484d),
                peach: rgb(0xf76b15),
                teal: rgb(0x12a594),
                pink: rgb(0xe93d82),
                sky: rgb(0x7ce2fe),
                muted: rgb(0xa0a0a0),
                faint: rgb(0x6e6e6e),
                line: rgb(0x3a3a3a),
                sel: rgb(0x262626),
                text: rgb(0xeeeeee),
                base: rgb(0x111111),
                light: false,
                mono: false,
                ..Pal::dark_states()
            }
            .derived()
        }
        pub fn light() -> Pal {
            Pal {
                accent: rgb(0x8a5a00),
                blue: rgb(0x0b62c4),
                mauve: rgb(0x7a3fb0),
                green: rgb(0x1f7a33),
                yellow: rgb(0x8a6200),
                red: rgb(0xc42127),
                peach: rgb(0xb84a00),
                teal: rgb(0x04756b),
                pink: rgb(0xb0237c),
                sky: rgb(0x006a92),
                muted: rgb(0x555555),
                faint: rgb(0x727272),
                line: rgb(0x8f8f8f),
                sel: rgb(0xe2d8c3),
                text: rgb(0x1a1a1a),
                base: rgb(0xf8f7f4),
                light: true,
                mono: false,
                ..Pal::dark_states()
            }
            .derived()
        }
        /// A theme from its colours: accent, blue, mauve, green, yellow, red,
        /// peach, teal, pink, sky, muted, faint, line, sel, text, base.
        const fn of(x: [u32; 16], light: bool) -> Pal {
            Pal {
                accent: rgb(x[0]),
                blue: rgb(x[1]),
                mauve: rgb(x[2]),
                green: rgb(x[3]),
                yellow: rgb(x[4]),
                red: rgb(x[5]),
                peach: rgb(x[6]),
                teal: rgb(x[7]),
                pink: rgb(x[8]),
                sky: rgb(x[9]),
                muted: rgb(x[10]),
                faint: rgb(x[11]),
                line: rgb(x[12]),
                sel: rgb(x[13]),
                text: rgb(x[14]),
                base: rgb(x[15]),
                // the frames in the accent, the states from their colours
                edge: rgb(x[0]),
                edge_dim: rgb(x[14]),
                working: rgb(x[1]),
                needs: rgb(x[6]),
                done: rgb(x[3]),
                idle: rgb(x[7]),
                limit: rgb(x[2]),
                down: rgb(x[5]),
                // `readable` makes them from the ground
                panel: rgb(x[15]),
                pop: rgb(x[15]),
                light,
                mono: false,
            }
        }

        /// The frames' and the states' colours from the palette's own (the
        /// way every theme starts).
        const fn derived(self) -> Pal {
            Pal {
                panel: self.base,
                pop: self.base,
                edge: self.accent,
                edge_dim: self.text,
                working: self.blue,
                needs: self.peach,
                done: self.green,
                idle: self.teal,
                limit: self.mauve,
                down: self.red,
                ..self
            }
        }

        /// Placeholders `derived` replaces (a struct needs every field).
        const fn dark_states() -> Pal {
            let z = Color::Reset;
            Pal {
                accent: z,
                blue: z,
                mauve: z,
                green: z,
                yellow: z,
                red: z,
                peach: z,
                teal: z,
                pink: z,
                sky: z,
                muted: z,
                faint: z,
                line: z,
                sel: z,
                text: z,
                base: z,
                edge: z,
                edge_dim: z,
                working: z,
                needs: z,
                done: z,
                idle: z,
                limit: z,
                down: z,
                panel: z,
                pop: z,
                light: false,
                mono: false,
            }
        }

        /// A theme's own frame and state colours: edge, edge_dim, then
        /// working, needs, done, idle, limit, down.
        const fn with(self, x: [u32; 8]) -> Pal {
            Pal {
                edge: rgb(x[0]),
                edge_dim: rgb(x[1]),
                working: rgb(x[2]),
                needs: rgb(x[3]),
                done: rgb(x[4]),
                idle: rgb(x[5]),
                limit: rgb(x[6]),
                down: rgb(x[7]),
                ..self
            }
        }

        /// A built-in theme by its name (`dark` is Graphite, `light` Paper).
        pub fn named(name: &str) -> Option<Pal> {
            #[rustfmt::skip]
            let p = match name {
                "dark" | "graphite" => Pal::dark(),
                "light" | "paper" => Pal::light(),
                "nord" => Pal::of([0xebcb8b, 0x81a1c1, 0xb48ead, 0xa3be8c, 0xebcb8b, 0xbf616a, 0xd08770, 0x8fbcbb, 0xc895bf, 0x88c0d0, 0xb4bccc, 0x6b7891, 0x3b4252, 0x353c4a, 0xeceff4, 0x2e3440], false)
                    .with([0x88c0d0, 0x8fa1b8, 0x88c0d0, 0xebcb8b, 0xa3be8c, 0x5e81ac, 0xb48ead, 0xbf616a]),
                "catppuccin" => Pal::of([0xf9e2af, 0x89b4fa, 0xcba6f7, 0xa6e3a1, 0xf9e2af, 0xf38ba8, 0xfab387, 0x94e2d5, 0xf5c2e7, 0x89dceb, 0xa6adc8, 0x6c7086, 0x313244, 0x2a2b3c, 0xcdd6f4, 0x1e1e2e], false)
                    .with([0xcba6f7, 0xa6adc8, 0x89b4fa, 0xfab387, 0xa6e3a1, 0x6c7086, 0xf5c2e7, 0xf38ba8]),
                "catppuccin-latte" => Pal::of([0x8839ef, 0x1e66f5, 0x8839ef, 0x40a02b, 0xdf8e1d, 0xd20f39, 0xfe640b, 0x179299, 0xea76cb, 0x04a5e5, 0x5c5f77, 0x8c8fa1, 0x9ca0b0, 0xe0d5f4, 0x4c4f69, 0xeff1f5], true),
                "tokyo-night" => Pal::of([0xe0af68, 0x7aa2f7, 0xbb9af7, 0x9ece6a, 0xe0af68, 0xf7768e, 0xff9e64, 0x73daca, 0xff7eb6, 0x7dcfff, 0xa9b1d6, 0x565f89, 0x292e42, 0x24283b, 0xc0caf5, 0x1a1b26], false)
                    .with([0x7aa2f7, 0xa9b1d6, 0x7dcfff, 0xff9e64, 0x9ece6a, 0x565f89, 0xbb9af7, 0xf7768e]),
                "gruvbox" => Pal::of([0xfabd2f, 0x83a598, 0xd3869b, 0xb8bb26, 0xfabd2f, 0xfb4934, 0xfe8019, 0x8ec07c, 0xd3869b, 0x83c0b8, 0xbdae93, 0x7c6f64, 0x3c3836, 0x32302f, 0xebdbb2, 0x282828], false)
                    .with([0xfabd2f, 0xbdae93, 0x83a598, 0xfe8019, 0xb8bb26, 0x7c6f64, 0xd3869b, 0xfb4934]),
                "dracula" => Pal::of([0xf1c86b, 0x82aaff, 0xbd93f9, 0x50fa7b, 0xf1fa8c, 0xff5555, 0xffb86c, 0x8be9fd, 0xff79c6, 0x8be9fd, 0xbfc3d9, 0x6272a4, 0x44475a, 0x343746, 0xf8f8f2, 0x282a36], false)
                    .with([0xbd93f9, 0xbfc3d9, 0x8be9fd, 0xffb86c, 0x50fa7b, 0x6272a4, 0xff79c6, 0xff5555]),
                "solarized-light" => Pal::of([0x8a6700, 0x1f6fb0, 0x5e63b8, 0x5f7000, 0x8a6700, 0xc4291f, 0xb0400f, 0x167f78, 0xb52b70, 0x1f6fb0, 0x4a5f66, 0x6a7e84, 0x8a9a9a, 0xe6d9b6, 0x073642, 0xfdf6e3], true)
                    .with([0x8a6700, 0x4a5f66, 0x1f6fb0, 0xb0400f, 0x5f7000, 0x167f78, 0x5e63b8, 0xc4291f]),
                // the cosmos: a point becoming everything, a pulsar's beams,
                // a black hole, the code, the network converging (dark, then
                // paper: near white in the theme's hue, its colours as inks)
                "singularity" => Pal::of([0xffd27a, 0x7aa8ff, 0xc49bff, 0x6fe3a8, 0xffe08a, 0xff6b8a, 0xffab5c, 0x6ee0d8, 0xff8fd0, 0xa8d8ff, 0xa99fc4, 0x6a5f88, 0x2a2240, 0x1e1830, 0xf2ecff, 0x0b0814], false)
                    .with([0xffd27a, 0xa99fc4, 0xa8c8ff, 0xffd27a, 0xb9f6ca, 0x6a5f88, 0xc49bff, 0xff6b8a]),
                "singularity-light" => Pal::of([0x8a5d00, 0x2a55c8, 0x6a35b8, 0x16764a, 0x806000, 0xb8204a, 0xa94f00, 0x0b716b, 0xa3247f, 0x1d63a0, 0x4f466a, 0x6f6690, 0x8e84b0, 0xdcd5ef, 0x1b1430, 0xf7f4fc], true)
                    .with([0x5b3fb0, 0x4f466a, 0x2a55c8, 0xa94f00, 0x16764a, 0x0b716b, 0x6a35b8, 0xb8204a]),
                "pulsar" => Pal::of([0x5ff0ff, 0x4aa8ff, 0xb18cff, 0x5ee6a0, 0xf4f08a, 0xff5f6e, 0xffaa55, 0x39d5c8, 0xff6fd8, 0x8fe7ff, 0x94b4cc, 0x557189, 0x15273a, 0x0f1d2c, 0xe6f6ff, 0x050a14], false)
                    .with([0x5ff0ff, 0x94b4cc, 0x5ff0ff, 0xff6fd8, 0xa8ffcf, 0x3b6a80, 0xb18cff, 0xff5f6e]),
                "pulsar-light" => Pal::of([0x006b7d, 0x0a55b0, 0x6236b0, 0x0f7048, 0x776000, 0xb81f35, 0xa8470a, 0x006b62, 0xa11a78, 0x075e8f, 0x3c5568, 0x5f7a8e, 0x7d98ac, 0xc4dde4, 0x0a1a2a, 0xf2f8fc], true)
                    .with([0x006b7d, 0x3c5568, 0x0a55b0, 0xa11a78, 0x0f7048, 0x006b62, 0x6236b0, 0xb81f35]),
                "event-horizon" => Pal::of([0xffb347, 0x6aa8ff, 0xb48cff, 0x7fd38a, 0xffd36b, 0xff5a4a, 0xff8a3d, 0x5fc9c0, 0xff7aa8, 0x9fd2ff, 0xb3a693, 0x6f6455, 0x26201a, 0x1b1612, 0xf4eee4, 0x07060a], false)
                    .with([0xffb347, 0xb3a693, 0xffcf7a, 0xff5a4a, 0x9fd2ff, 0x6f6455, 0xb48cff, 0xd0342c]),
                "event-horizon-light" => Pal::of([0x9a5200, 0x1f58b0, 0x6438a8, 0x26703a, 0x7d5800, 0xb3261a, 0xa84800, 0x0d6e66, 0xa2245f, 0x175f94, 0x5a4a3a, 0x7a6a58, 0x9a8670, 0xe8d7c2, 0x1a120a, 0xfaf6ef], true)
                    .with([0x9a5200, 0x5a4a3a, 0x1f58b0, 0xa84800, 0x26703a, 0x0d6e66, 0x6438a8, 0xb3261a]),
                "matrix" => Pal::of([0x39ff6e, 0x3fd0ff, 0xb58cff, 0x6dff8a, 0xe6ff6b, 0xff5c5c, 0xffb347, 0x2fe0b0, 0xff7ac8, 0x7fe6ff, 0x6fbf86, 0x3d7a4f, 0x0f2a17, 0x0b1f11, 0xc8ffd4, 0x020a04], false),
                "matrix-light" => Pal::of([0x0a7a2c, 0x005f9a, 0x5e38a8, 0x127230, 0x5f6c00, 0xb3201f, 0x9a5000, 0x006f60, 0x9b246f, 0x005e8a, 0x2f5a3a, 0x4f7a5a, 0x6e9a78, 0xc4e0cb, 0x052a10, 0xf2faf3], true),
                "matrix-mono" => Pal::of([0x3cff6b, 0x8cff9e, 0x4f9e62, 0x3cd65a, 0xb6ff4a, 0x2f8f45, 0xd6ff7a, 0x5be08a, 0x9cffb0, 0xc6ffd0, 0x5fae73, 0x3a6f47, 0x0d2614, 0x0a1c0f, 0xb8ffc4, 0x010802], false),
                "matrix-mono-light" => Pal::of([0x0a7a2c, 0x0d6a28, 0x3f6e4a, 0x1d7a37, 0x4f6a0e, 0x24502c, 0x4f7a00, 0x2a6e44, 0x2f7a48, 0x255f3a, 0x2c5636, 0x4c7656, 0x6c9676, 0xc3e0ca, 0x04260c, 0xf1f9f2], true),
                "convergence" => Pal::of([0xd38bff, 0x6c9cff, 0xb48bff, 0x6ee7a6, 0xffe27a, 0xff5f7a, 0xffa45c, 0x4fd8d0, 0xff7ae0, 0x9ccaff, 0xa3a8d4, 0x5f6496, 0x20234a, 0x171a38, 0xeef0ff, 0x08081a], false)
                    .with([0xd38bff, 0xa3a8d4, 0x4fd8d0, 0xffe27a, 0x6ee7a6, 0x5f6496, 0xff7ae0, 0xff5f7a]),
                "convergence-light" => Pal::of([0x6d28b8, 0x2a4fc8, 0x6a2fb0, 0x0f6e4c, 0x6f5c00, 0xb51f45, 0xa04a0c, 0x0b6e78, 0xa1197c, 0x1c5ea0, 0x4b4766, 0x6e6a8a, 0x8a86a8, 0xe0d4f0, 0x1d1a33, 0xf6f5fb], true)
                    .with([0x5b3fd6, 0x4b4766, 0x0b6e78, 0x7a5a00, 0x0f6e4c, 0x6e6a8a, 0xa1197c, 0xb51f45]),
                // a neural interface's HUD: electric yellow for what's yours
                // (the accent, the selection, an objective), its off-red for
                // the chrome, cyan for live data, on near-black; light: the
                // key art — black structure on the game's yellow (#F3E600),
                // its UI's deep teal (#136377), crimson (#C5003C), blood red
                // (#880425), olive and brown (#978D01, #58482C)
                "cyberpunk" => Pal::of([0xfcee0a, 0x2da8ff, 0xc653ff, 0x00ff9f, 0xfcee0a, 0xff003c, 0xff7a3d, 0x1ae6d2, 0xff2e88, 0x5ef6ff, 0x9a968a, 0x55524a, 0x5a1b24, 0x2b2600, 0xe8e3d3, 0x0a0a0d], false)
                    .with([0xff4a57, 0xb8434c, 0x5ef6ff, 0xfcee0a, 0x00ff9f, 0x3d7a80, 0xc653ff, 0xff003c]),
                // the dark one's structure in daylight: the key art's warm
                // white paper, red chrome and title tags, its yellow
                // (#F3E600) as the selection, teal data, near-black ink
                "cyberpunk-light" => Pal::of([0xb0002f, 0x00399a, 0x5b1e9e, 0x0a5c31, 0x6b5f00, 0xb0002f, 0x9a3c00, 0x005c66, 0x9e0063, 0x0d5470, 0x4a4632, 0x6e6850, 0x8f8870, 0xf3e600, 0x14120a, 0xfaf8ec], true)
                    .with([0xb0002f, 0x4a4535, 0x00708a, 0x9e0063, 0x0a5c31, 0x4a4535, 0x5b1e9e, 0x7a0420]),
                _ => return None,
            };
            Some(p)
        }

        /// Every colour made to read where it's drawn, whatever the palette
        /// says (the theme's hue and saturation kept, only the lightness
        /// moved, just enough). Dark, on its ground: text 11:1, muted 6:1,
        /// faint 4:1, every hue and state 4.5:1 (WCAG AA for text), frame
        /// lines 1.5:1. Light — paper near white (L* 95–98) with cards a
        /// shade under it (`panel`), popups lighter (`pop`) — on the cards,
        /// its darkest surface: text 12:1, muted 6:1, faint 4.5:1, every hue,
        /// state and frame colour 4.5:1, frame lines 3:1 (WCAG's for a
        /// control's edge); the selection a clear step off the cards, text
        /// on it 7:1.
        pub fn readable(self) -> Pal {
            let (Color::Rgb(br, bg, bb), false) = (self.base, self.mono) else {
                return self;
            };
            let base = (br, bg, bb);
            let rgb3 = |c: Color| match c {
                Color::Rgb(r, g, b) => Some((r, g, b)),
                _ => None,
            };
            let mix = |a: (u8, u8, u8), b: (u8, u8, u8), k: f64| {
                let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * k).round() as u8;
                Color::Rgb(m(a.0, b.0), m(a.1, b.1), m(a.2, b.2))
            };
            let text0 = rgb3(self.text).unwrap_or(base);
            // the surfaces: a light theme's cards a shade under the paper
            // (toward its own ink, so they keep its tint), popups lighter;
            // a dark theme's popups a shade above its ground
            let (panel, pop) = if self.light {
                (mix(base, text0, 0.045), mix(base, (255, 255, 255), 0.75))
            } else {
                (self.base, mix(base, text0, 0.07))
            };
            let on = rgb3(panel).unwrap_or(base);
            let fit = |c: Color, ratio: f64| match c {
                Color::Rgb(r, g, b) => {
                    let (r, g, b) = crate::logo::fit_to((r, g, b), on, ratio);
                    Color::Rgb(r, g, b)
                }
                c => c,
            };
            let (text, muted, faint, ink, line) = if self.light {
                (12.0, 6.0, 4.5, 4.5, 3.0)
            } else {
                (11.0, 6.0, 4.0, 4.5, 1.5)
            };
            let mut p = Pal {
                text: fit(self.text, text),
                muted: fit(self.muted, muted),
                faint: fit(self.faint, faint),
                line: fit(self.line, line),
                accent: fit(self.accent, ink),
                blue: fit(self.blue, ink),
                mauve: fit(self.mauve, ink),
                green: fit(self.green, ink),
                yellow: fit(self.yellow, ink),
                red: fit(self.red, ink),
                peach: fit(self.peach, ink),
                teal: fit(self.teal, ink),
                pink: fit(self.pink, ink),
                sky: fit(self.sky, ink),
                edge: fit(self.edge, ink),
                edge_dim: fit(self.edge_dim, ink),
                working: fit(self.working, ink),
                needs: fit(self.needs, ink),
                done: fit(self.done, ink),
                idle: fit(self.idle, ink),
                limit: fit(self.limit, ink),
                down: fit(self.down, ink),
                panel,
                pop,
                ..self
            };
            if self.light {
                // the selection as the theme gives it (a tint of its accent),
                // the text on it held to 7:1
                if let (Some(sel), Some(t)) = (rgb3(p.sel), rgb3(p.text)) {
                    let (r, g, b) = crate::logo::fit_to(t, sel, 7.0);
                    p.text = Color::Rgb(r, g, b);
                }
                return p;
            }
            // the selection: a step off the ground, the text on it still clear
            p.sel = fit(self.sel, 1.15);
            if let (Color::Rgb(sr, sg, sb), Color::Rgb(tr, tg, tb)) = (p.sel, p.text) {
                if crate::logo::contrast((sr, sg, sb), (tr, tg, tb)) < 7.0 {
                    p.sel = self.sel;
                }
            }
            p
        }

        /// A theme made from one of these and your own colours (a theme's
        /// `[themes.<name>]` table: `base` = a theme to start from, then
        /// any of accent, blue, … text as #rrggbb).
        pub fn custom(fields: &[(String, String)]) -> Pal {
            let get = |k: &str| fields.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
            let mut p = get("base").and_then(Pal::named).unwrap_or_else(Pal::dark);
            if let Some(l) = get("light") {
                p.light = matches!(l, "true" | "on" | "yes" | "1");
            }
            let set = |slot: &mut Color, k: &str| {
                if let Some(c) = get(k).and_then(super::parse_color) {
                    *slot = c;
                }
            };
            set(&mut p.accent, "accent");
            set(&mut p.blue, "blue");
            set(&mut p.mauve, "mauve");
            set(&mut p.green, "green");
            set(&mut p.yellow, "yellow");
            set(&mut p.red, "red");
            set(&mut p.peach, "peach");
            set(&mut p.teal, "teal");
            set(&mut p.pink, "pink");
            set(&mut p.sky, "sky");
            set(&mut p.muted, "muted");
            set(&mut p.faint, "faint");
            set(&mut p.line, "line");
            set(&mut p.sel, "sel");
            set(&mut p.text, "text");
            set(&mut p.base, "base_color");
            set(&mut p.edge, "edge");
            set(&mut p.edge_dim, "edge_dim");
            set(&mut p.working, "working");
            set(&mut p.needs, "needs");
            set(&mut p.done, "done");
            set(&mut p.idle, "idle");
            set(&mut p.limit, "limit");
            set(&mut p.down, "down");
            p
        }

        pub fn mono() -> Pal {
            let r = Color::Reset;
            Pal {
                accent: r,
                blue: r,
                mauve: r,
                green: r,
                yellow: r,
                red: r,
                peach: r,
                teal: r,
                pink: r,
                sky: r,
                muted: r,
                faint: r,
                line: r,
                sel: r,
                text: r,
                base: r,
                edge: r,
                edge_dim: r,
                working: r,
                needs: r,
                done: r,
                idle: r,
                limit: r,
                down: r,
                panel: r,
                pop: r,
                light: false,
                mono: true,
            }
        }
    }

    /// The palette in use. Set at start; `aleph setup` sets it again while
    /// you choose a theme or an accent, so the whole screen follows. Each
    /// palette is leaked once (a few hundred bytes per switch), which keeps
    /// `pal()` a plain `&'static` for everyone.
    static PAL: std::sync::RwLock<Option<&'static Pal>> = std::sync::RwLock::new(None);
    static DARK: OnceLock<Pal> = OnceLock::new();
    /// what `init` was last given (`again`)
    #[allow(clippy::type_complexity)]
    static LAST: std::sync::RwLock<Option<(String, Option<Color>)>> = std::sync::RwLock::new(None);

    /// Counts `init`s: whoever keeps colours made from the palette (the
    /// fleet's machine colours) makes them again when it moves.
    static GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    pub fn generation() -> u64 {
        GEN.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The built-in themes: (name, title, a light one?), in the gallery's order.
    pub const THEMES: &[(&str, &str, bool)] = &[
        (
            "auto",
            "Auto (Graphite or Paper, as the terminal is)",
            false,
        ),
        ("dark", "Graphite", false),
        ("light", "Paper", true),
        ("nord", "Nord", false),
        ("catppuccin", "Catppuccin Mocha", false),
        ("catppuccin-latte", "Catppuccin Latte", true),
        ("tokyo-night", "Tokyo Night", false),
        ("gruvbox", "Gruvbox", false),
        ("dracula", "Dracula", false),
        ("solarized-light", "Solarized Light", true),
        ("singularity", "Singularity", false),
        ("singularity-light", "Singularity (light)", true),
        ("pulsar", "Pulsar", false),
        ("pulsar-light", "Pulsar (light)", true),
        ("event-horizon", "Event Horizon", false),
        ("event-horizon-light", "Event Horizon (light)", true),
        ("matrix", "Matrix", false),
        ("matrix-light", "Matrix (light)", true),
        ("matrix-mono", "Matrix mono (all green)", false),
        ("matrix-mono-light", "Matrix mono (light)", true),
        ("convergence", "Convergence", false),
        ("convergence-light", "Convergence (light)", true),
        ("cyberpunk", "Cyberpunk", false),
        (
            "cyberpunk-light",
            "Cyberpunk (light: the key art's yellow)",
            true,
        ),
    ];

    /// Your own themes (`[themes.<name>]`), set from the settings.
    static CUSTOM: std::sync::RwLock<Vec<(String, Pal)>> = std::sync::RwLock::new(Vec::new());

    pub fn set_custom(v: Vec<(String, Pal)>) {
        if let Ok(mut g) = CUSTOM.write() {
            *g = v;
        }
    }

    pub fn custom_names() -> Vec<String> {
        CUSTOM
            .read()
            .map(|g| g.iter().map(|(n, _)| n.clone()).collect())
            .unwrap_or_default()
    }

    /// A theme's palette, without using it (the gallery's swatches): a
    /// built-in, one of yours, or for `auto` the one the terminal gets.
    pub fn pal_of(theme: &str) -> Pal {
        // a light theme, or one on its own ground: every ink readable on it
        let sure = |p: Pal| {
            if p.light || crate::look::of(theme).ground {
                p.readable()
            } else {
                p
            }
        };
        if let Some(p) = Pal::named(theme) {
            return sure(p);
        }
        if let Some(p) = CUSTOM
            .read()
            .ok()
            .and_then(|g| g.iter().find(|(n, _)| n == theme).map(|(_, p)| p.clone()))
        {
            return sure(p);
        }
        if light_terminal() {
            Pal::light()
        } else {
            Pal::dark()
        }
    }

    /// `theme` = auto | a built-in (dark, light, nord, …) | one of yours;
    /// `accent` = optional override.
    pub fn init(theme: &str, accent: Option<Color>) {
        if let Ok(mut g) = LAST.write() {
            *g = Some((theme.to_string(), accent));
        }
        GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        crate::look::set(theme);
        let mut p = if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            Pal::mono()
        } else {
            pal_of(theme)
        };
        // showcase mode: the cockpit's deep-space tones, when the dashboard
        // asked for them (nothing without the feature)
        crate::showcase::tint(&mut p);
        if let (Some(a), false) = (accent, p.mono) {
            p.accent = a;
        }
        if let Ok(mut g) = PAL.write() {
            *g = Some(Box::leak(Box::new(p)));
        }
    }

    /// `init` again with what it was last given (showcase mode changed the
    /// palette it wants); nothing before the first `init`.
    #[cfg_attr(not(feature = "showcase"), allow(dead_code))]
    pub fn again() {
        let last = LAST.read().ok().and_then(|g| g.clone());
        if let Some((theme, accent)) = last {
            init(&theme, accent);
        }
    }

    /// Does the terminal look light (so `auto` picks the paper palette)?
    pub fn terminal_is_light() -> bool {
        light_terminal()
    }

    /// The terminal's own answer when it gives one (OSC 11,
    /// [`crate::term::background`]), else COLORFGBG ("fg;bg", set by many
    /// terminals; bg 7 or 15 = light).
    fn light_terminal() -> bool {
        match crate::term::background() {
            Some(bg) => light_background(bg),
            None => colorfgbg_light(std::env::var("COLORFGBG").ok().as_deref()),
        }
    }

    fn colorfgbg_light(v: Option<&str>) -> bool {
        v.and_then(|v| v.rsplit(';').next().and_then(|b| b.parse::<u8>().ok()))
            .is_some_and(|b| b == 7 || b == 15)
    }

    /// Is a background light, i.e. does Paper's text read better on it than
    /// Graphite's?
    pub fn light_background(bg: (u8, u8, u8)) -> bool {
        let rgb = |c: Color| match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        };
        let on = |text: Color| crate::logo::contrast(bg, rgb(text));
        on(Pal::light().text) > on(Pal::dark().text)
    }

    pub fn pal() -> &'static Pal {
        match PAL.read().ok().and_then(|g| *g) {
            Some(p) => p,
            None => DARK.get_or_init(Pal::dark),
        }
    }
    pub fn mono() -> bool {
        pal().mono
    }
    pub fn ACCENT() -> Color {
        pal().accent
    }
    pub fn BLUE() -> Color {
        pal().blue
    }
    pub fn MAUVE() -> Color {
        pal().mauve
    }
    pub fn GREEN() -> Color {
        pal().green
    }
    pub fn YELLOW() -> Color {
        pal().yellow
    }
    pub fn RED() -> Color {
        pal().red
    }
    pub fn PEACH() -> Color {
        pal().peach
    }
    pub fn TEAL() -> Color {
        pal().teal
    }
    pub fn PINK() -> Color {
        pal().pink
    }
    pub fn SKY() -> Color {
        pal().sky
    }
    pub fn MUTED() -> Color {
        pal().muted
    }
    pub fn FAINT() -> Color {
        pal().faint
    }
    pub fn LINE() -> Color {
        pal().line
    }
    pub fn SEL() -> Color {
        pal().sel
    }
    pub fn TEXT() -> Color {
        pal().text
    }
    pub fn BASE() -> Color {
        pal().base
    }
    /// A session that's working.
    pub fn WORKING() -> Color {
        pal().working
    }
    /// A session waiting for you.
    pub fn NEEDS() -> Color {
        pal().needs
    }
    /// A session that finished.
    pub fn DONE() -> Color {
        pal().done
    }
    /// A session sitting idle.
    pub fn IDLE() -> Color {
        pal().idle
    }
    /// Stopped at a usage limit.
    pub fn LIMIT() -> Color {
        pal().limit
    }
    /// A machine or session that's down.
    pub fn DOWN() -> Color {
        pal().down
    }
    /// A focused panel's frame and title.
    pub fn EDGE() -> Color {
        pal().edge
    }
    /// Other panels' titles.
    pub fn EDGE_DIM() -> Color {
        pal().edge_dim
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn light_or_dark_backgrounds() {
            // Solarized, Dracula, Gruvbox, black and white
            assert!(light_background((0xfd, 0xf6, 0xe3)));
            assert!(!light_background((0x00, 0x2b, 0x36)));
            assert!(!light_background((0x28, 0x2a, 0x36)));
            assert!(light_background((0xfb, 0xf1, 0xc7)));
            assert!(!light_background((0, 0, 0)));
            assert!(light_background((255, 255, 255)));
            // middle grey reads better with dark text
            assert!(light_background((0x80, 0x80, 0x80)));
            assert!(!light_background((0x50, 0x50, 0x50)));
            assert!(colorfgbg_light(Some("0;15")));
            assert!(colorfgbg_light(Some("0;default;7")));
            assert!(!colorfgbg_light(Some("15;0")));
            assert!(!colorfgbg_light(Some("nonsense")));
            assert!(!colorfgbg_light(None));
        }
    }
}

/// Parse `#rgb`, `#rrggbb` or a palette name (machine palette / hues) into a colour.
pub fn parse_color(s: &str) -> Option<Color> {
    let s = s.trim();
    if let Some(h) = s.strip_prefix('#') {
        let v = u32::from_str_radix(h, 16).ok()?;
        return match h.len() {
            6 => Some(Color::Rgb(
                (v >> 16) as u8,
                (v >> 8 & 0xff) as u8,
                (v & 0xff) as u8,
            )),
            3 => {
                let x = |n: u32| ((n & 0xf) * 17) as u8;
                Some(Color::Rgb(x(v >> 8), x(v >> 4), x(v)))
            }
            _ => None,
        };
    }
    match s.to_lowercase().as_str() {
        "claude" => return Some(Color::Rgb(0xd9, 0x77, 0x57)),
        "gold" | "aleph" => return Some(c::pal().accent_default()),
        _ => {}
    }
    crate::theme::named(s)
}

pub fn hex(col: Color) -> String {
    match col {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => String::new(),
    }
}

pub fn muted() -> Style {
    Style::new().fg(c::MUTED())
}
pub fn faint() -> Style {
    Style::new().fg(c::FAINT())
}
pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
pub fn fg(col: Color) -> Style {
    Style::new().fg(col)
}
pub fn selected() -> Style {
    if c::mono() {
        return Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD);
    }
    Style::new()
        .bg(c::SEL())
        .fg(c::TEXT())
        .add_modifier(Modifier::BOLD)
}

/// A keycap (`enter`, `q`) as used in hint bars.
pub fn keycap(k: &str) -> Span<'static> {
    let st = if c::mono() {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new()
            .fg(c::TEXT())
            .bg(c::LINE())
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" {k} "), st)
}

/// Footer key hints: `[k] label` keycaps.
pub fn hints(items: &[(&str, &str)]) -> Line<'static> {
    let mut v = vec![Span::raw(" ")];
    for (k, label) in items {
        v.push(keycap(k));
        v.push(Span::styled(format!(" {label}   "), muted()));
    }
    Line::from(v)
}

/// Rows `buttons` needs for `items` in `w` columns.
pub fn button_rows(items: &[(&str, &str)], w: u16) -> u16 {
    let (mut rows, mut x) = (1u16, 1u16);
    for (k, label) in items {
        let bw = (width(k) + width(label) + 4) as u16;
        if x + bw > w && x > 1 {
            rows += 1;
            x = 1;
        }
        x += bw + 1;
    }
    rows
}

/// Clickable buttons (`[n] New session  [x] Close…`) in `r`, wrapping onto
/// its next rows; the rect of each one drawn, with its index in `items`.
pub fn buttons(f: &mut ratatui::Frame, r: Rect, items: &[(&str, &str)]) -> Vec<(Rect, usize)> {
    let mut lines: Vec<Line> = vec![Line::raw(" ")];
    let mut out = Vec::new();
    let (mut x, mut y) = (r.x + 1, r.y);
    for (i, (k, label)) in items.iter().enumerate() {
        let w = (width(k) + width(label) + 4) as u16;
        if x + w > r.x + r.width && x > r.x + 1 {
            if y + 1 >= r.y + r.height {
                break;
            }
            y += 1;
            x = r.x + 1;
            lines.push(Line::raw(" "));
        }
        if x + w > r.x + r.width {
            break;
        }
        if let Some(l) = lines.last_mut() {
            l.spans.push(keycap(k));
            l.spans.push(Span::styled(
                format!(" {label}"),
                Style::new().fg(c::TEXT()),
            ));
            l.spans.push(Span::raw("  "));
        }
        out.push((Rect::new(x, y, w - 1, 1), i));
        x += w + 1;
    }
    f.render_widget(ratatui::widgets::Paragraph::new(lines), r);
    out
}

/// `hints`, dropping the least important (middle) items until it fits `w`
/// columns; the first and last (usually "quit") always stay.
pub fn hints_fit(items: &[(&str, &str)], w: u16) -> Line<'static> {
    let mut v: Vec<(&str, &str)> = items.to_vec();
    while v.len() > 2 && line_width(&hints(&v)) > w as usize {
        v.remove(v.len() - 2);
    }
    hints(&v)
}

static SCALE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The `ui_scale` setting: 0 compact · 1 comfortable · 2 large. A terminal
/// app can't change the font, so "bigger" is room: padding inside panels,
/// margins around views, and (large) a blank line between list rows.
pub fn scale() -> u8 {
    SCALE.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_scale(s: &str) {
    let v = match s.trim() {
        "compact" | "0" | "small" => 0,
        "large" | "2" | "big" => 2,
        _ => 1,
    };
    SCALE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Inner padding of panels at the current scale.
pub fn padding() -> Padding {
    match scale() {
        0 => Padding::horizontal(1),
        1 => Padding::new(2, 2, 1, 0),
        _ => Padding::new(3, 3, 1, 1),
    }
}

/// A view's area minus the margins the current scale wants around it.
pub fn margin(r: Rect) -> Rect {
    let (mx, my) = match scale() {
        0 => (0, 0),
        1 => (1, 0),
        _ => (2, 1),
    };
    if r.width <= mx * 2 + 20 || r.height <= my * 2 + 6 {
        return r;
    }
    Rect {
        x: r.x + mx,
        y: r.y + my,
        width: r.width - mx * 2,
        height: r.height - my * 2,
    }
}

/// Blank lines between list rows (large only).
pub fn row_gap() -> usize {
    usize::from(scale() >= 2)
}

/// Rounded panel with a bold title; accent border when focused.
pub fn panel<'a>(title: impl Into<Line<'a>>, focused: bool) -> Block<'a> {
    use crate::look::{Frame, Titles};
    let lk = crate::look::get();
    let mut t: Line = title.into();
    if lk.titles == Titles::Caps {
        for s in t.spans.iter_mut() {
            s.content = s.content.to_uppercase().into();
        }
    }
    let (open, close) = match lk.frame {
        // the title sits in the bracket's arm
        Frame::Brackets => ("─ ", " ─"),
        Frame::Cut => ("─[ ", " ]"),
        _ => (" ", " "),
    };
    let edge = if focused { c::EDGE() } else { c::EDGE_DIM() };
    let set = border_set(false);
    let mut block = Block::bordered()
        .border_set(set)
        .border_style(fg(if focused { c::EDGE() } else { c::LINE() }))
        .padding(padding());
    if lk.cards && !c::mono() {
        // a card of the ground's colour (cyberpunk-light's yellow on black)
        block = block.style(Style::new().bg(c::BASE()));
    } else if lk.ground && c::pal().light && !c::mono() {
        // a light theme's panels are cards a shade under its paper
        block = block.style(Style::new().bg(CARD));
    }
    if lk.tags && !c::mono() {
        // a HUD tag: the title on a block of the frame's colour, in the
        // ground's (yellow on black, black on red), a space either side
        let tag = Style::new()
            .bg(edge)
            .fg(c::BASE())
            .add_modifier(Modifier::BOLD);
        let mut spans = vec![
            Span::styled(open.trim_end().to_string(), fg(edge)),
            Span::raw(" "),
        ];
        spans.push(Span::styled(" ", tag));
        // every part of the title in the ground's colour on the tag (its own
        // greys and identity colours wouldn't read on it)
        spans.extend(t.spans.into_iter().map(|s| Span::styled(s.content, tag)));
        spans.push(Span::styled(" ", tag));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(close.trim_start().to_string(), fg(edge)));
        return block.title(Line::from(spans));
    }
    t.spans.insert(0, Span::raw(open));
    t.spans.push(Span::raw(close));
    block.title(t.style(bold().fg(edge)))
}

/// The theme's frame for a block drawn by hand (`heavy`: the focused one of
/// several, a heavier line where the frame allows it).
pub fn border_set(heavy: bool) -> ratatui::symbols::border::Set {
    use crate::look::Frame;
    use ratatui::symbols::border;
    match crate::look::get().frame {
        Frame::Rounded if heavy => border::THICK,
        Frame::Rounded => border::ROUNDED,
        Frame::Sharp if heavy => border::THICK,
        Frame::Sharp => border::PLAIN,
        Frame::Heavy => border::THICK,
        Frame::Double => border::DOUBLE,
        Frame::Cut => border::Set {
            top_left: "╱",
            top_right: "╲",
            bottom_left: "╲",
            bottom_right: "╱",
            ..if heavy { border::THICK } else { border::PLAIN }
        },
        Frame::Brackets => border::Set {
            top_left: BRACKET[0],
            top_right: BRACKET[1],
            bottom_left: BRACKET[2],
            bottom_right: BRACKET[3],
            vertical_left: " ",
            vertical_right: " ",
            horizontal_top: " ",
            horizontal_bottom: " ",
        },
    }
}

/// A colour from outside the palette (an agent's own screen, a brand) as it
/// reads on the theme's own ground: moved just enough for 4.5:1, the 16 and
/// 256 terminal colours taken as xterm draws them. As it is on the
/// terminal's own background, or without colour. Cached: it runs per cell.
pub fn ink(col: Color) -> Color {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static SEEN: RefCell<(u64, HashMap<Color, Color>)> = RefCell::new((u64::MAX, HashMap::new()));
    }
    if matches!(col, Color::Reset) || !crate::look::get().ground {
        return col;
    }
    let p = c::pal();
    let Color::Rgb(br, bg, bb) = p.base else {
        return col;
    };
    let rgb = match col {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Indexed(n) => xterm(n),
        Color::Black => xterm(0),
        Color::Red => xterm(1),
        Color::Green => xterm(2),
        Color::Yellow => xterm(3),
        Color::Blue => xterm(4),
        Color::Magenta => xterm(5),
        Color::Cyan => xterm(6),
        Color::Gray => xterm(7),
        Color::DarkGray => xterm(8),
        Color::LightRed => xterm(9),
        Color::LightGreen => xterm(10),
        Color::LightYellow => xterm(11),
        Color::LightBlue => xterm(12),
        Color::LightMagenta => xterm(13),
        Color::LightCyan => xterm(14),
        Color::White => xterm(15),
        _ => return col,
    };
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let gen = c::generation();
        if s.0 != gen {
            *s = (gen, HashMap::new());
        }
        *s.1.entry(col).or_insert_with(|| {
            let (r, g, b) = crate::logo::fit_to(rgb, (br, bg, bb), 4.5);
            Color::Rgb(r, g, b)
        })
    })
}

/// A terminal colour as xterm draws it (the 16, the 6×6×6 cube, the greys).
fn xterm(n: u8) -> (u8, u8, u8) {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match n {
        0..=15 => BASE[n as usize],
        16..=231 => {
            let i = n - 16;
            let v = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            (v(i / 36), v(i / 6 % 6), v(i % 6))
        }
        _ => {
            let g = 8 + (n - 232) * 10;
            (g, g, g)
        }
    }
}

/// What a light theme's panel is filled with until `finish` knows where it
/// is: its card (`panel`), or inside a popup the popup's own surface.
pub const CARD: Color = Color::Rgb(1, 2, 3);

thread_local! {
    /// This frame's popups (`clear`): a card inside one is the popup's.
    static POPUPS: std::cell::RefCell<Vec<Rect>> = const { std::cell::RefCell::new(Vec::new()) };
    /// This frame's agent screens (`agent_screen`): the theme's ambience
    /// keeps out of them.
    static SCREENS: std::cell::RefCell<Vec<Rect>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// An agent's own screen was just drawn into `area` (a Focus or Tile tile, a
/// preview, a question's card): its blank cells are the agent's, not open
/// space for a theme's stars.
pub fn agent_screen(area: Rect) {
    SCREENS.with(|v| {
        let mut v = v.borrow_mut();
        // a frame has a handful; a frame nobody took them from adds no more
        if v.len() >= 64 {
            v.remove(0);
        }
        v.push(area);
    });
}

/// The agent screens drawn since the last call (once a frame).
pub fn agent_screens() -> Vec<Rect> {
    SCREENS.with(|v| std::mem::take(&mut *v.borrow_mut()))
}

/// Clear a popup's area. On a theme's own ground, a surface of its own so a
/// popup stands off the screen behind whatever its frame (open brackets
/// too): a light theme's lighter than its paper, a dark one's a shade above.
pub fn clear(f: &mut Frame, r: Rect) {
    f.render_widget(ratatui::widgets::Clear, r);
    if !crate::look::get().ground {
        return;
    }
    let p = c::pal();
    if !matches!(p.base, Color::Rgb(..)) {
        return;
    }
    POPUPS.with(|v| v.borrow_mut().push(r));
    // on cards, a popup is one more card
    let surface = if crate::look::get().cards {
        p.base
    } else {
        p.pop
    };
    let buf = f.buffer_mut();
    let r = r.intersection(buf.area);
    for y in r.top()..r.bottom() {
        for x in r.left()..r.right() {
            if let Some(cl) = buf.cell_mut((x, y)) {
                cl.bg = surface;
            }
        }
    }
    // on paper, a popup floats: a shadow a cell below it and to its right,
    // a shade under the cards (no terminal draws a real one)
    if p.light && !crate::look::get().cards {
        if let (Color::Rgb(ar, ag, ab), Color::Rgb(tr, tg, tb)) = (p.panel, p.text) {
            let m = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * 0.11).round() as u8;
            let shade = Color::Rgb(m(ar, tr), m(ag, tg), m(ab, tb));
            let (right, bottom) = (r.right(), r.bottom());
            let cells = (r.top() + 1..=bottom)
                .map(|y| (right, y))
                .chain((r.left() + 1..right).map(|x| (x, bottom)));
            for at in cells {
                if let Some(cl) = buf.cell_mut(at) {
                    cl.bg = shade;
                }
            }
        }
    }
}

/// The bracket frame's corners as `panel` draws them (glyphs nothing else
/// uses), and what `finish` makes of each: its corner and the side its arm
/// reaches to (+1 right, -1 left).
const BRACKET: [&str; 4] = ["┍", "┑", "┕", "┙"];
const BRACKET_TO: [(&str, i32); 4] = [("┌", 1), ("┐", -1), ("└", 1), ("┘", -1)];

/// The last touches on a full-screen frame, after everything is drawn (the
/// dashboard, setup, the browser, the launch card, a splash): the bracket
/// frames' arms, then the theme's ground under every cell left on the
/// terminal's own background (and its text colour on cells left on the
/// terminal's own foreground).
pub fn finish(buf: &mut Buffer) {
    let lk = crate::look::get();
    let area = buf.area;
    let brackets = lk.frame == crate::look::Frame::Brackets;
    for y in (area.top()..area.bottom()).filter(|_| brackets) {
        for x in area.left()..area.right() {
            let Some(k) = buf
                .cell((x, y))
                .and_then(|cl| BRACKET.iter().position(|b| cl.symbol() == *b))
            else {
                continue;
            };
            let (corner, dx) = BRACKET_TO[k];
            let style = buf.cell((x, y)).map(|cl| cl.style()).unwrap_or_default();
            if let Some(cl) = buf.cell_mut((x, y)) {
                cl.set_symbol(corner);
            }
            let ax = x as i32 + dx;
            if ax >= area.left() as i32 && ax < area.right() as i32 {
                if let Some(cl) = buf.cell_mut((ax as u16, y)) {
                    if cl.symbol() == " " {
                        cl.set_symbol("─").set_style(style);
                    }
                }
            }
            // a HUD's marks beside the top right corner: ///
            if k == 1 {
                let open = (x.saturating_sub(6)..x.saturating_sub(1)).all(|xx| {
                    xx >= area.left() && buf.cell((xx, y)).is_some_and(|cl| cl.symbol() == " ")
                });
                if open && x >= area.left() + 8 {
                    for xx in x - 5..x - 2 {
                        if let Some(cl) = buf.cell_mut((xx, y)) {
                            cl.set_symbol("/").set_style(style);
                        }
                    }
                }
            }
        }
    }
    if !lk.ground {
        return;
    }
    let p = c::pal();
    let dark_sel = matches!(p.sel, Color::Rgb(r, g, b) if lum_fast((r, g, b)) < 0.2);
    let inks = [p.text, p.muted, p.faint, p.line, Color::Reset];
    // a light theme's cards: the panel's surface, or inside a popup the popup's
    let popups = POPUPS.with(|v| std::mem::take(&mut *v.borrow_mut()));
    let w = area.width.max(1) as usize;
    for (i, cl) in buf.content.iter_mut().enumerate() {
        if cl.bg == CARD {
            let at = (area.x + (i % w) as u16, area.y + (i / w) as u16);
            let inside = popups.iter().any(|r| {
                at.0 >= r.x && at.0 < r.x + r.width && at.1 >= r.y && at.1 < r.y + r.height
            });
            cl.bg = if inside { p.pop } else { p.panel };
        }
        // cards: what isn't on one (the space between panels) is the
        // frame's colour, its text in the ground's
        if lk.cards && cl.bg == Color::Reset {
            cl.bg = p.edge;
            if inks.contains(&cl.fg) {
                cl.fg = p.base;
            }
        }
        if cl.bg == Color::Reset {
            cl.bg = p.base;
        }
        if cl.fg == Color::Reset {
            cl.fg = p.text;
        }
        // an inverted selection on a light ground (cyberpunk-light's black
        // row): its text in the ground's colour
        if p.light && cl.bg == p.sel && dark_sel && [p.text, p.muted, p.faint].contains(&cl.fg) {
            cl.fg = p.base;
        }
        // on a light ground, whatever still doesn't read on its own cell (a
        // brand's colour, a pill's): moved just enough (a dark ground's
        // faint details are faint on purpose)
        if p.light && cl.symbol() != " " {
            cl.fg = legible(cl.fg, cl.bg);
        }
    }
}

/// A splash's frame on the theme's ground: only the ground (a scene's dim
/// stars are dim on purpose, and it has no frames).
pub fn ground(buf: &mut Buffer) {
    if !crate::look::get().ground {
        return;
    }
    let p = c::pal();
    for cl in buf.content.iter_mut() {
        if cl.bg == Color::Reset {
            cl.bg = p.base;
        }
        if cl.fg == Color::Reset {
            cl.fg = p.text;
        }
    }
}

/// Relative luminance from a table (no powers per cell).
fn lum_fast((r, g, b): (u8, u8, u8)) -> f64 {
    static LIN: std::sync::OnceLock<[f64; 256]> = std::sync::OnceLock::new();
    let t = LIN.get_or_init(|| {
        let mut t = [0.0; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let x = i as f64 / 255.0;
            *v = if x <= 0.04045 {
                x / 12.92
            } else {
                ((x + 0.055) / 1.055).powf(2.4)
            };
        }
        t
    });
    0.2126 * t[r as usize] + 0.7152 * t[g as usize] + 0.0722 * t[b as usize]
}

/// A row on the theme's band (cyberpunk-light's black bars over the header
/// and footer): the frame's colour behind it, text in the ground's colour,
/// anything coloured kept readable on it.
pub fn band(buf: &mut Buffer, y: u16) {
    let p = c::pal();
    if p.mono || y >= buf.area.bottom() {
        return;
    }
    for x in buf.area.left()..buf.area.right() {
        if let Some(cl) = buf.cell_mut((x, y)) {
            cl.bg = p.edge;
            cl.fg = if [p.text, p.muted, p.faint, p.line, Color::Reset].contains(&cl.fg) {
                p.base
            } else {
                legible(cl.fg, p.edge)
            };
        }
    }
}

/// Colours already fitted to a background, for one palette (its generation).
type Fitted = (u64, std::collections::HashMap<(Color, Color), Color>);

/// `fg` on `bg`, moved (same hue) to 4.5:1 when it's under 3:1; as it is
/// otherwise. Cached per palette: it runs per cell, every frame.
pub fn legible(fg: Color, bg: Color) -> Color {
    use std::cell::RefCell;
    use std::collections::HashMap;
    thread_local! {
        static SEEN: RefCell<Fitted> = RefCell::new((u64::MAX, HashMap::new()));
    }
    let (Color::Rgb(r, g, b), Color::Rgb(br, bgg, bb)) = (fg, bg) else {
        return fg;
    };
    // most cells read already: a table lookup or two says so
    let (a, z) = (lum_fast((r, g, b)) + 0.05, lum_fast((br, bgg, bb)) + 0.05);
    if a.max(z) / a.min(z) >= 3.0 {
        return fg;
    }
    SEEN.with(|s| {
        let mut s = s.borrow_mut();
        let gen = c::generation();
        // a new palette, or too many colours (something animating): afresh
        if s.0 != gen || s.1.len() > 4096 {
            *s = (gen, HashMap::new());
        }
        *s.1.entry((fg, bg)).or_insert_with(|| {
            let (r, g, b) = crate::logo::fit_to((r, g, b), (br, bgg, bb), 4.5);
            Color::Rgb(r, g, b)
        })
    })
}

/// Recolour the selection bar (`┃`) of a list/table in the brand accent —
/// ratatui styles the highlight symbol like the row itself.
pub fn accent_bar(f: &mut Frame, inner: Rect, first_row_y: u16, offset: usize, sel: usize) {
    let y = first_row_y + sel.saturating_sub(offset) as u16;
    if y >= first_row_y && y < inner.y + inner.height {
        if let Some(cell) = f.buffer_mut().cell_mut((inner.x, y)) {
            cell.set_symbol("┃").set_fg(c::ACCENT());
        }
    }
}

/// A shimmer every 2.5 s, crossing a word in 1.2 s of it.
pub const SHIMMER_MS: u128 = 2500;
pub const SHIMMER_SWEEP_MS: f64 = 1200.0;

/// The colour a shimmer peaks at: brighter gold on a dark theme, a deeper
/// one on a light theme (lighter would vanish into the background there).
fn shimmer_peak() -> (u8, u8, u8) {
    if c::pal().light {
        (0xc2, 0x7c, 0x00)
    } else {
        (0xff, 0xf1, 0xc4)
    }
}

/// `word` one span per character, the band `ms` into its cycle brightening
/// the characters it passes. Without truecolour it stays as it was.
pub fn shimmer(word: &str, base: Style, ms: u128) -> Vec<Span<'static>> {
    let Some(Color::Rgb(..)) = base.fg else {
        return vec![Span::styled(word.to_string(), base)];
    };
    let n = word.chars().count() as f64;
    let t = (ms % SHIMMER_MS) as f64;
    let pos = -2.5 + t / SHIMMER_SWEEP_MS * (n + 5.0);
    word.chars()
        .enumerate()
        .map(|(i, ch)| {
            let k = (1.0 - (i as f64 - pos).abs() / 2.0).clamp(0.0, 1.0);
            Span::styled(ch.to_string(), shimmer_one(base, k))
        })
        .collect()
}

/// `base` brightened by `k` (0 to 1) toward the shimmer's peak.
pub fn shimmer_one(base: Style, k: f64) -> Style {
    let Some(Color::Rgb(r, g, b)) = base.fg else {
        return base;
    };
    let (pr, pg, pb) = shimmer_peak();
    let mix = |a: u8, z: u8| (a as f64 + (z as f64 - a as f64) * k).round() as u8;
    base.fg(Color::Rgb(mix(r, pr), mix(g, pg), mix(b, pb)))
}

/// Spinners (`ui.spinner`): Aleph's pulse (default), Claude Code's stars,
/// braille dots, or a plain line. The pulse, dots and line are one column in
/// any terminal (no East Asian Ambiguous glyph: `⋅`, not `·`); the stars are
/// Claude Code's own, `·` and `✽` included.
const SPIN_ALEPH: &[&str] = &["⋅", "∙", "∘", "ℵ", "ℵ", "∘", "∙", "⋅"];
const SPIN_STARS: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽", "✻", "✶", "✳", "✢"];
const SPIN_DOTS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SPIN_LINE: &[&str] = &["-", "\\", "|", "/"];

static SPIN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(4);

/// `ui.spinner` = theme | aleph | stars | dots | line (switchable live).
pub fn set_spinner(name: &str) {
    let v = match name {
        "stars" => 1,
        "dots" => 2,
        "line" => 3,
        "aleph" => 0,
        _ => 4,
    };
    SPIN.store(v, std::sync::atomic::Ordering::Relaxed);
}

pub fn spinner(since: Instant) -> &'static str {
    let (frames, ms): (&[&str], u128) = match SPIN.load(std::sync::atomic::Ordering::Relaxed) {
        1 => (SPIN_STARS, 110),
        2 => (SPIN_DOTS, 80),
        3 => (SPIN_LINE, 120),
        0 => (SPIN_ALEPH, 130),
        // the theme's own
        _ => crate::look::get().spin,
    };
    frames[(since.elapsed().as_millis() / ms) as usize % frames.len()]
}

/// Whimsical progress verbs, Claude Code style.
pub fn verb(seed: u64) -> &'static str {
    const V: [&str; 8] = [
        "Herding",
        "Summoning",
        "Wrangling",
        "Conjuring",
        "Spinning up",
        "Brewing",
        "Rallying",
        "Forging",
    ];
    V[(seed % V.len() as u64) as usize]
}

/// `col`, breathing: a triangle wave between full and about half strength
/// every `period_ms`. Truecolour fades toward grey; other palettes swap bold
/// and dim; with NO_COLOR it holds still.
pub fn pulse(col: Color, t: Instant, period_ms: u64) -> Style {
    if c::mono() || period_ms == 0 {
        return fg(col).add_modifier(Modifier::BOLD);
    }
    let ph = (t.elapsed().as_millis() as u64 % period_ms) as f64 / period_ms as f64;
    // 1 → 0 → 1 over a period
    let k = (2.0 * ph - 1.0).abs();
    match col {
        Color::Rgb(r, g, b) => {
            let a = 0.45 + 0.55 * k;
            let mix = |x: u8| (x as f64 * a + 120.0 * (1.0 - a)).round() as u8;
            fg(Color::Rgb(mix(r), mix(g), mix(b))).add_modifier(Modifier::BOLD)
        }
        _ if k > 0.5 => fg(col).add_modifier(Modifier::BOLD),
        _ => fg(col).add_modifier(Modifier::DIM),
    }
}

/// (glyph, colour, label) for a herdr agent status.
pub fn status(s: &str, t: Instant) -> (&'static str, Color, String) {
    match s {
        "working" => (spinner(t), c::WORKING(), "working".into()),
        "blocked" => (
            crate::look::get().glyphs.needs,
            c::NEEDS(),
            "needs you".into(),
        ),
        "idle" => (crate::look::get().glyphs.idle, c::IDLE(), "idle".into()),
        "done" => (crate::look::get().glyphs.done, c::DONE(), "done".into()),
        other => ("○", c::MUTED(), other.to_string()),
    }
}

/// Colour for a legacy session glyph (sessions started before hues existed).
pub fn glyph_color(g: &str) -> Color {
    match g {
        "●" => c::RED(),
        "◆" => c::PEACH(),
        "▲" => c::YELLOW(),
        "■" => c::GREEN(),
        "★" => c::TEAL(),
        "✚" => c::BLUE(),
        "♦" => c::MAUVE(),
        "▼" => c::PINK(),
        _ => c::MUTED(),
    }
}

// ------------------------------------------------------------------ meters

const EIGHTHS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
const SPARK: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// Green → yellow → red as a load fraction rises.
pub fn heat(frac: f64) -> Color {
    if frac >= 0.85 {
        c::RED()
    } else if frac >= 0.6 {
        c::YELLOW()
    } else if frac >= 0.3 {
        c::GREEN()
    } else {
        c::TEAL()
    }
}

/// A smooth horizontal bar `w` cells wide (eighth-block resolution).
pub fn bar(frac: f64, w: usize, col: Color) -> Vec<Span<'static>> {
    let f = frac.clamp(0.0, 1.0);
    let eighths = (f * (w * 8) as f64).round() as usize;
    let full = eighths / 8;
    let part = eighths % 8;
    let mut filled = "█".repeat(full);
    if part > 0 && full < w {
        filled.push_str(EIGHTHS[part]);
    }
    let used = full + usize::from(part > 0 && full < w);
    let track = if c::mono() { " " } else { "·" };
    vec![
        Span::styled(filled, Style::new().fg(col)),
        Span::styled(
            track.repeat(w.saturating_sub(used)),
            Style::new().fg(c::LINE()),
        ),
    ]
}

/// One-row sparkline of the last `w` samples (0..=1), coloured by `col`.
pub fn spark(samples: &[f64], w: usize, col: Color) -> Span<'static> {
    let start = samples.len().saturating_sub(w);
    let mut s: String = samples[start..]
        .iter()
        .map(|v| {
            // a gap: nothing was read then
            if v.is_finite() {
                SPARK[((v.clamp(0.0, 1.0) * 7.0).round()) as usize]
            } else {
                " "
            }
        })
        .collect();
    let pad = w.saturating_sub(samples.len() - start);
    s = format!("{}{}", " ".repeat(pad), s);
    Span::styled(s, Style::new().fg(col))
}

/// A small area chart: the last `w` samples (0..=1) as `h` rows of
/// eighth-blocks, newest on the right.
pub fn area(samples: &[f64], w: usize, h: usize, col: Color) -> Vec<Line<'static>> {
    let start = samples.len().saturating_sub(w);
    let s = &samples[start..];
    let pad = w.saturating_sub(s.len());
    (0..h)
        .map(|row| {
            // row 0 is the top
            let floor = (h - 1 - row) as f64;
            let mut t = " ".repeat(pad);
            for v in s {
                let eighths = (v.clamp(0.0, 1.0) * h as f64 * 8.0).round();
                let here = (eighths - floor * 8.0).clamp(0.0, 8.0) as usize;
                t.push_str(if here == 0 { " " } else { EIGHTHS_UP[here] });
            }
            Line::from(Span::styled(t, Style::new().fg(col)))
        })
        .collect()
}

/// A chart that fills `w` columns whatever the number of samples (fewer
/// samples are stretched, more keep the newest), drawn on a faint baseline
/// so even an idle series is anchored. Values 0..=1.
pub fn chart(samples: &[f64], w: usize, h: usize, col: Color) -> Vec<Line<'static>> {
    let w = w.max(1);
    let h = h.max(1);
    let data: Vec<f64> = if samples.is_empty() {
        vec![0.0; w]
    } else if samples.len() >= w {
        samples[samples.len() - w..].to_vec()
    } else if samples.len() == 1 {
        vec![samples[0]; w]
    } else {
        // stretch, interpolating between samples so it stays smooth
        let n = samples.len() - 1;
        (0..w)
            .map(|x| {
                let p = x as f64 * n as f64 / (w - 1).max(1) as f64;
                let i = (p.floor() as usize).min(n - 1);
                let t = p - i as f64;
                samples[i] * (1.0 - t) + samples[i + 1] * t
            })
            .collect()
    };
    // 0: empty, 1: the baseline, 2: the fill (dimmed), 3: the top edge
    let cell = |v: f64, row: usize| -> (&'static str, u8) {
        let floor = (h - 1 - row) as f64;
        let eighths = (v.clamp(0.0, 1.0) * h as f64 * 8.0).round();
        let here = (eighths - floor * 8.0).clamp(0.0, 8.0) as usize;
        let top = eighths <= (floor + 1.0) * 8.0;
        if here > 0 {
            (EIGHTHS_UP[here], if top { 3 } else { 2 })
        } else if row + 1 == h {
            ("─", 1)
        } else {
            (" ", 0)
        }
    };
    let style = |k: u8| match k {
        1 => fg(c::LINE()),
        2 if c::mono() => Style::new(),
        2 => fg(col).add_modifier(Modifier::DIM),
        _ => fg(col),
    };
    (0..h)
        .map(|row| {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut run = String::new();
            let mut kind = 0u8;
            for v in &data {
                let (sym, k) = cell(*v, row);
                if k != kind && !run.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut run), style(kind)));
                }
                kind = k;
                run.push_str(sym);
            }
            if !run.is_empty() {
                spans.push(Span::styled(run, style(kind)));
            }
            Line::from(spans)
        })
        .collect()
}

const EIGHTHS_UP: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// A single cell showing a level (per-core strips).
pub fn level(v: f64) -> Span<'static> {
    let v = v.clamp(0.0, 1.0);
    Span::styled(SPARK[(v * 7.0).round() as usize], Style::new().fg(heat(v)))
}

/// `12.3G`, `512M` from bytes.
pub fn bytes(b: f64) -> String {
    const U: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let mut v = b.max(0.0);
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if v >= 100.0 || i == 0 {
        format!("{v:.0}{}", U[i])
    } else {
        format!("{v:.1}{}", U[i])
    }
}

/// `3d 4h`, `5h 12m`, `7m`.
pub fn dur(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h {}m", secs / 3600, secs % 3600 / 60),
        _ => format!("{}d {}h", secs / 86400, secs % 86400 / 3600),
    }
}

// ------------------------------------------------------------------ text helpers

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut to `w` display columns with an ellipsis.
pub fn trunc(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

/// Cut from the left (keeps the end of a path).
pub fn trunc_left(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut rev: Vec<char> = Vec::new();
    let mut used = 0;
    for ch in s.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if used + cw > w - 1 {
            break;
        }
        rev.push(ch);
        used += cw;
    }
    std::iter::once('…').chain(rev.into_iter().rev()).collect()
}

pub fn pad(s: &str, w: usize) -> String {
    let t = trunc(s, w);
    let n = width(&t);
    format!("{t}{}", " ".repeat(w.saturating_sub(n)))
}

/// Word-wrap to `w` columns (hard-breaks words longer than a line).
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split(' ') {
        let ww = width(word);
        let cw = width(&cur);
        if cw > 0 && cw + 1 + ww > w {
            out.push(std::mem::take(&mut cur));
        }
        if ww > w {
            for ch in word.chars() {
                if width(&cur) + ch.width().unwrap_or(0) > w {
                    out.push(std::mem::take(&mut cur));
                }
                cur.push(ch);
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() || out.is_empty() {
        out.push(cur);
    }
    out
}

pub fn ago(s: u64) -> String {
    match s {
        0..=4 => "just now".into(),
        5..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

pub fn line_width(l: &Line) -> usize {
    l.spans.iter().map(|s| width(&s.content)).sum()
}

// ------------------------------------------------------------------ printed output

/// Render a styled line as ANSI text (or plain text when `color` is false).
pub fn to_ansi(l: &Line, color: bool) -> String {
    // showcase mode: the line's words masked across its spans, each span
    // keeping its part
    let masked: Option<Vec<String>> = {
        let parts: Vec<&str> = l.spans.iter().map(|s| s.content.as_ref()).collect();
        crate::showcase::spans(&parts)
    };
    let mut s = String::new();
    for (i, sp) in l.spans.iter().enumerate() {
        let content: std::borrow::Cow<str> = match &masked {
            Some(m) => std::borrow::Cow::Borrowed(m[i].as_str()),
            None => std::borrow::Cow::Borrowed(sp.content.as_ref()),
        };
        let st = l.style.patch(sp.style);
        if !color {
            s.push_str(&content);
            continue;
        }
        let mut codes: Vec<String> = Vec::new();
        if st.add_modifier.contains(Modifier::BOLD) {
            codes.push("1".into());
        }
        if st.add_modifier.contains(Modifier::DIM) {
            codes.push("2".into());
        }
        match st.fg {
            Some(Color::Rgb(r, g, b)) => codes.push(format!("38;2;{r};{g};{b}")),
            // the 256-colour palette (logos without truecolor)
            Some(Color::Indexed(n)) => codes.push(format!("38;5;{n}")),
            _ => {}
        }
        match st.bg {
            Some(Color::Rgb(r, g, b)) => codes.push(format!("48;2;{r};{g};{b}")),
            Some(Color::Indexed(n)) => codes.push(format!("48;5;{n}")),
            _ => {}
        }
        if codes.is_empty() {
            s.push_str(&content);
        } else {
            s.push_str(&format!("\x1b[{}m{}\x1b[0m", codes.join(";"), content));
        }
    }
    s
}

/// Width for printed cards: the terminal, clamped to something readable.
pub fn print_width() -> usize {
    crossterm::terminal::size()
        .map(|(w, _)| w as usize)
        .unwrap_or(100)
        .clamp(60, 110)
}

/// Centered rect of at most `w`×`h` inside `r`.
pub fn centered(r: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(r.width);
    let h = h.min(r.height);
    Rect {
        x: r.x + (r.width - w) / 2,
        y: r.y + (r.height - h) / 2,
        width: w,
        height: h,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc_left("/a/b/c/d", 5), "…/c/d");
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(
            wrap("one two three four", 9),
            vec!["one two", "three", "four"]
        );
        assert_eq!(wrap("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
        assert_eq!(width("日本"), 4);
        assert_eq!(trunc("日本語テキスト", 5), "日本…");
    }

    /// `width` is unicode-width's (ratatui's measure): East Asian Ambiguous
    /// glyphs count one column. A terminal set to draw them wide (iTerm2's
    /// "Treat ambiguous-width characters as double width", VTE's) breaks every
    /// ratatui screen (box drawing, blocks, ● ○ ▲, `·`, `…`); Aleph's own
    /// marks stay one column there too.
    #[test]
    fn glyph_widths() {
        let cjk = |s: &str| UnicodeWidthStr::width_cjk(s);
        // the mark, the record dot, the pulse, braille and the line: narrow anywhere
        for s in [crate::brand::MARK, "⏺"]
            .into_iter()
            .chain(SPIN_ALEPH.iter().copied())
            .chain(SPIN_DOTS.iter().copied())
            .chain(SPIN_LINE.iter().copied())
        {
            assert_eq!((width(s), cjk(s)), (1, 1), "{s:?}");
        }
        // one column to ratatui, two where ambiguous is wide
        for s in [
            "·", "…", "●", "○", "▲", "◆", "─", "│", "╭", "█", "▄", "┃", "✽",
        ] {
            assert_eq!((width(s), cjk(s)), (1, 2), "{s:?}");
        }
        // every spinner frame, agent and server glyph is one column to
        // ratatui (rows put them in a fixed column)
        for frames in [SPIN_ALEPH, SPIN_STARS, SPIN_DOTS, SPIN_LINE] {
            assert!(frames.iter().all(|f| width(f) == 1), "{frames:?}");
        }
        for a in crate::agents::ALL {
            let g = crate::icons::brand(a.id).0;
            assert_eq!(width(g), 1, "{}: {g:?}", a.id);
        }
        for s in crate::services::ALL {
            assert_eq!(width(s.glyph), 1, "{}: {:?}", s.id, s.glyph);
        }
    }
}

// ------------------------------------------------------------------ local time

/// This computer's UTC offset in seconds (`date +%z`, asked once).
pub fn utc_offset() -> i64 {
    use std::sync::OnceLock;
    static OFF: OnceLock<i64> = OnceLock::new();
    *OFF.get_or_init(|| {
        let o = crate::proc::cap("date", &["+%z"], std::time::Duration::from_secs(2));
        parse_offset(o.text().trim()).unwrap_or(0)
    })
}

/// `+0530` → 19800, `-0700` → -25200.
pub fn parse_offset(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if rest.len() != 4 {
        return None;
    }
    let h: i64 = rest[..2].parse().ok()?;
    let m: i64 = rest[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

/// (year, month 1-12, day) for days since 1970-01-01 (Howard Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Local `14:05` for a unix time.
pub fn clock(t: u64) -> String {
    let s = t as i64 + utc_offset();
    format!(
        "{:02}:{:02}",
        s.rem_euclid(86_400) / 3600,
        s.rem_euclid(3600) / 60
    )
}

/// The local day number of a unix time (for grouping).
pub fn local_day(t: u64) -> i64 {
    (t as i64 + utc_offset()).div_euclid(86_400)
}

/// `Today`, `Yesterday`, `Mon 20 Oct` (local).
pub fn day_label(t: u64, now: u64) -> String {
    let (d, today) = (local_day(t), local_day(now));
    match today - d {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        _ => {
            const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
            const MO: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let (_, m, day) = civil(d);
            format!(
                "{} {day} {}",
                WD[d.rem_euclid(7) as usize],
                MO[(m - 1) as usize]
            )
        }
    }
}

/// List rows for items grouped by local day, newest first: `None` is a day
/// header, `Some(i)` the i-th item.
pub fn day_rows(ts: impl IntoIterator<Item = u64>) -> Vec<Option<usize>> {
    let mut out = Vec::new();
    let mut last = None;
    for (i, t) in ts.into_iter().enumerate() {
        let day = local_day(t);
        if last != Some(day) {
            out.push(None);
            last = Some(day);
        }
        out.push(Some(i));
    }
    out
}

/// ` Today ─────────` across `w` columns.
pub fn day_header(t: u64, now: u64, w: usize) -> Line<'static> {
    let label = day_label(t, now);
    let fill = w.saturating_sub(width(&label) + 3);
    Line::from(vec![
        Span::styled(
            format!(" {label} "),
            fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ),
        Span::styled("─".repeat(fill), fg(c::LINE())),
    ])
}

/// `/home/selene/x` or `/Users/me/x` → `~/x` (for display only).
pub fn tilde_path(p: &str) -> String {
    for root in ["/home/", "/Users/"] {
        if let Some(rest) = p.strip_prefix(root) {
            return match rest.split_once('/') {
                Some((_, r)) => format!("~/{r}"),
                None => "~".into(),
            };
        }
    }
    if p == "/root" || p.starts_with("/root/") {
        return format!("~{}", &p[5..]);
    }
    p.to_string()
}

#[cfg(test)]
mod time_tests {
    use super::*;

    #[test]
    fn offsets_and_civil() {
        assert_eq!(parse_offset("+0530"), Some(19_800));
        assert_eq!(parse_offset("-0700"), Some(-25_200));
        assert_eq!(parse_offset("Z"), None);
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_748), (2026, 10, 22));
        assert_eq!(civil(-1), (1969, 12, 31));
    }

    #[test]
    fn days_and_paths() {
        let r = day_rows([200_000, 199_000, 100_000]);
        assert_eq!(r.len(), 5);
        assert_eq!((r[0], r[1], r[3]), (None, Some(0), None));
        assert_eq!(tilde_path("/home/selene/a/b"), "~/a/b");
        assert_eq!(tilde_path("/Users/me"), "~");
        assert_eq!(tilde_path("/root/x"), "~/x");
        assert_eq!(tilde_path("/srv/x"), "/srv/x");
    }
}
