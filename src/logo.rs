//! Pictures in text cells: Aleph's mark, and the logos of operating systems,
//! agents, GPU vendors and LLM servers.
//!
//! * The registry: [`Logo`]s rendered from assets/logos by `tools/logogen` and
//!   committed (src/logo_data.rs: 16×16 and 32×32 palette bitmaps, a 128 px
//!   PNG), so building Aleph never needs an SVG renderer. Lookups take what
//!   the app has in hand: [`os`] an OS line (`Linux · Ubuntu 24.04.3 LTS`),
//!   [`agent`] an agent id or alias, [`gpu`] a vendor or model, [`service`] a
//!   server id. [`os_info`] is the one table of operating systems (name, Nerd
//!   Font glyph, chip colour, family) the chips and badges share.
//! * Drawing: [`lines`] puts a logo in a box of cells, in the tier the
//!   setting `ui.logos` and the terminal call for ([`decide`]):
//!   - images: the picture itself, through kitty's graphics protocol (kitty,
//!     Ghostty; see [`kitty`]) or iTerm2's inline images (iTerm2, WezTerm;
//!     see [`iterm`]), whichever the terminal speaks ([`images`]);
//!   - thumbnails: quadrant blocks (`▘▝▀▖▌▞▛▗▚▐▜▄▙▟█`), each cell the best two
//!     colours of its 2×2 sub-pixels; half blocks (`▀▄`) on the Linux console;
//!   - ascii: a character ramp, fastfetch-style;
//!   - glyphs: one cell ([`glyph`]): Nerd Font glyphs for systems and GPUs,
//!     the brand glyphs for agents and servers;
//!   - text: a chip with the name.
//!
//!   Colours fit the theme ([`Paint`]): black, white and grey marks take its
//!   text colour, a colour too close to its background is made lighter or
//!   darker, transparent pixels stay the terminal's own background, and
//!   without truecolor each is the nearest of the 256 ([`q256`]). NO_COLOR
//!   draws the shapes alone.
//! * A cell is half as wide as it is tall, so a square picture is twice as
//!   many columns as rows (a quadrant cell's 2×2 sub-pixels are 1:2 too).
//!   [`mark`] draws Aleph's ℵ that way.

pub mod images;
pub mod iterm;
pub mod kitty;

use crate::config::Config;
use crate::ui;
use ratatui::prelude::*;

// ------------------------------------------------------------------ registry

/// What a logo stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Os,
    Agent,
    Gpu,
    Server,
}

impl Kind {
    pub const ALL: [Kind; 4] = [Kind::Os, Kind::Agent, Kind::Gpu, Kind::Server];

    /// Stable name (the folder under assets/logos).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Os => "os",
            Kind::Agent => "agent",
            Kind::Gpu => "gpu",
            Kind::Server => "server",
        }
    }
}

/// One logo: its brand colour and pictures (see src/logo_data.rs for the
/// encoding).
#[derive(Debug)]
pub struct Logo {
    pub id: &'static str,
    pub kind: Kind,
    /// as its owner (or Simple Icons) gives it; drawing fits it to the theme
    pub brand: Color,
    /// bitmap index k is `palette[k - 1]`; 0 is transparent
    palette: &'static [Color],
    x16: &'static str,
    x32: &'static str,
    /// 128 px, indexed: `256 / palette.len()` alpha steps per colour
    png: &'static [u8],
}

#[path = "logo_data.rs"]
mod data;

/// A logo's pixels: palette indices, row-major, `size`×`size`.
#[derive(Clone, Debug, PartialEq)]
pub struct Bitmap {
    pub size: usize,
    pub px: Vec<u8>,
}

impl Bitmap {
    pub fn at(&self, x: usize, y: usize) -> u8 {
        self.px[y * self.size + x]
    }
}

impl Logo {
    /// The bitmap at 16 or 32 px (anything above 16 gets the 32).
    pub fn bitmap(&self, size: usize) -> Bitmap {
        let (code, n) = if size <= 16 {
            (self.x16, 16)
        } else {
            (self.x32, 32)
        };
        Bitmap {
            size: n,
            px: unrle(code, self.palette.len(), n * n),
        }
    }

    /// What people call it: `Ubuntu`, `Claude Code`, `NVIDIA`, `Ollama`.
    pub fn name(&self) -> &'static str {
        match self.kind {
            Kind::Os => OSES
                .iter()
                .find(|r| r.info.logo == self.id)
                .map_or(self.id, |r| r.info.name),
            Kind::Agent => crate::agents::find(self.id).map_or(self.id, |a| a.name),
            Kind::Server => crate::services::find(self.id).map_or(self.id, |s| s.name),
            Kind::Gpu => match self.id {
                "nvidia" => "NVIDIA",
                "amd" => "AMD",
                "intel" => "Intel",
                "apple" => "Apple",
                id => id,
            },
        }
    }
}

/// Decode a run-length code: one `RUNS` character per run, `index · span +
/// (length − 1)`, `span = 64 / (colours + 1)`. Always `len` pixels long.
fn unrle(code: &str, colours: usize, len: usize) -> Vec<u8> {
    let span = 64 / (colours + 1);
    let mut v = Vec::with_capacity(len);
    for b in code.bytes() {
        let Some(k) = data::RUNS.iter().position(|r| *r == b) else {
            continue;
        };
        let n = (k % span + 1).min(len.saturating_sub(v.len()));
        v.extend(std::iter::repeat_n((k / span) as u8, n));
    }
    v.resize(len, 0);
    v
}

/// Every logo, in the registry's order (systems, GPUs, agents, servers).
pub fn all() -> &'static [Logo] {
    data::ALL
}

/// A logo by kind and exact id.
pub fn find(kind: Kind, id: &str) -> Option<&'static Logo> {
    data::ALL.iter().find(|l| l.kind == kind && l.id == id)
}

/// The logo for what the app has in hand: an OS line, an agent id or alias, a
/// GPU vendor or model, a server id or name. An exact logo id works for every
/// kind.
pub fn lookup(kind: Kind, what: &str) -> Option<&'static Logo> {
    find(kind, what.trim()).or_else(|| match kind {
        Kind::Os => os(what),
        Kind::Agent => agent(what),
        Kind::Gpu => gpu(what),
        Kind::Server => service(what),
    })
}

/// The logo of the system an OS line names: `macOS 27.0`, `Darwin`, `Ubuntu
/// 24.04.3 LTS`, `Linux · Fedora 40`, `Microsoft Windows 11`.
pub fn os(name: &str) -> Option<&'static Logo> {
    os_info(name).and_then(|i| find(Kind::Os, i.logo))
}

/// An agent's logo by id, kind, executable or alias (`cc`, `cursor-agent`).
pub fn agent(id: &str) -> Option<&'static Logo> {
    let id = crate::agents::find(id).map_or(id.trim(), |a| a.id);
    // Cursor's own
    let id = if id == crate::agentsdk::ID {
        "cursor"
    } else {
        id
    };
    find(Kind::Agent, id)
}

/// A GPU vendor's logo from a vendor or model (`nvidia`, `AMD Radeon RX 7900`,
/// `Intel(R) Arc(TM) A770`, `Apple M4`).
pub fn gpu(vendor: &str) -> Option<&'static Logo> {
    find(Kind::Gpu, gpu_vendor(vendor)?)
}

/// An LLM server's or gateway's logo by its `services` id or name.
pub fn service(id: &str) -> Option<&'static Logo> {
    let id = crate::services::find(id).map_or(id.trim(), |s| s.id);
    find(Kind::Server, id)
}

/// `nvidia` | `amd` | `intel` | `apple` for a GPU vendor or model name.
pub fn gpu_vendor(s: &str) -> Option<&'static str> {
    let l = s.to_lowercase();
    let any = |keys: &[&str]| keys.iter().any(|k| has_key(&l, k));
    // Intel first: its names never mention the others, and "Arc" alone is Intel's
    if any(&["intel", "arc", "iris", "uhd graphics", "xe graphics"]) {
        Some("intel")
    } else if any(&["amd", "radeon", "instinct", "firepro"]) {
        Some("amd")
    } else if any(&[
        "nvidia", "geforce", "quadro", "tesla", "rtx", "gtx", "titan", "a100", "h100", "h200",
        "b200", "l40*", "dgx",
    ]) {
        Some("nvidia")
    } else if any(&["apple", "m1", "m2", "m3", "m4", "m5"]) {
        Some("apple")
    } else {
        None
    }
}

/// Does lowercase `hay` contain `key` as a word? A key ending in `*` only has
/// to start one (`amzn*` matches `amzn2023`); a word is a run of letters and
/// digits, so `arch` isn't found in `aarch64`, nor `mac` in `machine`.
fn has_key(hay: &str, key: &str) -> bool {
    let (key, prefix) = match key.strip_suffix('*') {
        Some(k) => (k, true),
        None => (key, false),
    };
    hay.match_indices(key).any(|(i, _)| {
        let before = hay[..i].chars().next_back();
        let after = hay[i + key.len()..].chars().next();
        !before.is_some_and(char::is_alphanumeric)
            && (prefix || !after.is_some_and(char::is_alphanumeric))
    })
}

// ------------------------------------------------------------------ systems

/// Which family an operating system belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Os {
    Linux,
    Mac,
    Windows,
    Bsd,
    Unknown,
}

impl Os {
    /// The family as an emoji (a fallback where nothing better shows).
    pub fn emoji(self) -> &'static str {
        match self {
            Os::Linux => "🐧",
            Os::Mac => "🍎",
            Os::Windows => "🪟",
            Os::Bsd => "😈",
            Os::Unknown => "💻",
        }
    }
}

/// An operating system as people know it, for chips and badges.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct OsInfo {
    pub name: &'static str,
    /// its logo's id (`Kind::Os`); empty when there's none
    pub logo: &'static str,
    /// its Nerd Font glyph (nf-linux-*, nf-fa-apple, …)
    pub nerd: &'static str,
    /// a chip's background
    pub color: Color,
    pub family: Os,
}

struct OsRow {
    /// lowercase words that name it (see [`has_key`]), most specific rows first
    keys: &'static [&'static str],
    info: OsInfo,
}

const fn row(
    keys: &'static [&'static str],
    name: &'static str,
    logo: &'static str,
    nerd: &'static str,
    rgb: u32,
    family: Os,
) -> OsRow {
    OsRow {
        keys,
        info: OsInfo {
            name,
            logo,
            nerd,
            color: Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8),
            family,
        },
    }
}

/// The one table of operating systems: (words that name it, name, logo, Nerd
/// glyph, chip colour, family). Distributions beat the kernel's name (`Linux ·
/// Ubuntu …`), and the ones built on another come first (Pop!_OS, Mint and
/// elementary are Ubuntus; Raspberry Pi OS and Kali are Debians).
#[rustfmt::skip]
const OSES: &[OsRow] = {
    use Os::{Bsd as B, Linux as L, Mac as M, Windows as W};
    &[
        row(&["pop!_os", "pop_os", "popos"], "Pop!_OS", "popos", "\u{f32a}", 0x48b9c7, L),
        row(&["linux mint", "linuxmint", "lmde"], "Mint", "linuxmint", "\u{f30e}", 0x87cf3e, L),
        row(&["elementary"], "elementary", "elementary", "\u{f309}", 0x64baff, L),
        row(&["ubuntu", "kubuntu", "xubuntu", "lubuntu"], "Ubuntu", "ubuntu", "\u{f31b}", 0xe95420, L),
        row(&["raspbian", "raspberry pi"], "Raspberry Pi", "raspberrypi", "\u{f315}", 0xc51a4a, L),
        row(&["kali"], "Kali", "kalilinux", "\u{f327}", 0x557c94, L),
        row(&["debian"], "Debian", "debian", "\u{f306}", 0xd70a53, L),
        row(&["fedora"], "Fedora", "fedora", "\u{f30a}", 0x51a2da, L),
        row(&["centos"], "CentOS", "centos", "\u{f304}", 0x932279, L),
        row(&["rocky"], "Rocky", "rockylinux", "\u{f32b}", 0x10b981, L),
        row(&["almalinux", "alma"], "AlmaLinux", "almalinux", "\u{f31d}", 0x3b82f6, L),
        row(&["oracle"], "Oracle Linux", "oraclelinux", "\u{f31a}", 0xf80000, L),
        row(&["red hat", "redhat", "rhel*"], "RHEL", "redhat", "\u{f316}", 0xee0000, L),
        row(&["amazon linux", "amzn*"], "Amazon Linux", "amazonlinux", "\u{f270}", 0xff9900, L),
        row(&["manjaro"], "Manjaro", "manjaro", "\u{f312}", 0x35bf5c, L),
        row(&["endeavouros", "endeavour"], "EndeavourOS", "endeavouros", "\u{f322}", 0x7f3fbf, L),
        row(&["arch", "archlinux"], "Arch", "archlinux", "\u{f303}", 0x1793d1, L),
        row(&["opensuse*"], "openSUSE", "opensuse", "\u{f314}", 0x73ba25, L),
        row(&["suse", "sles*", "sled"], "SUSE", "suse", "\u{f314}", 0x30ba78, L),
        row(&["nixos"], "NixOS", "nixos", "\u{f313}", 0x7ebae4, L),
        row(&["alpine"], "Alpine", "alpinelinux", "\u{f300}", 0x0d597f, L),
        row(&["gentoo"], "Gentoo", "gentoo", "\u{f30d}", 0x54487a, L),
        row(&["void"], "Void", "voidlinux", "\u{f32e}", 0x478061, L),
        row(&["freebsd"], "FreeBSD", "freebsd", "\u{f30c}", 0xab2b28, B),
        row(&["openbsd"], "OpenBSD", "openbsd", "\u{f328}", 0xf2ca30, B),
        row(&["netbsd", "dragonfly", "bsd"], "BSD", "", "\u{f30c}", 0xab2b28, B),
        row(&["macos", "mac os", "mac", "os x", "osx", "darwin"], "macOS", "apple", "\u{f179}", 0xa2aaad, M),
        // WSL is a Linux, shown as the Windows it runs in
        row(&["wsl*"], "WSL", "windows", "\u{f17a}", 0x0078d4, L),
        row(&["windows", "win32", "win64", "mingw*", "msys*", "cygwin*"], "Windows", "windows", "\u{f17a}", 0x0078d4, W),
        row(&["linux"], "Linux", "linux", "\u{f31a}", 0xfcc624, L),
    ]
};

/// The system an OS line (or a `platform`, or both joined) names.
pub fn os_info(text: &str) -> Option<&'static OsInfo> {
    let l = text.to_lowercase();
    OSES.iter()
        .find(|r| r.keys.iter().any(|k| has_key(&l, k)))
        .map(|r| &r.info)
}

/// [`os_info`], or the system whose logo id `what` is (`apple`, `popos`).
fn os_row(what: &str) -> Option<&'static OsInfo> {
    let w = what.trim();
    OSES.iter()
        .find(|r| !w.is_empty() && r.info.logo == w)
        .map(|r| &r.info)
        .or_else(|| os_info(w))
}

// ------------------------------------------------------------------ colour

/// How colours come out where a logo is drawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Paint {
    /// NO_COLOR: the shapes alone, in the terminal's own colour
    pub mono: bool,
    /// 24-bit colour, else the nearest of the 256
    pub truecolor: bool,
    /// the background marks are fitted against (the theme's; never painted)
    pub base: (u8, u8, u8),
    /// ink for black, white and grey marks
    pub text: (u8, u8, u8),
}

impl Paint {
    /// This terminal with the current theme.
    pub fn here() -> Paint {
        let p = ui::c::pal();
        let rgb = |c: Color, or: (u8, u8, u8)| match c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => or,
        };
        Paint {
            mono: p.mono,
            truecolor: truecolor(),
            base: rgb(p.base, (0x11, 0x11, 0x11)),
            text: rgb(p.text, (0xee, 0xee, 0xee)),
        }
    }

    /// A brand colour as it's drawn here ([`fit`], then [`q256`] without
    /// truecolor); None under NO_COLOR.
    pub fn ink(&self, c: Color) -> Option<Color> {
        if self.mono {
            return None;
        }
        let Color::Rgb(r, g, b) = c else {
            return Some(c);
        };
        let (r, g, b) = fit((r, g, b), self.base, self.text);
        Some(if self.truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Indexed(q256(r, g, b))
        })
    }

    fn fg(&self, c: Color) -> Style {
        self.ink(c).map_or(Style::new(), |c| Style::new().fg(c))
    }
}

/// Does this terminal take 24-bit colour (`COLORTERM=truecolor` or `24bit`)?
pub fn truecolor() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("COLORTERM").is_ok_and(|v| {
            let v = v.to_lowercase();
            v.contains("truecolor") || v.contains("24bit")
        })
    })
}

/// The nearest colour of the xterm 256-colour palette: its 6×6×6 cube
/// (16–231) or its 24 greys (232–255).
pub fn q256(r: u8, g: u8, b: u8) -> u8 {
    const STEP: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| {
        (0..6)
            .min_by_key(|&i| (STEP[i] as i32 - v as i32).abs())
            .unwrap_or(0)
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let d = |x: u8, y: u8, z: u8| {
        let s = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        s(x, r) + s(y, g) + s(z, b)
    };
    let cube = d(STEP[ri], STEP[gi], STEP[bi]);
    // greys 8, 18, …, 238
    let avg = (r as i32 + g as i32 + b as i32) / 3;
    let k = ((avg - 8 + 5) / 10).clamp(0, 23);
    let v = (8 + 10 * k) as u8;
    if d(v, v, v) < cube {
        232 + k as u8
    } else {
        16 + 36 * ri as u8 + 6 * gi as u8 + bi as u8
    }
}

/// Black, white and greys: a mark's "ink", not a colour.
fn greyish((r, g, b): (u8, u8, u8)) -> bool {
    r.max(g).max(b) - r.min(g).min(b) <= 24
}

fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
}

/// WCAG contrast ratio (1–21).
pub fn contrast(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let (x, y) = (luminance(a) + 0.05, luminance(b) + 0.05);
    x.max(y) / x.min(y)
}

fn to_hsl((r, g, b): (u8, u8, u8)) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    if mx == mn {
        return (0.0, 0.0, l);
    }
    let d = mx - mn;
    let s = if l > 0.5 {
        d / (2.0 - mx - mn)
    } else {
        d / (mx + mn)
    };
    let h = if mx == r {
        (g - b) / d + if g < b { 6.0 } else { 0.0 }
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    (h / 6.0, s, l)
}

fn from_hsl(h: f64, s: f64, l: f64) -> (u8, u8, u8) {
    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;
    let f = |t: f64| {
        let t = t.rem_euclid(1.0);
        let v = if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0))
}

/// The colour a mark is drawn in on `base`: black, white and grey marks
/// (Apple, Cursor, Ollama …) take the theme's `text` colour; a colour too
/// close to the background (3:1, WCAG's bar for graphics; 4.5:1, its bar for
/// text, on a light background, where marks sit beside words) is made lighter
/// or darker, same hue, just enough; anything else keeps its brand colour.
/// tools/logogen makes the `-dark` / `-light` PNGs by the same rule.
pub fn fit(c: (u8, u8, u8), base: (u8, u8, u8), text: (u8, u8, u8)) -> (u8, u8, u8) {
    if greyish(c) {
        return text;
    }
    let bar = if light(base) { 4.5 } else { 3.0 };
    fit_to(c, base, bar)
}

/// Is `base` a light background: does black stand out on it more than white?
/// (A mid-tone paper, 0.45 luminance, is light: black 10:1, white 2:1.)
pub fn light(base: (u8, u8, u8)) -> bool {
    contrast((0, 0, 0), base) > contrast((255, 255, 255), base)
}

/// `c` made lighter or darker (same hue and saturation) just enough to stand
/// `ratio`:1 against `base`; as it is when it already does.
pub fn fit_to(c: (u8, u8, u8), base: (u8, u8, u8), ratio: f64) -> (u8, u8, u8) {
    if contrast(c, base) >= ratio {
        return c;
    }
    let (h, s, l) = to_hsl(c);
    // toward white on a dark ground, toward black on a light one
    let up = !light(base);
    // out of reach even at white or black: as far as it goes
    let end = from_hsl(h, s, if up { 1.0 } else { 0.0 });
    if contrast(end, base) < ratio {
        return end;
    }
    let (mut lo, mut hi) = if up { (l, 1.0) } else { (0.0, l) };
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        let ok = contrast(from_hsl(h, s, mid), base) >= ratio;
        // keep the lightness that just passes: as close to the brand as can be
        if up == ok {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    from_hsl(h, s, if up { hi } else { lo })
}

// ------------------------------------------------------------------ drawing

/// How logos are drawn (setting `ui.logos`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    Images,
    Thumbnails,
    Ascii,
    Glyphs,
    Text,
}

impl Tier {
    pub const ALL: [Tier; 5] = [
        Tier::Images,
        Tier::Thumbnails,
        Tier::Ascii,
        Tier::Glyphs,
        Tier::Text,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Images => "images",
            Tier::Thumbnails => "thumbnails",
            Tier::Ascii => "ascii",
            Tier::Glyphs => "glyphs",
            Tier::Text => "text",
        }
    }

    pub fn parse(s: &str) -> Option<Tier> {
        Tier::ALL.into_iter().find(|t| t.as_str() == s)
    }
}

/// A logo's box in cells (columns, rows) for `ui.logo_size`.
pub fn size(cfg: &Config) -> (usize, usize) {
    size_of(&cfg.get_or("ui.logo_size"))
}

/// small 6×3, medium 8×4, large 16×8.
pub fn size_of(name: &str) -> (usize, usize) {
    match name {
        "small" => (6, 3),
        "large" => (16, 8),
        _ => (8, 4),
    }
}

/// What [`decide`] chose, and why (`aleph icons` says).
#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub tier: Tier,
    pub why: String,
}

/// The tier for a logo `rows` tall here: the setting's, or for `auto`
/// images where the terminal shows them, else thumbnails with 3 rows or
/// more, else Nerd Font glyphs when the font has them, else names.
pub fn decide(cfg: &Config, rows: usize) -> Decision {
    decide_for(&cfg.get_or("ui.logos"), rows)
}

/// [`decide`] for a `ui.logos` value that isn't saved yet (setup's preview).
pub fn decide_for(setting: &str, rows: usize) -> Decision {
    let nerd = crate::icons::style() == crate::icons::Style_::Nerd;
    choose(setting, rows, images::usable().map(|s| s.1), nerd)
}

/// [`decide`] without the globals: `images` says whether this terminal shows
/// images (and which it is) or why not.
fn choose(setting: &str, rows: usize, images: Result<&str, String>, nerd: bool) -> Decision {
    let decision = |tier, why: String| Decision { tier, why };
    if let Some(t) = Tier::parse(setting).filter(|t| *t != Tier::Images) {
        return decision(t, format!("set to {setting}"));
    }
    let lead = match images {
        Ok(term) if setting == "images" => {
            return decision(Tier::Images, format!("set to images · {term} shows them"))
        }
        Ok(term) => return decision(Tier::Images, format!("{term} shows images")),
        Err(e) if setting == "images" => format!("set to images, but {e}"),
        Err(e) => e,
    };
    // block thumbnails only when asked for: many terminals (Warp, Terminal)
    // draw block characters from the font, with gaps between the rows
    let _ = rows;
    let (tier, how) = if setting == "images" {
        (Tier::Thumbnails, "block thumbnails".to_string())
    } else if nerd {
        (Tier::Glyphs, "the font's Nerd Font glyphs".to_string())
    } else {
        (Tier::Text, "no Nerd Font: names".to_string())
    };
    decision(tier, format!("{lead}; {how}"))
}

/// Is a logo `rows` tall drawn as a picture here (an image, or thumbnails /
/// ASCII art the setting asked for)? Places that make room for a picture
/// (machine cards, the machine view) keep their compact layout otherwise.
pub fn pictures(cfg: &Config, rows: usize) -> bool {
    matches!(
        decide(cfg, rows).tier,
        Tier::Images | Tier::Thumbnails | Tier::Ascii
    )
}

/// A logo for `what` ([`lookup`]: an id, an OS line, a GPU model, …) in a box
/// of `cols`×`rows` cells, drawn as [`decide`] says: exactly `rows` lines of
/// `cols` columns, the picture centred, so it fills a `Rect` of that size.
/// Something without a logo gets its glyph or its name.
///
/// With images, the cells are placeholders (kitty: the image's id is their
/// foreground colour, don't restyle them) or markers (iTerm2): the host hands
/// each frame to [`images::frame`], writes [`images::take`] after it and
/// [`images::clear`] before handing the terminal over; see [`images`].
#[allow(dead_code)] // E4: the views place logos with this
pub fn lines(kind: Kind, what: &str, cols: usize, rows: usize, cfg: &Config) -> Vec<Line<'static>> {
    let paint = Paint::here();
    let tier = decide(cfg, rows).tier;
    match lookup(kind, what) {
        Some(l) => render(l, tier, cols, rows, &paint),
        None => centred(vec![glyph_with(kind, what, nerd(), &paint)], cols, rows),
    }
}

fn nerd() -> bool {
    crate::icons::style() == crate::icons::Style_::Nerd
}

/// A logo in `tier`, `cols`×`rows` cells, exactly (see [`lines`]).
pub fn render(
    logo: &Logo,
    tier: Tier,
    cols: usize,
    rows: usize,
    paint: &Paint,
) -> Vec<Line<'static>> {
    match tier {
        Tier::Images => images::lines(logo, cols, rows, paint),
        Tier::Thumbnails => thumbnail(logo, cols, rows, paint, half_blocks()),
        Tier::Ascii => ascii(logo, cols, rows, paint),
        Tier::Glyphs => centred(
            vec![glyph_with(logo.kind, logo.id, nerd(), paint)],
            cols,
            rows,
        ),
        Tier::Text => centred(vec![chip(logo, cols, paint)], cols, rows),
    }
}

/// The Linux console's font has half blocks but no quadrants.
pub fn half_blocks() -> bool {
    std::env::var("TERM").is_ok_and(|t| t == "linux")
}

/// One cell for a logo: its Nerd Font glyph (systems, GPUs; with a Nerd Font)
/// or brand glyph (agents, servers) in its colour, else its name.
#[allow(dead_code)] // E4: one-row places (lists, chips) take this
pub fn glyph(kind: Kind, what: &str) -> Span<'static> {
    glyph_with(kind, what, nerd(), &Paint::here())
}

fn glyph_with(kind: Kind, what: &str, nerd: bool, paint: &Paint) -> Span<'static> {
    let bold = |c: Color| paint.fg(c).add_modifier(Modifier::BOLD);
    let unknown = |s: &str| {
        let s = if s.trim().is_empty() { "?" } else { s.trim() };
        Span::styled(ui::trunc(s, 8), ui::faint())
    };
    match kind {
        Kind::Agent => match crate::agents::find(what) {
            Some(a) => {
                let (g, col) = crate::icons::brand(a.id);
                Span::styled(g, bold(col))
            }
            None => unknown(what),
        },
        Kind::Server => match crate::services::find(what) {
            Some(s) => {
                let (r, g, b) = s.rgb;
                Span::styled(s.glyph, bold(Color::Rgb(r, g, b)))
            }
            None => unknown(what),
        },
        Kind::Os => match os_row(what) {
            Some(i) if nerd => Span::styled(i.nerd, paint.fg(i.color)),
            Some(i) => Span::styled(i.name, bold(i.color)),
            None => unknown(what),
        },
        Kind::Gpu => match gpu(what) {
            Some(l) if nerd => Span::styled("\u{f0b2a}", paint.fg(l.brand)),
            Some(l) => Span::styled(l.name(), bold(l.brand)),
            None => unknown(what),
        },
    }
}

/// The logo's name on a chip of its colour (an OS's chip colour from the
/// table), at most `max` columns.
fn chip(logo: &Logo, max: usize, paint: &Paint) -> Span<'static> {
    let label = format!(" {} ", ui::trunc(logo.name(), max.saturating_sub(2).max(1)));
    let label = ui::trunc(&label, max);
    if paint.mono {
        return Span::styled(
            label,
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        );
    }
    let col = match logo.kind {
        Kind::Os => os_row(logo.id).map_or(logo.brand, |i| i.color),
        _ => logo.brand,
    };
    let Some(bg) = paint.ink(col) else {
        return Span::raw(label);
    };
    // readable text on the chip
    let shade = match col {
        Color::Rgb(r, g, b) if luminance(fit((r, g, b), paint.base, paint.text)) > 0.4 => {
            (20, 20, 24)
        }
        _ => (250, 250, 250),
    };
    let fg = if paint.truecolor {
        Color::Rgb(shade.0, shade.1, shade.2)
    } else {
        Color::Indexed(q256(shade.0, shade.1, shade.2))
    };
    Span::styled(
        label,
        Style::new().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
    )
}

/// `spans` centred in `cols`×`rows` (the middle row), every line `cols` wide;
/// cut to fit when they're wider.
fn centred(spans: Vec<Span<'static>>, cols: usize, rows: usize) -> Vec<Line<'static>> {
    let width = |v: &[Span]| v.iter().map(|s| ui::width(&s.content)).sum::<usize>();
    let spans = if width(&spans) > cols {
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        let style = spans.first().map(|s| s.style).unwrap_or_default();
        vec![Span::styled(ui::trunc(&text, cols), style)]
    } else {
        spans
    };
    let w = width(&spans);
    (0..rows)
        .map(|r| {
            if r != (rows - 1) / 2 || w > cols {
                return Line::raw(" ".repeat(cols));
            }
            let left = (cols - w) / 2;
            let mut v = vec![Span::raw(" ".repeat(left))];
            v.extend(spans.iter().cloned());
            v.push(Span::raw(" ".repeat(cols - w - left)));
            Line::from(v)
        })
        .collect()
}

/// Where the square picture sits in a box of `cols`×`rows` cells of
/// `sx`×`sy` sub-pixels: (width, height, x, y) in sub-pixels. A cell is
/// twice as tall as wide, so the side is `min(cols, 2·rows)` cell widths.
fn place(cols: usize, rows: usize, sx: usize, sy: usize) -> (usize, usize, usize, usize) {
    let side = cols.min(rows * 2);
    let (w, h) = (side * sx, side * sy / 2);
    (w, h, (cols * sx - w) / 2, (rows * sy - h) / 2)
}

/// How much of a rectangle of the bitmap is ink, and its main colour.
fn sample(bm: &Bitmap, x0: f32, y0: f32, x1: f32, y1: f32) -> (f32, u8) {
    let mut acc = [0f32; 6];
    let mut area = 0.0;
    let n = bm.size;
    for y in (y0.floor() as usize)..(y1.ceil() as usize).min(n) {
        let hy = (y1.min(y as f32 + 1.0) - y0.max(y as f32)).max(0.0);
        for x in (x0.floor() as usize)..(x1.ceil() as usize).min(n) {
            let a = (x1.min(x as f32 + 1.0) - x0.max(x as f32)).max(0.0) * hy;
            acc[(bm.at(x, y) as usize).min(5)] += a;
            area += a;
        }
    }
    if area <= 0.0 {
        return (0.0, 0);
    }
    let main = (1..acc.len())
        .rev()
        .max_by(|&a, &b| acc[a].total_cmp(&acc[b]))
        .unwrap_or(1) as u8;
    (1.0 - acc[0] / area, main)
}

/// The picture as a grid of palette indices, `gw`×`gh` sub-pixels for the
/// whole box (0 outside the picture), from the bitmap that suits its size.
/// Under NO_COLOR every ink is 1.
fn grid(logo: &Logo, cols: usize, rows: usize, sx: usize, sy: usize, mono: bool) -> Vec<u8> {
    let (w, h, x0, y0) = place(cols, rows, sx, sy);
    let (gw, gh) = (cols * sx, rows * sy);
    let mut g = vec![0u8; gw * gh];
    if w == 0 || h == 0 {
        return g;
    }
    let bm = logo.bitmap(if w <= 16 && h <= 16 { 16 } else { 32 });
    let (fx, fy) = (bm.size as f32 / w as f32, bm.size as f32 / h as f32);
    for py in 0..h {
        for px in 0..w {
            let (ink, main) = sample(
                &bm,
                px as f32 * fx,
                py as f32 * fy,
                (px + 1) as f32 * fx,
                (py + 1) as f32 * fy,
            );
            if ink >= 0.5 && y0 + py < gh && x0 + px < gw {
                g[(y0 + py) * gw + x0 + px] = if mono { 1 } else { main };
            }
        }
    }
    g
}

/// Quadrant glyph for sub-pixels (top-left, top-right, bottom-left, bottom-right).
const QUAD: [char; 16] = [
    ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
];

/// The two colours a quadrant cell shows of its four sub-pixels (palette
/// indices, 0 = transparent): (the first's sub-pixels as bits, first,
/// second). Where any sub-pixel is transparent the second is too — the
/// terminal's background is never painted over — and the first is the ink
/// closest to all the others; else the pair that strays least.
fn two_colours(v: [u8; 4], rgb: &[(u8, u8, u8)]) -> (usize, u8, u8) {
    let d = |a: u8, b: u8| -> u32 {
        if a == b {
            return 0;
        }
        let (x, y) = (rgb[a as usize - 1], rgb[b as usize - 1]);
        let s = |p: u8, q: u8| (p as i32 - q as i32).pow(2) as u32;
        s(x.0, y.0) + s(x.1, y.1) + s(x.2, y.2)
    };
    let mut inks: Vec<u8> = v.iter().copied().filter(|&x| x > 0).collect();
    inks.sort_unstable();
    inks.dedup();
    if inks.is_empty() {
        return (0, 0, 0);
    }
    let cost = |f: u8, g: u8| -> u32 {
        v.iter()
            .filter(|&&x| x > 0)
            .map(|&x| {
                if g == 0 {
                    d(x, f)
                } else {
                    d(x, f).min(d(x, g))
                }
            })
            .sum()
    };
    let (f, g) = if v.contains(&0) || inks.len() == 1 {
        let f = inks.iter().copied().min_by_key(|&f| cost(f, 0)).unwrap();
        (f, 0)
    } else {
        let mut best = (inks[0], inks[1]);
        for &f in &inks {
            for &g in &inks {
                if f != g && cost(f, g) < cost(best.0, best.1) {
                    best = (f, g);
                }
            }
        }
        best
    };
    let bits = (0..4)
        .filter(|&i| v[i] > 0 && (g == 0 || d(v[i], f) <= d(v[i], g)))
        .fold(0, |b, i| b | 1 << i);
    (bits, f, g)
}

/// Cells (text, style) as a line, runs of one style in one span.
fn line_of(cells: Vec<(String, Style)>) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (t, st) in cells {
        match spans.last_mut() {
            Some(last) if last.style == st => last.content.to_mut().push_str(&t),
            _ => spans.push(Span::styled(t, st)),
        }
    }
    Line::from(spans)
}

/// A logo in block characters: quadrants (2×2 sub-pixels, two colours a
/// cell) or, with `halves`, half blocks (1×2, square pixels; what [`render`]
/// uses on the Linux console).
pub fn thumbnail(
    logo: &Logo,
    cols: usize,
    rows: usize,
    paint: &Paint,
    halves: bool,
) -> Vec<Line<'static>> {
    let (sx, sy) = if halves { (1, 2) } else { (2, 2) };
    let g = grid(logo, cols, rows, sx, sy, paint.mono);
    let gw = cols * sx;
    let rgb: Vec<(u8, u8, u8)> = logo
        .palette
        .iter()
        .map(|c| match *c {
            Color::Rgb(r, g, b) => (r, g, b),
            _ => (0, 0, 0),
        })
        .collect();
    let inks: Vec<Option<Color>> = logo.palette.iter().map(|&c| paint.ink(c)).collect();
    let ink = |i: u8| inks.get(i as usize - 1).copied().flatten();
    let style = |fg: u8, bg: u8| {
        let mut st = Style::new();
        if let Some(c) = ink(fg) {
            st = st.fg(c);
        }
        if bg > 0 {
            if let Some(c) = ink(bg) {
                st = st.bg(c);
            }
        }
        st
    };
    (0..rows)
        .map(|r| {
            let cells = (0..cols)
                .map(|c| {
                    let at = |dx: usize, dy: usize| g[(r * sy + dy) * gw + c * sx + dx];
                    let (ch, fg, bg) = if halves {
                        match (at(0, 0), at(0, 1)) {
                            (0, 0) => (' ', 0, 0),
                            (t, 0) => ('▀', t, 0),
                            (0, b) => ('▄', b, 0),
                            (t, b) if t == b => ('█', t, 0),
                            (t, b) => ('▀', t, b),
                        }
                    } else {
                        let (bits, f, b) =
                            two_colours([at(0, 0), at(1, 0), at(0, 1), at(1, 1)], &rgb);
                        (QUAD[bits], f, b)
                    };
                    if ch == ' ' {
                        (" ".to_string(), Style::new())
                    } else {
                        (ch.to_string(), style(fg, bg))
                    }
                })
                .collect();
            line_of(cells)
        })
        .collect()
}

/// A logo as text, fastfetch-style: each cell the character of the ramp for
/// how much of it is ink, in its main colour.
fn ascii(logo: &Logo, cols: usize, rows: usize, paint: &Paint) -> Vec<Line<'static>> {
    const RAMP: &[u8] = b" .:-=+*#%@";
    let (w, h, x0, y0) = place(cols, rows, 1, 1);
    let bm = logo.bitmap(32);
    let inks: Vec<Option<Color>> = logo.palette.iter().map(|&c| paint.ink(c)).collect();
    let (fx, fy) = if w > 0 && h > 0 {
        (32.0 / w as f32, 32.0 / h as f32)
    } else {
        (0.0, 0.0)
    };
    (0..rows)
        .map(|r| {
            let cells = (0..cols)
                .map(|c| {
                    let inside = c >= x0 && c < x0 + w && r >= y0 && r < y0 + h;
                    if !inside {
                        return (" ".to_string(), Style::new());
                    }
                    let (px, py) = ((c - x0) as f32, (r - y0) as f32);
                    let (ink, main) =
                        sample(&bm, px * fx, py * fy, (px + 1.0) * fx, (py + 1.0) * fy);
                    let k = (ink * (RAMP.len() - 1) as f32).round() as usize;
                    let ch = RAMP[k.min(RAMP.len() - 1)] as char;
                    match inks
                        .get((main as usize).saturating_sub(1))
                        .copied()
                        .flatten()
                    {
                        Some(col) if ch != ' ' => (ch.to_string(), Style::new().fg(col)),
                        _ => (ch.to_string(), Style::new()),
                    }
                })
                .collect();
            line_of(cells)
        })
        .collect()
}

// ------------------------------------------------------------------ the ℵ mark

/// Aleph's mark: 64×64 alpha, row-major (`tools/logogen`).
const ALEPH_MASK: &[u8; 64 * 64] = include_bytes!("../assets/brand/aleph-mask-64.bin");

/// Coverage (0..=1) of the mask over a rectangle of it, by box averaging.
fn coverage(mask: &[u8], size: usize, x0: f32, y0: f32, x1: f32, y1: f32) -> f32 {
    let (ix0, iy0) = (x0.floor().max(0.0) as usize, y0.floor().max(0.0) as usize);
    let (ix1, iy1) = (
        (x1.ceil() as usize).min(size),
        (y1.ceil() as usize).min(size),
    );
    let (mut sum, mut area) = (0.0f32, 0.0f32);
    for y in iy0..iy1 {
        let hy = (y1.min(y as f32 + 1.0) - y0.max(y as f32)).max(0.0);
        for x in ix0..ix1 {
            let wx = (x1.min(x as f32 + 1.0) - x0.max(x as f32)).max(0.0);
            let a = wx * hy;
            sum += a * mask[y * size + x] as f32 / 255.0;
            area += a;
        }
    }
    if area > 0.0 {
        sum / area
    } else {
        0.0
    }
}

/// A square mask drawn into `cols`×`rows` cells (the picture is centred and
/// keeps its aspect: a cell's sub-pixel is half as wide as tall).
pub fn quads(mask: &[u8], size: usize, cols: usize, rows: usize) -> Vec<String> {
    if cols == 0 || rows == 0 {
        return vec![];
    }
    // the largest square (in physical terms) that fits: width in sub-pixels
    // is 2·cols, height 2·rows, and a sub-pixel is 1:2 (w:h)
    let side_rows = rows.min(cols / 2).max(1); // square: cols = 2·rows
    let (pw, ph) = (side_rows * 4, side_rows * 2); // sub-pixels of the square
    let (off_x, off_y) = (
        (cols * 2 - pw.min(cols * 2)) / 2,
        (rows * 2 - ph.min(rows * 2)) / 2,
    );
    let sx = size as f32 / pw as f32;
    let sy = size as f32 / ph as f32;
    let mut out = Vec::with_capacity(rows);
    for r in 0..rows {
        let mut line = String::with_capacity(cols * 3);
        for c in 0..cols {
            let mut bits = 0usize;
            for (i, (dx, dy)) in [(0, 0), (1, 0), (0, 1), (1, 1)].iter().enumerate() {
                let px = (c * 2 + dx) as isize - off_x as isize;
                let py = (r * 2 + dy) as isize - off_y as isize;
                if px < 0 || py < 0 || px as usize >= pw || py as usize >= ph {
                    continue;
                }
                let (x0, y0) = (px as f32 * sx, py as f32 * sy);
                if coverage(mask, size, x0, y0, x0 + sx, y0 + sy) >= 0.5 {
                    bits |= 1 << i;
                }
            }
            line.push(QUAD[bits]);
        }
        out.push(line.trim_end().to_string());
    }
    out
}

/// Aleph's mark in braille, four times as fine as quadrants (a cell's 2×4
/// dots are square): the cells it covers in a `cols`×`rows` box, each with
/// its dots (braille's bit order; `0xFF` = every one). Centred, square.
pub fn mark_dots(cols: usize, rows: usize) -> Vec<(usize, usize, u8)> {
    if cols == 0 || rows == 0 {
        return vec![];
    }
    const BITS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];
    // a square: as many dots across as down (a cell is 2 dots wide, 4 high)
    let side = (cols * 2).min(rows * 4);
    let (off_x, off_y) = ((cols * 2 - side) / 2, (rows * 4 - side) / 2);
    let s = 64.0 / side as f32;
    let mut out = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            let mut bits = 0u8;
            for (i, col) in BITS.iter().enumerate() {
                for (j, bit) in col.iter().enumerate() {
                    let px = (c * 2 + i) as isize - off_x as isize;
                    let py = (r * 4 + j) as isize - off_y as isize;
                    if px < 0 || py < 0 || px as usize >= side || py as usize >= side {
                        continue;
                    }
                    let (x0, y0) = (px as f32 * s, py as f32 * s);
                    if coverage(ALEPH_MASK, 64, x0, y0, x0 + s, y0 + s) >= 0.5 {
                        bits |= bit;
                    }
                }
            }
            if bits != 0 {
                out.push((c, r, bits));
            }
        }
    }
    out
}

/// Aleph's mark in `cols`×`rows` cells, as styled lines (right-padded so it
/// can sit beside text).
pub fn mark(cols: usize, rows: usize, color: Color) -> Vec<Line<'static>> {
    quads(ALEPH_MASK, 64, cols, rows)
        .into_iter()
        .map(|l| {
            let pad = cols.saturating_sub(l.chars().count());
            Line::from(vec![
                Span::styled(l, Style::new().fg(color)),
                Span::raw(" ".repeat(pad)),
            ])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_draws_and_keeps_its_shape() {
        let m = quads(ALEPH_MASK, 64, 16, 8);
        assert_eq!(m.len(), 8);
        let ink: usize = m
            .iter()
            .map(|l| l.chars().filter(|c| *c != ' ').count())
            .sum();
        // an aleph covers a good part of its box, but nowhere near all of it
        assert!(ink > 30 && ink < 110, "{ink}\n{}", m.join("\n"));
        // the foot: the bottom row has ink on the left and the right
        let last = &m[7];
        assert!(last.starts_with(|c: char| c != ' ') || last.trim_start().len() < last.len());
        assert!(quads(ALEPH_MASK, 64, 0, 4).is_empty());
    }

    #[test]
    fn a_full_mask_is_all_blocks_and_an_empty_one_is_blank() {
        let full = [255u8; 16];
        assert_eq!(quads(&full, 4, 4, 2), vec!["████", "████"]);
        let empty = [0u8; 16];
        assert_eq!(quads(&empty, 4, 4, 2), vec!["", ""]);
    }

    #[test]
    fn every_logo_decodes() {
        assert!(all().len() >= 79);
        for l in all() {
            assert!(!l.palette.is_empty() && l.palette.len() <= 4, "{}", l.id);
            for n in [16, 32] {
                let b = l.bitmap(n);
                assert_eq!((b.size, b.px.len()), (n, n * n), "{}", l.id);
                assert!(b.px.iter().all(|&v| v as usize <= l.palette.len()));
                // a logo has ink, and doesn't fill its whole box
                let ink = b.px.iter().filter(|&&v| v > 0).count();
                assert!(ink > n * n / 20 && ink < n * n, "{} {n}: {ink}", l.id);
            }
            assert!(l.png.starts_with(b"\x89PNG\r\n\x1a\n"), "{}", l.id);
            assert!(!l.name().is_empty());
        }
    }

    #[test]
    fn every_agent_and_server_has_a_logo() {
        for a in crate::agents::ALL {
            assert!(agent(a.id).is_some(), "agent {} has no logo", a.id);
        }
        for s in crate::services::ALL {
            assert!(service(s.id).is_some(), "server {} has no logo", s.id);
        }
        // and nothing in the registry is an agent or server the app doesn't know
        for l in all() {
            match l.kind {
                Kind::Agent => assert!(crate::agents::find(l.id).is_some(), "{}", l.id),
                Kind::Server => assert!(crate::services::find(l.id).is_some(), "{}", l.id),
                _ => {}
            }
        }
        assert_eq!(agent("cc").unwrap().id, "claude");
        assert_eq!(agent("cursor-agent").unwrap().id, "cursor");
        assert_eq!(service("Ollama").unwrap().id, "ollama");
        assert!(agent("nosuch").is_none());
    }

    #[test]
    fn os_lines_find_their_logo() {
        let id = |s: &str| os(s).map(|l| l.id);
        assert_eq!(id("macOS 27.0"), Some("apple"));
        assert_eq!(id("Darwin"), Some("apple"));
        assert_eq!(id("Ubuntu 24.04.3 LTS"), Some("ubuntu"));
        assert_eq!(id("Linux · Fedora 40"), Some("fedora"));
        assert_eq!(id("Linux · Ubuntu 24.04.3 LTS"), Some("ubuntu"));
        assert_eq!(id("Pop!_OS 22.04 LTS"), Some("popos"));
        assert_eq!(id("Linux Mint 21.3"), Some("linuxmint"));
        assert_eq!(id("Debian GNU/Linux 12 (bookworm)"), Some("debian"));
        assert_eq!(id("Raspbian GNU/Linux 11"), Some("raspberrypi"));
        assert_eq!(id("Red Hat Enterprise Linux 9.4 (Plow)"), Some("redhat"));
        assert_eq!(id("Rocky Linux 9.4 (Blue Onyx)"), Some("rockylinux"));
        assert_eq!(id("AlmaLinux 9.4"), Some("almalinux"));
        assert_eq!(id("Oracle Linux Server 9.4"), Some("oraclelinux"));
        assert_eq!(id("Amazon Linux 2023"), Some("amazonlinux"));
        assert_eq!(id("openSUSE Tumbleweed"), Some("opensuse"));
        assert_eq!(id("SUSE Linux Enterprise Server 15 SP5"), Some("suse"));
        assert_eq!(id("Arch Linux"), Some("archlinux"));
        assert_eq!(id("Kubuntu 24.04"), Some("ubuntu"));
        assert_eq!(id("elementary OS 7.1 Horus"), Some("elementary"));
        assert_eq!(id("FreeBSD 14.1-RELEASE"), Some("freebsd"));
        assert_eq!(id("Microsoft Windows 11 Pro"), Some("windows"));
        assert_eq!(id("MINGW64_NT-10.0-19045"), Some("windows"));
        assert_eq!(id("Some New Distro 1 linux"), Some("linux"));
        // words, not letters: aarch64 isn't Arch, a machine isn't a Mac
        assert_eq!(id("Linux aarch64"), Some("linux"));
        assert_eq!(id("my machine"), None);
        assert_eq!(id(""), None);
        assert_eq!(os_info("NetBSD 10").unwrap().family, Os::Bsd);
        assert_eq!(os_info("WSL2").unwrap().name, "WSL");
        assert_eq!(os("Linux · Ubuntu").unwrap().name(), "Ubuntu");
        assert_eq!(os("Darwin").unwrap().name(), "macOS");
    }

    #[test]
    fn gpu_vendors() {
        let v = |s: &str| gpu(s).map(|l| l.id);
        assert_eq!(v("nvidia"), Some("nvidia"));
        assert_eq!(v("NVIDIA GeForce RTX 4090"), Some("nvidia"));
        assert_eq!(v("H100 80GB HBM3"), Some("nvidia"));
        assert_eq!(v("amd"), Some("amd"));
        assert_eq!(v("AMD Instinct MI300X"), Some("amd"));
        assert_eq!(v("Radeon RX 7900 XTX"), Some("amd"));
        // Intel is Intel, never NVIDIA
        assert_eq!(v("Intel"), Some("intel"));
        assert_eq!(v("Intel(R) Arc(TM) A770 Graphics"), Some("intel"));
        assert_eq!(v("Intel UHD Graphics 770"), Some("intel"));
        assert_eq!(v("Apple M4 Max"), Some("apple"));
        assert_eq!(v("apple"), Some("apple"));
        assert_eq!(v("Qualcomm Adreno"), None);
        assert_eq!(v(""), None);
        assert_eq!(gpu("intel").unwrap().name(), "Intel");
    }

    fn dark() -> Paint {
        Paint {
            mono: false,
            truecolor: true,
            base: (0x11, 0x11, 0x11),
            text: (0xee, 0xee, 0xee),
        }
    }

    fn light() -> Paint {
        Paint {
            base: (0xfc, 0xfc, 0xfc),
            text: (0x20, 0x20, 0x20),
            ..dark()
        }
    }

    /// A logo that's all ink: red, or red with a blue right half.
    fn solid(two: bool) -> Logo {
        Logo {
            id: "solid",
            kind: Kind::Os,
            brand: Color::Rgb(255, 0, 0),
            palette: if two {
                &[Color::Rgb(255, 0, 0), Color::Rgb(0, 0, 255)]
            } else {
                &[Color::Rgb(255, 0, 0)]
            },
            // one colour: span 32, so `_` (63) is a run of 32 of index 1;
            // two: span 21, so `S` (28) is 8 of index 1, `n` (49) 8 of
            // index 2, `a` (36) 16 of index 1 and `v` (57) 16 of index 2
            x16: if two {
                "SnSnSnSnSnSnSnSnSnSnSnSnSnSnSnSn"
            } else {
                "________"
            },
            x32: if two {
                "avavavavavavavavavavavavavavavavavavavavavavavavavavavavavavavav"
            } else {
                "________________________________"
            },
            png: &[],
        }
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn every_tier_fills_its_box_exactly() {
        let tiers = [Tier::Thumbnails, Tier::Ascii, Tier::Glyphs, Tier::Text];
        for l in all() {
            for (c, r) in [
                (6, 3),
                (8, 4),
                (16, 8),
                (10, 4),
                (5, 5),
                (2, 1),
                (1, 1),
                (0, 2),
                (3, 0),
            ] {
                for t in tiers {
                    let out = render(l, t, c, r, &dark());
                    assert_eq!(out.len(), r, "{} {t:?} {c}×{r}", l.id);
                    for line in &out {
                        assert_eq!(line.width(), c, "{} {t:?} {c}×{r}: {line:?}", l.id);
                    }
                }
                for line in thumbnail(l, c, r, &dark(), true) {
                    assert_eq!(line.width(), c, "{} half blocks {c}×{r}", l.id);
                }
            }
        }
    }

    #[test]
    fn thumbnails_draw_the_shape() {
        let red = Some(Color::Rgb(255, 0, 0));
        // all ink: every cell a full block, in the brand colour
        let t = thumbnail(&solid(false), 4, 2, &dark(), false);
        assert_eq!(text(&t), vec!["████", "████"]);
        assert!(t.iter().all(|l| l.spans.iter().all(|s| s.style.fg == red)));
        // a box twice as wide as the picture centres it
        assert_eq!(
            text(&thumbnail(&solid(false), 8, 2, &dark(), false)),
            vec!["  ████  "; 2]
        );
        // half blocks: square pixels, the same box
        assert_eq!(
            text(&thumbnail(&solid(false), 4, 2, &dark(), true)),
            vec!["████"; 2]
        );
        // two colours side by side: each cell takes its half's colour
        let t = thumbnail(&solid(true), 4, 2, &dark(), false);
        assert_eq!(text(&t), vec!["████"; 2]);
        let fgs: Vec<_> = t[0].spans.iter().map(|s| s.style.fg).collect();
        // (pure blue is too dark on the dark theme: it's lifted)
        assert_eq!(fgs, vec![red, dark().ink(Color::Rgb(0, 0, 255))]);
        assert_ne!(fgs[1], Some(Color::Rgb(0, 0, 255)));
        // the Aleph-sized real thing: Ubuntu's ring has ink and holes
        let u = text(&render(
            os("Ubuntu").unwrap(),
            Tier::Thumbnails,
            16,
            8,
            &dark(),
        ));
        let ink: usize = u
            .iter()
            .map(|l| l.chars().filter(|c| *c != ' ').count())
            .sum();
        assert!(ink > 30 && ink < 128, "{ink}\n{}", u.join("\n"));
    }

    #[test]
    fn a_cell_shows_its_best_two_colours() {
        let rgb = [(255, 0, 0), (250, 0, 0), (0, 0, 255)];
        assert_eq!(two_colours([0, 0, 0, 0], &rgb), (0, 0, 0));
        assert_eq!(two_colours([1, 1, 1, 1], &rgb), (15, 1, 0));
        // ▚: top-left and bottom-right
        assert_eq!(two_colours([1, 0, 0, 1], &rgb), (9, 1, 0));
        // transparency wins over colour: two near-reds take one ink, no background
        assert_eq!(two_colours([1, 2, 0, 0], &rgb), (3, 1, 0));
        // no transparency: red over blue, the near-red goes with the red
        assert_eq!(two_colours([1, 2, 3, 3], &rgb), (3, 1, 3));
    }

    #[test]
    fn transparent_pixels_are_never_painted() {
        for l in all() {
            for p in [dark(), light()] {
                for line in render(l, Tier::Thumbnails, 16, 8, &p) {
                    for s in &line.spans {
                        // single-colour marks: one ink per cell, the rest the terminal's
                        assert!(s.style.bg.is_none(), "{} paints a background", l.id);
                    }
                }
            }
        }
    }

    #[test]
    fn colours_fit_the_theme() {
        let fg = |id: &str, kind: Kind, p: &Paint| {
            render(find(kind, id).unwrap(), Tier::Thumbnails, 8, 4, p)
                .iter()
                .flat_map(|l| {
                    l.spans
                        .iter()
                        .filter_map(|s| s.style.fg)
                        .collect::<Vec<_>>()
                })
                .next()
        };
        // black marks take the theme's ink, on dark and on light
        assert_eq!(
            fg("apple", Kind::Os, &dark()),
            Some(Color::Rgb(0xee, 0xee, 0xee))
        );
        assert_eq!(
            fg("apple", Kind::Os, &light()),
            Some(Color::Rgb(0x20, 0x20, 0x20))
        );
        assert_eq!(
            fg("cursor", Kind::Agent, &dark()),
            Some(Color::Rgb(0xee, 0xee, 0xee))
        );
        // a brand colour that shows stays
        assert_eq!(
            fg("ubuntu", Kind::Os, &dark()),
            Some(Color::Rgb(0xe9, 0x54, 0x20))
        );
        // CentOS navy on dark gets lighter, Hugging Face yellow on white darker
        let Some(Color::Rgb(r, g, b)) = fg("centos", Kind::Os, &dark()) else {
            panic!()
        };
        assert!(contrast((r, g, b), (0x11, 0x11, 0x11)) >= 3.0 && b > 0x77);
        let Some(Color::Rgb(r, g, b)) = fg("tgi", Kind::Server, &light()) else {
            panic!()
        };
        assert!(contrast((r, g, b), (0xfc, 0xfc, 0xfc)) >= 3.0);
        // no truecolor: the 256; NO_COLOR: none at all
        let p256 = Paint {
            truecolor: false,
            ..dark()
        };
        assert_eq!(
            fg("ubuntu", Kind::Os, &p256),
            Some(Color::Indexed(q256(0xe9, 0x54, 0x20)))
        );
        let mono = Paint {
            mono: true,
            ..dark()
        };
        for t in [Tier::Thumbnails, Tier::Ascii, Tier::Glyphs] {
            for line in render(find(Kind::Os, "ubuntu").unwrap(), t, 8, 4, &mono) {
                assert!(line
                    .spans
                    .iter()
                    .all(|s| s.style.fg.is_none() && s.style.bg.is_none()));
            }
        }
        // but the shape is all there
        let m = text(&render(
            find(Kind::Os, "ubuntu").unwrap(),
            Tier::Thumbnails,
            8,
            4,
            &mono,
        ));
        assert!(m.iter().any(|l| l.trim().len() > 2), "{m:?}");
    }

    #[test]
    fn the_256_colour_palette() {
        assert_eq!(q256(0, 0, 0), 16);
        assert_eq!(q256(255, 255, 255), 231);
        assert_eq!(q256(255, 0, 0), 196);
        assert_eq!(q256(0, 255, 0), 46);
        assert_eq!(q256(0, 0, 255), 21);
        // greys go to the grey ramp
        assert_eq!(q256(128, 128, 128), 244);
        assert_eq!(q256(0x11, 0x11, 0x11), 233);
        // Ubuntu orange: the cube's (215, 95, 0)
        assert_eq!(q256(0xe9, 0x54, 0x20), 166);
    }

    #[test]
    fn ascii_follows_the_ink() {
        assert_eq!(
            text(&ascii(&solid(false), 4, 2, &dark())),
            vec!["@@@@", "@@@@"]
        );
        let a = text(&ascii(find(Kind::Os, "apple").unwrap(), 16, 8, &dark()));
        let all: String = a.concat();
        assert!(all.contains('@') && all.contains(' '), "{}", a.join("\n"));
        assert!(all.chars().all(|c| " .:-=+*#%@".contains(c)));
    }

    #[test]
    fn glyphs_and_chips() {
        let g = |k: Kind, w: &str, nerd: bool| glyph_with(k, w, nerd, &dark()).content.to_string();
        assert_eq!(g(Kind::Agent, "claude", false), "✻");
        assert_eq!(g(Kind::Agent, "cc", true), "✻");
        assert_eq!(g(Kind::Server, "ollama", false), "▣");
        assert_eq!(g(Kind::Os, "Ubuntu 24.04", true), "\u{f31b}");
        assert_eq!(g(Kind::Os, "apple", true), "\u{f179}");
        assert_eq!(g(Kind::Os, "Ubuntu 24.04", false), "Ubuntu");
        assert_eq!(g(Kind::Gpu, "Intel Arc", false), "Intel");
        assert_eq!(g(Kind::Gpu, "nvidia", true), "\u{f0b2a}");
        assert_eq!(g(Kind::Agent, "nosuch", false), "nosuch");
        assert_eq!(g(Kind::Os, "", false), "?");
        // the text tier: a chip with the name, centred
        let t = text(&render(
            find(Kind::Os, "ubuntu").unwrap(),
            Tier::Text,
            12,
            3,
            &dark(),
        ));
        assert_eq!(t, vec!["            ", "   Ubuntu   ", "            "]);
        let narrow = text(&render(
            find(Kind::Agent, "claude").unwrap(),
            Tier::Text,
            6,
            1,
            &dark(),
        ));
        assert_eq!(narrow[0].chars().count(), 6);
    }

    #[test]
    fn deciding_the_tier() {
        let warp = || Err("Warp doesn't show inline images".to_string());
        // no images: glyphs or names, never block thumbnails unasked
        let d = choose("auto", 4, warp(), false);
        assert_eq!(d.tier, Tier::Text);
        assert!(d.why.contains("Warp"), "{}", d.why);
        assert_eq!(choose("auto", 8, warp(), true).tier, Tier::Glyphs);
        assert_eq!(choose("auto", 2, warp(), true).tier, Tier::Glyphs);
        assert_eq!(choose("auto", 1, warp(), false).tier, Tier::Text);
        assert_eq!(choose("auto", 1, Ok("kitty"), false).tier, Tier::Images);
        // asked for images where there are none: the next best, saying so
        let d = choose("images", 4, warp(), false);
        assert_eq!(d.tier, Tier::Thumbnails);
        assert!(d.why.starts_with("set to images, but Warp"), "{}", d.why);
        assert_eq!(choose("images", 4, Ok("Ghostty"), false).tier, Tier::Images);
        // the others are taken as they are
        for t in ["thumbnails", "ascii", "glyphs", "text"] {
            let d = choose(t, 1, Ok("kitty"), true);
            assert_eq!(d.tier.as_str(), t);
            assert_eq!(d.why, format!("set to {t}"));
        }
        assert_eq!(size_of("small"), (6, 3));
        assert_eq!(size_of("medium"), (8, 4));
        assert_eq!(size_of("large"), (16, 8));
        // the settings take exactly these
        assert!(crate::config::check("ui.logos", "thumbnails").is_ok());
        assert!(crate::config::check("ui.logos", "sixel").is_err());
        assert!(crate::config::check("ui.logo_size", "large").is_ok());
    }

    #[test]
    fn lines_for_anything() {
        let cfg = crate::config::Config::in_memory(&["ui.logos=thumbnails"]);
        let l = lines(Kind::Agent, "claude", 8, 4, &cfg);
        assert_eq!(l.len(), 4);
        assert!(l.iter().all(|x| x.width() == 8));
        // no logo: its glyph or name, in the same box
        let l = lines(Kind::Gpu, "Qualcomm Adreno", 8, 4, &cfg);
        assert_eq!(l.len(), 4);
        assert!(text(&l)[1].contains("Qualcom"), "{:?}", text(&l));
    }

    #[test]
    fn lookup_takes_ids_and_names() {
        assert_eq!(lookup(Kind::Os, "ubuntu").unwrap().id, "ubuntu");
        assert_eq!(lookup(Kind::Os, "Ubuntu 22.04").unwrap().id, "ubuntu");
        assert_eq!(lookup(Kind::Gpu, "GeForce GTX 1080").unwrap().id, "nvidia");
        assert_eq!(lookup(Kind::Agent, "claude-code").unwrap().id, "claude");
        assert_eq!(lookup(Kind::Server, "vllm").unwrap().id, "vllm");
        assert!(lookup(Kind::Server, "nosuch").is_none());
    }
}
