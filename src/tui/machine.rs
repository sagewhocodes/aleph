//! The Fleet's machine side: the resources panel under the list (CPU, memory,
//! GPUs and VRAM, disks, network, agent processes — no process table unless
//! asked for), and the machine view on the right when a machine row is
//! selected (who it is, hardware, installed agents, local LLM servers,
//! sessions by state, recent events and folders).

use super::{Act, Dash};
use crate::fleet::Conn;
use crate::icons;
use crate::telemetry::Stats;
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ resources

fn dim(s: impl Into<String>) -> Span<'static> {
    Span::styled(s.into(), ui::faint())
}

fn txt(s: impl Into<String>) -> Span<'static> {
    Span::raw(s.into())
}

/// Width of the label column in the resources panel.
const LW: usize = 10;

/// One section of the resources panel: a quiet ruled header, aligned rows,
/// and (room permitting) a chart over time.
struct Section {
    title: Vec<Span<'static>>,
    /// right side of the header (now · avg · peak)
    note: String,
    rows: Vec<Line<'static>>,
    /// samples (0..=1), colour, top-of-axis label
    chart: Option<(Vec<f64>, Color, String)>,
}

fn label(t: &str) -> Span<'static> {
    Span::styled(ui::pad(t, LW), ui::muted())
}

/// `usage  ███████······  23%  detail`
fn meter_row(lbl: &str, frac: f64, w: usize, detail: Vec<Span<'static>>) -> Line<'static> {
    let bw = (w.saturating_sub(LW) * 2 / 5).clamp(10, 48);
    let col = ui::heat(frac);
    let mut v = vec![label(lbl)];
    v.extend(ui::bar(frac, bw, col));
    v.push(Span::styled(
        format!(" {:>4.0}%   ", frac * 100.0),
        ui::fg(col),
    ));
    let room = w.saturating_sub(LW + bw + 9);
    let mut used = 0;
    for sp in detail {
        let sw = ui::width(&sp.content);
        if used + sw > room {
            let rest = room.saturating_sub(used);
            if rest > 1 {
                v.push(Span::styled(ui::trunc(&sp.content, rest), sp.style));
            }
            break;
        }
        used += sw;
        v.push(sp);
    }
    Line::from(v)
}

fn text_row(lbl: &str, spans: Vec<Span<'static>>) -> Line<'static> {
    let mut v = vec![label(lbl)];
    v.extend(spans);
    Line::from(v)
}

fn temp_span(t: f64) -> Span<'static> {
    Span::styled(format!("{t:.0}°C"), ui::fg(ui::heat((t - 40.0) / 50.0)))
}

/// now · avg · peak of a 0..=1 series, as percentages.
fn stats_note(h: &[f64], now: f64) -> String {
    if h.is_empty() {
        return String::new();
    }
    let avg = h.iter().sum::<f64>() / h.len() as f64;
    let peak = h.iter().cloned().fold(0.0, f64::max);
    format!(
        "now {:.0}% · avg {:.0}% · peak {:.0}%",
        now * 100.0,
        avg * 100.0,
        peak * 100.0
    )
}

fn sections(st: &Stats, w: usize) -> Vec<Section> {
    let mut out = Vec::new();
    // ---- CPU
    let mut rows = Vec::new();
    let mut d = vec![txt(format!("{} cores", st.ncpu.max(st.cores.len())))];
    if !st.cpu_model.is_empty() {
        d.push(dim(format!(" · {}", st.cpu_model)));
    }
    rows.push(meter_row("usage", st.cpu, w, d));
    let mut l = Vec::new();
    if st.load[0] > 0.0 || st.load[1] > 0.0 {
        l.push(txt(format!(
            "{:.2}  {:.2}  {:.2}",
            st.load[0], st.load[1], st.load[2]
        )));
        l.push(dim("  (1, 5, 15 min)"));
    }
    if let Some(t) = st.cpu_temp {
        l.push(dim("   ·   "));
        l.push(temp_span(t));
    }
    if st.procs_total > 0 {
        l.push(dim(format!("   ·   {} processes", st.procs_total)));
    }
    if !l.is_empty() {
        rows.push(text_row("load", l));
    }
    if st.cores.len() > 1 {
        let per = w.saturating_sub(LW).max(8);
        for (i, chunk) in st.cores.chunks(per).enumerate() {
            let mut v = vec![label(if i == 0 { "cores" } else { "" })];
            v.extend(chunk.iter().map(|x| ui::level(*x)));
            rows.push(Line::from(v));
        }
    }
    out.push(Section {
        title: vec![txt("CPU")],
        note: stats_note(&st.cpu_hist, st.cpu),
        rows,
        chart: Some((st.cpu_hist.clone(), c::ACCENT(), "100%".into())),
    });
    // ---- memory
    let mut rows = vec![meter_row(
        "used",
        st.mem_frac(),
        w,
        vec![
            txt(ui::bytes(st.mem_used)),
            dim(format!(" of {}", ui::bytes(st.mem_total))),
        ],
    )];
    let mut l = Vec::new();
    for (k, v) in [
        ("cached", st.mem_cached),
        ("buffers", st.mem_buffers),
        ("shared", st.mem_shared),
        ("available", st.mem_available),
    ] {
        if v > 0.0 {
            if !l.is_empty() {
                l.push(dim("   ·   "));
            }
            l.push(dim(format!("{k} ")));
            l.push(txt(ui::bytes(v)));
        }
    }
    if !l.is_empty() {
        rows.push(text_row("", l));
    }
    if st.swap_total > 0.0 {
        rows.push(meter_row(
            "swap",
            st.swap_used / st.swap_total,
            w,
            vec![
                txt(ui::bytes(st.swap_used)),
                dim(format!(" of {}", ui::bytes(st.swap_total))),
            ],
        ));
    }
    out.push(Section {
        title: vec![txt("Memory")],
        note: stats_note(&st.mem_hist, st.mem_frac()),
        rows,
        chart: Some((st.mem_hist.clone(), c::MAUVE(), "100%".into())),
    });
    // ---- GPUs
    for (i, g) in st.gpus.iter().enumerate() {
        let model = g
            .name
            .replace("NVIDIA ", "")
            .replace("GeForce ", "")
            .replace("AMD ", "");
        let mut title = vec![txt(if st.gpus.len() > 1 {
            format!("GPU {i}  ")
        } else {
            "GPU  ".into()
        })];
        let mut b = icons::gpu(&g.vendor);
        b.style = b.style.remove_modifier(Modifier::BOLD);
        title.push(b);
        title.push(txt(format!(" {model}")));
        let mut rows = Vec::new();
        let mut d = Vec::new();
        if let Some(t) = g.temp {
            d.push(temp_span(t));
        }
        if let (Some(p), Some(lim)) = (g.power, g.power_limit) {
            d.push(dim(format!("   ·   {p:.0} / {lim:.0} W")));
        }
        if let Some(fan) = g.fan {
            d.push(dim(format!("   ·   fan {:.0}%", fan)));
        }
        if !g.pstate.is_empty() {
            d.push(dim(format!("   ·   {}", g.pstate)));
        }
        // an Apple GPU: its renderer and tiler beside its use
        if let (Some(r), Some(t)) = (g.renderer, g.tiler) {
            d.push(dim(format!(
                "   ·   renderer {:.0}%  tiler {:.0}%",
                r * 100.0,
                t * 100.0
            )));
        }
        rows.push(meter_row("util", g.util, w, d));
        let vf = if g.mem_total > 0.0 {
            g.mem_used / g.mem_total
        } else {
            0.0
        };
        rows.push(meter_row(
            // an Apple GPU's memory is the Mac's own, shared
            if g.vendor == "apple" {
                "shared"
            } else {
                "vram"
            },
            vf,
            w,
            vec![
                txt(ui::bytes(g.mem_used)),
                dim(format!(" of {}", ui::bytes(g.mem_total))),
            ],
        ));
        if let Some(sm) = g.clock_sm {
            let mut l = vec![txt(format!("{sm:.0} MHz"))];
            if let Some(mx) = g.clock_sm_max {
                l.push(dim(format!(" of {mx:.0}")));
            }
            if let Some(m) = g.clock_mem {
                l.push(dim(format!("   ·   memory {m:.0} MHz")));
            }
            rows.push(text_row("clocks", l));
        }
        out.push(Section {
            title,
            note: stats_note(&g.hist, g.util),
            rows,
            chart: Some((g.hist.clone(), c::GREEN(), "100%".into())),
        });
    }
    if st.gpus.is_empty() {
        if let Some(n) = &st.gpu_note {
            out.push(Section {
                title: vec![txt("GPU")],
                note: String::new(),
                rows: vec![text_row("", vec![dim(ui::trunc(n, w.saturating_sub(LW)))])],
                chart: None,
            });
        }
    }
    // ---- network and disks
    let mut rows = Vec::new();
    rows.push(text_row(
        "network",
        vec![
            Span::styled(format!("↓ {}/s", ui::bytes(st.net_rx)), ui::fg(c::GREEN())),
            dim("     "),
            Span::styled(format!("↑ {}/s", ui::bytes(st.net_tx)), ui::fg(c::BLUE())),
        ],
    ));
    for (m, t, u) in st.disks.iter().filter(|x| x.1 > 0.0).take(4) {
        let short = if m == "/" {
            "disk /".to_string()
        } else {
            format!("disk {}", ui::trunc(m.rsplit('/').next().unwrap_or(m), 4))
        };
        rows.push(meter_row(
            &short,
            u / t,
            w,
            vec![
                txt(ui::bytes(*u)),
                dim(format!(
                    " of {}   ·   {} free",
                    ui::bytes(*t),
                    ui::bytes(t - u)
                )),
            ],
        ));
    }
    // net_hist is a fraction of 1 Gbit/s; the chart scales to its own peak
    let peak = st.net_hist.iter().cloned().fold(0.0, f64::max);
    let peak_bytes = peak * 125_000_000.0;
    let norm: Vec<f64> = if peak > 0.0 {
        st.net_hist.iter().map(|x| x / peak).collect()
    } else {
        st.net_hist.clone()
    };
    out.push(Section {
        title: vec![txt("Network & disk")],
        note: if peak > 0.0 {
            format!("peak {}/s", ui::bytes(peak_bytes))
        } else {
            String::new()
        },
        rows,
        chart: (!norm.is_empty()).then(|| (norm, c::SKY(), format!("{}/s", ui::bytes(peak_bytes)))),
    });
    // ---- agents running there
    if !st.agent_procs.is_empty() {
        let mut l = Vec::new();
        for (id, n, rss) in &st.agent_procs {
            let mut g = icons::agent(id);
            g.style = g.style.remove_modifier(Modifier::BOLD);
            l.push(g);
            l.push(txt(format!(" {} ×{n} ", crate::agents::label(id))));
            l.push(dim(format!("{}     ", ui::bytes(*rss))));
        }
        out.push(Section {
            title: vec![txt("Agents")],
            note: String::new(),
            rows: vec![text_row("running", l)],
            chart: None,
        });
    }
    out
}

/// `── CPU ─────────────── now 3% · avg 5% · peak 12% ─`
fn rule(title: &[Span<'static>], note: &str, w: usize) -> Line<'static> {
    let mut v = vec![Span::styled("── ", ui::fg(c::LINE()))];
    for t in title {
        let mut t = t.clone();
        t.style = t.style.patch(ui::muted());
        v.push(t);
    }
    v.push(Span::raw(" "));
    let used: usize = v.iter().map(|s| ui::width(&s.content)).sum();
    let nw = if note.is_empty() {
        0
    } else {
        ui::width(note) + 4
    };
    v.push(Span::styled(
        "─".repeat(w.saturating_sub(used + nw)),
        ui::fg(c::LINE()),
    ));
    if nw > 0 {
        v.push(Span::styled(format!(" {note} "), ui::faint()));
        v.push(Span::styled("──", ui::fg(c::LINE())));
    }
    Line::from(v)
}

/// The resources panel's lines for `w` × `h`: sections first, then the
/// height left over shared among their charts (2–10 rows each).
pub(crate) fn resource_view(st: &Stats, w: usize, h: usize) -> Vec<Line<'static>> {
    let secs = sections(st, w);
    let fixed: usize = secs.iter().map(|s| 1 + s.rows.len()).sum::<usize>() + secs.len();
    let charts = secs.iter().filter(|s| s.chart.is_some()).count().max(1);
    let ch = (h.saturating_sub(fixed) / charts).min(10);
    let mut out = Vec::new();
    for (i, s) in secs.iter().enumerate() {
        if i > 0 {
            out.push(Line::raw(""));
        }
        out.push(rule(&s.title, &s.note, w));
        out.extend(s.rows.iter().cloned());
        if let (Some((data, col, top)), true) = (&s.chart, ch >= 2) {
            let cw = w.saturating_sub(LW);
            // percentages: scale to the next step above the peak, so a quiet
            // machine's 3% is still visible (the axis says the scale)
            let (data, top) = if top == "100%" {
                let peak = data.iter().cloned().fold(0.0, f64::max);
                let scale = [0.1, 0.25, 0.5, 1.0]
                    .into_iter()
                    .find(|x| peak * 1.15 <= *x)
                    .unwrap_or(1.0);
                (
                    data.iter().map(|v| v / scale).collect::<Vec<_>>(),
                    format!("{:.0}%", scale * 100.0),
                )
            } else {
                (data.clone(), top.clone())
            };
            for (j, row) in ui::chart(&data, cw, ch, *col).into_iter().enumerate() {
                let axis = if j == 0 {
                    top.clone()
                } else if j + 1 == ch {
                    "0".into()
                } else {
                    String::new()
                };
                let mut v = vec![Span::styled(
                    format!("{:>w$}  ", axis, w = LW - 2),
                    ui::faint(),
                )];
                v.extend(row.spans);
                out.push(Line::from(v));
            }
        }
    }
    out
}

/// The resources panel (Fleet, left under the list).
pub(crate) fn resources(
    f: &mut Frame,
    r: Rect,
    alias: &str,
    color: Color,
    st: Option<&Stats>,
    t0: Instant,
) {
    let mut title = vec![
        Span::styled("▌", ui::fg(color)),
        Span::styled(format!("{} ", crate::showcase::shown(alias)), ui::fg(color)),
        Span::styled("resources", ui::muted()),
    ];
    if let Some(st) = st {
        if st.uptime > 0 {
            title.push(Span::styled(
                format!(" · up {}", ui::dur(st.uptime)),
                ui::faint(),
            ));
        }
    }
    let block = ui::panel(Line::from(title), false)
        .title_bottom(Line::from(Span::styled(" v gpu · processes ", ui::faint())).right_aligned());
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height == 0 || inner.width < 20 {
        return;
    }
    let lines = match st {
        Some(st) if st.samples > 0 => {
            resource_view(st, inner.width as usize, inner.height as usize)
        }
        Some(st) if st.err.is_some() => vec![Line::from(Span::styled(
            ui::trunc(st.err.as_deref().unwrap_or(""), inner.width as usize),
            ui::fg(c::YELLOW()),
        ))],
        _ => vec![Line::from(Span::styled(
            format!("{} reading {alias}'s stats…", ui::spinner(t0)),
            ui::muted(),
        ))],
    };
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ machine view

/// History's most-used folders on a machine (cached: the file is read at most
/// every 15 s).
fn recent_dirs(host: &str) -> Vec<crate::history::Recent> {
    type Cache = Option<(String, Instant, Vec<crate::history::Recent>)>;
    static CACHE: Mutex<Cache> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    if let Some((h, t, v)) = g.as_ref() {
        if h == host && t.elapsed() < Duration::from_secs(15) {
            return v.clone();
        }
    }
    let v: Vec<_> = crate::history::recent_dirs(Some(host))
        .into_iter()
        .take(6)
        .collect();
    *g = Some((host.to_string(), Instant::now(), v.clone()));
    v
}

fn head(t: &str) -> Line<'static> {
    Line::from(Span::styled(
        t.to_string(),
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    ))
}

fn cfg_moshi(d: &Dash, alias: &str) -> String {
    d.cfg.host_get(alias, "moshi").unwrap_or_default()
}

fn kv(k: &str, v: Vec<Span<'static>>) -> Line<'static> {
    let mut l = vec![Span::styled(format!("{k:<10}"), ui::faint())];
    l.extend(v);
    Line::from(l)
}

/// The machine's buttons (each is also its Fleet key).
const BUTTONS: &[(Act, &str)] = &[
    (Act::New, "New session"),
    (Act::Presets, "Presets"),
    (Act::Close, "Close…"),
    (Act::Wall, "Tile"),
    (Act::EditHost, "Edit"),
    (Act::Refresh, "Reconnect"),
];

/// The right side when a machine row is selected; returns its buttons'
/// rects (for the mouse).
pub(crate) fn view(d: &Dash, f: &mut Frame, r: Rect, alias: &str) -> Vec<(Rect, Act)> {
    let Some(h) = d.fleet.host(alias) else {
        return vec![];
    };
    let st = d.stats(alias);
    let os = super::fleet::host_os(d, h);
    let hcol = h.color;
    // ---- the card: who it is
    let (state, scol) = match &h.conn {
        Conn::Up => (
            format!("● herdr {}", h.herdr_version.clone().unwrap_or_default()),
            c::GREEN(),
        ),
        Conn::Down(e) if crate::fleet::not_running(e) => {
            ("○ herdr not running".to_string(), c::MUTED())
        }
        Conn::Down(_) => ("✕ unreachable".to_string(), c::RED()),
        Conn::Unknown => (format!("{} connecting", ui::spinner(d.t0)), c::YELLOW()),
    };
    let block = ui::panel(
        Line::from({
            let mut t = icons::os_badge(&os, 0);
            t.push(Span::raw(" "));
            t.push(Span::styled(
                alias.to_string(),
                ui::fg(hcol).add_modifier(Modifier::BOLD),
            ));
            t
        }),
        true,
    )
    .border_style(ui::fg(hcol))
    .title(Line::from(Span::styled(format!(" {state} "), ui::fg(scol))).right_aligned());
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height < 3 || inner.width < 24 {
        return vec![];
    }
    // the buttons (one or two rows), then the facts
    let keys: Vec<String> = BUTTONS.iter().map(|(a, _)| d.keys.key_for(*a)).collect();
    let items: Vec<(&str, &str)> = BUTTONS
        .iter()
        .zip(&keys)
        .map(|((_, label), k)| (k.as_str(), *label))
        .collect();
    let rows = ui::button_rows(&items, inner.width).min(2);
    let [brow, _, inner] = Layout::vertical([
        Constraint::Length(rows),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(inner);
    let hits: Vec<(Rect, Act)> = ui::buttons(f, brow, &items)
        .into_iter()
        .map(|(rect, i)| (rect, BUTTONS[i].0))
        .collect();
    let w = inner.width as usize;
    let mut left: Vec<Line> = Vec::new();
    // identity
    let mut id = vec![Span::styled(
        if os.is_empty() {
            "—".to_string()
        } else {
            super::fleet::os_pretty(&os)
        },
        ui::bold(),
    )];
    if let Some(st) = &st {
        if !st.hostname.is_empty() && st.hostname != alias {
            id.push(dim(format!(" · {}", st.hostname)));
        }
        if st.uptime > 0 {
            id.push(dim(format!(" · up {}", ui::dur(st.uptime))));
        }
    }
    left.push(Line::from(id));
    let mut addr = vec![dim(if h.addr.is_empty() {
        "…".to_string()
    } else {
        h.addr.clone()
    })];
    if let Some(l) = h.latency {
        addr.push(dim(format!(" · {} ms", l.as_millis())));
    }
    if let Some((theirs, ours)) = h.drift() {
        addr.push(Span::styled(
            format!(" · ≠ herdr {theirs} here, {ours} on this computer"),
            ui::fg(c::YELLOW()),
        ));
    } else if h.herdr_restart() {
        addr.push(Span::styled(
            " · ↻ herdr wants a restart",
            ui::fg(c::YELLOW()),
        ));
    }
    left.push(Line::from(addr));
    if let Conn::Down(e) = &h.conn {
        left.push(Line::raw(""));
        for l in ui::wrap(e, w.saturating_sub(2)).into_iter().take(2) {
            left.push(Line::from(Span::styled(l, ui::fg(c::RED()))));
        }
        left.push(Line::from(vec![
            Span::styled("hint: ", ui::fg(c::BLUE()).add_modifier(Modifier::BOLD)),
            txt(super::fleet::hint_for(e)),
        ]));
    }
    left.push(Line::raw(""));

    // hardware
    left.push(head("Hardware"));
    match &st {
        Some(st) if st.samples > 0 => {
            if !st.cpu_model.is_empty() || st.ncpu > 0 {
                left.push(kv(
                    "cpu",
                    vec![
                        txt(st.cpu_model.clone()),
                        dim(format!("  {} cores", st.ncpu.max(st.cores.len()))),
                    ],
                ));
            }
            left.push(kv(
                "memory",
                vec![
                    txt(ui::bytes(st.mem_total)),
                    dim(if st.swap_total > 0.0 {
                        format!("  + {} swap", ui::bytes(st.swap_total))
                    } else {
                        String::new()
                    }),
                ],
            ));
            for g in &st.gpus {
                let model = g.name.replace("NVIDIA ", "");
                left.push(kv(
                    "gpu",
                    vec![
                        icons::gpu(&g.vendor),
                        txt(format!(" {model}")),
                        dim(format!("  {} VRAM", ui::bytes(g.mem_total))),
                    ],
                ));
            }
            for (m, t, _) in st.disks.iter().filter(|x| x.1 > 0.0).take(2) {
                left.push(kv("disk", vec![txt(ui::bytes(*t)), dim(format!("  {m}"))]));
            }
        }
        _ => {
            let gpus = d
                .agents
                .probe(alias)
                .map(|p| p.gpus.clone())
                .unwrap_or_default();
            for g in gpus {
                left.push(kv("gpu", vec![icons::gpu(&g), txt(format!(" {g}"))]));
            }
            left.push(Line::from(dim(if d.cfg.get_or("stats") == "off" {
                "machine stats are off (Settings › stats)"
            } else {
                "reading its stats…"
            })));
        }
    }
    left.push(Line::raw(""));
    // the OS logo beside the facts, fastfetch-style (as wide as there's room)
    let left_w = if w >= 100 { w / 2 } else { w };
    let (lc, lr) = if left_w >= 64 {
        (16, 8)
    } else if left_w >= 44 {
        (8, 4)
    } else {
        (0, 0)
    };
    if lc > 0 && !os.is_empty() && crate::logo::pictures(&d.cfg, lr) {
        let logo = crate::logo::lines(crate::logo::Kind::Os, &os, lc, lr, &d.cfg);
        let mut merged: Vec<Line> = Vec::new();
        let n = lr.max(left.len());
        let mut facts = left.into_iter();
        for i in 0..n {
            let mut sp: Vec<Span> = match logo.get(i) {
                Some(l) => l.spans.clone(),
                None if i < lr => vec![Span::raw(" ".repeat(lc))],
                None => Vec::new(),
            };
            let fact = facts.next();
            if i < lr {
                sp.push(Span::raw("  "));
            }
            if let Some(f) = fact {
                if i >= lr {
                    // under the logo: keep the facts' column
                    sp.push(Span::raw(" ".repeat(lc + 2)));
                }
                sp.extend(f.spans);
            }
            merged.push(Line::from(sp));
        }
        left = merged;
    }

    // agents installed there
    let mut right: Vec<Line> = Vec::new();
    right.push(head("Agents installed"));
    match d.agents.status(alias) {
        crate::agentwatch::Status::Ready { probe, refreshing } => {
            if probe.agents.is_empty() {
                right.push(Line::from(dim("none of the known agents")));
            }
            for a in &probe.agents {
                let mut l = vec![
                    icons::agent(&a.id),
                    txt(format!(" {}", crate::agents::label(&a.id))),
                ];
                if !a.version.is_empty() {
                    l.push(dim(format!("  {}", a.version)));
                }
                if a.on_path == Some(false) {
                    l.push(Span::styled("  not on herdr's PATH", ui::fg(c::YELLOW())));
                }
                right.push(Line::from(l));
            }
            // Moshi's own hook there, and mosh (attach_via = mosh)
            if probe.tool("moshi-hook").is_some() {
                let hooked = probe.moshi.join(", ");
                right.push(Line::from(vec![
                    Span::styled("◈ moshi-hook", ui::fg(c::BLUE())),
                    dim(match cfg_moshi(d, alias).as_str() {
                        "full" => "  Aleph sends everything too (host.moshi = full)".to_string(),
                        "off" => "  Aleph sends nothing (host.moshi = off)".to_string(),
                        _ if hooked.is_empty() => {
                            "  hooks no agent yet: Aleph sends all".to_string()
                        }
                        _ => format!(
                            "  tells the phone about {hooked}; Aleph adds down / up, limits"
                        ),
                    }),
                ]));
            }
            if probe.tool("mosh-server").is_some() {
                right.push(Line::from(vec![
                    Span::styled("⇄ mosh", ui::fg(c::GREEN())),
                    dim("  attaches can survive roaming (attach_via = mosh)"),
                ]));
            }
            if refreshing {
                right.push(Line::from(dim(format!(
                    "{} checking again…",
                    ui::spinner(d.t0)
                ))));
            }
        }
        crate::agentwatch::Status::Checking => right.push(Line::from(Span::styled(
            format!("{} looking…", ui::spinner(d.t0)),
            ui::muted(),
        ))),
        crate::agentwatch::Status::Failed(e) => right.push(Line::from(Span::styled(
            ui::trunc(&e, w),
            ui::fg(c::YELLOW()),
        ))),
        crate::agentwatch::Status::Unknown => right.push(Line::from(dim("not checked yet"))),
    }
    right.push(Line::from(dim(format!(
        "{} agents install <agent> --host {alias}",
        crate::doctor::me()
    ))));
    right.push(Line::raw(""));

    // sessions by state
    right.push(head("Sessions"));
    let t = super::fleet::tally(h.sessions.iter());
    if h.sessions.is_empty() {
        right.push(Line::from(dim("none running — n starts one here")));
    } else {
        let mut v = vec![txt(format!("{}  ", h.sessions.len()))];
        for (g, n, label, col) in [
            (ui::spinner(d.t0), t.working, "working", c::WORKING()),
            (crate::look::needs(), t.blocked, "need you", c::NEEDS()),
            ("✓", t.done, "done", c::GREEN()),
            ("●", t.idle, "idle", c::MUTED()),
            ("◷", t.limit, "at limit", c::LIMIT()),
        ] {
            if n > 0 {
                v.push(Span::styled(format!("{g} {n} {label}   "), ui::fg(col)));
            }
        }
        right.push(Line::from(v));
        let mut v = Vec::new();
        for (a, n) in &t.agents {
            v.push(icons::agent(a));
            v.push(txt(format!(" {} {n}   ", crate::agents::label(a))));
        }
        right.push(Line::from(v));
    }
    right.push(Line::raw(""));

    // usage today, from the agents' own logs
    right.push(head("Usage today"));
    match super::usage::today_for(d, alias).filter(|r| r.requests > 0) {
        Some(r) => {
            right.push(Line::from(super::usage::today_line(r)));
            right.push(Line::from(dim(format!(
                "{} usage --host {alias} · Monitor › Usage (u)",
                crate::doctor::me()
            ))));
        }
        None => right.push(Line::from(dim("nothing yet today"))),
    }
    right.push(Line::raw(""));

    // local LLM servers
    right.push(head("Local LLM servers"));
    match d.services.iter().find(|(a, _)| a == alias) {
        Some((_, w_)) => {
            let scan = w_.snapshot();
            if scan.found.is_empty() {
                right.push(Line::from(dim(if scan.at == 0 {
                    "looking…"
                } else {
                    "none running"
                })));
            } else {
                for s in &scan.found {
                    let mut l = vec![crate::services::badge(s.service)];
                    l.push(txt(format!(" {}", s.service.name)));
                    if let Some(p) = s.port {
                        l.push(dim(format!("  :{p}")));
                    }
                    let m = s.models_text();
                    if !m.is_empty() {
                        l.push(dim(format!("  {m}")));
                    }
                    right.push(Line::from(l));
                }
            }
        }
        None => right.push(Line::from(dim(format!(
            "off — Settings › services, or {} services --host {alias}",
            crate::doctor::me()
        )))),
    }
    right.push(Line::raw(""));

    // recent: events and folders
    let mut tail: Vec<Line> = Vec::new();
    let evs = super::inbox::for_host(d, alias, 5);
    if !evs.is_empty() {
        tail.push(head("Recent"));
        let now = crate::cache::now();
        for e in evs {
            let (head_, _) = super::inbox::reason(&e);
            tail.push(Line::from(vec![
                dim(format!("{:>8}  ", ui::ago(now.saturating_sub(e.t)))),
                Span::styled(
                    if e.name.is_empty() {
                        alias.to_string()
                    } else {
                        e.name.clone()
                    },
                    ui::bold(),
                ),
                txt(format!(" {head_}")),
            ]));
        }
        tail.push(Line::raw(""));
    }
    let dirs = recent_dirs(alias);
    if !dirs.is_empty() {
        tail.push(head("Recent folders"));
        for r_ in dirs {
            tail.push(Line::from(vec![
                txt(ui::trunc_left(&h.short_path(&r_.dir), w.saturating_sub(16))),
                dim(format!(
                    "  ×{}  {}",
                    r_.count,
                    ui::ago(crate::cache::now().saturating_sub(r_.last))
                )),
            ]));
        }
        tail.push(Line::raw(""));
    }
    tail.push(ui::hints(&[
        ("i", super::fleet::folder_hint(d, alias)),
        ("b", "broadcast"),
        (super::digit(super::View::Monitor), "monitor"),
        ("H", "hub"),
    ]));

    // two columns when there's room, else one
    if w >= 96 {
        let [a, b] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .spacing(3)
            .areas(inner);
        let mut l = left;
        l.extend(tail);
        f.render_widget(Paragraph::new(l), a);
        f.render_widget(Paragraph::new(right), b);
    } else {
        let mut l = left;
        l.extend(right);
        l.extend(tail);
        f.render_widget(Paragraph::new(l), inner);
    }
    hits
}
