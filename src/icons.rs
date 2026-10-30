//! Logos for machines, GPUs and agents — one place, three styles
//! (setting `icons`): `emoji` (default: colourful, works in any modern
//! terminal), `nerd` (Nerd Font glyphs, crisp if your font has them) and
//! `text` (plain letters, for NO_COLOR or minimal fonts). Every function
//! returns a styled span whose width ratatui measures correctly. Pictures of
//! the logos themselves are `logo`'s; `aleph icons` shows them all.

use crate::ui::{self, c};
use ratatui::prelude::*;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style_ {
    Emoji,
    Nerd,
    Text,
}

static STYLE: AtomicU8 = AtomicU8::new(0);

/// Set at start (and from Settings) by the `icons` setting (NO_COLOR forces text).
pub fn init(setting: &str) {
    let s = if ui::c::mono() {
        Style_::Text
    } else {
        match setting {
            "nerd" => Style_::Nerd,
            "text" => Style_::Text,
            "emoji" => Style_::Emoji,
            // auto: real logos only when the terminal's own font has them
            _ if terminal_nerd_font() == Some(true) => Style_::Nerd,
            _ => Style_::Emoji,
        }
    };
    STYLE.store(s as u8, Ordering::Relaxed);
}

fn is_nerd(font: &str) -> bool {
    let l = font.to_lowercase();
    l.contains("nerd")
        || l.contains("nfm")
        || l.ends_with(" nf")
        || l.contains(" nf ")
        || l.contains("nf-")
}

/// Does the terminal we're drawing in use a font with Nerd Font logos?
/// Some(true/false) when its settings say, None when we can't tell. An
/// installed Nerd Font doesn't count: the terminal has to use it (Warp, for
/// one, doesn't fall back to it). `CC_NERD_FONT=1/0` overrides.
pub fn terminal_nerd_font() -> Option<bool> {
    terminal_font().0
}

/// (has Nerd Font logos?, the font's name when we could read it).
pub fn terminal_font() -> (Option<bool>, Option<String>) {
    use std::sync::OnceLock;
    static V: OnceLock<(Option<bool>, Option<String>)> = OnceLock::new();
    V.get_or_init(|| {
        if let Some(v) = crate::brand::env("NERD_FONT") {
            return (
                Some(matches!(v.as_str(), "1" | "true" | "yes" | "on")),
                None,
            );
        }
        let tp = std::env::var("TERM_PROGRAM").unwrap_or_default();
        let term = std::env::var("TERM").unwrap_or_default();
        let home = crate::config::home();
        let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
        let defaults = |domain: &str, key: &str| -> Option<String> {
            let o = crate::proc::run(
                std::process::Command::new("defaults").args(["read", domain, key]),
                std::time::Duration::from_secs(2),
                None,
            )
            .ok()?;
            o.ok().then(|| o.text())
        };
        let named = |f: Option<String>| {
            let f = f.map(|x| x.trim().to_string()).filter(|x| !x.is_empty());
            (Some(f.as_deref().is_some_and(is_nerd)), f)
        };
        match tp.as_str() {
            // Warp: its font, and it falls back to any installed font for the
            // glyphs its own lacks (seen: Hack + an installed Nerd Font shows
            // the logos)
            "WarpTerminal" => {
                let f = [
                    "dev.warp.Warp-Stable",
                    "dev.warp.Warp-Preview",
                    "dev.warp.Warp",
                ]
                .iter()
                .find_map(|d| defaults(d, "FontName"))
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty());
                let own = f.as_deref().is_some_and(is_nerd);
                let installed = nerd_font_installed();
                let name = match (f, own, installed) {
                    (Some(f), true, _) => f,
                    (f, false, true) => format!(
                        "{} · logos from the installed Nerd Font",
                        f.unwrap_or_else(|| "Hack (Warp's default)".into())
                    ),
                    (f, ..) => f.unwrap_or_else(|| "Hack (Warp's default)".into()),
                };
                (Some(own || installed), Some(name))
            }
            // Terminal.app: the default profile's font (an archived NSFont)
            "Apple_Terminal" => named(apple_terminal_font()),
            "iTerm.app" => match defaults("com.googlecode.iterm2", "New Bookmarks") {
                Some(t) => {
                    let f: Vec<&str> = t.lines().filter(|l| l.contains("Normal Font")).collect();
                    (
                        Some(f.iter().any(|l| is_nerd(l))),
                        f.first()
                            .and_then(|l| l.split('"').nth(3))
                            .map(str::to_string),
                    )
                }
                None => (None, None),
            },
            // these ship Nerd Font symbols as a fallback
            "ghostty" | "WezTerm" => (Some(true), None),
            _ if term == "xterm-kitty" || std::env::var_os("KITTY_WINDOW_ID").is_some() => {
                (Some(true), None)
            }
            _ if std::env::var_os("ALACRITTY_WINDOW_ID").is_some() => (
                Some(is_nerd(&format!(
                    "{}{}",
                    read(home.join(".config/alacritty/alacritty.toml")),
                    read(home.join(".config/alacritty/alacritty.yml"))
                ))),
                None,
            ),
            _ => (None, None),
        }
    })
    .clone()
}

/// Terminal.app's default profile font name (`SFMono-Regular`,
/// `JetBrainsMonoNFM-Regular`), from its preferences.
fn apple_terminal_font() -> Option<String> {
    let run = |prog: &str, args: &[&str]| -> Option<String> {
        let o = crate::proc::run(
            std::process::Command::new(prog).args(args),
            std::time::Duration::from_secs(2),
            None,
        )
        .ok()?;
        o.ok().then(|| o.text())
    };
    let plist = crate::config::home().join("Library/Preferences/com.apple.Terminal.plist");
    let plist = plist.to_string_lossy().to_string();
    for key in ["Default Window Settings", "Startup Window Settings"] {
        let Some(profile) = run("defaults", &["read", "com.apple.Terminal", key]) else {
            continue;
        };
        let kp = format!("Window Settings.{}.Font", profile.trim());
        let Some(b) = run("plutil", &["-extract", &kp, "raw", "-o", "-", &plist]) else {
            continue;
        };
        let bytes = crate::stream::b64(b.trim());
        // the archive holds the font's PostScript name as plain ASCII
        let text: String = String::from_utf8_lossy(&bytes)
            .chars()
            .map(|c| if c.is_ascii_graphic() { c } else { ' ' })
            .collect();
        if let Some(name) = text.split_whitespace().find(|w| {
            w.contains('-') && !w.starts_with('$') && w.len() > 4 && !w.contains("NSKeyed")
        }) {
            return Some(name.trim_start_matches(['^', '_']).to_string());
        }
    }
    None
}

/// Is a Nerd Font installed on this computer? (Its logos are vector glyphs:
/// the Apple, Ubuntu, Debian … marks themselves.)
pub fn nerd_font_installed() -> bool {
    let home = crate::config::home();
    let dirs = [
        home.join("Library/Fonts"),
        "/Library/Fonts".into(),
        home.join(".local/share/fonts"),
        home.join(".fonts"),
        "/usr/share/fonts".into(),
        "/usr/local/share/fonts".into(),
    ];
    let nerd = |n: &str| {
        let l = n.to_lowercase();
        l.contains("nerd") || l.contains("nerdfont") || l.ends_with("nf.ttf") || l.contains(" nf ")
    };
    dirs.iter().any(|d: &std::path::PathBuf| {
        std::fs::read_dir(d).is_ok_and(|rd| {
            rd.flatten().any(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                nerd(&n)
                    || (e.path().is_dir()
                        && std::fs::read_dir(e.path()).is_ok_and(|r2| {
                            r2.flatten().any(|x| nerd(&x.file_name().to_string_lossy()))
                        }))
            })
        })
    })
}

pub fn style() -> Style_ {
    match STYLE.load(Ordering::Relaxed) {
        1 => Style_::Nerd,
        2 => Style_::Text,
        _ => Style_::Emoji,
    }
}

/// Which OS family a `Stats.os` / `platform` string describes.
pub use crate::logo::Os;

#[allow(dead_code)] // the views read the whole row (`logo::os_info`) now
pub fn os_of(os: &str, platform: &str) -> Os {
    crate::logo::os_info(&format!("{os} {platform}")).map_or(Os::Unknown, |i| i.family)
}

/// The OS logo (`🐧`, `🍎`, `🪟` / Nerd glyphs / `lnx` `mac` `win`).
pub fn os(o: Os) -> Span<'static> {
    let (e, n, t, col) = match o {
        Os::Linux => ("🐧", "\u{f17c}", "lnx", c::YELLOW()),
        Os::Mac => ("🍎", "\u{f179}", "mac", c::TEXT()),
        Os::Windows => ("🪟", "\u{f17a}", "win", c::BLUE()),
        Os::Bsd => ("😈", "\u{f30c}", "bsd", c::RED()),
        Os::Unknown => ("💻", "\u{f108}", "???", c::MUTED()),
    };
    match style() {
        Style_::Emoji => Span::raw(e),
        Style_::Nerd => Span::styled(n, ui::fg(col)),
        Style_::Text => Span::styled(t, ui::muted()),
    }
}

/// An operating system as people know it: Ubuntu, Debian, macOS, …
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distro {
    pub name: &'static str,
    /// its logo in Nerd Fonts (nf-linux-*, nf-fa-apple, …)
    pub nerd: &'static str,
    pub color: Color,
}

/// The distribution a machine's OS line names (`Linux · Ubuntu 24.04.3 LTS`),
/// from the one OS table (`logo::os_info`).
pub fn distro(os: &str) -> Option<Distro> {
    crate::logo::os_info(os).map(|i| Distro {
        name: i.name,
        nerd: i.nerd,
        color: i.color,
    })
}

/// The OS as a small logo for dense rows: the distribution's Nerd glyph, or
/// the family emoji, or a short word.
pub fn os_glyph(os: &str) -> Span<'static> {
    match (style(), distro(os)) {
        (Style_::Nerd, Some(d)) if !ui::c::mono() => Span::styled(d.nerd, ui::fg(d.color)),
        (Style_::Nerd, Some(d)) => Span::raw(d.nerd),
        // no logo font: the name, in its colours (never a fruit or a penguin)
        (_, Some(d)) => Span::styled(
            ui::pad(&ui::trunc(d.name, 6), 6),
            if ui::c::mono() {
                ui::bold()
            } else {
                ui::fg(d.color)
            },
        ),
        (_, None) => Span::styled(ui::pad("", 6), ui::faint()),
    }
}

/// The OS as a prominent badge (machine cards, the machine view): the logo
/// and name in the distribution's colours, padded to `w` columns so a column
/// of machines lines up. emoji/text: a chip in the brand colour; nerd: the
/// distribution's own logo and its name.
pub fn os_badge(os: &str, w: usize) -> Vec<Span<'static>> {
    let Some(d) = distro(os) else {
        return vec![Span::raw(ui::pad(&self::os(Os::Unknown).content, w))];
    };
    let mono = ui::c::mono();
    let (logo, name) = match style() {
        Style_::Nerd => (format!("{} ", d.nerd), d.name.to_string()),
        Style_::Text => (String::new(), d.name.to_string()),
        Style_::Emoji => (String::new(), d.name.to_string()),
    };
    let label = format!(" {logo}{name} ");
    let pad = w.saturating_sub(ui::width(&label));
    let chip = if mono {
        Style::new().add_modifier(Modifier::REVERSED)
    } else if style() == Style_::Text {
        Style::new().fg(d.color)
    } else {
        // readable text on the brand colour
        let dark = matches!(d.color, Color::Rgb(r, g, b) if (r as u32 * 299 + g as u32 * 587 + b as u32 * 114) / 1000 > 150);
        Style::new().bg(d.color).fg(if dark {
            Color::Rgb(20, 20, 24)
        } else {
            Color::Rgb(250, 250, 250)
        })
    };
    vec![Span::styled(label, chip), Span::raw(" ".repeat(pad))]
}

/// A GPU-vendor badge: `NVIDIA` in NVIDIA green, `AMD` in red, `Intel` in
/// blue; a vendor we don't know is named as it is, never guessed.
pub fn gpu(vendor: &str) -> Span<'static> {
    let other = ui::trunc(vendor.trim(), 12);
    let (label, col) = match crate::logo::gpu_vendor(vendor) {
        Some("nvidia") => ("NVIDIA".to_string(), Color::Rgb(118, 185, 0)),
        Some("amd") => ("AMD".into(), c::RED()),
        Some("intel") => ("Intel".into(), Color::Rgb(0x00, 0x71, 0xc5)),
        Some(_) => ("Apple GPU".into(), c::TEXT()),
        None if other.is_empty() => ("GPU".into(), c::MUTED()),
        None => (other, c::MUTED()),
    };
    let st = if ui::c::mono() {
        Style::new().add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(col).add_modifier(Modifier::BOLD)
    };
    match style() {
        Style_::Nerd => Span::styled(format!("\u{f0b2a} {label}"), st),
        _ => Span::styled(label, st),
    }
}

/// An agent's badge: its glyph in its brand colour (from the registry).
pub fn agent(id: &str) -> Span<'static> {
    let (g, col) = brand(id);
    if ui::c::mono() {
        return Span::styled(g.to_string(), ui::bold());
    }
    Span::styled(
        g.to_string(),
        Style::new().fg(col).add_modifier(Modifier::BOLD),
    )
}

/// (single-width glyph, brand colour) for an agent id.
pub fn brand(id: &str) -> (&'static str, Color) {
    match crate::agents::find(id).map(|a| a.id).unwrap_or(id) {
        "claude" => ("✻", Color::Rgb(217, 119, 87)),
        "codex" => ("◎", Color::Rgb(16, 163, 127)),
        "opencode" => ("⌬", Color::Rgb(250, 178, 131)),
        "gemini" => ("✦", Color::Rgb(66, 133, 244)),
        "copilot" => ("◈", Color::Rgb(137, 87, 229)),
        "cursor" | crate::agentsdk::ID => ("▲", Color::Rgb(220, 220, 220)),
        "devin" => ("◆", Color::Rgb(80, 200, 160)),
        "amp" => ("↯", Color::Rgb(255, 92, 92)),
        "droid" => ("⬢", Color::Rgb(255, 150, 50)),
        "kimi" => ("◐", Color::Rgb(90, 140, 255)),
        "kilo" => ("◇", Color::Rgb(240, 200, 60)),
        "qwen" => ("◉", Color::Rgb(110, 80, 240)),
        "grok" => ("✕", Color::Rgb(230, 230, 230)),
        "pi" => ("π", Color::Rgb(120, 200, 120)),
        "omp" => ("⊕", Color::Rgb(200, 120, 200)),
        "hermes" => ("☿", Color::Rgb(180, 180, 255)),
        "cline" => ("◭", Color::Rgb(100, 180, 255)),
        "kiro" => ("◬", Color::Rgb(140, 100, 255)),
        "agy" => ("◒", Color::Rgb(66, 133, 244)),
        // (squares and arrows are the LLM servers': `services::ALL`)
        "qodercli" => ("◓", Color::Rgb(0, 200, 180)),
        "mastracode" => ("✧", Color::Rgb(255, 120, 80)),
        "letta" => ("◑", Color::Rgb(255, 180, 0)),
        "maki" => ("◍", Color::Rgb(255, 100, 150)),
        "muse" => ("♪", Color::Rgb(0, 130, 250)),
        "crush" => ("❖", Color::Rgb(255, 80, 180)),
        "commandcode" => ("⌘", Color::Rgb(255, 210, 60)),
        "prime" => ("ρ", Color::Rgb(255, 255, 255)),
        "shell" => ("$", c::MUTED()),
        _ => ("●", c::MUTED()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_detection() {
        assert_eq!(os_of("Ubuntu 24.04.3 LTS", "linux"), Os::Linux);
        assert_eq!(os_of("macOS 27.0", "macos"), Os::Mac);
        assert_eq!(os_of("", "Darwin"), Os::Mac);
        assert_eq!(os_of("Microsoft Windows 11", ""), Os::Windows);
        assert_eq!(os_of("FreeBSD 14", ""), Os::Bsd);
        assert_eq!(os_of("", ""), Os::Unknown);
        // a word, not letters inside one
        assert_eq!(os_of("my machine", ""), Os::Unknown);
    }

    #[test]
    fn gpu_badges_name_the_vendor() {
        // (the Doctor view's preview may flip the style to Nerd meanwhile: ends_with)
        let label = |v: &str| gpu(v).content.to_string();
        assert!(label("nvidia").ends_with("NVIDIA"));
        assert!(label("AMD Radeon Pro W7900").ends_with("AMD"));
        // Intel used to come out as NVIDIA
        assert!(label("Intel(R) Arc(TM) A770").ends_with("Intel"));
        assert!(label("apple").ends_with("Apple GPU"));
        assert!(label("Qualcomm").ends_with("Qualcomm"));
        assert!(label("").ends_with("GPU"));
    }

    #[test]
    fn every_agent_has_a_single_width_glyph() {
        for a in crate::agents::ALL {
            let (g, _) = brand(a.id);
            assert_eq!(ui::width(g), 1, "{}", a.id);
        }
    }

    #[test]
    fn a_server_never_looks_like_an_agent() {
        // servers: boxes (▢▣▤…, ⊞⊟⊠⊡, ◰◱◲◳) and arrows for gateways (⇄ ⇋)
        let square = |g: &str| {
            g.chars().all(|c| {
                matches!(c as u32, 0x25A0..=0x25A9 | 0x25F0..=0x25F3 | 0x229E..=0x22A1 | 0x21C4 | 0x21CB)
            })
        };
        for s in crate::services::ALL {
            assert!(
                square(s.glyph),
                "{} {} isn't a box or an arrow",
                s.id,
                s.glyph
            );
        }
        for a in crate::agents::ALL {
            let (g, _) = brand(a.id);
            assert!(!square(g), "agent {} {g} looks like a server", a.id);
            assert!(
                crate::services::ALL.iter().all(|s| s.glyph != g),
                "agent {} shares {g} with a server",
                a.id
            );
        }
    }
}

#[cfg(test)]
mod font_tests {
    use super::*;

    #[test]
    fn nerd_font_names_and_distros() {
        assert!(is_nerd("JetBrainsMono Nerd Font"));
        assert!(is_nerd("JetBrainsMonoNFM-Regular 13"));
        assert!(is_nerd("Hack NF"));
        assert!(!is_nerd("Hack"));
        assert!(!is_nerd("Menlo-Regular 12"));
        assert_eq!(distro("Linux · Ubuntu 24.04.3 LTS").unwrap().name, "Ubuntu");
        assert_eq!(distro("Darwin · macOS 27.0").unwrap().name, "macOS");
        assert_eq!(
            distro("Linux · Debian GNU/Linux 12").unwrap().name,
            "Debian"
        );
        assert_eq!(distro("Linux · Arch Linux").unwrap().name, "Arch");
        assert_eq!(distro("Linux").unwrap().name, "Linux");
        assert!(distro("").is_none());
    }
}

/// What this terminal can show, as lines: its font, the icons mode, and
/// numbered samples of each kind of logo (`aleph icons`, the Doctor view).
pub fn preview_lines(setting: &str) -> Vec<Line<'static>> {
    let (nerd, font) = terminal_font();
    let tp = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "?".into());
    let kv = |k: &str, v: Vec<Span<'static>>| {
        let mut l = vec![Span::styled(format!("  {k:<10}"), ui::faint())];
        l.extend(v);
        Line::from(l)
    };
    let mut out = vec![
        kv(
            "terminal",
            vec![Span::raw(format!(
                "{tp}  ·  TERM={}  ·  COLORTERM={}",
                std::env::var("TERM").unwrap_or_default(),
                std::env::var("COLORTERM").unwrap_or_else(|_| "—".into())
            ))],
        ),
        kv(
            "font",
            vec![
                Span::raw(font.unwrap_or_else(|| "couldn't tell".into())),
                match nerd {
                    Some(true) => Span::styled("  ·  has logo glyphs", ui::fg(c::GREEN())),
                    Some(false) => Span::styled("  ·  no logo glyphs", ui::fg(c::YELLOW())),
                    None => Span::raw(""),
                },
            ],
        ),
        kv(
            "icons",
            vec![Span::raw(format!(
                "{setting} → {}",
                match style() {
                    Style_::Nerd => "logos (Nerd Font)",
                    Style_::Emoji => "brand chips",
                    Style_::Text => "names",
                }
            ))],
        ),
        Line::raw(""),
    ];
    let oses = [
        "Linux · Ubuntu 24.04",
        "Darwin · macOS 15",
        "Linux · Debian GNU/Linux 12",
        "Linux · Fedora 41",
        "Linux · Arch Linux",
        "Linux · NixOS",
    ];
    let mut l = vec![Span::styled("  1  logo glyphs   ", ui::faint())];
    for o in oses {
        if let Some(d) = distro(o) {
            l.push(Span::styled(format!("{}   ", d.nerd), ui::fg(d.color)));
        }
    }
    l.push(Span::styled(
        "← logos here? your font has them",
        ui::faint(),
    ));
    out.push(Line::from(l));
    let saved = style();
    for (n, st, label) in [
        (2, Style_::Emoji, "chips          "),
        (3, Style_::Nerd, "logo chips     "),
    ] {
        STYLE.store(st as u8, Ordering::Relaxed);
        let mut l = vec![Span::styled(format!("  {n}  {label}"), ui::faint())];
        for o in oses {
            l.extend(os_badge(o, 0));
            l.push(Span::raw(" "));
        }
        out.push(Line::from(l));
    }
    STYLE.store(saved as u8, Ordering::Relaxed);
    if style() != Style_::Nerd {
        out.push(Line::raw(""));
        out.push(Line::from(vec![
            Span::styled("  hint: ", ui::fg(c::BLUE())),
            Span::styled(
                "row 1 shows logos? then turn them on: aleph settings set icons nerd".to_string(),
                ui::muted(),
            ),
        ]));
    }
    if nerd != Some(true) {
        out.push(Line::raw(""));
        for t in [
            "for real logos, set the terminal's font to a Nerd Font (e.g. JetBrainsMono Nerd Font):",
            "  Warp: Settings › Appearance › Text   ·   Terminal.app: Settings › Profiles › Text › Font",
            "  iTerm2: Settings › Profiles › Text › Font   ·   then restart — icons=auto notices",
        ] {
            out.push(Line::from(vec![
                Span::styled("  hint: ", ui::fg(c::BLUE())),
                Span::styled(t.to_string(), ui::muted()),
            ]));
        }
        out.push(Line::from(vec![
            Span::styled("  hint: ", ui::fg(c::BLUE())),
            Span::styled(
                if nerd_font_installed() {
                    "a Nerd Font is already installed on this computer".to_string()
                } else {
                    "get one: brew install --cask font-jetbrains-mono-nerd-font".to_string()
                },
                ui::muted(),
            ),
        ]));
    }
    out
}

/// How logos come out here: the tier `ui.logos` picks and why, their size and
/// colours (`aleph icons`, the Doctor view).
pub fn logo_lines(cfg: &crate::config::Config) -> Vec<Line<'static>> {
    use crate::logo;
    let (cols, rows) = logo::size(cfg);
    let d = logo::decide(cfg, rows);
    let p = logo::Paint::here();
    let kv = |k: &str, v: String, note: String| {
        Line::from(vec![
            Span::styled(format!("  {k:<10}"), ui::faint()),
            Span::raw(v),
            Span::styled(
                if note.is_empty() {
                    note
                } else {
                    format!("  ·  {note}")
                },
                ui::faint(),
            ),
        ])
    };
    vec![
        kv(
            "logos",
            format!("{} → {}", cfg.get_or("ui.logos"), d.tier.as_str()),
            d.why,
        ),
        kv("images", logo::images::describe(), String::new()),
        kv(
            "size",
            format!("{} → {cols}×{rows} cells", cfg.get_or("ui.logo_size")),
            String::new(),
        ),
        kv(
            "colour",
            if p.mono {
                "none (NO_COLOR): shapes only".into()
            } else if p.truecolor {
                "truecolor".into()
            } else {
                "the 256 colours".into()
            },
            if p.mono || p.truecolor {
                String::new()
            } else {
                "COLORTERM isn't truecolor".into()
            },
        ),
    ]
}

/// `aleph icons [os|agent|gpu|server|<id>]`: what this terminal can show —
/// fonts, glyphs, and which logo tier `ui.logos` picks and why — then every
/// logo (or those asked for) in every tier.
pub fn cmd(cfg: &crate::config::Config) -> std::process::ExitCode {
    use crate::logo::images;
    use std::io::IsTerminal;
    // this command writes the pictures it shows itself, where they'd show
    let images = std::io::stdout().is_terminal() && images::support().is_ok();
    images::set_output(images);
    crate::out::head("This terminal");
    for l in preview_lines(&cfg.get_or("icons")) {
        crate::out::out(l);
    }
    for l in logo_lines(cfg) {
        crate::out::out(l);
    }
    // main hands this command the config alone: the filter is the word after `icons`
    let only = std::env::args()
        .skip_while(|a| a != "icons")
        .nth(1)
        .filter(|a| !a.starts_with('-'));
    let shown = gallery(cfg, only.as_deref(), images);
    if shown == 0 {
        crate::out::err(&format!(
            "no logo is called {}: try os, agent, gpu, server or an id",
            only.unwrap_or_default()
        ));
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

/// Every logo (of a kind, or one) in every tier at the configured size, in
/// rows as wide as the terminal (`images` too: we write them); how many
/// logos that is.
fn gallery(cfg: &crate::config::Config, only: Option<&str>, images: bool) -> usize {
    use crate::logo::{self, images, iterm, Kind, Tier};
    use std::io::Write;
    // a kind, or anything a lookup understands: an id, an OS line, a GPU model, an alias
    let logos: Vec<&logo::Logo> = match only.map(str::to_lowercase).as_deref() {
        None => logo::all().iter().collect(),
        Some(o) => match o {
            "os" | "system" | "systems" => Some(Kind::Os),
            "agent" | "agents" => Some(Kind::Agent),
            "gpu" | "gpus" => Some(Kind::Gpu),
            "server" | "servers" => Some(Kind::Server),
            _ => None,
        }
        .map_or_else(
            || {
                Kind::ALL
                    .iter()
                    .filter_map(|&k| logo::lookup(k, o))
                    .collect()
            },
            |k| logo::all().iter().filter(|l| l.kind == k).collect(),
        ),
    };
    if logos.is_empty() {
        return 0;
    }
    let (cols, rows) = logo::size(cfg);
    let paint = logo::Paint::here();
    let width = ui::print_width();
    // (tier, half blocks, label)
    let mut tiers = vec![
        (
            Tier::Thumbnails,
            false,
            "thumbnails · quadrant blocks, two colours a cell".to_string(),
        ),
        (
            Tier::Thumbnails,
            true,
            "thumbnails · half blocks (the Linux console's)".to_string(),
        ),
        (Tier::Ascii, false, "ascii · a character ramp".to_string()),
        (Tier::Glyphs, false, "glyphs · one cell".to_string()),
        (Tier::Text, false, "text · chips".to_string()),
    ];
    // iTerm2's pictures are written in place over their rows, not drawn as cells
    let inline = images && images::proto() == images::Proto::Iterm;
    if images {
        let label = format!("images · {}", images::proto().label());
        tiers.insert(0, (Tier::Images, false, label));
    }
    for (tier, halves, label) in tiers {
        crate::out::say(Line::raw(""));
        crate::out::head(&format!("Logos · {label}"));
        let small = matches!(tier, Tier::Glyphs | Tier::Text);
        let (c, r) = if small {
            (cols.max(12), 1)
        } else {
            (cols, rows)
        };
        let per = (width.saturating_sub(2) / (c + 2)).max(1);
        for kind in Kind::ALL {
            let group: Vec<&&logo::Logo> = logos.iter().filter(|l| l.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            let title = match kind {
                Kind::Os => "systems",
                Kind::Agent => "agents",
                Kind::Gpu => "GPUs",
                Kind::Server => "LLM servers and gateways",
            };
            crate::out::out(Line::styled(format!("  {title}"), ui::faint()));
            for chunk in group.chunks(per) {
                let pics: Vec<Vec<Line<'static>>> = chunk
                    .iter()
                    .map(|l| match (tier, halves) {
                        (Tier::Thumbnails, true) => logo::thumbnail(l, c, r, &paint, true),
                        _ => logo::render(l, tier, c, r, &paint),
                    })
                    .collect();
                let written = tier == Tier::Images && inline;
                if written {
                    let at: Vec<(&logo::Logo, usize)> = chunk
                        .iter()
                        .enumerate()
                        .map(|(i, l)| (**l, 2 + i * (c + 2)))
                        .collect();
                    let mut out = std::io::stdout();
                    let _ = out.write_all(iterm::printed(&at, c, r, &paint).as_bytes());
                    let _ = out.flush();
                } else if images {
                    let mut out = std::io::stdout();
                    let _ = out.write_all(images::take().as_bytes());
                    let _ = out.flush();
                }
                for row in (0..r).filter(|_| !written) {
                    let mut spans = vec![Span::raw("  ")];
                    for p in &pics {
                        spans.extend(p[row].spans.iter().cloned());
                        spans.push(Span::raw("  "));
                    }
                    crate::out::out(Line::from(spans));
                }
                if tier != Tier::Text {
                    let names: String = chunk
                        .iter()
                        .map(|l| ui::pad(&ui::trunc(l.id, c + 1), c + 2))
                        .collect();
                    crate::out::out(Line::styled(format!("  {}", names.trim_end()), ui::faint()));
                }
            }
        }
    }
    logos.len()
}
