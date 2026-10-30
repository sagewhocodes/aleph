//! Monitor view renderers (btop × htop × nvtop): the cross-machine table
//! with per-machine charts (`fleet`), the all-in-one machine page
//! (`overview`, with a focus), the CPU / memory / GPU / process / network
//! sub-views, the compact machine panel for the Fleet view, the machine
//! header and the sub-view tab strip.
//!
//! These only draw: state, keys and tabs live with the caller. Every renderer
//! fits any rect (0×0 up) and any `Stats` (empty, partial, NaN, absurd):
//! missing data reads as a muted `—` or a one-line reason, a machine that
//! hasn't sent its first sample shows the star spinner, a broken stream its
//! red reason. Histories are braille charts stretched to the full width
//! (`monitor_chart`). Colour carries meaning: the heat scale for load (cool
//! teal → red), one colour per series (the legend says which), the accent
//! for agents and the machine colour for chips. NO_COLOR still reads: bars,
//! glyphs, textures and numbers carry it.

use crate::agents::{self, Agent};
use crate::icons;
use crate::telemetry::{Gpu, GpuProc, Proc, Stats};
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::cmp::Ordering;
use std::sync::OnceLock;
use std::time::Instant;

#[path = "monitor_chart.rs"]
mod chart;
#[path = "monitor_procs.rs"]
mod ptable;

use chart::{Paint as CP, Plot, Ser};
use ptable::{age, top_lines};
pub use ptable::{procs, ProcUi};

// ------------------------------------------------------------------ public types

/// The Monitor view's sub-views, in tab order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sub {
    #[default]
    Fleet,
    Overview,
    Cpu,
    Memory,
    Gpu,
    Procs,
    Net,
    /// LLM servers and gateways on every machine
    Services,
    /// every session's states over the last hours
    Timeline,
    /// tokens, time and cost from the agents' logs
    Usage,
}

impl Sub {
    pub const ALL: [Sub; 10] = [
        Sub::Fleet,
        Sub::Overview,
        Sub::Cpu,
        Sub::Memory,
        Sub::Gpu,
        Sub::Procs,
        Sub::Net,
        Sub::Services,
        Sub::Timeline,
        Sub::Usage,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Sub::Fleet => "Fleet",
            Sub::Overview => "Overview",
            Sub::Cpu => "CPU",
            Sub::Memory => "Memory",
            Sub::Gpu => "GPU",
            Sub::Procs => "Processes",
            Sub::Net => "Network",
            Sub::Services => "Services",
            Sub::Timeline => "Timeline",
            Sub::Usage => "Usage",
        }
    }
    /// For narrow strips.
    pub fn short(self) -> &'static str {
        match self {
            Sub::Fleet => "Fleet",
            Sub::Overview => "Over",
            Sub::Cpu => "CPU",
            Sub::Memory => "Mem",
            Sub::Gpu => "GPU",
            Sub::Procs => "Procs",
            Sub::Net => "Net",
            Sub::Services => "Svc",
            Sub::Timeline => "Time",
            Sub::Usage => "Use",
        }
    }
    /// The key that jumps to it: its name's first letter (underlined in the strip).
    pub fn key(self) -> char {
        match self {
            Sub::Fleet => 'f',
            Sub::Overview => 'o',
            Sub::Cpu => 'c',
            Sub::Memory => 'm',
            Sub::Gpu => 'g',
            Sub::Procs => 'p',
            Sub::Net => 'n',
            Sub::Services => 's',
            Sub::Timeline => 't',
            Sub::Usage => 'u',
        }
    }
    /// Lower-case letters only: upper case is left for toggles (`F`, `P`, `S`).
    pub fn from_key(k: char) -> Option<Sub> {
        Sub::ALL.into_iter().find(|s| s.key() == k)
    }
    fn idx(self) -> usize {
        Sub::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }
    /// The pages to offer: Services only while looking for LLM servers is on.
    pub fn shown(services: bool) -> Vec<Sub> {
        Sub::ALL
            .into_iter()
            .filter(|s| services || *s != Sub::Services)
            .collect()
    }
    pub fn next(self) -> Sub {
        Sub::ALL[(self.idx() + 1) % Sub::ALL.len()]
    }
    pub fn prev(self) -> Sub {
        Sub::ALL[(self.idx() + Sub::ALL.len() - 1) % Sub::ALL.len()]
    }
}

/// What the Overview gives most of its room to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    /// every resource, processes as a small top-N
    #[default]
    All,
    Cpu,
    Mem,
    Gpu,
    /// the process list, resources as compact meters
    Procs,
}

impl Focus {
    pub const ALL: [Focus; 5] = [Focus::All, Focus::Cpu, Focus::Mem, Focus::Gpu, Focus::Procs];
    pub fn name(self) -> &'static str {
        match self {
            Focus::All => "all",
            Focus::Cpu => "CPU",
            Focus::Mem => "memory",
            Focus::Gpu => "GPU",
            Focus::Procs => "processes",
        }
    }
    pub fn next(self) -> Focus {
        let i = Focus::ALL.iter().position(|x| *x == self).unwrap_or(0);
        Focus::ALL[(i + 1) % Focus::ALL.len()]
    }
}

/// Process-table order. CPU, memory, average CPU and CPU time sort
/// descending, uptime youngest first, PID and name ascending.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Cpu,
    Mem,
    Avg,
    Age,
    Time,
    Pid,
    Name,
}

impl Sort {
    pub const ALL: [Sort; 7] = [
        Sort::Cpu,
        Sort::Mem,
        Sort::Avg,
        Sort::Age,
        Sort::Time,
        Sort::Pid,
        Sort::Name,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Sort::Cpu => "cpu",
            Sort::Mem => "memory",
            Sort::Avg => "avg cpu",
            Sort::Age => "uptime",
            Sort::Time => "cpu time",
            Sort::Pid => "pid",
            Sort::Name => "name",
        }
    }
    pub fn next(self) -> Sort {
        let i = Sort::ALL.iter().position(|s| *s == self).unwrap_or(0);
        Sort::ALL[(i + 1) % Sort::ALL.len()]
    }
    fn desc(self) -> bool {
        matches!(self, Sort::Cpu | Sort::Mem | Sort::Avg | Sort::Time)
    }
    fn arrow(self, rev: bool) -> &'static str {
        if self.desc() != rev {
            "▼"
        } else {
            "▲"
        }
    }
}

/// One machine in the cross-machine table.
#[derive(Clone, Copy)]
pub struct MachineRow<'a> {
    pub alias: &'a str,
    pub color: Color,
    pub stats: Option<&'a Stats>,
    /// reachable
    pub up: bool,
    /// an error, `herdr not running`, … (shown in place of numbers when down)
    pub note: &'a str,
}

/// Agents running on a machine, by agent id.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentUse {
    pub id: &'static str,
    pub name: &'static str,
    pub n: usize,
    pub rss: f64,
    /// fraction of one core, summed
    pub cpu: f64,
}

// ------------------------------------------------------------------ numbers

/// NaN / ±inf → 0: nothing absurd reaches a bar, a width or a format.
fn fin(x: f64) -> f64 {
    if x.is_finite() {
        x
    } else {
        0.0
    }
}
fn fr(x: f64) -> f64 {
    fin(x).clamp(0.0, 1.0)
}
fn ratio(a: f64, b: f64) -> f64 {
    let b = fin(b);
    if b > 0.0 {
        (fin(a) / b).clamp(0.0, 1.0)
    } else {
        0.0
    }
}
fn ok(x: Option<f64>) -> Option<f64> {
    x.filter(|v| v.is_finite())
}
/// fraction → `54%`
fn pct(x: f64) -> String {
    format!("{:.0}%", (fin(x) * 100.0).clamp(0.0, 9999.0))
}
/// fraction → `54.3%`
fn pct1(x: f64) -> String {
    format!("{:.1}%", (fin(x) * 100.0).clamp(0.0, 999.9))
}
fn by(x: f64) -> String {
    ui::bytes(fin(x).clamp(0.0, 1e21))
}
fn rate(x: f64) -> String {
    format!("{}/s", by(x))
}
fn deg(t: f64) -> String {
    format!("{:.0}°C", fin(t).clamp(-99.0, 999.0))
}
fn int(x: f64) -> String {
    format!("{:.0}", fin(x).clamp(-1e6, 1e6))
}
fn load(x: f64) -> String {
    format!("{:.2}", fin(x).clamp(0.0, 99999.0))
}
fn ghz(mhz: f64) -> String {
    format!("{:.1} GHz", (fin(mhz) / 1000.0).clamp(0.0, 999.0))
}
/// 35 °C cool … 90 °C hot.
fn temp_heat(t: f64) -> Color {
    ui::heat(((fin(t) - 35.0) / 55.0).clamp(0.0, 1.0))
}
fn load_frac(st: &Stats) -> f64 {
    if st.ncpu > 0 {
        fr(st.load[0] / st.ncpu as f64)
    } else {
        0.0
    }
}
fn digits(n: usize) -> usize {
    n.to_string().len()
}
/// htop's TIME+: `12:03.45`, `3h05:12`, `12d4h`.
fn ptime(s: f64) -> String {
    let s = fin(s).clamp(0.0, 1e12);
    if s < 3600.0 {
        format!("{}:{:05.2}", (s / 60.0) as u64, s % 60.0)
    } else {
        let t = s as u64;
        if t < 100 * 3600 {
            format!("{}h{:02}:{:02}", t / 3600, t % 3600 / 60, t % 60)
        } else {
            format!("{}d{}h", t / 86400, t % 86400 / 3600)
        }
    }
}
/// `1.2k` for big counts.
fn count(n: usize) -> String {
    if n >= 10_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}
fn avg_peak(data: &[f64]) -> (f64, f64) {
    let (mut s, mut n, mut p) = (0.0, 0usize, 0.0f64);
    for v in data.iter().copied().filter(|v| v.is_finite()) {
        s += v;
        n += 1;
        p = p.max(v);
    }
    (if n > 0 { s / n as f64 } else { 0.0 }, p)
}
/// `avg_peak` of a fraction history, each sample clamped to 0..1.
fn avg_peak_frac(data: &[f64]) -> (f64, f64) {
    let (mut s, mut p) = (0.0, 0.0f64);
    for v in data.iter().map(|v| fr(*v)) {
        s += v;
        p = p.max(v);
    }
    let n = data.len();
    (if n > 0 { s / n as f64 } else { 0.0 }, p)
}
/// Seconds per sample for the time axes (1 until measured).
fn period(st: &Stats) -> f64 {
    if st.period.is_finite() && st.period > 0.0 {
        st.period.min(3600.0)
    } else {
        1.0
    }
}

/// Control characters out (a process name can contain anything).
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Two histories added sample by sample, aligned at the newest.
fn sum2(a: &[f64], b: &[f64]) -> Vec<f64> {
    sum_hists([a, b].into_iter())
}

fn sum_hists<'a>(hs: impl Iterator<Item = &'a [f64]>) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::new();
    for h in hs {
        // align at the newest sample
        if h.len() > out.len() {
            let mut v = vec![0.0; h.len() - out.len()];
            v.extend(out);
            out = v;
        }
        let off = out.len() - h.len();
        for (i, x) in h.iter().enumerate() {
            out[off + i] += fin(*x).max(0.0);
        }
    }
    out
}

// ------------------------------------------------------------------ text building

/// A line under construction that knows its width.
#[derive(Default)]
struct Ln {
    v: Vec<Span<'static>>,
    w: usize,
}

impl Ln {
    fn new() -> Ln {
        Ln::default()
    }
    fn s(&mut self, t: impl Into<String>, st: Style) {
        let t: String = t.into();
        self.w += ui::width(&t);
        self.v.push(Span::styled(t, st));
    }
    fn add(&mut self, sp: Vec<Span<'static>>) {
        for s in sp {
            self.w += ui::width(&s.content);
            self.v.push(s);
        }
    }
    /// Pad with spaces to column `col`.
    fn to(&mut self, col: usize) {
        if col > self.w {
            self.s(" ".repeat(col - self.w), Style::new());
        }
    }
    fn line(self) -> Line<'static> {
        Line::from(self.v)
    }
    fn spans(self) -> Vec<Span<'static>> {
        self.v
    }
}

fn spans_w(sp: &[Span]) -> usize {
    sp.iter().map(|s| ui::width(&s.content)).sum()
}

/// Exactly `w` columns: cut (with an ellipsis) or pad.
fn fit(sp: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(sp.len() + 1);
    let mut used = 0;
    for s in sp {
        if used >= w {
            break;
        }
        let sw = ui::width(&s.content);
        if used + sw <= w {
            used += sw;
            out.push(s);
        } else {
            let t = ui::trunc(&s.content, w - used);
            used += ui::width(&t);
            out.push(Span::styled(t, s.style));
            break;
        }
    }
    if used < w {
        out.push(Span::raw(" ".repeat(w - used)));
    }
    out
}

fn sep() -> Span<'static> {
    Span::styled(" · ", ui::faint())
}

/// Items joined by ` · `, skipping any that would overflow `w`.
fn join_fit(items: Vec<Vec<Span<'static>>>, w: usize) -> Vec<Span<'static>> {
    join_with(items, w, sep)
}

/// Items joined by two spaces (for legends and meters).
fn join_sp(items: Vec<Vec<Span<'static>>>, w: usize) -> Vec<Span<'static>> {
    join_with(items, w, || Span::raw("  "))
}

fn join_with(
    items: Vec<Vec<Span<'static>>>,
    w: usize,
    gap: impl Fn() -> Span<'static>,
) -> Vec<Span<'static>> {
    let gw = ui::width(&gap().content);
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for it in items {
        let iw = spans_w(&it);
        if iw == 0 {
            continue;
        }
        let need = iw + if out.is_empty() { 0 } else { gw };
        if used + need > w {
            continue;
        }
        if !out.is_empty() {
            out.push(gap());
        }
        out.extend(it);
        used += need;
    }
    out
}

/// Like `join_fit`, but when not everything fits the survivors are picked by
/// priority (lower = more important) and shown in their original order.
fn join_prio(items: Vec<(u8, Vec<Span<'static>>)>, w: usize) -> Vec<Span<'static>> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by_key(|&i| items[i].0);
    let mut keep = vec![false; items.len()];
    let (mut used, mut n) = (0, 0);
    for i in order {
        let iw = spans_w(&items[i].1);
        if iw == 0 {
            continue;
        }
        let need = iw + if n == 0 { 0 } else { 3 };
        if used + need <= w {
            keep[i] = true;
            used += need;
            n += 1;
        }
    }
    let kept = items
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|((_, it), _)| it)
        .collect();
    join_fit(kept, w)
}

/// Items joined by ` · `, wrapped onto as many lines as they need.
fn flow(items: Vec<Vec<Span<'static>>>, w: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for it in items {
        let iw = spans_w(&it);
        if iw == 0 {
            continue;
        }
        if !cur.is_empty() && used + 3 + iw > w {
            lines.push(Line::from(std::mem::take(&mut cur)));
            used = 0;
        }
        if !cur.is_empty() {
            cur.push(sep());
            used += 3;
        }
        cur.extend(it);
        used += iw;
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

/// `key value` with a faint key.
fn kv(k: &str, v: impl Into<String>, st: Style) -> Vec<Span<'static>> {
    vec![
        Span::styled(format!("{k} "), ui::faint()),
        Span::styled(v.into(), st),
    ]
}

fn dash() -> Span<'static> {
    Span::styled("—", ui::faint())
}

fn chip(alias: &str, color: Color) -> Vec<Span<'static>> {
    vec![
        Span::styled("▌", ui::fg(color)),
        Span::styled(clean(alias), ui::fg(color).add_modifier(Modifier::BOLD)),
    ]
}

/// A section label: bold, muted (or in its series colour, doubling as the legend).
fn label(s: &str, w: usize, col: Option<Color>) -> Span<'static> {
    Span::styled(
        ui::pad(s, w),
        ui::fg(col.unwrap_or(c::MUTED())).add_modifier(Modifier::BOLD),
    )
}

// ------------------------------------------------------------------ geometry

/// Never draw outside the frame, whatever rect the caller hands us.
fn area(f: &Frame, r: Rect) -> Rect {
    r.intersection(f.area())
}

/// A sub-rect, clipped to `r`.
fn sub(r: Rect, dx: u16, dy: u16, w: u16, h: u16) -> Rect {
    let x = r.x.saturating_add(dx.min(r.width));
    let y = r.y.saturating_add(dy.min(r.height));
    Rect {
        x,
        y,
        width: w.min(r.right().saturating_sub(x)),
        height: h.min(r.bottom().saturating_sub(y)),
    }
}
/// Rows `dy..dy+h` of `r`.
fn band(r: Rect, dy: usize, h: usize) -> Rect {
    sub(r, 0, clamp16(dy), r.width, clamp16(h))
}
fn clamp16(x: usize) -> u16 {
    x.min(u16::MAX as usize) as u16
}

/// Columns `dx..dx+w` of `r`.
fn cols(r: Rect, dx: usize, w: usize) -> Rect {
    sub(r, clamp16(dx), 0, clamp16(w), r.height)
}

/// `n` widths for `w` cells by weight (the last takes the rounding).
fn split(w: usize, weights: &[u16]) -> Vec<usize> {
    let tot: usize = weights.iter().map(|x| *x as usize).sum::<usize>().max(1);
    let mut out = Vec::with_capacity(weights.len());
    let mut used = 0;
    for (i, wt) in weights.iter().enumerate() {
        let x = if i + 1 == weights.len() {
            w - used
        } else {
            (w * *wt as usize / tot).min(w - used)
        };
        used += x;
        out.push(x);
    }
    out
}

fn put(f: &mut Frame, r: Rect, lines: Vec<Line<'static>>) {
    let r = area(f, r);
    if r.width > 0 && r.height > 0 && !lines.is_empty() {
        f.render_widget(Paragraph::new(lines), r);
    }
}

/// A one-line message in the middle of `r`.
fn centered(f: &mut Frame, r: Rect, line: Line<'static>) {
    let r = area(f, r);
    if r.height == 0 {
        return;
    }
    put(
        f,
        band(r, (r.height as usize).saturating_sub(1) / 2, 1),
        vec![line.centered()],
    );
}

/// A rounded panel; returns the inner rect. Too small for a frame: no frame,
/// the whole rect is content.
fn panel(f: &mut Frame, r: Rect, title: Line<'static>, right: Option<Line<'static>>) -> Rect {
    panel_b(f, r, title, right, None)
}

fn panel_b(
    f: &mut Frame,
    r: Rect,
    title: Line<'static>,
    right: Option<Line<'static>>,
    bottom: Option<Line<'static>>,
) -> Rect {
    let r = area(f, r);
    if r.width < 8 || r.height < 3 {
        return r;
    }
    let tw = ui::line_width(&title) + 2;
    let mut b = ui::panel(title, false);
    if let Some(rt) = right {
        // the right title gives way to the left one, then shortens
        let room = (r.width as usize).saturating_sub(tw + 6);
        let rw = ui::line_width(&rt);
        if rw > 0 && room >= 6 {
            let rt = if rw > room {
                Line::from(fit(rt.spans, room))
            } else {
                rt
            };
            b = b.title_top(padded(rt).right_aligned());
        }
    }
    if let Some(bt) = bottom {
        if ui::line_width(&bt) + 6 <= r.width as usize {
            b = b.title_bottom(padded(bt));
        }
    }
    let inner = b.inner(r);
    f.render_widget(b, r);
    inner
}

/// A border title with a space either side; spans without a colour get the
/// text colour (they'd take the border's otherwise).
/// Rows a panel's border and padding take at the current `ui_scale`.
fn chrome_h() -> usize {
    let p = ui::padding();
    2 + (p.top + p.bottom) as usize
}

/// Columns a panel's border and padding take at the current `ui_scale`.
fn chrome_w() -> usize {
    let p = ui::padding();
    2 + (p.left + p.right) as usize
}

fn padded(l: Line<'static>) -> Line<'static> {
    let mut v = vec![Span::raw(" ")];
    v.extend(l.spans.into_iter().map(|s| {
        if s.style.fg.is_none() && !s.content.trim().is_empty() {
            let st = s.style.fg(c::TEXT());
            s.style(st)
        } else {
            s
        }
    }));
    v.push(Span::raw(" "));
    Line::from(v)
}

/// Right-title items joined by ` · `.
fn right_line(items: Vec<Vec<Span<'static>>>) -> Option<Line<'static>> {
    let v = join_fit(items, 200);
    (!v.is_empty()).then(|| Line::from(v))
}

fn epoch() -> Instant {
    static T: OnceLock<Instant> = OnceLock::new();
    *T.get_or_init(Instant::now)
}

/// No sample yet: the spinner, or the red reason. True when it drew.
fn waiting(f: &mut Frame, r: Rect, st: &Stats, t0: Instant) -> bool {
    if st.samples > 0 {
        return false;
    }
    let w = r.width as usize;
    let line = match &st.err {
        Some(e) => Line::from(vec![
            Span::styled("✕ ", ui::fg(c::RED())),
            Span::styled(
                ui::trunc(&format!("no stats: {}", clean(e)), w.saturating_sub(2)),
                ui::fg(c::RED()),
            ),
        ]),
        None => Line::from(vec![
            Span::styled(format!("{} ", ui::spinner(t0)), ui::fg(c::ACCENT())),
            Span::styled("sampling…", ui::muted()),
        ]),
    };
    centered(f, r, line);
    true
}

/// The stream broke after data arrived: the last sample is still shown.
fn stale_note(st: &Stats) -> Option<Line<'static>> {
    st.err.as_ref().map(|e| {
        Line::from(vec![
            Span::styled("✕ ", ui::fg(c::RED())),
            Span::styled(
                format!("{} · last sample", ui::trunc(&clean(e), 40)),
                ui::fg(c::RED()),
            ),
        ])
    })
}

// ------------------------------------------------------------------ meters

/// A meter bar `w` cells wide, btop style: `■` cells whose colour runs
/// along the heat scale by position (a full bar ends red) on a dim `■`
/// track, so meters in consecutive rows stay apart. Any non-zero value
/// shows at least one cell.
fn gbar(frac: f64, w: usize) -> Vec<Span<'static>> {
    if w == 0 {
        return vec![];
    }
    let f = fr(frac);
    let mut full = ((f * w as f64).round() as usize).min(w);
    if f > 0.0 && full == 0 {
        full = 1;
    }
    let mut out: Vec<Span<'static>> = Vec::new();
    let (mut cur, mut cur_c) = (String::new(), None::<Color>);
    for i in 0..full {
        let col = chart::grad(((i as f64 + 0.5) / w as f64 * 16.0).round() / 16.0);
        if cur_c != Some(col) && !cur.is_empty() {
            out.push(Span::styled(
                std::mem::take(&mut cur),
                ui::fg(cur_c.unwrap_or(col)),
            ));
        }
        cur_c = Some(col);
        cur.push('■');
    }
    if !cur.is_empty() {
        out.push(Span::styled(cur, ui::fg(cur_c.unwrap_or(c::TEXT()))));
    }
    if full < w {
        let t = if c::mono() { " " } else { "■" };
        out.push(Span::styled(t.repeat(w - full), ui::fg(c::LINE())));
    }
    out
}

/// One aligned meter row: a label, the bar, the value and a detail.
struct M {
    label: &'static str,
    /// the label's colour (a chart's series colour, so it reads as the legend)
    col: Option<Color>,
    /// None: no data (an empty track and a dash)
    frac: Option<f64>,
    /// the value shown after the bar (default: the percentage)
    val: Option<(String, Style)>,
    detail: Vec<Span<'static>>,
}

impl M {
    fn new(label: &'static str, frac: Option<f64>) -> M {
        M {
            label,
            col: None,
            frac,
            val: None,
            detail: vec![],
        }
    }
    fn col(mut self, c: Color) -> M {
        self.col = Some(c);
        self
    }
    fn val(mut self, s: String, st: Style) -> M {
        self.val = Some((s, st));
        self
    }
    fn detail(mut self, d: Vec<Span<'static>>) -> M {
        self.detail = d;
        self
    }
}

/// Meter rows in `w` cells, every column aligned: the labels, the bars (one
/// width), the right-aligned values and the details (dropped when narrow).
fn meters(rows: Vec<M>, w: usize) -> Vec<Line<'static>> {
    // labels in a column at least 6 wide, so meter blocks line up with the
    // label rows around them
    let lw = (rows.iter().map(|m| ui::width(m.label)).max().unwrap_or(0) + 1).max(6);
    let vw = rows
        .iter()
        .map(|m| m.val.as_ref().map_or(4, |v| ui::width(&v.0)))
        .max()
        .unwrap_or(4)
        .max(4);
    let dw = rows.iter().map(|m| spans_w(&m.detail)).max().unwrap_or(0);
    let fixed = lw + 1 + vw;
    // the detail column gets what it needs when the bar keeps ≥ 8 cells
    let dw = if dw > 0 && w >= fixed + 8 + 2 + dw.min(12) {
        dw.min(w - fixed - 8 - 2)
    } else {
        0
    };
    let bw = w
        .saturating_sub(fixed + if dw > 0 { 2 + dw } else { 0 })
        .min(if dw > 0 { 48 } else { 80 });
    rows.into_iter()
        .map(|m| {
            let mut l = Ln::new();
            l.add(vec![label(m.label, lw, m.col)]);
            match m.frac.map(fr) {
                Some(v) => {
                    l.add(gbar(v, bw));
                    let (t, st) = m.val.unwrap_or_else(|| {
                        (pct(v), ui::fg(ui::heat(v)).add_modifier(Modifier::BOLD))
                    });
                    l.add(nu(&format!(" {t:>vw$}"), st));
                }
                None => {
                    l.add(gbar(0.0, bw));
                    l.s(format!(" {:>vw$}", "—"), ui::faint());
                }
            }
            if dw > 0 && !m.detail.is_empty() {
                l.s("  ", Style::new());
                l.add(fit(m.detail, dw));
            }
            l.line()
        })
        .collect()
}

/// One segment of a stacked bar.
struct Part {
    frac: f64,
    color: Color,
    /// the segment's glyph under NO_COLOR
    glyph: &'static str,
    name: &'static str,
}

/// A stacked bar: segments take their share of `w` cells (cumulative
/// rounding, so they never overflow), then the track.
fn stack(parts: &[Part], w: usize) -> Vec<Span<'static>> {
    let mono = c::mono();
    let mut out = Vec::new();
    let (mut cum, mut used) = (0.0f64, 0usize);
    for p in parts {
        cum += fr(p.frac);
        let end = (cum.min(1.0) * w as f64).round() as usize;
        let n = end.saturating_sub(used).min(w - used);
        if n > 0 {
            let g = if mono { p.glyph } else { "■" };
            out.push(Span::styled(g.repeat(n), ui::fg(p.color)));
            used += n;
        }
    }
    if used < w {
        let t = if mono { " " } else { "■" };
        out.push(Span::styled(t.repeat(w - used), ui::fg(c::LINE())));
    }
    out
}

fn swatch(p: &Part) -> Span<'static> {
    Span::styled(if c::mono() { p.glyph } else { "■" }, ui::fg(p.color))
}

/// `█ usr 40%` legend items.
fn legend(parts: &[Part], vals: &[String]) -> Vec<Vec<Span<'static>>> {
    parts
        .iter()
        .zip(vals)
        .map(|(p, v)| {
            vec![
                swatch(p),
                Span::styled(format!(" {} ", p.name), ui::muted()),
                Span::styled(v.clone(), Style::new()),
            ]
        })
        .collect()
}

/// `━━ util 54%` items for a chart's series (index = texture under NO_COLOR).
fn series_legend(items: Vec<(Color, &str, Vec<Span<'static>>)>) -> Vec<Vec<Span<'static>>> {
    items
        .into_iter()
        .enumerate()
        .map(|(i, (col, name, v))| {
            let mut s = vec![
                chart::swatch(i, col),
                Span::styled(format!(" {name} "), ui::muted()),
            ];
            s.extend(v);
            s
        })
        .collect()
}

fn cpu_parts(st: &Stats) -> Vec<Part> {
    let (u, s, io, stl) = (
        fr(st.cpu_user),
        fr(st.cpu_system),
        fr(st.cpu_iowait),
        fr(st.cpu_steal),
    );
    let busy = fr(st.cpu);
    if u + s + io + stl <= 0.0 {
        return vec![Part {
            frac: busy,
            color: ui::heat(busy),
            glyph: "█",
            name: "busy",
        }];
    }
    let mut v = vec![
        Part {
            frac: u,
            color: c::GREEN(),
            glyph: "█",
            name: "usr",
        },
        Part {
            frac: s,
            color: c::RED(),
            glyph: "▓",
            name: "sys",
        },
    ];
    let other = (busy - u - s - stl).max(0.0);
    if other > 0.005 {
        v.push(Part {
            frac: other,
            color: c::BLUE(),
            glyph: "▒",
            name: "other",
        });
    }
    if stl > 0.0 {
        v.push(Part {
            frac: stl,
            color: c::TEAL(),
            glyph: "▒",
            name: "steal",
        });
    }
    if io > 0.0 {
        v.push(Part {
            frac: io,
            color: c::YELLOW(),
            glyph: "░",
            name: "iowait",
        });
    }
    v
}

/// (used, buffers, cache) as fractions of total; free is the rest.
fn mem_parts(st: &Stats) -> Vec<Part> {
    let total = fin(st.mem_total).max(0.0);
    let (used, buf, cache, _) = mem_bytes(st);
    vec![
        Part {
            frac: ratio(used, total),
            color: c::GREEN(),
            glyph: "█",
            name: "used",
        },
        Part {
            frac: ratio(buf, total),
            color: c::BLUE(),
            glyph: "▓",
            name: "buffers",
        },
        Part {
            frac: ratio(cache, total),
            color: c::YELLOW(),
            glyph: "▒",
            name: "cache",
        },
    ]
}

/// (used, buffers, cache, free) bytes, clamped to the total.
fn mem_bytes(st: &Stats) -> (f64, f64, f64, f64) {
    let total = fin(st.mem_total).max(0.0);
    let used = fin(st.mem_used).clamp(0.0, total);
    let buf = fin(st.mem_buffers).clamp(0.0, total - used);
    let cache = fin(st.mem_cached).clamp(0.0, total - used - buf);
    (used, buf, cache, (total - used - buf - cache).max(0.0))
}

fn vram_total(st: &Stats) -> (f64, f64) {
    st.gpus.iter().fold((0.0, 0.0), |(u, t), g| {
        (u + fin(g.mem_used).max(0.0), t + fin(g.mem_total).max(0.0))
    })
}

// ------------------------------------------------------------------ cores

/// The cores in `w`×`h`, the richest way that fits: htop-style meters
/// `07 ██████···· 46%` in the fewest columns, else compact meters without the
/// number `07 ■■■■■` (the bar's colour says how busy), else a heat grid of
/// one glyph a core, rows of 8 or 16 labelled by their first core (a 64- or
/// 128-thread machine still gets its grid beside the chart). None when not
/// even that fits (use the strip). Returns the lines and the width used.
fn core_grid(cores: &[f64], w: usize, h: usize) -> Option<(Vec<Line<'static>>, usize)> {
    let n = cores.len();
    if n == 0 || w == 0 || h == 0 {
        return None;
    }
    let iw = digits(n - 1);
    let cols = n.div_ceil(h);
    let room = w.saturating_sub(2 * (cols - 1)) / cols;
    // (label + space + bar [+ " 100%"]) a cell, the widest bar that fits
    let meters = |fixed: usize, min_bar: usize, max_bar: usize, num: bool| {
        if room < fixed + min_bar {
            return None;
        }
        let bw = (room - fixed).min(max_bar);
        let rows = n.div_ceil(cols);
        let lines = (0..rows)
            .map(|row| {
                let mut l = Ln::new();
                for col in 0..cols {
                    let i = col * rows + row;
                    if i >= n {
                        break;
                    }
                    if col > 0 {
                        l.s("  ", Style::new());
                    }
                    let v = fr(cores[i]);
                    l.s(format!("{i:>iw$} "), ui::faint());
                    l.add(gbar(v, bw));
                    if num {
                        l.add(nu(&format!(" {:>4}", pct(v)), ui::fg(ui::heat(v))));
                    }
                }
                l.line()
            })
            .collect::<Vec<_>>();
        Some((lines, cols * (fixed + bw) + 2 * (cols - 1)))
    };
    meters(iw + 1 + 1 + 4, 5, 14, true)
        .or_else(|| meters(iw + 1, 4, 8, false))
        .or_else(|| heat_grid(cores, w, h))
}

/// One level glyph a core, in rows of 16 (else 8) split into groups of 8,
/// each row labelled by its first core: `32 ▁▂▁▁▃▁▁▁ ▁▁▁▁▁▁▁▁`.
fn heat_grid(cores: &[f64], w: usize, h: usize) -> Option<(Vec<Line<'static>>, usize)> {
    let n = cores.len();
    let iw = digits(n.saturating_sub(1));
    let width = |k: usize| iw + 1 + k + k.saturating_sub(1) / 8;
    let k = [16usize, 8, 24, 32, 48, 64]
        .into_iter()
        .find(|&k| width(k) <= w && n.div_ceil(k) <= h)?;
    let lines = cores
        .chunks(k)
        .enumerate()
        .map(|(r, row)| {
            let mut l = Ln::new();
            l.s(format!("{:>iw$} ", r * k), ui::faint());
            for (j, v) in row.iter().enumerate() {
                if j > 0 && j % 8 == 0 {
                    l.s(" ", Style::new());
                }
                l.add(vec![ui::level(fr(*v))]);
            }
            l.line()
        })
        .collect();
    Some((lines, width(k.min(n))))
}

/// One level glyph per core, in groups of 8.
fn strip_lines(cores: &[f64], w: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if w == 0 {
        return lines;
    }
    let mut l = Ln::new();
    for (i, v) in cores.iter().enumerate() {
        if i > 0 && i % 8 == 0 && l.w > 0 && l.w + 2 <= w {
            l.s(" ", Style::new());
        }
        if l.w >= w {
            lines.push(std::mem::take(&mut l).line());
        }
        l.add(vec![ui::level(fr(*v))]);
    }
    if l.w > 0 {
        lines.push(l.line());
    }
    lines
}

/// How many lines `strip_lines` takes: the same wrapping, without building them.
fn strip_rows(n: usize, w: usize) -> usize {
    if n == 0 || w == 0 {
        return 0;
    }
    let (mut rows, mut lw) = (1, 0);
    for i in 0..n {
        if i > 0 && i % 8 == 0 && lw > 0 && lw + 2 <= w {
            lw += 1;
        }
        if lw >= w {
            rows += 1;
            lw = 0;
        }
        lw += 1;
    }
    rows
}

// ------------------------------------------------------------------ processes & agents

fn plist(st: &Stats) -> &[Proc] {
    if st.procs.is_empty() {
        &st.top
    } else {
        &st.procs
    }
}

fn agent_of(p: &Proc) -> Option<&'static Agent> {
    let hit = |w: &str| agents::ALL.iter().find(|a| a.exes.contains(&w));
    let name = p.name.trim();
    if let Some(a) = hit(name) {
        return Some(a);
    }
    // `node …/bin/codex`, `bun …/opencode.js`
    if matches!(name, "node" | "bun" | "deno" | "python" | "python3") {
        for tok in p.cmd.split_whitespace().skip(1).take(2) {
            let base = tok.rsplit('/').next().unwrap_or(tok);
            let base = base
                .strip_suffix(".js")
                .or_else(|| base.strip_suffix(".mjs"))
                .unwrap_or(base);
            if let Some(a) = hit(base) {
                return Some(a);
            }
        }
    }
    None
}

/// Agents running on the machine, busiest first: the process table matched
/// against `agents::ALL` executables; for Claude Code the collector's own
/// count wins when it saw more.
pub fn agents_running(st: &Stats) -> Vec<AgentUse> {
    let mut out: Vec<AgentUse> = Vec::new();
    for p in plist(st) {
        if let Some(a) = agent_of(p) {
            let (rss, cpu) = (fin(p.rss).max(0.0), fin(p.cpu).max(0.0));
            match out.iter_mut().find(|u| u.id == a.id) {
                Some(u) => {
                    u.n += 1;
                    u.rss += rss;
                    u.cpu += cpu;
                }
                None => out.push(AgentUse {
                    id: a.id,
                    name: a.name,
                    n: 1,
                    rss,
                    cpu,
                }),
            }
        }
    }
    if st.claude_procs > 0 {
        let have = out.iter().position(|u| u.id == "claude");
        let idx = match have {
            Some(i) => i,
            None => {
                out.push(AgentUse {
                    id: "claude",
                    name: "Claude Code",
                    n: 0,
                    rss: 0.0,
                    cpu: 0.0,
                });
                out.len() - 1
            }
        };
        let u = &mut out[idx];
        if st.claude_procs > u.n {
            u.n = st.claude_procs;
            u.rss = u.rss.max(fin(st.claude_rss).max(0.0));
        }
    }
    out.sort_by(|a, b| b.n.cmp(&a.n).then(a.id.cmp(b.id)));
    out
}

fn agent_line(a: &AgentUse, w: usize) -> Line<'static> {
    let mut l = Ln::new();
    l.add(vec![icons::agent(a.id), Span::raw(" ")]);
    l.s(
        ui::pad(a.name, 14.min(w.saturating_sub(2))),
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    );
    l.s(format!(" ×{:<3}", a.n), ui::bold());
    if a.rss > 0.0 {
        l.s(format!("{:>7}", by(a.rss)), ui::muted());
    }
    if a.cpu > 0.005 {
        l.s(format!("  {:>5} cpu", pct(a.cpu)), ui::faint());
    }
    l.line()
}

/// `✻ claude ×3 1.2G · codex ×1`
fn agents_inline(ag: &[AgentUse]) -> Vec<Vec<Span<'static>>> {
    ag.iter()
        .map(|a| {
            let mut v = vec![
                icons::agent(a.id),
                Span::styled(format!(" {}", a.id), ui::fg(c::ACCENT())),
                Span::styled(format!(" ×{}", a.n), ui::bold()),
            ];
            if a.rss > 0.0 {
                v.push(Span::styled(format!(" {}", by(a.rss)), ui::muted()));
            }
            v
        })
        .collect()
}

fn state_style(s: &str) -> Style {
    match s.chars().next() {
        Some('R') => ui::fg(c::GREEN()).add_modifier(Modifier::BOLD),
        Some('D') => ui::fg(c::YELLOW()).add_modifier(Modifier::BOLD),
        Some('Z') => ui::fg(c::RED()).add_modifier(Modifier::BOLD),
        Some('T') | Some('t') => ui::fg(c::PEACH()),
        _ => ui::faint(),
    }
}

fn cpu_style(v: f64) -> Style {
    if fin(v) < 0.01 {
        ui::faint()
    } else {
        ui::fg(ui::heat(fr(v)))
    }
}

fn mem_style(frac: f64) -> Style {
    if fin(frac) < 0.005 {
        ui::faint()
    } else {
        ui::fg(ui::heat(fr(frac * 2.5)))
    }
}

/// What the COMMAND column shows: the name, then the arguments (the program
/// path dropped when it's the same program). Capped for the matcher.
fn cmd_text(p: &Proc) -> (String, usize) {
    let name = clean(p.name.trim());
    let cmd = clean(p.cmd.trim());
    let nl = name.chars().count();
    // kernel threads: ps shows `[name]`
    if cmd.is_empty() || (cmd.starts_with('[') && cmd.ends_with(']') && !name.is_empty()) {
        return (name, nl);
    }
    let (first, rest) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
    let base = first.rsplit('/').next().unwrap_or(first);
    let s = if name.is_empty() {
        return (cmd.chars().take(300).collect(), 0);
    } else if base == name || (nl >= 15 && base.starts_with(name.as_str())) {
        format!("{name} {}", rest.trim())
    } else {
        format!("{name} {cmd}")
    };
    (s.trim_end().chars().take(300).collect(), nl)
}

/// Spans for a command line: name bold (accent for agents, faint for kernel
/// threads), arguments muted, fuzzy matches underlined in the accent.
fn cmd_spans(text: &str, nl: usize, hl: &[usize], name_st: Style, w: usize) -> Line<'static> {
    let total = ui::width(text);
    let cut = total > w;
    let lim = if cut { w.saturating_sub(1) } else { w };
    let mut spans: Vec<Span<'static>> = Vec::new();
    let (mut cur, mut cur_st) = (String::new(), Style::new());
    let mut used = 0;
    let mut hi = hl.iter().peekable();
    for (i, ch) in text.chars().enumerate() {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + cw > lim {
            break;
        }
        used += cw;
        let mut st = if i < nl { name_st } else { ui::muted() };
        while hi.peek().is_some_and(|&&p| p < i) {
            hi.next();
        }
        if hi.peek() == Some(&&i) {
            st = st
                .fg(c::ACCENT())
                .add_modifier(Modifier::UNDERLINED | Modifier::BOLD);
        }
        if st != cur_st && !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut cur), cur_st));
        }
        cur_st = st;
        cur.push(ch);
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, cur_st));
    }
    if cut && w > 0 {
        spans.push(Span::styled("…", ui::faint()));
    }
    Line::from(spans)
}

fn name_style(p: &Proc) -> Style {
    if agent_of(p).is_some() {
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
    } else if p.cmd.trim().is_empty() && !p.name.is_empty() && p.rss <= 0.0 {
        ui::faint()
    } else {
        ui::bold()
    }
}

fn pid_num(p: &Proc) -> u64 {
    p.pid.trim().parse().unwrap_or(u64::MAX)
}

fn cmp_f(a: f64, b: f64) -> Ordering {
    fin(a).total_cmp(&fin(b))
}

/// `load 3.12 · 5m 2.80 · 15m 2.11`: the 1-minute average, then the
/// longer ones labelled so they don't read as other quantities.
fn load_items(st: &Stats) -> Vec<Span<'static>> {
    let lf = load_frac(st);
    vec![
        Span::styled("load ", ui::faint()),
        Span::styled(
            load(st.load[0]),
            ui::fg(ui::heat(lf)).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · 5m ", ui::faint()),
        Span::styled(load(st.load[1]), ui::muted()),
        Span::styled(" · 15m ", ui::faint()),
        Span::styled(load(st.load[2]), ui::muted()),
    ]
}

/// A value with its unit dimmed, so the number stands out: `11.6G` →
/// **11.6**G, `47.4K/s`, `44°C`, `54%`. Leading padding stays with the
/// number; anything that doesn't start with a number is left whole.
fn nu(s: &str, st: Style) -> Vec<Span<'static>> {
    let start = s.len() - s.trim_start().len();
    let body = &s[start..];
    let end = body
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(body.len());
    if end == 0 || end == body.len() || c::mono() {
        return vec![Span::styled(s.to_string(), st)];
    }
    vec![
        Span::styled(s[..start + end].to_string(), st),
        Span::styled(body[end..].to_string(), ui::muted()),
    ]
}

/// `used / total` with both units dimmed.
fn of(a: &str, b: &str, st: Style) -> Vec<Span<'static>> {
    let mut v = nu(a, st);
    v.push(Span::styled(" / ", ui::faint()));
    v.extend(nu(b, ui::muted()));
    v
}

// ------------------------------------------------------------------ GPU pieces

/// Series colours, the same everywhere: util, VRAM, power, temperature.
fn c_util() -> Color {
    c::MAUVE()
}
fn c_vram() -> Color {
    c::PINK()
}
fn c_power() -> Color {
    c::YELLOW()
}
fn c_temp() -> Color {
    c::RED()
}

fn gpu_vram(g: &Gpu) -> f64 {
    ratio(g.mem_used, g.mem_total)
}

fn gpu_pow(g: &Gpu) -> Option<f64> {
    match (ok(g.power), ok(g.power_limit)) {
        (Some(p), Some(l)) if l > 0.0 => Some(ratio(p, l)),
        _ => None,
    }
}

fn gpu_label(g: &Gpu) -> String {
    let n = clean(g.name.trim());
    if n.is_empty() {
        "GPU".into()
    } else {
        n
    }
}

fn gpu_idx(g: &Gpu) -> String {
    let i = clean(g.index.trim());
    if i.is_empty() {
        "?".into()
    } else {
        i
    }
}

/// `384/450W`, `384W` or —.
fn gpu_watts(g: &Gpu) -> Option<String> {
    match (ok(g.power), ok(g.power_limit)) {
        (Some(p), Some(l)) if l > 0.0 => Some(format!("{}/{}W", int(p), int(l))),
        (Some(p), _) => Some(format!("{}W", int(p))),
        _ => None,
    }
}

fn throttle_col(s: &str) -> Color {
    let s = s.to_ascii_lowercase();
    if s.contains("thermal") || s.contains("slowdown") || s.contains("hw") {
        c::RED()
    } else if s.contains("power") {
        c::YELLOW()
    } else {
        c::MUTED()
    }
}

/// The GPU meters (labels in the history chart's series colours): util with
/// the SM clock, VRAM used / total, power draw / limit, temperature with the
/// fan, and the memory bus with its clock.
fn gpu_meters(g: &Gpu, w: usize, n: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let sm = match (ok(g.clock_sm), ok(g.clock_sm_max)) {
        (Some(s), Some(m)) if m > 0.0 => vec![
            Span::styled("SM ", ui::faint()),
            Span::styled(int(s), ui::bold()),
            Span::styled(format!(" / {} MHz", int(m)), ui::muted()),
        ],
        (Some(s), _) => vec![
            Span::styled("SM ", ui::faint()),
            Span::styled(int(s), ui::bold()),
            Span::styled(" MHz", ui::muted()),
        ],
        _ => vec![],
    };
    // an Apple GPU: what the I/O Registry gives (no power, temperature or
    // clock without sudo): its use, the memory it shares, renderer, tiler
    if g.renderer.is_some() || g.tiler.is_some() {
        let cores = g
            .cores
            .map(|c| vec![Span::styled(format!("{c} cores"), ui::muted())])
            .unwrap_or_default();
        rows.push(M::new("UTIL", Some(fr(g.util))).col(c_util()).detail(cores));
        let mut shared = of(&by(g.mem_used), &by(g.mem_total), ui::bold());
        if let Some(a) = g.mem_alloc.filter(|a| *a > 0.0) {
            shared.push(Span::styled(format!(" · {} allocated", by(a)), ui::faint()));
        }
        let mem = (fin(g.mem_total) > 0.0).then(|| gpu_vram(g));
        rows.push(M::new("MEMORY", mem).col(c_vram()).detail(shared));
        if let Some(r) = g.renderer {
            rows.push(M::new("RENDERER", Some(fr(r))).col(c_power()));
        }
        if let Some(t) = g.tiler {
            rows.push(M::new("TILER", Some(fr(t))).col(c_temp()));
        }
        rows.truncate(n);
        return meters(rows, w);
    }
    rows.push(M::new("UTIL", Some(fr(g.util))).col(c_util()).detail(sm));
    let vram = (fin(g.mem_total) > 0.0).then(|| gpu_vram(g));
    rows.push(M::new("VRAM", vram).col(c_vram()).detail(of(
        &by(g.mem_used),
        &by(g.mem_total),
        ui::bold(),
    )));
    let watts = match (ok(g.power), ok(g.power_limit)) {
        (Some(p), Some(l)) if l > 0.0 => vec![
            Span::styled(int(p), ui::bold()),
            Span::styled(format!(" / {} W", int(l)), ui::muted()),
        ],
        (Some(p), _) => vec![Span::raw(format!("{} W", int(p)))],
        _ => vec![],
    };
    rows.push(M::new("POWER", gpu_pow(g)).col(c_power()).detail(watts));
    let mut tdet = vec![];
    if let Some(fan) = ok(g.fan) {
        tdet.extend(kv("fan", pct(fan / 100.0), Style::new()));
    }
    if let Some(m) = ok(g.temp_mem) {
        if !tdet.is_empty() {
            tdet.push(Span::raw("  "));
        }
        tdet.extend(kv("mem", deg(m), ui::fg(temp_heat(m))));
    }
    let tm = match ok(g.temp) {
        Some(t) => M::new("TEMP", Some((t / 100.0).clamp(0.0, 1.0)))
            .val(deg(t), ui::fg(temp_heat(t)).add_modifier(Modifier::BOLD)),
        None => M::new("TEMP", None),
    };
    rows.push(tm.col(c_temp()).detail(tdet));
    if let Some(m) = ok(g.mem_util) {
        let mut d = vec![];
        if let Some(cm) = ok(g.clock_mem) {
            d = vec![
                Span::styled("mem ", ui::faint()),
                Span::raw(int(cm)),
                Span::styled(" MHz", ui::muted()),
            ];
        }
        rows.push(M::new("MEM BUS", Some(fr(m))).detail(d));
    }
    rows.truncate(n);
    meters(rows, w)
}

/// Everything else nvidia-smi says, as (key, value) rows.
fn gpu_kv(g: &Gpu) -> Vec<(&'static str, Vec<Span<'static>>)> {
    let mut v: Vec<(&'static str, Vec<Span<'static>>)> = Vec::new();
    match (ok(g.clock_sm), ok(g.clock_sm_max)) {
        (Some(s), Some(m)) if m > 0.0 => v.push((
            "SM clock",
            vec![
                Span::styled(int(s), ui::bold()),
                Span::styled(format!(" / {} MHz", int(m)), ui::muted()),
            ],
        )),
        (Some(s), _) => v.push((
            "SM clock",
            vec![
                Span::styled(int(s), ui::bold()),
                Span::styled(" MHz", ui::muted()),
            ],
        )),
        _ => {}
    }
    if let Some(m) = ok(g.clock_mem) {
        v.push((
            "mem clock",
            vec![Span::raw(int(m)), Span::styled(" MHz", ui::muted())],
        ));
    }
    if !g.pstate.trim().is_empty() {
        let p = clean(g.pstate.trim());
        let hint = match p.as_str() {
            "P0" => " max performance",
            "P2" | "P3" => " compute",
            "P8" => " idle",
            "P12" => " min power",
            _ => "",
        };
        v.push((
            "P-state",
            vec![Span::styled(p, ui::bold()), Span::styled(hint, ui::faint())],
        ));
    }
    if !g.pcie.trim().is_empty() {
        v.push(("PCIe", vec![Span::raw(clean(g.pcie.trim()))]));
    }
    if let Some(c) = g.cores {
        v.push(("cores", vec![Span::styled(c.to_string(), ui::bold())]));
    }
    if g.vendor == "apple" {
        v.push((
            "memory",
            vec![Span::styled("shared with the CPU (unified)", ui::muted())],
        ));
    }
    match (ok(g.enc), ok(g.dec)) {
        (None, None) => {}
        (e, d) => {
            let p = |x: Option<f64>| match x {
                Some(x) => Span::styled(pct(x), cpu_style(x)),
                None => dash(),
            };
            v.push((
                "enc / dec",
                vec![p(e), Span::styled(" / ", ui::faint()), p(d)],
            ));
        }
    }
    if let Some(fan) = ok(g.fan) {
        v.push(("fan", vec![Span::raw(pct(fan / 100.0))]));
    }
    if let Some(m) = ok(g.temp_mem) {
        v.push(("mem temp", vec![Span::styled(deg(m), ui::fg(temp_heat(m)))]));
    }
    if g.vendor == "nvidia" || !g.throttle.is_empty() {
        let mut it = Vec::new();
        for (i, t) in g.throttle.iter().take(4).enumerate() {
            if i > 0 {
                it.push(Span::styled(", ", ui::faint()));
            }
            it.push(Span::styled(clean(t), ui::fg(throttle_col(t))));
        }
        if it.is_empty() {
            it.push(Span::styled("none", ui::fg(c::GREEN())));
        }
        v.push(("throttle", it));
    }
    if let Some(e) = g.ecc {
        v.push((
            "ECC",
            if e > 0 {
                vec![Span::styled(
                    format!("{e} uncorrected"),
                    ui::fg(c::RED()).add_modifier(Modifier::BOLD),
                )]
            } else {
                vec![Span::styled("0 errors", ui::muted())]
            },
        ));
    }
    if !g.bus.trim().is_empty() {
        // `00000000:01:00.0` → `01:00.0` (domain 0 is the only one most boxes have)
        let b = clean(g.bus.trim());
        let b = b
            .strip_prefix("00000000:")
            .or_else(|| b.strip_prefix("0000:"))
            .unwrap_or(&b);
        v.push(("bus", vec![Span::styled(b.to_string(), ui::faint())]));
    }
    v
}

/// Aligned `key  value` rows in one or two columns (column-major).
fn kv_grid(
    rows: Vec<(&'static str, Vec<Span<'static>>)>,
    w: usize,
    h: usize,
) -> Vec<Line<'static>> {
    if rows.is_empty() || w == 0 || h == 0 {
        return vec![];
    }
    let kw = rows.iter().map(|r| ui::width(r.0)).max().unwrap_or(4) + 2;
    let ncols = if rows.len() > h && w >= 2 * (kw + 12) + 3 {
        2
    } else {
        1
    };
    // two columns side by side, not spread across a wide card
    let w = if ncols == 2 {
        w.min(2 * (kw + 34) + 3)
    } else {
        w.min(kw + 48)
    };
    let cw = (w - 3 * (ncols - 1)) / ncols;
    let per = rows.len().div_ceil(ncols);
    let cells: Vec<Vec<Span<'static>>> = rows
        .into_iter()
        .map(|(k, v)| {
            let mut s = vec![Span::styled(ui::pad(k, kw), ui::faint())];
            s.extend(v);
            fit(s, cw)
        })
        .collect();
    (0..per.min(h))
        .map(|r| {
            let mut l = Ln::new();
            for c in 0..ncols {
                if let Some(cell) = cells.get(c * per + r) {
                    if c > 0 {
                        l.s("   ", Style::new());
                    }
                    l.add(cell.clone());
                }
            }
            l.line()
        })
        .collect()
}

/// The GPU history: util as an area, VRAM, power (% of the limit) and
/// temperature (°C on the same 0–100 scale) as lines.
fn gpu_chart(f: &mut Frame, r: Rect, g: &Gpu, period: f64, axes: bool) {
    let temp: Vec<f64> = g.temp_hist.iter().map(|t| fin(*t) / 100.0).collect();
    let mut s = vec![Ser::area(&g.hist, CP::Solid(c_util()))];
    if fin(g.mem_total) > 0.0 {
        s.push(Ser::line(&g.mem_hist, c_vram()));
    }
    if gpu_pow(g).is_some() {
        s.push(Ser::line(&g.power_hist, c_power()));
    }
    if !temp.is_empty() {
        s.push(Ser::line(&temp, c_temp()));
    }
    let mut p = Plot::new(s, 1.0);
    if axes {
        p = p.pct_axis();
        if r.height >= 8 {
            p = p.time(period);
        }
    }
    chart::plot(f, r, &p);
}

fn gpu_procs_of<'a>(st: &'a Stats, g: &Gpu) -> Vec<&'a GpuProc> {
    st.gpu_procs
        .iter()
        .filter(|p| p.gpu == g.index || (p.gpu == g.bus && !g.bus.is_empty()))
        .collect()
}

/// The GPU process table: PID USER [GPU] TYPE SM% MEM% AVG VRAM CPU% UP COMMAND.
fn gpu_proc_lines(
    st: &Stats,
    ps: &[&GpuProc],
    w: usize,
    n: usize,
    show_gpu: bool,
) -> Vec<Line<'static>> {
    if n == 0 || w == 0 {
        return vec![];
    }
    let head = ui::faint().add_modifier(Modifier::BOLD);
    // (title, width, right-aligned); dropped from the end of `drop` when narrow
    let mut colv: Vec<(&str, usize, bool)> = vec![("PID", 7, true), ("USER", 9, false)];
    if show_gpu {
        colv.push(("GPU", 3, true));
    }
    colv.extend([
        ("TYPE", 4, false),
        ("SM%", 4, true),
        ("AVG", 4, true),
        ("MEM%", 4, true),
        ("VRAM", 7, true),
        ("CPU%", 5, true),
        ("UPTIME", 6, true),
    ]);
    let need = |v: &[(&str, usize, bool)]| v.iter().map(|c| c.1 + 1).sum::<usize>() + 14;
    for d in ["UPTIME", "MEM%", "USER", "AVG", "CPU%", "TYPE"] {
        if need(&colv) <= w {
            break;
        }
        colv.retain(|c| c.0 != d);
    }
    let mut h = Ln::new();
    for (t, cw, r) in &colv {
        if *r {
            h.s(format!("{t:>cw$} "), head);
        } else {
            h.s(format!("{} ", ui::pad(t, *cw)), head);
        }
    }
    h.s("COMMAND", head);
    let mut out = vec![h.line()];
    let pf = |x: Option<f64>| match ok(x) {
        Some(v) => (pct(v), cpu_style(v)),
        None => ("—".to_string(), ui::faint()),
    };
    for p in ps.iter().take(n - 1) {
        let mut l = Ln::new();
        for (t, cw, _) in &colv {
            let (s, sty) = match *t {
                "PID" => (clean(&p.pid), ui::muted()),
                "USER" => (clean(&p.user), ui::muted()),
                "GPU" => (clean(&p.gpu), ui::fg(c_util())),
                "TYPE" => (
                    clean(&p.kind),
                    if p.kind.contains('C') {
                        ui::fg(c::GREEN())
                    } else {
                        ui::faint()
                    },
                ),
                "SM%" => pf(p.sm),
                "AVG" => pf(p.sm_avg),
                "MEM%" => pf(p.mem_util),
                "VRAM" => (by(p.mem), ui::fg(c_vram()).add_modifier(Modifier::BOLD)),
                "CPU%" => match ok(p.cpu) {
                    Some(v) => (
                        format!("{:.1}", (v * 100.0).clamp(0.0, 9999.0)),
                        cpu_style(v),
                    ),
                    None => ("—".into(), ui::faint()),
                },
                _ => match p.age {
                    Some(a) => (age(a), ui::muted()),
                    None => ("—".into(), ui::faint()),
                },
            };
            if matches!(*t, "USER" | "TYPE") {
                l.s(format!("{} ", ui::pad(&s, *cw)), sty);
            } else if matches!(*t, "PID" | "GPU" | "UPTIME") {
                l.s(format!("{:>cw$} ", ui::trunc(&s, *cw)), sty);
            } else {
                // numbers right-aligned, units dimmed
                l.add(nu(&format!("{:>cw$} ", ui::trunc(&s, *cw)), sty));
            }
        }
        // the name bold, then the command line's arguments
        let name = clean(p.name.trim());
        let cmd = clean(p.cmd.trim());
        let args = cmd
            .split_once(' ')
            .map(|x| x.1.to_string())
            .unwrap_or_default();
        // the session it's from, first (telemetry::owner)
        if !p.owner.is_empty() {
            l.s(
                format!("◆ {} ", p.owner),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            );
        }
        let txt = if args.is_empty() {
            name.clone()
        } else {
            format!("{name} {args}")
        };
        let nl = name.chars().count();
        let agent = st
            .procs
            .iter()
            .any(|x| x.pid == p.pid && agent_of(x).is_some());
        let nst = if agent {
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
        } else {
            ui::bold()
        };
        let room = w.saturating_sub(l.w);
        l.add(cmd_spans(&txt, nl, &[], nst, room).spans);
        out.push(l.line());
    }
    out
}

fn no_gpu_reason(st: &Stats) -> String {
    match &st.gpu_note {
        Some(n) if !n.trim().is_empty() => clean(n.trim()),
        _ if st.has_gpu_tool => "nvidia-smi reports no GPUs".into(),
        _ => "no GPU (no nvidia-smi or rocm-smi)".into(),
    }
}

/// `1× RTX 4090` or `3 GPUs` (mixed models), the names short (`short_gpu`).
fn gpu_models(st: &Stats) -> Option<String> {
    let g = st.gpus.first()?;
    let same = st.gpus.iter().all(|x| x.name == g.name);
    Some(if same {
        format!("{}× {}", st.gpus.len(), short_gpu(&gpu_label(g)))
    } else {
        format!("{} GPUs", st.gpus.len())
    })
}

/// A GPU's name for headers and titles: without the vendor (its badge says
/// it) and the edition (`RTX PRO 6000 Blackwell Max-Q Workstation Edition` →
/// `RTX PRO 6000 Blackwell Max-Q`). The GPU view keeps the full name.
fn short_gpu(name: &str) -> String {
    let mut n = name.trim();
    for v in ["NVIDIA ", "AMD ", "Intel(R) ", "Intel ", "GeForce "] {
        n = n.strip_prefix(v).unwrap_or(n);
    }
    for e in [
        " Workstation Edition",
        " Server Edition",
        " Edition",
        " Graphics",
    ] {
        n = n.strip_suffix(e).unwrap_or(n);
    }
    n.trim().to_string()
}

/// `2× Intel Xeon Gold 6544Y` on a dual-socket machine, else the model.
fn cpu_title(st: &Stats) -> String {
    let m = clean(st.cpu_model.trim());
    if st.sockets > 1 && !m.is_empty() {
        format!("{}× {m}", st.sockets)
    } else {
        m
    }
}

/// One line for a GPU: `0 util ███ 54% · vram ██ 21% · 44°C · 25/450W`.
fn gpu_line(g: &Gpu, w: usize) -> Line<'static> {
    let mut l = Ln::new();
    l.s(
        format!("{:<2}", gpu_idx(g)),
        ui::fg(c_util()).add_modifier(Modifier::BOLD),
    );
    let mut tail = Ln::new();
    if let Some(t) = ok(g.temp) {
        tail.s(format!("  {}", deg(t)), ui::fg(temp_heat(t)));
    }
    if let Some(p) = gpu_watts(g) {
        tail.s(format!("  {p}"), ui::muted());
    }
    let bw = (w.saturating_sub(l.w + 2 * 6 + 6 + tail.w) / 2).clamp(3, 30);
    let u = fr(g.util);
    l.add(gbar(u, bw));
    l.s(
        format!(" {:>4}", pct(u)),
        ui::fg(ui::heat(u)).add_modifier(Modifier::BOLD),
    );
    let m = gpu_vram(g);
    l.s("  vram ", ui::faint());
    l.add(gbar(m, bw));
    l.s(format!(" {:>4}", pct(m)), ui::fg(ui::heat(m)));
    if l.w + tail.w <= w {
        l.add(tail.spans());
    }
    l.line()
}

// ------------------------------------------------------------------ strip & header

/// The sub-view tab strip: `‹ ▌b31 ›  Fleet · Overview · CPU · …   updated 1s ago`.
/// Returns each tab's rect for mouse clicks.
/// Between the strip's targets: ` · `, or one space between bare letters.
fn strip_gap(v: usize) -> &'static str {
    if v >= 2 {
        " "
    } else {
        " · "
    }
}

pub fn strip(
    f: &mut Frame,
    r: Rect,
    sub: Sub,
    alias: &str,
    color: Color,
    updated: Option<Instant>,
    pages: &[Sub],
) -> Vec<(Rect, Sub)> {
    let r = area(f, r);
    let mut hits = Vec::new();
    if r.width == 0 || r.height == 0 {
        return hits;
    }
    let r = band(r, 0, 1);
    let w = r.width as usize;
    let alias = clean(alias);
    let fresh: (String, Style) = match updated {
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s >= 15 {
                (
                    format!("stale · updated {}", ui::ago(s)),
                    ui::fg(c::YELLOW()),
                )
            } else {
                (format!("● live · {}", ui::ago(s)), ui::fg(c::GREEN()))
            }
        }
        None => (
            format!("{} waiting for stats", ui::spinner(epoch())),
            ui::faint(),
        ),
    };
    let fw = ui::width(&fresh.0);
    let chip_full = 4 + 1 + ui::width(&alias);
    let tabs_w = |v: usize| -> usize {
        let names: usize = pages
            .iter()
            .map(|s| match v {
                0 => ui::width(s.name()),
                1 => ui::width(s.short()),
                _ => 1,
            })
            .sum();
        names + ui::width(strip_gap(v)) * (pages.len().saturating_sub(1))
    };
    // the richest layout that fits: names → short names → letters; freshness goes first
    let mut choice = None;
    'outer: for v in 0..3 {
        for with_fresh in [true, false] {
            let need = chip_full + 2 + tabs_w(v) + if with_fresh { 3 + fw } else { 0 };
            if need <= w {
                choice = Some((v, with_fresh, true));
                break 'outer;
            }
        }
    }
    let (v, with_fresh, arrows) = choice.unwrap_or((2, false, false));
    let mut l = Ln::new();
    if arrows {
        l.s("‹ ", ui::faint());
    }
    l.add(chip(&alias, color));
    if arrows {
        l.s(" ›", ui::faint());
    }
    l.s("  ", Style::new());
    for (i, s) in pages.iter().enumerate() {
        if i > 0 {
            l.s(strip_gap(v), ui::faint());
        }
        let label = match v {
            0 => s.name().to_string(),
            1 => s.short().to_string(),
            _ => s.name().chars().next().unwrap_or(' ').to_string(),
        };
        let x0 = l.w;
        let on = *s == sub;
        let base = if on {
            if c::mono() {
                Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
            } else {
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
            }
        } else {
            ui::muted()
        };
        let mut ch = label.chars();
        if let Some(first) = ch.next() {
            l.s(first.to_string(), base.add_modifier(Modifier::UNDERLINED));
            let rest: String = ch.collect();
            if !rest.is_empty() {
                l.s(rest, base);
            }
        }
        let lw = l.w - x0;
        if x0 < w {
            hits.push((
                Rect {
                    x: r.x + clamp16(x0),
                    y: r.y,
                    width: clamp16(lw.min(w - x0)),
                    height: 1,
                },
                *s,
            ));
        }
    }
    put(f, r, vec![l.line()]);
    if with_fresh && fw < w {
        put(
            f,
            cols(r, w - fw, fw),
            vec![Line::from(Span::styled(fresh.0, fresh.1))],
        );
    }
    hits
}

/// The machine strip: OS logo, alias, hostname, OS, kernel, CPU, GPU,
/// uptime, load, tasks (whatever fits, by priority).
pub fn header(f: &mut Frame, r: Rect, alias: &str, color: Color, st: &Stats) {
    let r = area(f, r);
    if r.is_empty() {
        return;
    }
    put(
        f,
        band(r, 0, 1),
        vec![header_line(alias, color, st, r.width as usize)],
    );
}

/// A distribution (or OS) as its logo is drawn: name, brand colour, Nerd
/// Font glyph and the generic emoji.
struct Distro {
    name: &'static str,
    color: Color,
    nerd: &'static str,
    emoji: &'static str,
}

/// Which distribution `os` (`/etc/os-release` PRETTY_NAME, `sw_vers`) is,
/// from the one OS table (`logo::os_info`).
fn distro_of(os: &str, platform: &str) -> Distro {
    match crate::logo::os_info(&format!("{os} {platform}")) {
        Some(i) => Distro {
            name: i.name,
            color: i.color,
            nerd: i.nerd,
            emoji: i.family.emoji(),
        },
        None => Distro {
            name: "",
            color: Color::Rgb(0x7f, 0x84, 0x9c),
            nerd: "\u{f108}",
            emoji: icons::Os::Unknown.emoji(),
        },
    }
}

/// `24.04` from `Ubuntu 24.04.3 LTS`, `27` from `macOS 27.0`.
fn os_version(os: &str) -> String {
    let Some(tok) = os
        .split_whitespace()
        .find(|t| t.starts_with(|c: char| c.is_ascii_digit()))
    else {
        return String::new();
    };
    let parts: Vec<&str> = tok.split('.').collect();
    match parts.as_slice() {
        [major, minor, ..] if major.len() >= 2 && minor.len() == 2 => format!("{major}.{minor}"),
        [major, ..] => major.to_string(),
        _ => String::new(),
    }
}

/// The machine's OS as its logo, prominently: a pill in the distribution's
/// colour (`  Ubuntu 24.04 ` in Ubuntu orange, the Nerd Font logo inside
/// with `icons = nerd`, the emoji before it otherwise), bold text under
/// NO_COLOR. `version` adds the release.
pub(crate) fn os_badge(os: &str, platform: &str, version: bool) -> Vec<Span<'static>> {
    let d = distro_of(os, platform);
    let mut name = d.name.to_string();
    if version {
        let v = os_version(os);
        if !v.is_empty() {
            name = if name.is_empty() {
                v
            } else {
                format!("{name} {v}")
            };
        }
    }
    if name.is_empty() {
        return vec![icons::os(icons::Os::Unknown)];
    }
    if c::mono() || icons::style() == icons::Style_::Text {
        return vec![Span::styled(name, ui::bold())];
    }
    // light brand colours take dark text
    let ink = match d.color {
        Color::Rgb(r, g, b) if 0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64 > 150.0 => {
            Color::Rgb(20, 20, 30)
        }
        _ => Color::Rgb(255, 255, 255),
    };
    let pill = Style::new()
        .fg(ink)
        .bg(d.color)
        .add_modifier(Modifier::BOLD);
    match icons::style() {
        icons::Style_::Nerd => vec![Span::styled(format!(" {} {name} ", d.nerd), pill)],
        _ => vec![
            Span::raw(d.emoji),
            Span::raw(" "),
            Span::styled(format!(" {name} "), pill),
        ],
    }
}

fn header_line(alias: &str, color: Color, st: &Stats, w: usize) -> Line<'static> {
    let f = ui::faint();
    let mut lead = Vec::new();
    if st.samples > 0 || !st.os.trim().is_empty() {
        lead.extend(os_badge(&st.os, &st.platform, true));
        lead.push(Span::raw(" "));
    }
    lead.extend(chip(alias, color));
    let mut items: Vec<(u8, Vec<Span<'static>>)> = vec![(0, lead)];
    let host = clean(st.hostname.trim());
    if !host.is_empty() && host != alias {
        items.push((2, vec![Span::styled(host, ui::bold())]));
    }
    if let Some(e) = stale_note(st) {
        items.push((0, e.spans));
    }
    // the OS's own name only when the badge can't say it (an unknown distro)
    if !st.os.trim().is_empty() && distro_of(&st.os, &st.platform).name == "Linux" {
        items.push((8, vec![Span::styled(clean(st.os.trim()), ui::muted())]));
    }
    if !st.kernel.trim().is_empty() {
        items.push((7, vec![Span::styled(clean(st.kernel.trim()), f)]));
    }
    let mut cpu = cpu_title(st);
    let n = st.ncpu.max(st.cores.len());
    if n > 0 {
        cpu = if cpu.is_empty() {
            format!("{n} threads")
        } else {
            format!("{cpu} · {n} threads")
        };
    }
    if !cpu.is_empty() {
        items.push((4, vec![Span::styled(cpu, ui::muted())]));
    }
    if let (Some(g), Some(m)) = (st.gpus.first(), gpu_models(st)) {
        items.push((
            2,
            vec![
                icons::gpu(&g.vendor),
                Span::styled(format!(" {m}"), ui::fg(c_util())),
            ],
        ));
    } else if st.samples > 0 && (st.gpu_note.is_some() || st.has_gpu_tool) {
        items.push((5, vec![Span::styled("no GPU", ui::faint())]));
    }
    if st.uptime > 0 {
        items.push((1, kv("up", ui::dur(st.uptime), ui::muted())));
    }
    if st.samples > 0 {
        items.push((1, load_items(st)));
    }
    if st.procs_total > 0 {
        let mut t = format!("{} procs", count(st.procs_total));
        if st.threads_total > 0 {
            t.push_str(&format!(" · {} threads", count(st.threads_total)));
        }
        items.push((6, vec![Span::styled(t, f)]));
    }
    if let Some(u) = st.users_logged {
        items.push((
            8,
            vec![Span::styled(
                format!("{u} user{}", if u == 1 { "" } else { "s" }),
                f,
            )],
        ));
    }
    Line::from(join_prio(items, w))
}

// ------------------------------------------------------------------ overview

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sec {
    Cpu,
    Gpu,
    Mem,
    Net,
    Disk,
    /// network and disks in one panel (narrow layouts)
    Io,
    Procs,
    Agents,
}

/// A layout row: its sections with width weights, a height weight, and a
/// drop priority (when the minimums don't fit, the highest goes first).
struct Band {
    secs: Vec<(Sec, u16)>,
    weight: u16,
    prio: u8,
}

/// The smallest outer height a section is useful at.
fn sec_min(s: Sec, st: &Stats) -> usize {
    // content rows, plus the border and padding of the current scale
    chrome_h()
        + match s {
            Sec::Cpu => 3,
            Sec::Gpu => st.gpus.len().clamp(1, 4),
            Sec::Mem => 3,
            Sec::Net | Sec::Disk => 2,
            Sec::Io => 3,
            Sec::Procs => 3,
            Sec::Agents => 1,
        }
}

/// Where each section goes for a width and focus.
fn plan(w: usize, st: &Stats, focus: Focus, procs_on: bool, agents: bool) -> Vec<Band> {
    let gpu = !st.gpus.is_empty();
    let b = |secs: Vec<(Sec, u16)>, weight: u16, prio: u8| Band { secs, weight, prio };
    let wide = w >= 140;
    let narrow = w < 80;
    let procs = procs_on || focus == Focus::Procs;
    // agents show in the process panel's border; their own panel only when
    // the process list is off
    let proc_row = |weight: u16| b(vec![(Sec::Procs, 1)], weight, 9);
    let io = |v: &mut Vec<(Sec, u16)>| {
        if wide {
            v.push((Sec::Net, 1));
            v.push((Sec::Disk, 1));
        } else {
            v.push((Sec::Io, 1));
        }
    };
    let mut out: Vec<Band> = Vec::new();
    if narrow {
        // one column: the focus big, the rest compact
        let mut order = vec![Sec::Cpu];
        if gpu {
            order.push(Sec::Gpu);
        }
        order.extend([Sec::Mem, Sec::Io]);
        for (i, s) in order.into_iter().enumerate() {
            let big = matches!(
                (focus, s),
                (Focus::Cpu, Sec::Cpu) | (Focus::Mem, Sec::Mem) | (Focus::Gpu, Sec::Gpu)
            );
            let weight = match focus {
                Focus::All => [4, 3, 3, 2][i.min(3)],
                Focus::Procs => 0,
                _ if big => 12,
                _ => 0,
            };
            out.push(b(vec![(s, 1)], weight, if big { 0 } else { 2 + i as u8 }));
        }
        if procs {
            out.push(b(
                vec![(Sec::Procs, 1)],
                if focus == Focus::Procs { 12 } else { 3 },
                if focus == Focus::Procs { 0 } else { 9 },
            ));
        }
        return out;
    }
    match focus {
        Focus::All => {
            let mut a = vec![(Sec::Cpu, 6)];
            if gpu {
                a.push((Sec::Gpu, 4));
            }
            out.push(b(a, 9, 0));
            let mut m = vec![(Sec::Mem, 1)];
            io(&mut m);
            if !procs && agents && wide {
                m.push((Sec::Agents, 1));
            }
            out.push(b(m, 6, 1));
            if procs {
                out.push(proc_row(6));
            }
        }
        Focus::Procs => {
            let mut a = vec![(Sec::Cpu, 1), (Sec::Mem, 1)];
            if gpu {
                a.push((Sec::Gpu, 1));
            }
            if wide {
                a.push((Sec::Io, 1));
            }
            out.push(b(a, 3, 2));
            out.push(proc_row(14));
        }
        f => {
            let big = match f {
                Focus::Cpu => Sec::Cpu,
                Focus::Mem => Sec::Mem,
                _ if gpu => Sec::Gpu,
                _ => Sec::Cpu,
            };
            out.push(b(vec![(big, 1)], 12, 0));
            let mut rest: Vec<(Sec, u16)> = [Sec::Cpu, Sec::Mem, Sec::Gpu]
                .into_iter()
                .filter(|s| *s != big && (*s != Sec::Gpu || gpu))
                .map(|s| (s, 1))
                .collect();
            rest.push((Sec::Io, 1));
            out.push(b(rest, 5, 1));
            if procs {
                out.push(proc_row(5));
            }
        }
    }
    out
}

/// Heights for the bands: minimums (dropping bands by priority until they
/// fit), then the rest by weight. 0 = left out.
fn alloc_bands(h: usize, bands: &[Band], st: &Stats) -> Vec<usize> {
    let mins: Vec<usize> = bands
        .iter()
        .map(|b| {
            b.secs
                .iter()
                .map(|(s, _)| sec_min(*s, st))
                .max()
                .unwrap_or(3)
        })
        .collect();
    let mut on = vec![true; bands.len()];
    while on.iter().any(|x| *x)
        && mins
            .iter()
            .zip(&on)
            .filter(|(_, o)| **o)
            .map(|(m, _)| m)
            .sum::<usize>()
            > h
    {
        // drop the highest-priority-number band still on
        let i = (0..bands.len())
            .filter(|&i| on[i])
            .max_by_key(|&i| (bands[i].prio, i))
            .unwrap_or(0);
        on[i] = false;
    }
    let mut hs: Vec<usize> = mins
        .iter()
        .zip(&on)
        .map(|(m, o)| if *o { *m } else { 0 })
        .collect();
    let used: usize = hs.iter().sum();
    let left = h.saturating_sub(used);
    let ws: Vec<u16> = bands
        .iter()
        .zip(&on)
        .map(|(b, o)| if *o { b.weight } else { 0 })
        .collect();
    let tot: usize = ws.iter().map(|w| *w as usize).sum();
    if tot > 0 {
        let mut given = 0;
        for (i, wt) in ws.iter().enumerate() {
            let add = (left * *wt as usize).checked_div(tot).unwrap_or(0);
            hs[i] += add;
            given += add;
        }
        if let Some(i) = (0..hs.len()).rev().find(|&i| ws[i] > 0) {
            hs[i] += left - given;
        }
    } else if let Some(i) = (0..hs.len()).rev().find(|&i| on[i]) {
        hs[i] += left;
    }
    hs
}

/// The all-in-one machine page with its header (the Fleet view's machine
/// pane uses this).
pub fn overview(f: &mut Frame, r: Rect, alias: &str, color: Color, st: &Stats, t0: Instant) {
    let r = area(f, r);
    if r.is_empty() {
        return;
    }
    put(
        f,
        band(r, 0, 1),
        vec![header_line(alias, color, st, r.width as usize)],
    );
    overview_body(f, band(r, 1, r.height as usize), st, t0, Focus::All, true);
}

/// The overview's panels: CPU, GPU, memory, network, disks, processes and
/// agents, sized by `focus` (the focused resource gets most of the room, the
/// others compact meters); `procs_on` false leaves the process list out.
pub fn overview_body(
    f: &mut Frame,
    r: Rect,
    st: &Stats,
    t0: Instant,
    focus: Focus,
    procs_on: bool,
) {
    let r = area(f, r);
    if r.is_empty() || waiting(f, r, st, t0) {
        return;
    }
    let ag = agents_running(st);
    let bands = plan(r.width as usize, st, focus, procs_on, !ag.is_empty());
    let hs = alloc_bands(r.height as usize, &bands, st);
    let mut y = 0;
    for (b, h) in bands.iter().zip(hs) {
        if h == 0 {
            continue;
        }
        let row = band(r, y, h);
        y += h;
        let ws: Vec<u16> = b.secs.iter().map(|x| x.1).collect();
        let widths = split(row.width as usize, &ws);
        let mut x = 0;
        for ((s, _), cw) in b.secs.iter().zip(widths) {
            let sr = cols(row, x, cw);
            x += cw;
            let big = matches!(
                (focus, s),
                (Focus::Cpu, Sec::Cpu) | (Focus::Mem, Sec::Mem) | (Focus::Gpu, Sec::Gpu)
            );
            match s {
                Sec::Cpu => sec_cpu(f, sr, st),
                Sec::Gpu => sec_gpu(f, sr, st, big),
                Sec::Mem => sec_mem(f, sr, st),
                Sec::Net => sec_net(f, sr, st),
                Sec::Disk => sec_disk(f, sr, st),
                Sec::Io => sec_io(f, sr, st),
                Sec::Procs => sec_procs(
                    f,
                    sr,
                    st,
                    if focus == Focus::Mem {
                        Sort::Mem
                    } else {
                        Sort::Cpu
                    },
                ),
                Sec::Agents => sec_agents(f, sr, &ag),
            }
        }
    }
}

/// `usage ██████···· 54%   usr 40% · sys 10% · iowait 3% · load 3.12 2.80 2.11`
fn cpu_head(st: &Stats, w: usize, prec: bool) -> Line<'static> {
    let v = fr(st.cpu);
    let mut l = Ln::new();
    let bw = (w * 45 / 100).clamp(4, 60).min(w.saturating_sub(6));
    l.add(gbar(v, bw));
    l.s(
        format!(" {:>5}", if prec { pct1(v) } else { pct(v) }),
        ui::fg(ui::heat(v)).add_modifier(Modifier::BOLD),
    );
    let mut items: Vec<(u8, Vec<Span<'static>>)> = Vec::new();
    let parts = cpu_parts(st);
    if parts.len() > 1 {
        let vals: Vec<String> = parts
            .iter()
            .map(|p| if prec { pct1(p.frac) } else { pct(p.frac) })
            .collect();
        for (i, it) in legend(&parts, &vals).into_iter().enumerate() {
            items.push((if i < 2 { 0 } else { 2 }, it));
        }
    }
    items.push((1, load_items(st)));
    let rest = join_prio(items, w.saturating_sub(l.w + 3));
    if !rest.is_empty() {
        l.s("   ", Style::new());
        l.add(rest);
    }
    l.line()
}

/// The total CPU history as a heat-coloured area (on a 10 / 20 / 50 / 100 %
/// scale, whichever its peak needs).
fn cpu_area(f: &mut Frame, r: Rect, st: &Stats) {
    let axes = r.width >= 40 && r.height >= 4;
    let top = chart::pct_top(avg_peak_frac(&st.cpu_hist).1);
    let mut p = Plot::new(vec![Ser::area(&st.cpu_hist, CP::Heat)], top);
    if axes {
        p = p.pct_axis();
        if r.height >= 8 {
            p = p.time(period(st));
        }
    }
    chart::plot(f, r, &p);
}

fn sec_cpu(f: &mut Frame, r: Rect, st: &Stats) {
    let n = st.ncpu.max(st.cores.len());
    let mut items = Vec::new();
    let model = cpu_title(st);
    if !model.is_empty() {
        items.push(vec![Span::styled(ui::trunc(&model, 34), ui::faint())]);
    }
    if n > 0 {
        items.push(vec![Span::styled(format!("{n} threads"), ui::faint())]);
    }
    if let Some(m) = ok(st.cpu_mhz) {
        items.push(vec![Span::styled(ghz(m), ui::muted())]);
    }
    if let Some(t) = ok(st.cpu_temp) {
        items.push(vec![Span::styled(deg(t), ui::fg(temp_heat(t)))]);
    }
    let inner = panel(f, r, Line::from("CPU"), right_line(items));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    put(f, band(inner, 0, 1), vec![cpu_head(st, w, false)]);
    // a blank line between the total and the detail, when there's room
    let gap = usize::from(h >= 10);
    let rest = h - 1 - gap;
    if rest == 0 {
        return;
    }
    let body = band(inner, 1 + gap, rest);
    // cores beside the chart when there's room (a heat grid fits a narrow
    // panel), else below it
    if rest >= 4 && w >= 50 {
        if let Some((lines, gw)) = core_grid(&st.cores, w * 45 / 100, rest) {
            let cw = w.saturating_sub(gw + 3);
            cpu_area(f, cols(body, 0, cw), st);
            put(f, cols(body, w - gw, gw), lines);
            return;
        }
    }
    // the strip says what it is: `cores ▁▃▅▂…`
    let strip = || -> Vec<Line<'static>> {
        strip_lines(&st.cores, w.saturating_sub(6))
            .into_iter()
            .map(|s| {
                let mut v = vec![Span::styled("cores ", ui::faint())];
                v.extend(s.spans);
                Line::from(v)
            })
            .collect()
    };
    let (lines, _) = if rest >= 6 {
        core_grid(&st.cores, w, (rest / 2).max(1)).unwrap_or_else(|| (strip(), w))
    } else {
        (strip(), w)
    };
    let gl = lines.len().min(rest);
    let mut ch = rest - gl;
    if ch >= 2 {
        cpu_area(f, band(body, 0, ch), st);
    } else {
        ch = 0;
    }
    put(f, band(body, ch, gl), lines);
}

/// Swap: `swap ████···· 93%  7.5G / 8.0G`, or `swap — none`.
fn swap_meter(st: &Stats, w: usize) -> Line<'static> {
    if fin(st.swap_total) <= 0.0 {
        return Line::from(vec![
            label("swap", 5, None),
            dash(),
            Span::styled(" none", ui::faint()),
        ]);
    }
    let v = ratio(st.swap_used, st.swap_total);
    meters(
        vec![M::new("swap", Some(v)).detail(of(&by(st.swap_used), &by(st.swap_total), ui::bold()))],
        w,
    )
    .remove(0)
}

/// `█ used 41.2G · █ buffers 1.1G · █ cache 80.3G · free 3.2G · avail 83G`,
/// dropping buffers, then avail, then free when narrow.
fn mem_legend(st: &Stats) -> Vec<(u8, Vec<Span<'static>>)> {
    let (u, b, ca, free) = mem_bytes(st);
    let parts = mem_parts(st);
    let mut items: Vec<(u8, Vec<Span<'static>>)> = legend(&parts, &[by(u), by(b), by(ca)])
        .into_iter()
        .zip([0u8, 4, 1])
        .map(|(it, p)| (p, it))
        .collect();
    items.push((2, kv("free", by(free), Style::new())));
    if st.mem_available > 0.0 {
        items.push((3, kv("avail", by(st.mem_available), ui::fg(c::GREEN()))));
    }
    items
}

fn mem_total_line(st: &Stats, w: usize) -> Line<'static> {
    let v = st.mem_frac();
    let mut l = Ln::new();
    l.add(stack(&mem_parts(st), w.saturating_sub(6)));
    l.s(
        format!(" {:>5}", pct(v)),
        ui::fg(ui::heat(fr(v))).add_modifier(Modifier::BOLD),
    );
    l.line()
}

/// Memory over time as lines: used (green), used + buffers + cache
/// (yellow), swap (red) when there is swap.
fn mem_chart(f: &mut Frame, r: Rect, st: &Stats) {
    let uc = sum2(&st.mem_hist, &st.mem_cache_hist);
    let swap: Vec<f64> = if st.swap_total > 0.0 {
        st.swap_hist.clone()
    } else {
        vec![]
    };
    let mut s = vec![Ser::line(&st.mem_hist, c::GREEN())];
    if !st.mem_cache_hist.is_empty() {
        s.push(Ser::line(&uc, c::YELLOW()));
    }
    if !swap.is_empty() {
        s.push(Ser::line(&swap, c::RED()));
    }
    let mut p = Plot::new(s, 1.0);
    if r.width >= 40 && r.height >= 4 {
        p = p.pct_axis();
        if r.height >= 8 {
            p = p.time(period(st));
        }
    }
    chart::plot(f, r, &p);
}

fn sec_mem(f: &mut Frame, r: Rect, st: &Stats) {
    let right = (st.mem_total > 0.0).then(|| {
        Line::from(vec![
            Span::styled(by(st.mem_used), ui::fg(ui::heat(fr(st.mem_frac())))),
            Span::styled(format!(" / {}", by(st.mem_total)), ui::faint()),
        ])
    });
    let inner = panel(f, r, Line::from("Memory"), right);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if st.mem_total <= 0.0 {
        put(
            f,
            inner,
            vec![Line::from(Span::styled(
                "no memory data on this machine",
                ui::faint(),
            ))],
        );
        return;
    }
    let mut lines = vec![mem_total_line(st, w)];
    if h >= 2 {
        lines.push(Line::from(join_prio(mem_legend(st), w)));
    }
    if h >= 3 {
        lines.push(swap_meter(st, w));
    }
    // who uses it, then (room left) its history
    let rest = h.saturating_sub(lines.len() + 1);
    if rest >= 2 {
        let k = if rest >= 12 { 5 } else { rest.min(8) };
        let users = mem_users(st, w, k);
        if !users.is_empty() {
            lines.push(Line::raw(""));
            lines.extend(users);
        }
    }
    let n = lines.len().min(h);
    put(f, band(inner, 0, n), lines);
    if h >= n + 5 {
        mem_chart(f, band(inner, n + 1, h - n - 1), st);
    }
}

/// What holds the memory, by program (all its processes together):
/// `chrome ×38  ███▌······  6.1G  10%`.
fn mem_users(st: &Stats, w: usize, n: usize) -> Vec<Line<'static>> {
    let list = plist(st);
    if list.is_empty() || n == 0 || st.mem_total <= 0.0 {
        return vec![];
    }
    // (name, processes, rss, style of the first)
    let mut by_name: Vec<(String, usize, f64, Style)> = Vec::new();
    for p in list.iter().filter(|p| fin(p.rss) > 0.0) {
        let name = clean(p.name.trim());
        match by_name.iter_mut().find(|x| x.0 == name) {
            Some(x) => {
                x.1 += 1;
                x.2 += fin(p.rss);
            }
            None => by_name.push((name, 1, fin(p.rss), name_style(p))),
        }
    }
    by_name.sort_by(|a, b| cmp_f(b.2, a.2).then(a.0.cmp(&b.0)));
    let nw = (w / 3).clamp(8, 22);
    let top = by_name.first().map_or(1.0, |x| x.2.max(1.0));
    by_name
        .into_iter()
        .take(n)
        .map(|(name, k, rss, sty)| {
            let mut l = Ln::new();
            let cnt = if k > 1 {
                format!(" ×{k}")
            } else {
                String::new()
            };
            let nm = ui::trunc(&name, nw.saturating_sub(ui::width(&cnt)));
            l.s(nm, sty);
            l.s(cnt, ui::faint());
            l.to(nw);
            let bw = w.saturating_sub(nw + 1 + 7 + 5).min(32);
            l.s(" ", Style::new());
            // bars relative to the biggest, so the ranking reads at a glance
            l.add(stack(
                &[Part {
                    frac: ratio(rss, top),
                    color: c::GREEN(),
                    glyph: "█",
                    name: "",
                }],
                bw,
            ));
            l.add(nu(&format!(" {:>6}", by(rss)), ui::bold()));
            let m = ratio(rss, st.mem_total);
            l.add(nu(&format!(" {:>4}", pct(m)), mem_style(m)));
            l.line()
        })
        .collect()
}

fn sec_gpu(f: &mut Frame, r: Rect, st: &Stats, big: bool) {
    let n = st.gpus.len();
    let mut right = Vec::new();
    if let (Some(g), Some(m)) = (st.gpus.first(), gpu_models(st)) {
        right.push(vec![
            icons::gpu(&g.vendor),
            Span::styled(format!(" {m}"), ui::muted()),
        ]);
        let (u, t) = vram_total(st);
        if t > 0.0 {
            right.push(vec![Span::styled(
                format!("{} / {}", by(u), by(t)),
                ui::faint(),
            )]);
        }
    }
    let inner = panel(
        f,
        r,
        Line::from(if n > 1 { "GPUs" } else { "GPU" }),
        right_line(right),
    );
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if n == 0 {
        put(
            f,
            inner,
            vec![Line::from(vec![
                dash(),
                Span::styled(format!(" {}", no_gpu_reason(st)), ui::muted()),
            ])],
        );
        return;
    }
    // the process table takes the bottom when focused (and there's room)
    let ps: Vec<&GpuProc> = st.gpu_procs.iter().collect();
    let ph = if big && !ps.is_empty() && h >= 12 + n {
        (1 + ps.len()).min(h / 3).max(3)
    } else {
        0
    };
    let gh = h - ph;
    let per = (gh / n).max(1);
    for (i, g) in st.gpus.iter().enumerate() {
        let y = i * per;
        if y >= gh {
            break;
        }
        let bh = if i + 1 == n { gh - y } else { per };
        gpu_block(f, band(inner, y, bh), st, g, n > 1);
    }
    if ph > 0 {
        let lines = gpu_proc_lines(st, &ps, w, ph, n > 1);
        put(f, band(inner, gh, ph), lines);
    }
}

/// A GPU in `r`: meters beside its history chart when there's room, the
/// meters alone, or one line.
fn gpu_block(f: &mut Frame, r: Rect, st: &Stats, g: &Gpu, head: bool) {
    let (w, h) = (r.width as usize, r.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    // one line (with its index) when there's no room to say which GPU it is
    if h == 1 || (head && h < 3) {
        put(f, band(r, 0, 1), vec![gpu_line(g, w)]);
        return;
    }
    let mut y = 0;
    if head && h >= 3 {
        let mut l = Ln::new();
        l.s(
            format!("GPU {} ", gpu_idx(g)),
            ui::fg(c_util()).add_modifier(Modifier::BOLD),
        );
        l.s(short_gpu(&gpu_label(g)), ui::bold());
        if !g.pstate.trim().is_empty() {
            l.s(format!("  {}", clean(g.pstate.trim())), ui::faint());
        }
        put(f, band(r, 0, 1), vec![l.line()]);
        y = 1;
    }
    let rh = h - y;
    let body = band(r, y, rh);
    if rh >= 3 && w >= 90 {
        let mw = (w * 50 / 100).clamp(40, 70);
        put(f, cols(body, 0, mw), gpu_meters(g, mw, rh));
        gpu_chart(f, cols(body, mw + 2, w - mw - 2), g, period(st), rh >= 5);
    } else {
        // meters, then the chart under them with what's left
        let ms = gpu_meters(g, w, rh);
        let k = ms.len();
        put(f, band(body, 0, k), ms);
        if rh >= k + 4 {
            gpu_chart(f, band(body, k + 1, rh - k - 1), g, period(st), true);
        }
    }
}

/// Network rows: `eth0  ↓ 12.3M/s  ↑ 1.2M/s  ⣀⣠⣤ (rx+tx)`.
fn iface_lines(st: &Stats, w: usize, n: usize) -> Vec<Line<'static>> {
    let mut nets: Vec<&crate::telemetry::Net> = st.nets.iter().collect();
    nets.sort_by(|a, b| cmp_f(b.rx + b.tx, a.rx + a.tx));
    let nw = nets
        .iter()
        .map(|x| ui::width(&x.name))
        .max()
        .unwrap_or(4)
        .clamp(4, 14);
    let mut out = Vec::new();
    for x in nets.into_iter().take(n) {
        let mut l = Ln::new();
        l.s(ui::pad(&clean(&x.name), nw), ui::bold());
        l.s("  ↓", ui::fg(c::GREEN()));
        l.add(nu(&format!("{:>9}", rate(x.rx)), ui::fg(c::GREEN())));
        l.s("  ↑", ui::fg(c::BLUE()));
        l.add(nu(&format!("{:>9}", rate(x.tx)), ui::fg(c::BLUE())));
        if w >= l.w + 8 {
            let sw = w - l.w - 2;
            let (_, peak) = avg_peak(&x.hist);
            l.s("  ", Style::new());
            l.add(chart::spark(
                &x.hist,
                sw,
                peak.max(1.0),
                CP::Solid(c::TEAL()),
            ));
        }
        out.push(l.line());
    }
    out
}

/// A mirrored throughput chart: `up` grows from the middle upwards, `down`
/// downwards, each on its own round scale, the time axis between them.
#[allow(clippy::too_many_arguments)]
fn mirror(
    f: &mut Frame,
    r: Rect,
    up: &[f64],
    up_c: Color,
    down: &[f64],
    down_c: Color,
    period: f64,
) {
    let r = area(f, r);
    if r.height < 2 || r.width == 0 {
        return;
    }
    let th = (r.height as usize).div_ceil(2);
    let (_, pu) = avg_peak(up);
    let (_, pd) = avg_peak(down);
    let (mu, md) = (
        chart::nice_max(pu.max(1024.0)),
        chart::nice_max(pd.max(1024.0)),
    );
    let lab = |v: f64| if v > 0.0 { by(v) } else { String::new() };
    let top = Plot::new(vec![Ser::area(up, CP::Solid(up_c))], mu).axis(lab);
    let mut bot = Plot::new(vec![Ser::area(down, CP::Solid(down_c))], md)
        .axis(lab)
        .flipped();
    if r.height >= 4 {
        bot = bot.time(period);
    }
    // the labels share one gutter, so the halves line up
    let g = top.label_w().max(bot.label_w());
    chart::plot(f, band(r, 0, th), &top.gutter(g));
    chart::plot(f, band(r, th, r.height as usize - th), &bot.gutter(g));
}

fn net_right(st: &Stats) -> Line<'static> {
    let (rx, tx) = net_total(st);
    Line::from(vec![
        Span::styled(
            format!("↓ {}", rate(rx)),
            ui::fg(c::GREEN()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ↑ {}", rate(tx)),
            ui::fg(c::BLUE()).add_modifier(Modifier::BOLD),
        ),
    ])
}

fn net_total(st: &Stats) -> (f64, f64) {
    if st.nets.is_empty() {
        (fin(st.net_rx).max(0.0), fin(st.net_tx).max(0.0))
    } else {
        st.nets.iter().fold((0.0, 0.0), |(a, b), n| {
            (a + fin(n.rx).max(0.0), b + fin(n.tx).max(0.0))
        })
    }
}

fn has_net(st: &Stats) -> bool {
    !st.nets.is_empty() || st.net_rx + st.net_tx > 0.0 || !st.net_rx_hist.is_empty()
}

fn sec_net(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(f, r, Line::from("Network"), Some(net_right(st)));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if !has_net(st) {
        put(
            f,
            inner,
            vec![Line::from(Span::styled("no network data", ui::faint()))],
        );
        return;
    }
    let k = st.nets.len().min(if h >= 8 {
        3
    } else {
        h.saturating_sub(1).max(1)
    });
    let lines = iface_lines(st, w, k);
    let n = lines.len();
    put(f, band(inner, 0, n), lines);
    if h >= n + 4 {
        mirror(
            f,
            band(inner, n + 1, h - n - 1),
            &st.net_rx_hist,
            c::GREEN(),
            &st.net_tx_hist,
            c::BLUE(),
            period(st),
        );
    }
}

/// Disk rows: `nvme0n1  r 1.2M/s  w 340K/s  ███·· 12%`, then filesystems.
fn disk_lines(st: &Stats, w: usize, h: usize, fs: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let nw = st
        .disk_io
        .iter()
        .map(|d| ui::width(&d.name))
        .chain(st.disks.iter().map(|d| ui::width(&d.0)))
        .max()
        .unwrap_or(4)
        .clamp(4, 14);
    for d in &st.disk_io {
        let mut l = Ln::new();
        l.s(ui::pad(&clean(&d.name), nw), ui::bold());
        l.s("  r", ui::fg(c::TEAL()));
        l.add(nu(&format!("{:>9}", rate(d.read)), ui::fg(c::TEAL())));
        l.s("  w", ui::fg(c::PEACH()));
        l.add(nu(&format!("{:>9}", rate(d.write)), ui::fg(c::PEACH())));
        if let Some(b) = ok(d.busy) {
            let bw = w.saturating_sub(l.w + 2 + 5 + 5).min(16);
            if bw >= 3 {
                l.s("  ", Style::new());
                l.add(gbar(b, bw));
                l.add(nu(&format!(" {:>4}", pct(b)), ui::fg(ui::heat(fr(b)))));
                l.s(" busy", ui::faint());
            }
        }
        out.push(l.line());
    }
    if fs {
        for (m, t, u) in &st.disks {
            out.push(fs_line(m, *t, *u, nw, w));
        }
    }
    out.truncate(h);
    out
}

/// `/home  ██████████······  77%  1.4T / 1.8T  412G free`
fn fs_line(m: &str, t: f64, u: f64, nw: usize, w: usize) -> Line<'static> {
    let mut l = Ln::new();
    l.s(ui::pad(&clean(m), nw), ui::bold());
    l.s("  ", Style::new());
    if fin(t) <= 0.0 {
        l.add(vec![dash()]);
        return l.line();
    }
    let v = ratio(u, t);
    let free = (fin(t) - fin(u)).max(0.0);
    let detail = format!("  {:>6} / {:<6}", by(u), by(t));
    let freet = format!("  {} free", by(free));
    let show_free = w >= l.w + 12 + 5 + ui::width(&detail) + ui::width(&freet);
    let bw = w
        .saturating_sub(
            l.w + 5 + ui::width(&detail) + if show_free { ui::width(&freet) } else { 0 },
        )
        .clamp(3, 60);
    l.add(gbar(v, bw));
    l.add(nu(
        &format!(" {:>4}", pct(v)),
        ui::fg(ui::heat(v)).add_modifier(Modifier::BOLD),
    ));
    if l.w + ui::width(&detail) <= w {
        l.s("  ", Style::new());
        l.add(of(
            &format!("{:>6}", by(u)),
            &format!("{:<6}", by(t)),
            Style::new(),
        ));
    }
    if show_free {
        let st = if v >= 0.9 {
            ui::fg(c::RED())
        } else {
            ui::muted()
        };
        l.s("  ", Style::new());
        l.add(nu(&by(free), st));
        l.s(" free", ui::faint());
    }
    l.line()
}

fn disk_right(st: &Stats) -> Option<Line<'static>> {
    (!st.disk_io.is_empty()).then(|| {
        let (rd, wr) = st.disk_io.iter().fold((0.0, 0.0), |(a, b), d| {
            (a + fin(d.read).max(0.0), b + fin(d.write).max(0.0))
        });
        Line::from(vec![
            Span::styled(
                format!("r {}", rate(rd)),
                ui::fg(c::TEAL()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  w {}", rate(wr)),
                ui::fg(c::PEACH()).add_modifier(Modifier::BOLD),
            ),
        ])
    })
}

fn disk_hists(st: &Stats) -> (Vec<f64>, Vec<f64>) {
    (
        sum_hists(st.disk_io.iter().map(|d| d.read_hist.as_slice())),
        sum_hists(st.disk_io.iter().map(|d| d.write_hist.as_slice())),
    )
}

fn sec_disk(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(f, r, Line::from("Disks"), disk_right(st));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let lines = disk_lines(st, w, h, true);
    if lines.is_empty() {
        put(
            f,
            inner,
            vec![Line::from(Span::styled("no disk data", ui::faint()))],
        );
        return;
    }
    let n = lines.len();
    put(f, band(inner, 0, n), lines);
    if h >= n + 5 && !st.disk_io.is_empty() {
        let (rd, wr) = disk_hists(st);
        mirror(
            f,
            band(inner, n + 1, h - n - 1),
            &rd,
            c::TEAL(),
            &wr,
            c::PEACH(),
            period(st),
        );
    }
}

fn sec_io(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(f, r, Line::from("Network & disks"), Some(net_right(st)));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let mut lines = iface_lines(st, w, 2);
    lines.extend(disk_lines(st, w, h, true));
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "no network or disk data",
            ui::faint(),
        )));
    }
    lines.truncate(h);
    let n = lines.len();
    put(f, band(inner, 0, n), lines);
    if h >= n + 5 && has_net(st) {
        mirror(
            f,
            band(inner, n + 1, h - n - 1),
            &st.net_rx_hist,
            c::GREEN(),
            &st.net_tx_hist,
            c::BLUE(),
            period(st),
        );
    }
}

fn sec_procs(f: &mut Frame, r: Rect, st: &Stats, sort: Sort) {
    let total = if st.procs_total > 0 {
        st.procs_total
    } else {
        plist(st).len()
    };
    let inner_h = (r.height as usize).saturating_sub(3);
    let right = (total > 0).then(|| {
        Line::from(Span::styled(
            format!(
                "top {} of {} · by {}",
                inner_h.min(plist(st).len()),
                count(total),
                sort.name()
            ),
            ui::faint(),
        ))
    });
    // the agents running there, in the bottom border
    let ag = agents_running(st);
    let bottom = (!ag.is_empty()).then(|| Line::from(join_fit(agents_inline(&ag), 120)));
    let inner = panel_b(f, r, Line::from("Processes"), right, bottom);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    put(f, inner, top_lines(st, sort, w, h));
}

fn sec_agents(f: &mut Frame, r: Rect, ag: &[AgentUse]) {
    let total: usize = ag.iter().map(|a| a.n).sum();
    let right = (total > 0).then(|| {
        Line::from(Span::styled(
            format!("{total} running"),
            ui::fg(c::ACCENT()),
        ))
    });
    let inner = panel(f, r, Line::from("Agents"), right);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let lines = if ag.is_empty() {
        vec![Line::from(Span::styled("no agents running", ui::faint()))]
    } else if ag.len() <= h {
        ag.iter().map(|a| agent_line(a, w)).collect()
    } else {
        flow(agents_inline(ag), w)
    };
    put(f, inner, lines);
}

// ------------------------------------------------------------------ CPU view

/// The CPU in depth: a stacked user / system / iowait chart beside the
/// per-core meters, load average and temperature charts, the clock and task
/// facts, and a short top-CPU list.
pub fn cpu(f: &mut Frame, r: Rect, st: &Stats) {
    let r = area(f, r);
    if r.is_empty() || waiting(f, r, st, epoch()) {
        return;
    }
    let (w, h) = (r.width as usize, r.height as usize);
    let n = st.cores.len();
    let procs_h = if h >= 30 {
        (h * 24 / 100).clamp(6, 13)
    } else if h >= 20 {
        6
    } else {
        0
    };
    if w >= 110 && n > 0 && h >= 16 {
        // left: usage over load; right: cores, temperatures, clock & tasks
        let top_h = h - procs_h;
        let rows = top_h.saturating_sub(chrome_h() + 7).max(4);
        let gw = core_grid(&st.cores, w * 40 / 100, rows).map_or(w * 34 / 100, |(_, gw)| gw);
        let rw = (gw + chrome_w()).max(w * 32 / 100).min(w / 2);
        let lw = w - rw;
        let left = cols(band(r, 0, top_h), 0, lw);
        let right = cols(band(r, 0, top_h), lw, rw);
        let uh = (top_h * 64 / 100).max(top_h.min(8));
        cpu_usage(f, band(left, 0, uh), st);
        if top_h > uh + 3 {
            load_panel(f, band(left, uh, top_h - uh), st);
        }
        // the cores take what their grid needs, the temperatures their list
        // (two columns when wide), the clock & tasks the rest
        let iw = rw.saturating_sub(chrome_w());
        // two columns of cores when they fit, so temperatures and the clock get room
        let half = n.div_ceil(2).max(4);
        let grid =
            core_grid(&st.cores, iw, half.min(rows)).or_else(|| core_grid(&st.cores, iw, rows));
        let ch = grid.map_or(strip_rows(n, iw), |(l, _)| l.len()) + chrome_h();
        let ch = ch.min(top_h);
        cores_panel(f, band(right, 0, ch), st);
        let rest = top_h - ch;
        let sensors = temp_list(st).len().max(1);
        let want = chrome_h()
            + if iw >= 40 {
                sensors.div_ceil(2)
            } else {
                sensors
            };
        let th = if rest >= want + 6 {
            // spare rows: the temperature history under the list
            want + ((rest - want - 6) / 2).min(6)
        } else {
            want.min(rest.saturating_sub(5)).max(rest.min(3))
        };
        if th >= 3 {
            temps_panel(f, band(right, ch, th), st);
        }
        if rest > th + 2 {
            clock_panel(f, band(right, ch + th, rest - th), st);
        }
        if procs_h > 0 {
            top_cpu(f, band(r, top_h, procs_h), st);
        }
        return;
    }
    // narrow: usage, the cores grid, load beside temperatures, top processes
    let top_min = 8;
    let mid_h = if h >= 22 {
        (h * 26 / 100).clamp(7, 14)
    } else {
        0
    };
    let top_h = h.saturating_sub(procs_h + mid_h).max(top_min.min(h));
    let top = band(r, 0, top_h);
    {
        // stacked: the usage chart, then the cores (as many columns as it takes)
        let iw = w.saturating_sub(chrome_w());
        let grid_rows = core_grid(&st.cores, iw, (top_h / 2).max(1))
            .map_or_else(|| strip_rows(n, iw).max(1), |(l, _)| l.len());
        let ch = top_h.saturating_sub(grid_rows + chrome_h());
        if ch >= 5 {
            cpu_usage(f, band(top, 0, ch), st);
            cores_panel(f, band(top, ch, top_h - ch), st);
        } else {
            cpu_usage(f, top, st);
        }
    }
    if mid_h > 0 {
        let mid = band(r, top_h, mid_h);
        let ws = if w >= 110 {
            split(w, &[40, 32, 28])
        } else {
            split(w, &[50, 50, 0])
        };
        load_panel(f, cols(mid, 0, ws[0]), st);
        temps_panel(f, cols(mid, ws[0], ws[1]), st);
        if ws[2] > 0 {
            clock_panel(f, cols(mid, ws[0] + ws[1], ws[2]), st);
        }
    }
    if procs_h > 0 {
        top_cpu(f, band(r, top_h + mid_h, procs_h), st);
    }
}

/// The busiest processes (top 10 at most).
fn top_cpu(f: &mut Frame, r: Rect, st: &Stats) {
    let total = if st.procs_total > 0 {
        st.procs_total
    } else {
        plist(st).len()
    };
    let inner = panel(
        f,
        r,
        Line::from("Top CPU"),
        Some(Line::from(Span::styled(
            format!("% of one core · {} processes", count(total)),
            ui::faint(),
        ))),
    );
    let n = (inner.height as usize).min(11);
    put(f, inner, top_lines(st, Sort::Cpu, inner.width as usize, n));
}

/// The usage panel: the split legend, then the stacked history.
fn cpu_usage(f: &mut Frame, r: Rect, st: &Stats) {
    let (avg, peak) = avg_peak_frac(&st.cpu_hist);
    let right = match stale_note(st) {
        Some(e) => e,
        None => Line::from(vec![
            Span::styled("now ", ui::faint()),
            Span::styled(
                pct(st.cpu),
                ui::fg(ui::heat(fr(st.cpu))).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · avg ", ui::faint()),
            Span::raw(pct(avg)),
            Span::styled(" · peak ", ui::faint()),
            Span::raw(pct(peak)),
        ]),
    };
    let inner = panel(f, r, Line::from("CPU usage"), Some(right));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let split = st.cpu_user_hist.len() >= 2 && (st.cpu_user + st.cpu_system) > 0.0;
    let mut items = Vec::new();
    if split {
        items = series_legend(vec![
            (
                c::GREEN(),
                "user",
                vec![Span::styled(pct1(st.cpu_user), ui::bold())],
            ),
            (
                c::RED(),
                "system",
                vec![Span::styled(pct1(st.cpu_system), ui::bold())],
            ),
            (
                c::YELLOW(),
                "iowait",
                vec![Span::styled(pct1(st.cpu_iowait), ui::bold())],
            ),
        ]);
        if st.cpu_steal > 0.0 {
            items.push(kv("steal", pct1(st.cpu_steal), ui::bold()));
        }
    } else {
        items.push(kv(
            "busy",
            pct1(st.cpu),
            ui::fg(ui::heat(fr(st.cpu))).add_modifier(Modifier::BOLD),
        ));
    }
    let idle = (1.0 - fr(st.cpu) - fr(st.cpu_iowait)).max(0.0);
    items.push(kv("idle", pct1(idle), ui::muted()));
    put(f, band(inner, 0, 1), vec![Line::from(join_sp(items, w))]);
    if h < 3 {
        return;
    }
    // a blank line under the legend when the chart is tall
    let gap = usize::from(h >= 12);
    let cr = band(inner, 1 + gap, h - 1 - gap);
    // stacked: user, system on it, iowait on both
    let us = sum2(&st.cpu_user_hist, &st.cpu_system_hist);
    let usi = sum2(&us, &st.cpu_iowait_hist);
    let series = if split {
        vec![
            Ser::area(&st.cpu_user_hist, CP::Solid(c::GREEN())),
            Ser::area(&us, CP::Solid(c::RED())).on(&st.cpu_user_hist),
            Ser::area(&usi, CP::Solid(c::YELLOW())).on(&us),
        ]
    } else {
        vec![Ser::area(&st.cpu_hist, CP::Heat)]
    };
    let peak = avg_peak_frac(if split { &usi } else { &st.cpu_hist }).1;
    let mut p = Plot::new(series, chart::pct_top(peak)).pct_axis();
    if cr.height >= 8 {
        p = p.time(period(st));
    }
    chart::plot(f, cr, &p);
}

fn cores_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let n = st.cores.len();
    let (avg, peak) = avg_peak_frac(&st.cores);
    let right = (n > 0).then(|| {
        Line::from(vec![
            Span::styled(format!("{n} · avg "), ui::faint()),
            Span::styled(pct(avg), ui::fg(ui::heat(fr(avg)))),
            Span::styled(" · busiest ", ui::faint()),
            Span::styled(pct(peak), ui::fg(ui::heat(fr(peak)))),
        ])
    });
    let inner = panel(f, r, Line::from("Cores"), right);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if n == 0 {
        put(
            f,
            inner,
            vec![Line::from(Span::styled(
                "no per-core data on this machine",
                ui::faint(),
            ))],
        );
        return;
    }
    let lines = match core_grid(&st.cores, w, h) {
        Some((l, _)) => l,
        None => strip_lines(&st.cores, w),
    };
    put(f, inner, lines);
}

fn load_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let ncpu = st.ncpu.max(st.cores.len());
    let mut right = vec![
        Span::styled("1m ", ui::faint()),
        Span::styled(
            load(st.load[0]),
            ui::fg(ui::heat(load_frac(st))).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  5m {}  15m {}", load(st.load[1]), load(st.load[2])),
            ui::muted(),
        ),
    ];
    if ncpu > 0 {
        right.push(Span::styled(format!(" · {ncpu} cores"), ui::faint()));
    }
    let inner = panel(f, r, Line::from("Load average"), Some(Line::from(right)));
    if inner.is_empty() {
        return;
    }
    let (_, peak) = avg_peak(&st.load_hist);
    let max = chart::nice_max((peak.max(fin(st.load[0])) * 1.1).max(1.0));
    let mut p = Plot::new(vec![Ser::line(&st.load_hist, ui::heat(load_frac(st)))], max)
        .axis(|v| format!("{v:.1}"));
    if inner.height >= 6 {
        p = p.time(period(st));
    }
    chart::plot(f, inner, &p);
}

/// Every sensor worth a line: the CPU package, the rest, each GPU.
fn temp_list(st: &Stats) -> Vec<(String, f64)> {
    let mut list: Vec<(String, f64)> = Vec::new();
    if let Some(t) = ok(st.cpu_temp) {
        if !st.temps.iter().any(|(_, x)| (*x - t).abs() < 0.01) {
            list.push(("cpu".into(), t));
        }
    }
    list.extend(
        st.temps
            .iter()
            .filter(|t| t.1.is_finite())
            .map(|(k, t)| (clean(k), *t)),
    );
    for g in &st.gpus {
        if let Some(t) = ok(g.temp) {
            list.push((format!("gpu{}", gpu_idx(g)), t));
        }
    }
    list
}

fn temps_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let hot = temp_list(st).iter().map(|x| x.1).fold(f64::MIN, f64::max);
    let right = (hot > f64::MIN).then(|| {
        Line::from(vec![
            Span::styled("hottest ", ui::faint()),
            Span::styled(
                deg(hot),
                ui::fg(temp_heat(hot)).add_modifier(Modifier::BOLD),
            ),
        ])
    });
    let inner = panel(f, r, Line::from("Temperatures"), right);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let list = temp_list(st);
    if list.is_empty() {
        put(
            f,
            inner,
            vec![Line::from(Span::styled(
                "no temperature sensors",
                ui::faint(),
            ))],
        );
        return;
    }
    // two columns when the list is long and the panel wide
    let ncols = if (list.len() > h || list.len() > 4) && w >= 40 {
        2
    } else {
        1
    };
    let cw = (w - 3 * (ncols - 1)) / ncols;
    let lw = list
        .iter()
        .map(|(k, _)| ui::width(k))
        .max()
        .unwrap_or(4)
        .clamp(4, 14)
        .min(cw / 2);
    let bw = cw.saturating_sub(lw + 1 + 6).min(40);
    let cell = |k: &str, t: f64| -> Vec<Span<'static>> {
        let mut l = Ln::new();
        l.s(ui::pad(k, lw), ui::muted());
        l.s(" ", Style::new());
        // 20 °C … 100 °C
        if bw >= 3 {
            l.add(gbar(((t - 20.0) / 80.0).clamp(0.0, 1.0), bw));
        }
        l.s(
            format!(" {:>5}", deg(t)),
            ui::fg(temp_heat(t)).add_modifier(Modifier::BOLD),
        );
        fit(l.spans(), cw)
    };
    let per = list.len().div_ceil(ncols).min(h);
    let shown = per * ncols;
    let mut lines = Vec::new();
    for row in 0..per {
        let mut l = Ln::new();
        for c in 0..ncols {
            if let Some((k, t)) = list.get(c * per + row) {
                if c > 0 {
                    l.s("   ", Style::new());
                }
                l.add(cell(k, *t));
            }
        }
        lines.push(l.line());
    }
    if list.len() > shown {
        if let Some(last) = lines.last_mut() {
            *last = Line::from(Span::styled(
                format!("… {} more", list.len() - shown + 1),
                ui::faint(),
            ));
        }
    }
    let nl = lines.len();
    put(f, band(inner, 0, nl), lines);
    // the CPU temperature's history below, when there's room
    if h >= nl + 3 && st.cpu_temp_hist.len() >= 2 {
        let (_, peak) = avg_peak(&st.cpu_temp_hist);
        let max = chart::nice_max(peak.max(50.0));
        let p = Plot::new(vec![Ser::line(&st.cpu_temp_hist, c_temp())], max)
            .axis(|v| format!("{v:.0}°"));
        chart::plot(f, band(inner, nl, h - nl), &p);
    }
}

fn clock_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(f, r, Line::from("Clock & tasks"), None);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let mut rows: Vec<M> = Vec::new();
    match (ok(st.cpu_mhz), ok(st.cpu_mhz_max)) {
        (Some(a), Some(m)) if m > 0.0 => {
            rows.push(M::new("clock", Some(ratio(a, m))).val(ghz(a), ui::bold()));
            if let Some(p) = ok(st.cpu_mhz_peak) {
                rows.push(M::new("fastest", Some(ratio(p, m))).val(ghz(p), ui::muted()));
            }
        }
        _ => {}
    }
    let mut lines = meters(rows, w);
    let mut facts: Vec<Vec<Span<'static>>> = Vec::new();
    if let Some(m) = ok(st.cpu_mhz_max) {
        facts.push(kv("rated", ghz(m), ui::muted()));
    } else if let Some(a) = ok(st.cpu_mhz) {
        facts.push(kv("clock", ghz(a), ui::bold()));
    }
    if st.procs_total > 0 {
        facts.push(kv("processes", count(st.procs_total), ui::bold()));
    }
    if st.threads_total > 0 {
        facts.push(kv("threads", count(st.threads_total), ui::bold()));
    }
    let running = if st.procs_running > 0 {
        st.procs_running.to_string()
    } else {
        clean(st.runnable.trim())
    };
    if !running.is_empty() {
        facts.push(kv("running", running, ui::fg(c::GREEN())));
    }
    if st.ncpu > 0 {
        facts.push(kv(
            "load / core",
            load(st.load[0] / st.ncpu as f64),
            ui::fg(ui::heat(load_frac(st))),
        ));
    }
    if st.uptime > 0 {
        facts.push(kv("up", ui::dur(st.uptime), ui::muted()));
    }
    if let Some(u) = st.users_logged {
        facts.push(kv("users", u.to_string(), ui::muted()));
    }
    // one fact per line when there's room, else wrapped
    if facts.len() + lines.len() <= h {
        lines.extend(facts.into_iter().map(Line::from));
    } else {
        lines.extend(flow(facts, w));
    }
    lines.truncate(h);
    put(f, inner, lines);
}

// ------------------------------------------------------------------ memory view

/// Memory in depth: the composition (a thick stacked bar and an explained
/// table) beside swap, a history chart of used / cache / swap, and the ten
/// biggest processes.
pub fn memory(f: &mut Frame, r: Rect, st: &Stats) {
    let r = area(f, r);
    if r.is_empty() || waiting(f, r, st, epoch()) {
        return;
    }
    let (w, h) = (r.width as usize, r.height as usize);
    if st.mem_total <= 0.0 {
        let inner = panel(f, r, Line::from("Memory"), None);
        centered(
            f,
            inner,
            Line::from(Span::styled("no memory data on this machine", ui::faint())),
        );
        return;
    }
    let top_h = if h >= 24 {
        9 + chrome_h()
    } else {
        (3 + chrome_h()).min(h)
    };
    let procs_h = if h >= 30 {
        (h * 26 / 100).clamp(6, 13)
    } else if h >= 20 {
        6
    } else {
        0
    };
    let chart_h = h.saturating_sub(top_h + procs_h);
    let top = band(r, 0, top_h);
    if w >= 110 && top_h >= 8 {
        let lw = w * 64 / 100;
        mem_summary(f, cols(top, 0, lw), st, false);
        swap_panel(f, cols(top, lw, w - lw), st);
    } else {
        mem_summary(f, top, st, true);
    }
    if chart_h >= 4 {
        // the history beside what holds the memory, by program
        let mid = band(r, top_h, chart_h);
        if w >= 110 {
            let lw = w * 58 / 100;
            mem_history(f, cols(mid, 0, lw), st);
            programs_panel(f, cols(mid, lw, w - lw), st);
        } else {
            mem_history(f, mid, st);
        }
    }
    if procs_h > 0 {
        let pr = band(r, top_h + chart_h, procs_h);
        let inner = panel(
            f,
            pr,
            Line::from("Top memory"),
            Some(Line::from(Span::styled("resident set size", ui::faint()))),
        );
        let n = (inner.height as usize).min(11);
        put(f, inner, top_lines(st, Sort::Mem, inner.width as usize, n));
    }
}

/// The composition: a (thick) stacked bar, the explained table or a legend,
/// and with `swap` (no swap panel beside it) the swap meter last.
fn mem_summary(f: &mut Frame, r: Rect, st: &Stats, swap: bool) {
    let right = match stale_note(st) {
        Some(e) => e,
        None => Line::from(vec![
            Span::styled(
                by(st.mem_used),
                ui::fg(ui::heat(fr(st.mem_frac()))).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" of {} in use", by(st.mem_total)), ui::faint()),
        ]),
    };
    let inner = panel(f, r, Line::from("Memory"), Some(right));
    let (w, full_h) = (inner.width as usize, inner.height as usize);
    if w == 0 || full_h == 0 {
        return;
    }
    let swap = swap && full_h >= 3;
    let h = full_h - usize::from(swap);
    let total = mem_total_line(st, w);
    let mut lines = vec![total.clone()];
    if h >= 8 {
        // a thick bar
        let mut l2 = total;
        if let Some(last) = l2.spans.last_mut() {
            *last = Span::raw("");
        }
        lines.push(l2);
    }
    let (u, b, ca, free) = mem_bytes(st);
    let parts = mem_parts(st);
    let total_b = fin(st.mem_total);
    let rows: Vec<(Span<'static>, &str, f64, &str)> = vec![
        (
            swatch(&parts[0]),
            "used",
            u,
            "programs + kernel, can't be reclaimed",
        ),
        (swatch(&parts[1]), "buffers", b, "block-device cache"),
        (swatch(&parts[2]), "cache", ca, "file cache, reclaimable"),
        (
            Span::styled(if c::mono() { " " } else { "·" }, ui::fg(c::LINE())),
            "free",
            free,
            "untouched",
        ),
        (
            Span::raw(" "),
            "shared",
            fin(st.mem_shared),
            "tmpfs / shm (inside cache)",
        ),
        (
            Span::raw(" "),
            "available",
            fin(st.mem_available),
            "what a new program can get",
        ),
    ];
    let rows: Vec<_> = rows
        .into_iter()
        .filter(|r| !(matches!(r.1, "shared" | "available") && r.2 <= 0.0))
        .collect();
    let room = h.saturating_sub(lines.len() + 1);
    if room >= rows.len() {
        lines.push(Line::raw(""));
        // two columns of the table when wide enough
        let two = w >= 96 && rows.len() >= 4;
        let per = if two {
            rows.len().div_ceil(2)
        } else {
            rows.len()
        };
        let cw = if two { (w - 3) / 2 } else { w };
        let cell = |(sw, name, v, why): &(Span<'static>, &str, f64, &str)| -> Vec<Span<'static>> {
            let mut l = Ln::new();
            l.add(vec![sw.clone()]);
            l.s(format!(" {name:<10}"), ui::muted());
            l.add(nu(&format!("{:>7}", by(*v)), ui::bold()));
            l.add(nu(
                &format!("  {:>4}", pct(ratio(*v, total_b))),
                ui::muted(),
            ));
            if cw >= l.w + 6 + 10 {
                l.s(format!("   {}", ui::trunc(why, cw - l.w - 3)), ui::faint());
            }
            fit(l.spans(), cw)
        };
        for i in 0..per {
            let mut l = Ln::new();
            l.add(cell(&rows[i]));
            if two {
                if let Some(x) = rows.get(i + per) {
                    l.s("   ", Style::new());
                    l.add(cell(x));
                }
            }
            lines.push(l.line());
        }
    } else {
        lines.push(Line::from(join_prio(mem_legend(st), w)));
    }
    lines.truncate(h);
    put(f, inner, lines);
    if swap {
        put(f, band(inner, full_h - 1, 1), vec![swap_meter(st, w)]);
    }
}

fn swap_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let right = (st.swap_total > 0.0).then(|| {
        Line::from(Span::styled(
            format!("{} total", by(st.swap_total)),
            ui::faint(),
        ))
    });
    let inner = panel(f, r, Line::from("Swap"), right);
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let mut lines = vec![swap_meter(st, w)];
    if st.swap_total > 0.0 {
        let v = ratio(st.swap_used, st.swap_total);
        let note = if v >= 0.8 {
            ("nearly full: memory is short", c::RED())
        } else if v >= 0.3 {
            ("in use: some memory paged out", c::YELLOW())
        } else {
            ("barely used", c::GREEN())
        };
        lines.push(Line::from(Span::styled(note.0, ui::fg(note.1))));
    }
    let n = lines.len();
    put(f, band(inner, 0, n), lines);
    if h >= n + 3 && st.swap_total > 0.0 {
        let v = ratio(st.swap_used, st.swap_total);
        let p = Plot::new(vec![Ser::line(&st.swap_hist, ui::heat(v))], 1.0).pct_axis();
        chart::plot(f, band(inner, n + 1, h - n - 1), &p);
    }
}

/// Memory by program: every process of a name together, biggest first.
fn programs_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(
        f,
        r,
        Line::from("By program"),
        Some(Line::from(Span::styled(
            "resident, all processes of a name",
            ui::faint(),
        ))),
    );
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let lines = mem_users(st, w, h);
    if lines.is_empty() {
        put(
            f,
            inner,
            vec![Line::from(Span::styled("no process data", ui::faint()))],
        );
        return;
    }
    put(f, inner, lines);
}

fn mem_history(f: &mut Frame, r: Rect, st: &Stats) {
    let (avg, peak) = avg_peak_frac(&st.mem_hist);
    let right = Line::from(vec![
        Span::styled("now ", ui::faint()),
        Span::styled(
            pct(st.mem_frac()),
            ui::fg(ui::heat(fr(st.mem_frac()))).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · avg ", ui::faint()),
        Span::raw(pct(avg)),
        Span::styled(" · peak ", ui::faint()),
        Span::raw(pct(peak)),
    ]);
    let inner = panel(f, r, Line::from("History"), Some(right));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let (_, b, ca, _) = mem_bytes(st);
    let mut items = vec![
        (
            c::GREEN(),
            "used",
            vec![Span::styled(pct(st.mem_frac()), ui::bold())],
        ),
        (
            c::YELLOW(),
            "buffers + cache",
            vec![Span::styled(pct(ratio(b + ca, st.mem_total)), ui::bold())],
        ),
    ];
    if st.swap_total > 0.0 {
        items.push((
            c::RED(),
            "swap",
            vec![Span::styled(
                pct(ratio(st.swap_used, st.swap_total)),
                ui::bold(),
            )],
        ));
    }
    put(
        f,
        band(inner, 0, 1),
        vec![Line::from(join_sp(series_legend(items), w))],
    );
    if h >= 3 {
        mem_chart(f, band(inner, 1, h - 1), st);
    }
}

// ------------------------------------------------------------------ GPU view

/// The GPUs in depth: per GPU a card of meters and every nvidia-smi signal
/// with one overlaid history chart (util · VRAM · power · temperature), a
/// grid of cards when there are several, and the GPU processes with their
/// SM use, VRAM, CPU and uptime. `st.gpu_note` centred when there are none.
pub fn gpu(f: &mut Frame, r: Rect, st: &Stats) {
    let r = area(f, r);
    if r.is_empty() || waiting(f, r, st, epoch()) {
        return;
    }
    let n = st.gpus.len();
    if n == 0 {
        let inner = panel(f, r, Line::from("GPU"), None);
        let mid = (inner.height as usize).saturating_sub(2) / 2;
        put(
            f,
            band(inner, mid, 2),
            vec![
                Line::from(Span::styled("no GPUs", ui::bold())).centered(),
                Line::from(Span::styled(no_gpu_reason(st), ui::muted())).centered(),
            ],
        );
        return;
    }
    let (w, h) = (r.width as usize, r.height as usize);
    let mut y = 0;
    if n >= 2 && h >= 10 {
        put(f, band(r, 0, 1), vec![gpu_totals(st, w)]);
        y = 1;
    }
    let ps: Vec<&GpuProc> = st.gpu_procs.iter().collect();
    let body_h = h - y;
    let procs_h = if body_h >= 16 {
        (3 + ps.len().max(1)).clamp(5, body_h * 30 / 100)
    } else {
        0
    };
    let cards_h = body_h - procs_h;
    let cards = band(r, y, cards_h);
    if n == 1 {
        gpu_card(f, cards, st, &st.gpus[0]);
    } else {
        // the grid whose cards come closest to a pleasant 3 : 1 (cells)
        let mut best = (1usize, f64::MAX);
        for cn in 1..=n.min(4) {
            let rows = n.div_ceil(cn);
            let (cw, ch) = (w / cn, cards_h / rows);
            if (cw < 60 && cn > 1) || ch == 0 {
                continue;
            }
            let score = (cw as f64 / ch as f64 - 3.0).abs() + if ch < 7 { 10.0 } else { 0.0 };
            if score < best.1 {
                best = (cn, score);
            }
        }
        let cn = best.0;
        let rows = n.div_ceil(cn);
        let ch = (cards_h / rows).max(1);
        for (i, g) in st.gpus.iter().enumerate() {
            let (row, col) = (i / cn, i % cn);
            if row * ch >= cards_h {
                break;
            }
            let x = w * col / cn;
            let x2 = w * (col + 1) / cn;
            let hh = if row + 1 == rows {
                cards_h - row * ch
            } else {
                ch
            };
            gpu_card(
                f,
                sub(
                    cards,
                    clamp16(x),
                    clamp16(row * ch),
                    clamp16(x2 - x),
                    clamp16(hh),
                ),
                st,
                g,
            );
        }
    }
    if procs_h > 0 {
        let pr = band(r, y + cards_h, procs_h);
        let vram: f64 = ps.iter().map(|p| fin(p.mem).max(0.0)).sum();
        let inner = panel(
            f,
            pr,
            Line::from("GPU processes"),
            Some(Line::from(Span::styled(
                format!("{} · {} VRAM", ps.len(), by(vram)),
                ui::faint(),
            ))),
        );
        let lines = if ps.is_empty() {
            vec![Line::from(Span::styled(
                "no processes on the GPU",
                ui::faint(),
            ))]
        } else {
            gpu_proc_lines(st, &ps, inner.width as usize, inner.height as usize, n > 1)
        };
        put(f, inner, lines);
    }
}

fn gpu_totals(st: &Stats, w: usize) -> Line<'static> {
    let n = st.gpus.len();
    let (mut p, mut pl, mut hot) = (0.0, 0.0, None::<f64>);
    for g in &st.gpus {
        p += ok(g.power).unwrap_or(0.0).max(0.0);
        pl += ok(g.power_limit).unwrap_or(0.0).max(0.0);
        if let Some(t) = ok(g.temp) {
            hot = Some(hot.map_or(t, |h| h.max(t)));
        }
    }
    let (mu, mt) = vram_total(st);
    let u = st.gpu_util().map(fr).unwrap_or(0.0);
    let mut items = vec![
        vec![
            icons::gpu(&st.gpus[0].vendor),
            Span::styled(
                format!(" {n} GPUs"),
                ui::fg(c_util()).add_modifier(Modifier::BOLD),
            ),
        ],
        kv(
            "util",
            pct(u),
            ui::fg(ui::heat(u)).add_modifier(Modifier::BOLD),
        ),
    ];
    if mt > 0.0 {
        items.push(kv(
            "VRAM",
            format!("{} / {}", by(mu), by(mt)),
            ui::fg(ui::heat(ratio(mu, mt))),
        ));
    }
    if p > 0.0 {
        items.push(kv(
            "power",
            if pl > 0.0 {
                format!("{} / {} W", int(p), int(pl))
            } else {
                format!("{} W", int(p))
            },
            ui::fg(ui::heat(ratio(p, pl))),
        ));
    }
    if let Some(t) = hot {
        items.push(kv("hottest", deg(t), ui::fg(temp_heat(t))));
    }
    if !st.gpu_procs.is_empty() {
        items.push(kv(
            "processes",
            st.gpu_procs.len().to_string(),
            Style::new(),
        ));
    }
    Line::from(join_fit(items, w))
}

fn gpu_card(f: &mut Frame, r: Rect, st: &Stats, g: &Gpu) {
    let title = Line::from(vec![
        Span::styled(
            format!("GPU {}", gpu_idx(g)),
            ui::fg(c_util()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", gpu_label(g)), ui::bold()),
    ]);
    let mut right = Vec::new();
    if !g.pstate.trim().is_empty() {
        right.push(vec![Span::styled(clean(g.pstate.trim()), ui::muted())]);
    }
    if !g.pcie.trim().is_empty() {
        right.push(vec![Span::styled(
            format!("PCIe {}", clean(g.pcie.trim())),
            ui::faint(),
        )]);
    }
    right.push(vec![icons::gpu(&g.vendor)]);
    let inner = panel(f, r, title, right_line(right));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    // facts: meters left, the rest of nvidia-smi right (or below) — what the
    // meters already say (SM clock, fan, memory temperature / clock) left out
    let nm = if ok(g.mem_util).is_some() { 5 } else { 4 };
    let mut kvs: Vec<_> = gpu_kv(g)
        .into_iter()
        .filter(|(k, _)| {
            !matches!(*k, "SM clock" | "fan" | "mem temp")
                && !(*k == "mem clock" && ok(g.mem_util).is_some())
        })
        .collect();
    // the history in numbers
    if g.hist.len() >= 2 {
        let (a, p) = avg_peak_frac(&g.hist);
        kvs.push((
            "util avg",
            vec![
                Span::styled(pct(a), ui::fg(ui::heat(a)).add_modifier(Modifier::BOLD)),
                Span::styled(format!(" · peak {}", pct(p)), ui::muted()),
            ],
        ));
    }
    if let (Some(l), true) = (ok(g.power_limit), g.power_hist.len() >= 2) {
        let (a, p) = avg_peak_frac(&g.power_hist);
        kvs.push((
            "power avg",
            vec![
                Span::styled(format!("{} W", int(a * l)), ui::bold()),
                Span::styled(format!(" · peak {} W", int(p * l)), ui::muted()),
            ],
        ));
    }
    if g.temp_hist.len() >= 2 {
        let (_, p) = avg_peak(&g.temp_hist);
        kvs.push((
            "temp peak",
            vec![Span::styled(deg(p), ui::fg(temp_heat(p)))],
        ));
    }
    let ps = gpu_procs_of(st, g);
    if !ps.is_empty() {
        let vram: f64 = ps.iter().map(|p| fin(p.mem).max(0.0)).sum();
        kvs.push((
            "processes",
            vec![
                Span::styled(ps.len().to_string(), ui::bold()),
                Span::styled(format!(" · {} VRAM", by(vram)), ui::muted()),
            ],
        ));
    }
    let (facts_h, chart_y) = if w >= 100 {
        let lw = (w * 52 / 100).min(84);
        let fh = nm.max(kvs.len().div_ceil(2)).min(h);
        let left = gpu_meters(g, lw, fh);
        let right = kv_grid(kvs, w - lw - 3, fh);
        let n = left.len().max(right.len());
        let lines: Vec<Line<'static>> = (0..n)
            .map(|i| {
                let mut l = Ln::new();
                if let Some(x) = left.get(i) {
                    l.add(x.spans.clone());
                }
                l.to(lw + 3);
                if let Some(x) = right.get(i) {
                    l.add(x.spans.clone());
                }
                l.line()
            })
            .collect();
        let k = lines.len();
        put(f, band(inner, 0, k), lines);
        (k, k)
    } else {
        let mut lines = gpu_meters(g, w, nm.min(h));
        let left = h.saturating_sub(lines.len() + 6);
        lines.extend(kv_grid(kvs, w, left.min(5)));
        lines.truncate(h);
        let k = lines.len();
        put(f, band(inner, 0, k), lines);
        (k, k)
    };
    let rest = h.saturating_sub(facts_h + 2);
    if rest >= 4 {
        put(
            f,
            band(inner, chart_y + 1, 1),
            vec![Line::from(join_sp(gpu_legend(g), w))],
        );
        let cr = band(inner, chart_y + 2, rest);
        gpu_chart(f, cr, g, period(st), true);
    }
}

/// `━━ util 54%  ━━ vram 21%  ━━ power 6%  ━━ temp 44°C` for `gpu_chart`.
fn gpu_legend(g: &Gpu) -> Vec<Vec<Span<'static>>> {
    let b = |s: String| vec![Span::styled(s, ui::bold())];
    let mut items = vec![(c_util(), "util", b(pct(g.util)))];
    if fin(g.mem_total) > 0.0 {
        items.push((c_vram(), "vram", b(pct(gpu_vram(g)))));
    }
    if let Some(p) = gpu_pow(g) {
        items.push((c_power(), "power", b(format!("{} of limit", pct(p)))));
    }
    if let Some(t) = ok(g.temp) {
        items.push((c_temp(), "temp", b(deg(t))));
    }
    series_legend(items)
}

// ------------------------------------------------------------------ network view

/// Network and disks: the total throughput (download up, upload down, each
/// on its own scale, the time axis between), every interface with its own
/// download and upload charts, then the disks — per device read / write
/// with busy time, filesystem usage, and their read-up / write-down chart.
pub fn net(f: &mut Frame, r: Rect, st: &Stats) {
    let r = area(f, r);
    if r.is_empty() || waiting(f, r, st, epoch()) {
        return;
    }
    let h = r.height as usize;
    let ni = st.nets.len().clamp(1, 6);
    let disk_rows = 2 * st.disk_io.len() + st.disks.len() + usize::from(!st.disks.is_empty());
    // the throughput chart first (≈ 38 %), then the interfaces at the
    // richest density that still leaves the disk chart ≥ 6 rows
    let tp = if h >= 24 { (h * 38 / 100).max(8) } else { 0 };
    let avail = h - tp;
    let disk_min = chrome_h() + disk_rows.max(1);
    let if_h = [4usize, 2, 1]
        .into_iter()
        .map(|per| chrome_h() + 1 + ni * per)
        .find(|need| avail >= need + disk_min + 6)
        .unwrap_or_else(|| {
            (chrome_h() + 1 + ni).min(avail.saturating_sub(disk_min.min(avail / 2)))
        });
    let if_h = if_h.min(avail);
    // no room for the disk chart: the disks take what their rows need and
    // the throughput chart the slack
    let mut tp = tp;
    let mut rest = h - tp - if_h;
    if tp > 0 && rest < disk_min + 6 && rest > disk_min {
        tp += rest - disk_min;
        rest = disk_min;
    }
    if tp > 0 {
        throughput_panel(f, band(r, 0, tp), st);
    }
    if if_h >= 3 {
        ifaces_panel(f, band(r, tp, if_h), st);
    }
    if rest >= 3 {
        disks_panel(f, band(r, tp + if_h, rest), st);
    }
}

fn throughput_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let (_, prx) = avg_peak(&st.net_rx_hist);
    let (_, ptx) = avg_peak(&st.net_tx_hist);
    let right = Line::from(vec![
        Span::styled("peak ", ui::faint()),
        Span::styled(format!("↓ {}", rate(prx)), ui::fg(c::GREEN())),
        Span::styled(format!("  ↑ {}", rate(ptx)), ui::fg(c::BLUE())),
    ]);
    let inner = panel(f, r, Line::from("Throughput"), Some(right));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    let (rx, tx) = net_total(st);
    let items = series_legend(vec![
        (
            c::GREEN(),
            "download",
            vec![Span::styled(rate(rx), ui::bold())],
        ),
        (
            c::BLUE(),
            "upload",
            vec![Span::styled(rate(tx), ui::bold())],
        ),
    ]);
    put(f, band(inner, 0, 1), vec![Line::from(join_sp(items, w))]);
    let gap = usize::from(h >= 12);
    if h >= 3 + gap {
        mirror(
            f,
            band(inner, 1 + gap, h - 1 - gap),
            &st.net_rx_hist,
            c::GREEN(),
            &st.net_tx_hist,
            c::BLUE(),
            period(st),
        );
    }
}

/// One rate with its history: `↓  47.4K/s` then a chart `rows` tall
/// (braille; one row is a sparkline) and the peak at the right.
#[allow(clippy::too_many_arguments)]
fn rate_row(
    f: &mut Frame,
    r: Rect,
    x0: usize,
    arrow: &str,
    v: f64,
    hist: &[f64],
    col: Color,
    rows: usize,
) {
    let w = r.width as usize;
    let pw = 8;
    let mut l = Ln::new();
    l.s(format!("{arrow} "), ui::fg(col));
    l.add(nu(
        &format!("{:>10}", rate(v)),
        ui::fg(col).add_modifier(Modifier::BOLD),
    ));
    let cx = x0 + l.w + 2;
    let (_, peak) = avg_peak(hist);
    // from its own column on: the name to the left stays
    put(f, sub(r, clamp16(x0), 0, clamp16(l.w), 1), vec![l.line()]);
    if w < cx + pw + 8 {
        return;
    }
    let cw = w - cx - pw - 2;
    let max = chart::nice_max(peak.max(1024.0));
    let p = Plot::new(vec![Ser::area(hist, CP::Solid(col))], max);
    chart::plot(f, sub(r, clamp16(cx), 0, clamp16(cw), clamp16(rows)), &p);
    put(
        f,
        sub(r, clamp16(w - pw), 0, clamp16(pw), 1),
        vec![Line::from(Span::styled(
            format!("{:>pw$}", by(peak)),
            ui::faint(),
        ))],
    );
}

fn ifaces_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let inner = panel(f, r, Line::from("Interfaces"), Some(net_right(st)));
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if st.nets.is_empty() {
        let msg = if has_net(st) {
            format!(
                "all interfaces  ↓ {}  ↑ {}",
                rate(st.net_rx),
                rate(st.net_tx)
            )
        } else {
            "no per-interface data on this machine".into()
        };
        put(f, inner, vec![Line::from(Span::styled(msg, ui::faint()))]);
        return;
    }
    let mut nets: Vec<&crate::telemetry::Net> = st.nets.iter().collect();
    nets.sort_by(|a, b| cmp_f(b.rx + b.tx, a.rx + a.tx));
    let nw = nets
        .iter()
        .map(|x| ui::width(&x.name))
        .max()
        .unwrap_or(5)
        .clamp(5, 16);
    let body = h.saturating_sub(1);
    let n = nets.len();
    // rows per interface: 4 (two-row charts), 2 (sparklines), 1 (both on one line)
    let per = if body >= 4 * n {
        4
    } else if body >= 2 * n {
        2
    } else {
        1
    };
    let head = ui::faint().add_modifier(Modifier::BOLD);
    let mut hd = Ln::new();
    hd.s(ui::pad("IFACE", nw), head);
    hd.s(format!("  {:>12}", "rate"), head);
    hd.s("  history", head);
    let pw = 8;
    if w > hd.w + pw {
        hd.to(w - pw);
        hd.s(format!("{:>pw$}", "peak"), head);
    }
    put(f, band(inner, 0, 1), vec![hd.line()]);
    let x0 = nw + 1;
    let mut y = 1;
    for x in nets {
        if y >= h {
            break;
        }
        put(
            f,
            band(inner, y, 1),
            vec![Line::from(Span::styled(
                ui::pad(&clean(&x.name), nw),
                ui::bold(),
            ))],
        );
        if per == 1 {
            // `↓ rate spark  ↑ rate spark` on one line
            let half = cols(band(inner, y, 1), 0, w / 2);
            rate_row(f, half, x0, "↓", x.rx, &x.rx_hist, c::GREEN(), 1);
            let rest = cols(band(inner, y, 1), w / 2, w - w / 2);
            rate_row(f, rest, 3, "↑", x.tx, &x.tx_hist, c::BLUE(), 1);
            y += 1;
            continue;
        }
        let k = per / 2;
        rate_row(
            f,
            band(inner, y, k),
            x0,
            "↓",
            x.rx,
            &x.rx_hist,
            c::GREEN(),
            k,
        );
        rate_row(
            f,
            band(inner, y + k, k),
            x0,
            "↑",
            x.tx,
            &x.tx_hist,
            c::BLUE(),
            k,
        );
        y += per;
    }
}

/// Disks: each device's read and write (with busy time), each
/// filesystem's usage, then all devices' read (up) / write (down) history.
fn disks_panel(f: &mut Frame, r: Rect, st: &Stats) {
    let mut right = disk_right(st).map(|l| l.spans).unwrap_or_default();
    let (mut t, mut u) = (0.0, 0.0);
    for d in &st.disks {
        t += fin(d.1).max(0.0);
        u += fin(d.2).max(0.0);
    }
    if t > 0.0 {
        if !right.is_empty() {
            right.push(sep());
        }
        right.push(Span::styled(
            format!("{} free of {}", by((t - u).max(0.0)), by(t)),
            ui::faint(),
        ));
    }
    let inner = panel(
        f,
        r,
        Line::from("Disks"),
        (!right.is_empty()).then(|| Line::from(right)),
    );
    let (w, h) = (inner.width as usize, inner.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if st.disk_io.is_empty() && st.disks.is_empty() {
        put(
            f,
            inner,
            vec![Line::from(Span::styled(
                "no disk I/O data on this machine",
                ui::faint(),
            ))],
        );
        return;
    }
    let nw = st
        .disk_io
        .iter()
        .map(|d| ui::width(&d.name))
        .chain(st.disks.iter().map(|d| ui::width(&d.0)))
        .max()
        .unwrap_or(6)
        .clamp(6, 16);
    let x0 = nw + 1;
    // the busy meter sits at the right of the read row
    let busy_w = 8 + 5 + 5;
    let mut y = 0;
    for d in &st.disk_io {
        if y + 2 > h {
            break;
        }
        put(
            f,
            band(inner, y, 1),
            vec![Line::from(Span::styled(
                ui::pad(&clean(&d.name), nw),
                ui::bold(),
            ))],
        );
        let cw = w.saturating_sub(busy_w + 2);
        rate_row(
            f,
            cols(band(inner, y, 1), 0, cw),
            x0,
            "r",
            d.read,
            &d.read_hist,
            c::TEAL(),
            1,
        );
        rate_row(
            f,
            cols(band(inner, y + 1, 1), 0, cw),
            x0,
            "w",
            d.write,
            &d.write_hist,
            c::PEACH(),
            1,
        );
        if let (Some(b), true) = (ok(d.busy), w >= busy_w + x0 + 14) {
            let mut l = Ln::new();
            l.add(gbar(b, 8));
            l.s(
                format!(" {:>4}", pct(b)),
                ui::fg(ui::heat(fr(b))).add_modifier(Modifier::BOLD),
            );
            l.s(" busy", ui::faint());
            put(
                f,
                sub(inner, clamp16(w - busy_w), clamp16(y), clamp16(busy_w), 1),
                vec![l.line()],
            );
        }
        y += 2;
    }
    if !st.disks.is_empty() && y < h {
        if y > 0 {
            y += 1;
        }
        for (m, t, u) in &st.disks {
            if y >= h {
                break;
            }
            put(f, band(inner, y, 1), vec![fs_line(m, *t, *u, nw, w)]);
            y += 1;
        }
    }
    if h >= y + 6 && !st.disk_io.is_empty() {
        let (rd, wr) = disk_hists(st);
        mirror(
            f,
            band(inner, y + 1, h - y - 1),
            &rd,
            c::TEAL(),
            &wr,
            c::PEACH(),
            period(st),
        );
    }
}

// ------------------------------------------------------------------ fleet

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FC {
    Machine,
    Cpu,
    Mem,
    Gpu,
    Vram,
    Temp,
    Power,
    Net,
    Disk,
    Agents,
    Up,
}

/// Display order.
const FC_SHOW: [FC; 11] = [
    FC::Machine,
    FC::Cpu,
    FC::Mem,
    FC::Gpu,
    FC::Vram,
    FC::Temp,
    FC::Power,
    FC::Net,
    FC::Disk,
    FC::Agents,
    FC::Up,
];
/// What stays when the table gets narrow.
const FC_PRIO: [FC; 11] = [
    FC::Machine,
    FC::Cpu,
    FC::Mem,
    FC::Gpu,
    FC::Agents,
    FC::Vram,
    FC::Temp,
    FC::Net,
    FC::Up,
    FC::Power,
    FC::Disk,
];
const GAP: usize = 2;

struct FW {
    machine: usize,
    cpu_bw: usize,
    cpu_sw: usize,
    mem_bw: usize,
    gpu_bw: usize,
    vram_bw: usize,
}

impl FW {
    fn width(&self, c: FC) -> usize {
        match c {
            FC::Machine => self.machine,
            FC::Cpu => self.cpu_bw + 5 + if self.cpu_sw > 0 { self.cpu_sw + 1 } else { 0 },
            FC::Mem => self.mem_bw + 5,
            FC::Gpu => self.gpu_bw + 5,
            FC::Vram => self.vram_bw + 5,
            FC::Temp => 5,
            FC::Power => 10,
            FC::Net => 13,
            FC::Disk => 4,
            FC::Agents => 6,
            FC::Up => 7,
        }
    }
    fn bw_mut(&mut self, c: FC) -> Option<&mut usize> {
        match c {
            FC::Cpu => Some(&mut self.cpu_bw),
            FC::Mem => Some(&mut self.mem_bw),
            FC::Gpu => Some(&mut self.gpu_bw),
            FC::Vram => Some(&mut self.vram_bw),
            _ => None,
        }
    }
}

fn fleet_cols(w: usize, mw: usize) -> (Vec<FC>, FW) {
    let mut fw = FW {
        machine: mw,
        cpu_bw: 6,
        cpu_sw: 0,
        mem_bw: 6,
        gpu_bw: 6,
        vram_bw: 6,
    };
    let mut chosen: Vec<FC> = Vec::new();
    let mut used = 0;
    for c in FC_PRIO {
        let need = fw.width(c) + if chosen.is_empty() { 0 } else { GAP };
        if used + need <= w {
            chosen.push(c);
            used += need;
        }
    }
    if chosen.is_empty() {
        fw.machine = w;
        return (vec![FC::Machine], fw);
    }
    let mut extra = w - used;
    if chosen.contains(&FC::Cpu) && extra >= 9 {
        fw.cpu_sw = (extra - 1).min(12);
        extra -= fw.cpu_sw + 1;
    }
    loop {
        let mut grew = false;
        for c in [FC::Cpu, FC::Mem, FC::Gpu, FC::Vram] {
            if extra == 0 || !chosen.contains(&c) {
                continue;
            }
            if let Some(b) = fw.bw_mut(c) {
                if *b < 16 {
                    *b += 1;
                    extra -= 1;
                    grew = true;
                }
            }
        }
        if !grew {
            break;
        }
    }
    if fw.cpu_sw > 0 {
        let more = extra.min(20);
        fw.cpu_sw += more;
    }
    chosen.sort_by_key(|c| FC_SHOW.iter().position(|x| x == c));
    (chosen, fw)
}

fn fc_title(c: FC) -> (&'static str, bool) {
    match c {
        FC::Machine => ("MACHINE", false),
        FC::Cpu => ("CPU", false),
        FC::Mem => ("MEM", false),
        FC::Gpu => ("GPU", false),
        FC::Vram => ("VRAM", false),
        FC::Temp => ("TEMP", true),
        FC::Power => ("POWER", true),
        FC::Net => ("NET ↓ ↑", false),
        FC::Disk => ("DISK", true),
        FC::Agents => ("AGENTS", false),
        FC::Up => ("UP", true),
    }
}

/// bar + `100%`.
fn bar_cell(v: f64, bw: usize) -> Vec<Span<'static>> {
    let v = fr(v);
    let mut s = gbar(v, bw);
    s.extend(nu(
        &format!(" {:>4}", pct(v)),
        ui::fg(ui::heat(v)).add_modifier(Modifier::BOLD),
    ));
    s
}

fn hottest(st: &Stats) -> Option<f64> {
    let mut t: Option<f64> = ok(st.cpu_temp);
    for g in &st.gpus {
        if let Some(x) = ok(g.temp) {
            t = Some(t.map_or(x, |y| y.max(x)));
        }
    }
    if t.is_none() {
        for (_, x) in &st.temps {
            if x.is_finite() {
                t = Some(t.map_or(*x, |y| y.max(*x)));
            }
        }
    }
    t
}

fn fleet_cell(c: FC, st: &Stats, fw: &FW, ag: usize) -> Vec<Span<'static>> {
    let w = fw.width(c);
    let none = || fit(vec![dash()], w);
    let right = |s: String, style: Style| vec![Span::styled(format!("{s:>w$}"), style)];
    match c {
        FC::Machine => vec![],
        FC::Cpu => {
            let mut s = bar_cell(st.cpu, fw.cpu_bw);
            if fw.cpu_sw > 0 {
                s.push(Span::raw(" "));
                s.extend(chart::spark(&st.cpu_hist, fw.cpu_sw, 1.0, CP::Heat));
            }
            s
        }
        FC::Mem if st.mem_total > 0.0 => bar_cell(st.mem_frac(), fw.mem_bw),
        FC::Gpu if !st.gpus.is_empty() => bar_cell(st.gpu_util().unwrap_or(0.0), fw.gpu_bw),
        FC::Vram if !st.gpus.is_empty() => {
            let (u, t) = vram_total(st);
            if t > 0.0 {
                bar_cell(ratio(u, t), fw.vram_bw)
            } else {
                none()
            }
        }
        FC::Temp => match hottest(st) {
            Some(t) => right(deg(t), ui::fg(temp_heat(t))),
            None => fit(vec![Span::raw(" ".repeat(w - 1)), dash()], w),
        },
        FC::Power => {
            let (mut p, mut l, mut any) = (0.0, 0.0, false);
            for g in &st.gpus {
                if let Some(x) = ok(g.power) {
                    p += x.max(0.0);
                    any = true;
                }
                l += ok(g.power_limit).unwrap_or(0.0).max(0.0);
            }
            if !any {
                fit(vec![Span::raw(" ".repeat(w - 1)), dash()], w)
            } else if l > 0.0 {
                right(
                    format!("{}/{}W", int(p), int(l)),
                    ui::fg(ui::heat(ratio(p, l))),
                )
            } else {
                right(format!("{}W", int(p)), ui::muted())
            }
        }
        FC::Net => {
            let (rx, tx) = net_total(st);
            vec![
                Span::styled(format!("↓{:>5}", by(rx)), ui::fg(c::GREEN())),
                Span::styled(format!(" ↑{:>5}", by(tx)), ui::fg(c::BLUE())),
            ]
        }
        FC::Disk => {
            let full = st
                .disks
                .iter()
                .filter(|d| fin(d.1) > 0.0)
                .map(|d| ratio(d.2, d.1))
                .fold(None, |a: Option<f64>, x| Some(a.map_or(x, |y| y.max(x))));
            match full {
                Some(v) => right(pct(v), ui::fg(ui::heat(v))),
                None => fit(vec![Span::raw(" ".repeat(w - 1)), dash()], w),
            }
        }
        FC::Agents => {
            if ag > 0 {
                fit(
                    vec![Span::styled(
                        format!("✻ {ag}"),
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                    )],
                    w,
                )
            } else {
                none()
            }
        }
        FC::Up => {
            if st.uptime > 0 {
                right(ui::dur(st.uptime), ui::muted())
            } else {
                fit(vec![Span::raw(" ".repeat(w - 1)), dash()], w)
            }
        }
        _ => none(),
    }
}

/// The machine cell: `▌b31 🐧 NVIDIA` — chip, OS logo, GPU badge.
/// The machine cell: `▌b31  🐧  Ubuntu   NVIDIA` — chip, OS badge (in a
/// column `bw` wide), GPU vendor badge.
fn machine_cell(m: &MachineRow, aw: usize, bw: usize, mw: usize) -> Vec<Span<'static>> {
    let mut l = Ln::new();
    l.s("▌", ui::fg(m.color));
    l.s(
        ui::pad(&clean(m.alias), aw),
        ui::fg(m.color).add_modifier(Modifier::BOLD),
    );
    if let Some(s) = m
        .stats
        .filter(|s| bw > 0 && (s.samples > 0 || !s.os.is_empty()))
    {
        l.s(" ", Style::new());
        let x = l.w;
        l.add(os_badge(&s.os, &s.platform, false));
        l.to(x + bw);
        if let Some(g) = s.gpus.first() {
            l.s(" ", Style::new());
            l.add(vec![icons::gpu(&g.vendor)]);
        }
    }
    fit(l.spans(), mw)
}

/// The widest OS badge among the rows (their column's width).
fn badge_w(rows: &[MachineRow]) -> usize {
    rows.iter()
        .filter_map(|m| m.stats)
        .filter(|s| s.samples > 0 || !s.os.is_empty())
        .map(|s| spans_w(&os_badge(&s.os, &s.platform, false)))
        .max()
        .unwrap_or(0)
}

/// Cross-machine comparison: one row per machine — CPU (with history),
/// memory, GPU, VRAM, hottest sensor, GPU power, network, fullest disk,
/// agents and uptime, columns dropping by priority as the table narrows; the
/// selected row carries the accent bar. Two lines per machine when there's
/// room, and below the table each machine's CPU / memory / GPU history.
pub fn fleet(f: &mut Frame, r: Rect, rows: &[MachineRow], sel: usize, t0: Instant) {
    let r = area(f, r);
    if r.is_empty() {
        return;
    }
    let n = rows.len();
    let up = rows.iter().filter(|m| m.up).count();
    // the table takes what it needs; charts get the rest
    let want = chrome_h() + 1 + n * 2 + usize::from(n > 0) * 2;
    let charts = rows
        .iter()
        .filter(|m| m.stats.is_some_and(|s| s.samples > 0))
        .count();
    let table_h = if charts > 0 && r.height as usize >= want + 10 {
        want
    } else {
        r.height as usize
    };
    let tr = band(r, 0, table_h);
    let right = Line::from(Span::styled(format!("{up}/{n} up"), ui::faint()));
    let inner = panel(f, tr, Line::from("Machines"), Some(right));
    let framed = inner != tr;
    if inner.is_empty() {
        return;
    }
    if rows.is_empty() {
        centered(
            f,
            inner,
            Line::from(Span::styled(
                "no machines yet — aleph host add <alias>",
                ui::muted(),
            )),
        );
        return;
    }
    let (w, h) = (inner.width as usize, inner.height as usize);
    let aw = rows
        .iter()
        .map(|m| ui::width(m.alias))
        .max()
        .unwrap_or(4)
        .clamp(4, 20);
    // chip + alias, the OS badge column, the GPU vendor
    let bw = badge_w(rows);
    let gpu_w = if rows
        .iter()
        .any(|m| m.stats.is_some_and(|s| !s.gpus.is_empty()))
    {
        1 + 6
    } else {
        0
    };
    let mw = 1 + aw + 1 + bw + gpu_w;
    let (mut cols_v, mut fw) = fleet_cols(w, mw);
    // narrow: the badges give way to the memory column
    let mut bw = bw;
    if !cols_v.contains(&FC::Mem) {
        bw = 0;
        (cols_v, fw) = fleet_cols(w, 1 + aw);
    }
    let footer = h >= n + 3;
    let body_h = h.saturating_sub(1 + usize::from(footer) * 2);
    let two = body_h >= 2 * n && w >= 60;
    let per = if two { 2 } else { 1 };
    let vis = body_h / per;
    let sel = sel.min(n - 1);
    let off = if vis > 0 && sel >= vis {
        sel + 1 - vis
    } else {
        0
    };

    let head_st = ui::faint().add_modifier(Modifier::BOLD);
    let mut hl = Ln::new();
    for (i, cc) in cols_v.iter().enumerate() {
        if i > 0 {
            hl.s(" ".repeat(GAP), Style::new());
        }
        let (t, right) = fc_title(*cc);
        let cw = fw.width(*cc);
        let t = ui::trunc(t, cw);
        hl.s(
            if right {
                format!("{t:>cw$}")
            } else {
                ui::pad(&t, cw)
            },
            head_st,
        );
    }
    let mut lines = vec![hl.line()];
    let mut bar_rows = Vec::new();
    for (i, m) in rows.iter().enumerate().skip(off).take(vis) {
        let is_sel = i == sel;
        let style = if is_sel { ui::selected() } else { Style::new() };
        let mut l = Ln::new();
        l.add(machine_cell(m, aw, bw, fw.machine));
        let note = clean(m.note.trim());
        let st_live = m.stats.filter(|s| s.samples > 0);
        let ag = m
            .stats
            .map(|s| agents_running(s).iter().map(|a| a.n).sum())
            .unwrap_or(0);
        match (m.up, m.stats, st_live) {
            (false, _, _) => {
                l.s("  ✕ ", ui::fg(c::RED()));
                l.s(
                    if note.is_empty() {
                        "unreachable".to_string()
                    } else {
                        note.clone()
                    },
                    ui::fg(c::RED()),
                );
            }
            (true, None, _) => {
                l.s(format!("  {} ", ui::spinner(t0)), ui::fg(c::ACCENT()));
                l.s("connecting to stats…", ui::muted());
            }
            (true, Some(s), None) => match &s.err {
                Some(e) => {
                    l.s("  ✕ ", ui::fg(c::RED()));
                    l.s(format!("no stats: {}", clean(e)), ui::fg(c::RED()));
                }
                None => {
                    l.s(format!("  {} ", ui::spinner(t0)), ui::fg(c::ACCENT()));
                    l.s("sampling…", ui::muted());
                }
            },
            (true, _, Some(s)) => {
                let stale = !s.live() || s.err.is_some();
                for cc in cols_v.iter().skip(1) {
                    l.s(" ".repeat(GAP), Style::new());
                    let mut cell = fit(fleet_cell(*cc, s, &fw, ag), fw.width(*cc));
                    if stale {
                        for sp in cell.iter_mut() {
                            sp.style = sp.style.fg(c::FAINT());
                        }
                    }
                    l.add(cell);
                }
                let tag = if let Some(e) = &s.err {
                    Some((format!("✕ {}", clean(e)), ui::fg(c::RED())))
                } else if stale {
                    Some(("stale".to_string(), ui::fg(c::YELLOW())))
                } else if !note.is_empty() && !two {
                    Some((note.clone(), ui::fg(c::YELLOW())))
                } else {
                    None
                };
                if let Some((t, ts)) = tag {
                    if l.w + 2 + 6 <= w {
                        let room = w - l.w - 2;
                        l.s(format!("  {}", ui::trunc(&t, room)), ts);
                    }
                }
            }
        }
        if is_sel {
            bar_rows.push(lines.len());
        }
        lines.push(Line::from(fit(l.spans(), w)).style(style));
        if two {
            let mut d = Ln::new();
            d.s("  ", Style::new());
            let mut items: Vec<Vec<Span<'static>>> = Vec::new();
            if m.up && !note.is_empty() {
                items.push(vec![Span::styled(note.clone(), ui::fg(c::YELLOW()))]);
            }
            if let Some(s) = st_live {
                items.extend(fleet_detail(s));
            }
            d.add(join_fit(items, w.saturating_sub(2)));
            if is_sel {
                bar_rows.push(lines.len());
            }
            lines.push(Line::from(fit(d.spans(), w)).style(style));
        }
    }
    put(f, band(inner, 0, h), lines);
    if framed {
        for y in bar_rows {
            if let Some(cell) = f
                .buffer_mut()
                .cell_mut((inner.x.saturating_sub(1), inner.y + clamp16(y)))
            {
                cell.set_symbol("┃").set_fg(c::ACCENT());
            }
        }
    }
    if footer {
        let tot = fleet_totals(rows);
        put(f, band(inner, h - 1, 1), vec![Line::from(join_fit(tot, w))]);
    }
    if table_h < r.height as usize {
        fleet_charts(f, band(r, table_h, r.height as usize - table_h), rows, sel);
    }
}

/// Small multiples: each machine's CPU, memory and GPU history in one chart.
fn fleet_charts(f: &mut Frame, r: Rect, rows: &[MachineRow], sel: usize) {
    let live: Vec<(usize, &MachineRow, &Stats)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.stats.filter(|s| s.samples > 0).map(|s| (i, m, s)))
        .collect();
    if live.is_empty() || r.height < 5 {
        return;
    }
    let (w, h) = (r.width as usize, r.height as usize);
    let n = live.len();
    // as many across as keeps each ≥ 50 wide; rows ≥ 7 tall
    let mut across = (w / 50).clamp(1, n.min(4));
    while across < n && n.div_ceil(across) * 7 > h {
        across += 1;
    }
    let down = n.div_ceil(across);
    let ch = h / down;
    if ch < 5 {
        return;
    }
    for (k, (i, m, s)) in live.into_iter().enumerate() {
        let (row, col) = (k / across, k % across);
        let x = w * col / across;
        let x2 = w * (col + 1) / across;
        let hh = if row + 1 == down { h - row * ch } else { ch };
        machine_chart(
            f,
            sub(
                r,
                clamp16(x),
                clamp16(row * ch),
                clamp16(x2 - x),
                clamp16(hh),
            ),
            m,
            s,
            i == sel,
        );
    }
}

fn machine_chart(f: &mut Frame, r: Rect, m: &MachineRow, s: &Stats, selected: bool) {
    let mut title = os_badge(&s.os, &s.platform, true);
    title.push(Span::raw(" "));
    title.extend(chip(m.alias, m.color));
    let gpu_hist: Vec<f64> = match s.gpus.len() {
        0 => vec![],
        1 => s.gpus[0].hist.clone(),
        k => sum_hists(s.gpus.iter().map(|g| g.hist.as_slice()))
            .into_iter()
            .map(|v| v / k as f64)
            .collect(),
    };
    let mut items = vec![
        (c::SKY(), "cpu", vec![Span::styled(pct(s.cpu), ui::bold())]),
        (
            c::GREEN(),
            "mem",
            vec![Span::styled(pct(s.mem_frac()), ui::bold())],
        ),
    ];
    if let Some(g) = s.gpu_util() {
        items.push((c_util(), "gpu", vec![Span::styled(pct(g), ui::bold())]));
    }
    let right = Line::from(join_sp(series_legend(items), 60));
    let r = area(f, r);
    if r.width < 8 || r.height < 3 {
        return;
    }
    let mut b = ui::panel(Line::from(title), selected);
    if ui::line_width(&right) + 12 + ui::width(m.alias) <= r.width as usize {
        b = b.title_top(padded(right).right_aligned());
    }
    let inner = b.inner(r);
    f.render_widget(b, r);
    let mut p = usage_plot(&s.cpu_hist, &s.mem_hist, &gpu_hist);
    if inner.width >= 40 {
        p = p.pct_axis();
    }
    if inner.height >= 6 {
        p = p.time(period(s));
    }
    chart::plot(f, inner, &p);
}

fn fleet_detail(s: &Stats) -> Vec<Vec<Span<'static>>> {
    let mut v: Vec<Vec<Span<'static>>> = Vec::new();
    let f = ui::faint();
    if !s.os.trim().is_empty() {
        v.push(vec![Span::styled(clean(s.os.trim()), ui::muted())]);
    }
    let mut cpu = String::new();
    if !s.cpu_model.trim().is_empty() {
        cpu.push_str(&cpu_title(s));
    }
    if s.ncpu > 0 {
        if !cpu.is_empty() {
            cpu.push_str(" · ");
        }
        cpu.push_str(&format!("{} threads", s.ncpu));
    }
    if !cpu.is_empty() {
        v.push(vec![Span::styled(cpu, f)]);
    }
    if let Some(m) = gpu_models(s) {
        v.push(vec![Span::styled(m, ui::fg(c_util()))]);
    }
    v.push(vec![
        Span::styled("load ", f),
        Span::styled(load(s.load[0]), ui::fg(ui::heat(load_frac(s)))),
    ]);
    if s.mem_total > 0.0 {
        v.push(vec![Span::styled(
            format!("{} / {} RAM", by(s.mem_used), by(s.mem_total)),
            f,
        )]);
    }
    v
}

fn fleet_totals(rows: &[MachineRow]) -> Vec<Vec<Span<'static>>> {
    let (mut cores, mut gpus, mut mem, mut ag) = (0usize, 0usize, 0.0f64, 0usize);
    for s in rows.iter().filter(|m| m.up).filter_map(|m| m.stats) {
        if s.samples == 0 {
            continue;
        }
        cores += s.ncpu.max(s.cores.len());
        gpus += s.gpus.len();
        mem += fin(s.mem_total).max(0.0);
        ag += agents_running(s).iter().map(|a| a.n).sum::<usize>();
    }
    let up = rows.iter().filter(|m| m.up).count();
    let down = rows.len() - up;
    let f = ui::faint();
    let mut v = vec![vec![Span::styled(format!("{up} up"), ui::fg(c::GREEN()))]];
    if down > 0 {
        v.push(vec![Span::styled(format!("{down} down"), ui::fg(c::RED()))]);
    }
    if cores > 0 {
        v.push(vec![Span::styled(format!("{cores} threads"), f)]);
    }
    if gpus > 0 {
        v.push(vec![Span::styled(
            format!("{gpus} GPU{}", if gpus == 1 { "" } else { "s" }),
            f,
        )]);
    }
    if mem > 0.0 {
        v.push(vec![Span::styled(format!("{} RAM", by(mem)), f)]);
    }
    v.push(vec![Span::styled(
        format!("✻ {ag} agent{}", if ag == 1 { "" } else { "s" }),
        ui::fg(c::ACCENT()),
    )]);
    v
}

// ------------------------------------------------------------------ compact

/// The Fleet view's machine panel: `mode` 0 resources · 1 GPU · 2 processes
/// (taken modulo 3); the modes are listed in the bottom border.
pub fn compact(
    f: &mut Frame,
    r: Rect,
    alias: &str,
    color: Color,
    st: &Stats,
    mode: u8,
    t0: Instant,
) {
    let r = area(f, r);
    if r.is_empty() {
        return;
    }
    let mode = mode % 3;
    let mut title = Vec::new();
    if st.samples > 0 || !st.os.trim().is_empty() {
        title.extend(os_badge(&st.os, &st.platform, true));
        title.push(Span::raw(" "));
    }
    title.extend(chip(alias, color));
    let host = clean(st.hostname.trim());
    if !host.is_empty() && host != alias {
        title.push(Span::styled(format!(" {host}"), ui::muted()));
    }
    let right = if let Some(e) = stale_note(st) {
        Some(e)
    } else {
        let mut items = vec![];
        if let (Some(g), Some(m)) = (st.gpus.first(), gpu_models(st)) {
            items.push(vec![
                icons::gpu(&g.vendor),
                Span::styled(format!(" {m}"), ui::faint()),
            ]);
        }
        if st.uptime > 0 {
            items.push(vec![Span::styled(
                format!("up {}", ui::dur(st.uptime)),
                ui::faint(),
            )]);
        }
        right_line(items)
    };
    let mut tabs = Vec::new();
    for (i, name) in ["resources", "gpu", "procs"].iter().enumerate() {
        if i > 0 {
            tabs.push(Span::styled(" · ", ui::faint()));
        }
        tabs.push(Span::styled(
            name.to_string(),
            if i as u8 == mode {
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
            } else {
                ui::faint()
            },
        ));
    }
    let inner = panel_b(f, r, Line::from(title), right, Some(Line::from(tabs)));
    if inner.is_empty() || waiting(f, inner, st, t0) {
        return;
    }
    let (w, h) = (inner.width as usize, inner.height as usize);
    match mode {
        1 => {
            if st.gpus.is_empty() {
                put(
                    f,
                    inner,
                    vec![Line::from(vec![
                        dash(),
                        Span::styled(format!(" {}", no_gpu_reason(st)), ui::muted()),
                    ])],
                );
                return;
            }
            let n = st.gpus.len();
            let ps: Vec<&GpuProc> = st.gpu_procs.iter().collect();
            let ph = if !ps.is_empty() && h >= 6 * n + 3 {
                (1 + ps.len()).min(h / 3)
            } else {
                0
            };
            let gh = h - ph;
            let per = (gh / n).max(1);
            for (i, g) in st.gpus.iter().enumerate() {
                let y = i * per;
                if y >= gh {
                    break;
                }
                let bh = if i + 1 == n { gh - y } else { per };
                gpu_block(f, band(inner, y, bh), st, g, n > 1);
            }
            if ph > 0 {
                put(
                    f,
                    band(inner, gh, ph),
                    gpu_proc_lines(st, &ps, w, ph, n > 1),
                );
            }
        }
        2 => put(f, inner, top_lines(st, Sort::Cpu, w, h)),
        _ => compact_summary(f, inner, st),
    }
}

/// Resources at a glance: CPU, cores, memory and swap, each GPU, disks,
/// network, temperatures and agents — then their history in the room left.
fn compact_summary(f: &mut Frame, r: Rect, st: &Stats) {
    let (w, h) = (r.width as usize, r.height as usize);
    let mut rows: Vec<M> = Vec::new();
    let mut cpu_d = load_items(st);
    if let Some(t) = ok(st.cpu_temp) {
        cpu_d.push(Span::raw("  "));
        cpu_d.push(Span::styled(deg(t), ui::fg(temp_heat(t))));
    }
    rows.push(M::new("CPU", Some(fr(st.cpu))).col(c::SKY()).detail(cpu_d));
    if st.mem_total > 0.0 {
        let mut d = of(&by(st.mem_used), &by(st.mem_total), ui::bold());
        if st.swap_total > 0.0 && st.swap_used > 1e6 {
            let sv = ratio(st.swap_used, st.swap_total);
            d.push(Span::styled("  swap ", ui::faint()));
            d.push(Span::styled(pct(sv), ui::fg(ui::heat(sv))));
        }
        rows.push(M::new("MEM", Some(st.mem_frac())).col(c::GREEN()).detail(d));
    }
    for g in &st.gpus {
        let mut d = vec![];
        if fin(g.mem_total) > 0.0 {
            let m = gpu_vram(g);
            d.push(Span::styled("vram ", ui::faint()));
            d.push(Span::styled(
                pct(m),
                ui::fg(ui::heat(m)).add_modifier(Modifier::BOLD),
            ));
            d.push(Span::styled(format!(" {}", by(g.mem_used)), ui::muted()));
        }
        if let Some(t) = ok(g.temp) {
            d.push(Span::raw("  "));
            d.push(Span::styled(deg(t), ui::fg(temp_heat(t))));
        }
        if let Some(p) = gpu_watts(g) {
            d.push(Span::styled(format!("  {p}"), ui::muted()));
        }
        let label = if st.gpus.len() > 1 {
            match gpu_idx(g).as_str() {
                "0" => "GPU0",
                "1" => "GPU1",
                "2" => "GPU2",
                "3" => "GPU3",
                _ => "GPU",
            }
        } else {
            "GPU"
        };
        rows.push(M::new(label, Some(fr(g.util))).col(c_util()).detail(d));
    }
    let mut lines = meters(rows, w);
    // the cores: a meter grid when the panel is tall, else a strip
    let grid = (h >= lines.len() + 4 + 8 + 8)
        .then(|| core_grid(&st.cores, w, 8))
        .flatten();
    match grid {
        Some((g, _)) => {
            lines.push(Line::raw(""));
            lines.extend(g);
            lines.push(Line::raw(""));
        }
        None if lines.len() < h => {
            let cs = strip_lines(&st.cores, w.saturating_sub(6));
            for s in cs.into_iter().take(2) {
                let mut l = Ln::new();
                l.s("cores ", ui::faint());
                l.add(s.spans);
                lines.push(l.line());
            }
        }
        None => {}
    }
    if st.gpus.is_empty() && st.gpu_note.is_some() {
        lines.push(Line::from(vec![
            label("GPU", 6, None),
            dash(),
            Span::styled(format!(" {}", no_gpu_reason(st)), ui::faint()),
        ]));
    }
    // io: network, disk I/O and the fullest filesystems
    let (rx, tx) = net_total(st);
    let mut io = vec![vec![
        Span::styled(format!("↓{}", rate(rx)), ui::fg(c::GREEN())),
        Span::styled(format!(" ↑{}", rate(tx)), ui::fg(c::BLUE())),
    ]];
    if !st.disk_io.is_empty() {
        let (rd, wr) = st.disk_io.iter().fold((0.0, 0.0), |(a, b), d| {
            (a + fin(d.read).max(0.0), b + fin(d.write).max(0.0))
        });
        io.push(vec![
            Span::styled(format!("r {}", rate(rd)), ui::fg(c::TEAL())),
            Span::styled(format!(" w {}", rate(wr)), ui::fg(c::PEACH())),
        ]);
    }
    for (m, t, u) in &st.disks {
        if fin(*t) > 0.0 {
            let v = ratio(*u, *t);
            io.push(vec![
                Span::styled(format!("{} ", ui::trunc(&clean(m), 12)), ui::faint()),
                Span::styled(pct(v), ui::fg(ui::heat(v))),
            ]);
        }
    }
    // a blank line between the meters and the facts, when there's room
    let last_blank = lines.last().is_some_and(|l| l.spans.is_empty());
    if h >= lines.len() + 8 && !last_blank {
        lines.push(Line::raw(""));
    }
    let mut l = Ln::new();
    l.add(vec![label("IO", 6, None)]);
    l.add(join_fit(io, w.saturating_sub(6)));
    lines.push(l.line());
    // temperatures
    let mut temps: Vec<Vec<Span<'static>>> = st
        .temps
        .iter()
        .filter(|t| t.1.is_finite())
        .take(6)
        .map(|(k, t)| kv(&ui::trunc(&clean(k), 10), deg(*t), ui::fg(temp_heat(*t))))
        .collect();
    for g in &st.gpus {
        if let Some(t) = ok(g.temp) {
            temps.push(kv(
                &format!("gpu{}", gpu_idx(g)),
                deg(t),
                ui::fg(temp_heat(t)),
            ));
        }
    }
    if !temps.is_empty() {
        let mut l = Ln::new();
        l.add(vec![label("TEMP", 6, None)]);
        l.add(join_fit(temps, w.saturating_sub(6)));
        lines.push(l.line());
    }
    // agents
    let ag = agents_running(st);
    if !ag.is_empty() {
        let mut l = Ln::new();
        l.add(vec![label("AGENT", 6, Some(c::ACCENT()))]);
        l.add(join_fit(agents_inline(&ag), w.saturating_sub(6)));
        lines.push(l.line());
    }
    lines.truncate(h);
    let n = lines.len();
    put(f, band(r, 0, n), lines);
    // the history fills the rest
    if h >= n + 4 {
        let cr = band(r, n + 1, h - n - 1);
        let gpu_hist: Vec<f64> = match st.gpus.len() {
            0 => vec![],
            k => sum_hists(st.gpus.iter().map(|g| g.hist.as_slice()))
                .into_iter()
                .map(|v| v / k as f64)
                .collect(),
        };
        let mut p = usage_plot(&st.cpu_hist, &st.mem_hist, &gpu_hist).pct_axis();
        if cr.height >= 6 {
            p = p.time(period(st));
        }
        chart::plot(f, cr, &p);
    }
}

/// CPU (sky), memory (green) and GPU (mauve) history as lines, on the
/// 10 / 20 / 50 / 100 % scale their peak needs.
fn usage_plot<'a>(cpu: &'a [f64], mem: &'a [f64], gpu: &'a [f64]) -> Plot<'a> {
    let peak = [cpu, mem, gpu]
        .iter()
        .map(|h| avg_peak_frac(h).1)
        .fold(0.0, f64::max);
    let mut ser = vec![Ser::line(cpu, c::SKY()), Ser::line(mem, c::GREEN())];
    if !gpu.is_empty() {
        ser.push(Ser::line(gpu, c_util()));
    }
    Plot::new(ser, chart::pct_top(peak))
}

#[cfg(test)]
#[path = "monitor_draw_tests.rs"]
mod tests;
