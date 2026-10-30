//! The doctor inside Settings: the same checks as `aleph doctor`, for this
//! computer and every machine at once (a thread each), streamed in part by
//! part.
//!
//! Settings › Doctor lists them by machine (the daemon and the shortcuts are
//! this computer's), the machines with problems first: a strip with every
//! machine's state, then a block per machine — its head (where ssh goes, its
//! system, its herdr, its tally) and right under it what needs attention
//! there (failing checks and warnings: a mark, the name, the key fact, the
//! version); ↵ on a head shows every check, `g` arranges it by agent instead
//! (each agent's problems titled by machine, then the machines' other
//! checks). The selected row is shown in detail beside the list (under it
//! when narrow): a check's finding, why, its fix and what `f` does; a head's
//! whole checklist. Every other section shows its own checks in its Health
//! panel.

use super::{Bg, Chip, Dash};
use crate::config::Config;
use crate::doctor::{self, Counts, Fix, Kind, Line, Part, St, Summary};
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::{Padding, Paragraph};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) enum Msg {
    /// part `i` of run `gen` finished
    Part(u64, usize, Vec<Line>, Summary),
    /// a fix ran: what came of it
    Fixed(Result<String, String>),
}

/// What's being checked, and its lines once done.
type PartState = (String, Option<(Vec<Line>, Summary)>);

pub(crate) struct State {
    parts: Vec<PartState>,
    gen: u64,
    started: Option<Instant>,
    took: Option<Duration>,
    /// the list's first line on screen
    pub top: usize,
    /// lines the list shows (page up / down)
    pub page: usize,
    /// the selected row: a head, a check, or this terminal (in list order)
    pub sel: usize,
    /// a block per agent instead of per machine (`g`)
    pub by_agent: bool,
    /// the blocks showing every check, not just their problems (↵ on a head)
    pub open: std::collections::HashSet<String>,
}

impl Default for State {
    fn default() -> State {
        State {
            parts: vec![],
            gen: 0,
            started: None,
            took: None,
            top: 0,
            page: 10,
            sel: 0,
            by_agent: false,
            open: Default::default(),
        }
    }
}

impl State {
    pub fn running(&self) -> bool {
        self.parts.iter().any(|p| p.1.is_none())
    }
    /// Every line so far, part by part (heads included).
    pub fn lines(&self) -> Vec<&Line> {
        self.parts
            .iter()
            .filter_map(|p| p.1.as_ref())
            .flat_map(|(l, _)| l.iter())
            .collect()
    }
    pub fn sum(&self) -> Option<Summary> {
        if self.parts.is_empty() || self.running() {
            return None;
        }
        let mut s = Summary::default();
        for (_, x) in &self.parts {
            if let Some((_, p)) = x {
                s.ok += p.ok;
                s.warn += p.warn;
                s.bad += p.bad;
            }
        }
        Some(s)
    }
}

#[cfg(test)]
pub(crate) fn set_lines(d: &mut Dash, v: Vec<Line>) {
    let mut s = Summary::default();
    for l in &v {
        match l.st {
            St::Ok => s.ok += 1,
            St::Warn => s.warn += 1,
            St::Bad => s.bad += 1,
            _ => {}
        }
    }
    d.doc.parts = vec![("test".into(), Some((v, s)))];
}

/// Run it once, the first time it's needed.
pub(crate) fn ensure(d: &mut Dash) {
    if d.doc.parts.is_empty() {
        run(d);
    }
}

pub(crate) fn run(d: &mut Dash) {
    d.doc.gen += 1;
    d.doc.top = 0;
    d.doc.sel = 0;
    d.doc.took = None;
    d.doc.started = Some(Instant::now());
    let parts = doctor::parts(&d.cfg, d.flag.as_deref());
    d.doc.parts = parts
        .iter()
        .map(|p| {
            let what = match p {
                Part::Local => "this computer".to_string(),
                Part::Daemon => "the daemon".into(),
                Part::Host(t) => t.ssh.clone(),
                Part::NoHost => "machines".into(),
                Part::Shortcuts => "shortcuts".into(),
            };
            (what, None)
        })
        .collect();
    for (i, p) in parts.into_iter().enumerate() {
        let (g, tx) = (d.doc.gen, d.tx.clone());
        thread::spawn(move || {
            let cfg = Config::load();
            let mut lines = Vec::new();
            let s = doctor::run_part(&cfg, &p, &mut |l| lines.push(l));
            let _ = tx.send(Bg::Doc(Msg::Part(g, i, lines, s)));
        });
    }
}

pub(crate) fn on_msg(d: &mut Dash, m: Msg) {
    match m {
        Msg::Part(g, i, lines, s) if g == d.doc.gen => {
            if let Some(p) = d.doc.parts.get_mut(i) {
                p.1 = Some((lines, s));
            }
            if !d.doc.running() {
                d.doc.took = d.doc.started.map(|t| t.elapsed());
            }
        }
        Msg::Part(..) => {}
        Msg::Fixed(r) => {
            match r {
                Ok(m) => d.toast(super::Level::Ok, m),
                Err(e) => d.error(e),
            }
            run(d);
        }
    }
}

/// `f`: carry out a fix (asked first by the caller). Quiet ones run here,
/// off the UI thread; the rest need the terminal (a hand-off).
pub(crate) fn apply(d: &mut Dash, f: Fix) -> Option<super::Handoff> {
    use super::Handoff;
    match f {
        Fix::KeySetup(h) => Some(Handoff::KeySetup(h)),
        Fix::MachineAdd(h) => Some(Handoff::MachineAdd(h)),
        Fix::InstallAgent { agent, host } => Some(Handoff::Install {
            agent,
            host,
            dir: None,
        }),
        f @ Fix::CpuSensor { .. } => Some(Handoff::Fix(f)),
        f => {
            let tx = d.tx.clone();
            d.toast(super::Level::Info, format!("{}…", f.label()));
            thread::spawn(move || {
                let r = f
                    .run_quiet(&Config::load())
                    .unwrap_or_else(|| Err("that one needs a terminal".into()));
                let _ = tx.send(Bg::Doc(Msg::Fixed(r)));
            });
            None
        }
    }
}

/// The tab badge: the settings file not parsing first, a spinner while the
/// doctor runs, then its problems / warnings.
pub(crate) fn badge(d: &Dash) -> (String, Option<Color>) {
    if crate::config::broken() {
        return ("✗ config".into(), Some(c::RED()));
    }
    match (d.doc.sum(), d.doc.started) {
        (None, Some(t)) if d.doc.running() => (ui::spinner(t).to_string(), Some(c::ACCENT())),
        (Some(s), _) if s.bad > 0 => (format!("{}✗", s.bad), Some(c::RED())),
        (Some(s), _) if s.warn > 0 => (format!("{}!", s.warn), Some(c::YELLOW())),
        _ => (String::new(), None),
    }
}

/// Checks that belong to a Settings section, with their section heads:
/// (head, line) — problems first, then warnings, notes and passes.
pub(crate) fn for_section<'a>(d: &'a Dash, section: &str) -> Vec<(&'a str, &'a Line)> {
    let mut out = Vec::new();
    let mut head = "";
    for l in d.doc.lines() {
        if l.st == St::Head {
            head = &l.label;
        } else if doctor::section_for(head, &l.label) == section {
            out.push((head, l));
        }
    }
    out.sort_by_key(|(_, l)| match l.st {
        St::Bad => 0,
        St::Warn => 1,
        St::Info => 2,
        _ => 3,
    });
    out
}

/// Problems and warnings in a section: (bad, warn).
pub(crate) fn counts(d: &Dash, section: &str) -> (usize, usize) {
    let v = for_section(d, section);
    (
        v.iter().filter(|(_, l)| l.st == St::Bad).count(),
        v.iter().filter(|(_, l)| l.st == St::Warn).count(),
    )
}

// ------------------------------------------------------------------ the list

/// One row of the Doctor list.
pub(crate) enum Row<'a> {
    /// a machine (or This Mac, or an agent): its key (`group/b31`,
    /// `agent/Claude Code`), its name, a few facts, its tally, whether every
    /// check shows
    Head {
        key: String,
        name: String,
        facts: String,
        n: Counts,
        open: bool,
    },
    /// a check: its section head, the line, and its title in this view (an
    /// agent's check is titled by its machine)
    Check(&'a str, &'a Line, Option<String>),
    /// a closed block's passes, folded: how many
    More(usize),
    /// a part still being checked
    Waiting(&'a str),
    /// this terminal: its font and which logos it can show
    Terminal,
}

impl Row<'_> {
    fn selectable(&self) -> bool {
        matches!(self, Row::Head { .. } | Row::Check(..) | Row::Terminal)
    }
}

/// A failing check or a warning: shown without opening its block.
fn needs_attention(l: &Line) -> bool {
    matches!(Kind::of(l), Kind::Fail | Kind::Warn)
}

/// A block: its head, then its checks — every one when open, else just what
/// needs attention and how many passes are folded away.
fn block<'a>(
    out: &mut Vec<Row<'a>>,
    doc: &State,
    key: String,
    name: String,
    facts: String,
    items: Vec<(&'a str, &'a Line, Option<String>)>,
) {
    let open = doc.open.contains(&key);
    let n = Counts::of(items.iter().map(|(_, l, _)| *l));
    out.push(Row::Head {
        key,
        name,
        facts,
        n,
        open,
    });
    let total = items.len();
    let shown: Vec<_> = items
        .into_iter()
        .filter(|(_, l, _)| open || needs_attention(l))
        .collect();
    let folded = total - shown.len();
    out.extend(shown.into_iter().map(|(h, l, t)| Row::Check(h, l, t)));
    if folded > 0 {
        out.push(Row::More(folded));
    }
}

/// The list: a block per machine (or per agent, then the machines' other
/// checks), problems first; then what's still being checked, then this
/// terminal.
pub(crate) fn view(doc: &State) -> Vec<Row<'_>> {
    let mut out = Vec::new();
    if doc.by_agent {
        for (agent, items) in doctor::agent_groups(doc.lines()) {
            let mut on: Vec<&str> = items.iter().map(|(m, _, _)| m.as_str()).collect();
            on.dedup();
            let facts = format!("on {}", on.join(", "));
            let items = items.into_iter().map(|(m, h, l)| (h, l, Some(m))).collect();
            block(&mut out, doc, format!("agent/{agent}"), agent, facts, items);
        }
    }
    let mut gs = doctor::groups(doc.lines());
    gs.sort_by_key(|(_, v)| Counts::of(v.iter().map(|(_, l)| *l)).worst().rank());
    for (name, items) in gs {
        // by agent, a machine keeps its other checks (ssh, herdr, …)
        let items: Vec<_> = items
            .into_iter()
            .filter(|(h, l)| {
                !doc.by_agent || doctor::area_of(h, &l.label) != "agents" || l.label == "agents"
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        let facts = doctor::group_facts(&items);
        let items = items.into_iter().map(|(h, l)| (h, l, None)).collect();
        block(&mut out, doc, format!("group/{name}"), name, facts, items);
    }
    for (what, p) in &doc.parts {
        if p.is_none() {
            out.push(Row::Waiting(what));
        }
    }
    out.push(Row::Terminal);
    out
}

/// How many rows can be selected.
fn selectable(doc: &State) -> usize {
    view(doc).iter().filter(|r| r.selectable()).count()
}

/// Move the selection (↑↓, page up / down).
pub(crate) fn step(d: &mut Dash, by: isize) {
    let n = selectable(&d.doc);
    let cur = d.doc.sel.min(n.saturating_sub(1)) as isize;
    d.doc.sel = (cur + by).clamp(0, n.saturating_sub(1) as isize) as usize;
}

/// Select a row (a click).
pub(crate) fn select(d: &mut Dash, i: usize) {
    d.doc.sel = i.min(selectable(&d.doc).saturating_sub(1));
}

/// The selected check, when it's a check: (head, line).
pub(crate) fn selected(doc: &State) -> Option<(&str, &Line)> {
    let rows = view(doc);
    let n = rows.iter().filter(|r| r.selectable()).count();
    let sel = doc.sel.min(n.saturating_sub(1));
    rows.into_iter()
        .filter(|r| r.selectable())
        .nth(sel)
        .and_then(|r| match r {
            Row::Check(h, l, _) => Some((h, l)),
            _ => None,
        })
}

/// The selected block's key, when a head is selected.
pub(crate) fn selected_head(doc: &State) -> Option<String> {
    let rows = view(doc);
    let n = rows.iter().filter(|r| r.selectable()).count();
    let sel = doc.sel.min(n.saturating_sub(1));
    rows.into_iter()
        .filter(|r| r.selectable())
        .nth(sel)
        .and_then(|r| match r {
            Row::Head { key, .. } => Some(key),
            _ => None,
        })
}

/// Every check of a block (`group/b31`, `agent/Claude Code`), problems first.
fn block_checks(doc: &State, key: &str) -> Vec<(String, Line)> {
    if let Some(agent) = key.strip_prefix("agent/") {
        return doctor::agent_groups(doc.lines())
            .into_iter()
            .find(|(a, _)| a == agent)
            .map(|(_, v)| {
                v.into_iter()
                    .map(|(_, h, l)| (h.to_string(), l.clone()))
                    .collect()
            })
            .unwrap_or_default();
    }
    let name = key.strip_prefix("group/").unwrap_or(key);
    doctor::groups(doc.lines())
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| {
            v.into_iter()
                .filter(|(h, l)| {
                    !doc.by_agent || doctor::area_of(h, &l.label) != "agents" || l.label == "agents"
                })
                .map(|(h, l)| (h.to_string(), l.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// ↵ on a head: show every check of that block, or fold them again.
pub(crate) fn toggle_open(d: &mut Dash) -> bool {
    let Some(key) = selected_head(&d.doc) else {
        return false;
    };
    if !d.doc.open.remove(&key) {
        d.doc.open.insert(key);
    }
    true
}

/// `g`: a block per agent, or per machine again.
pub(crate) fn toggle_by_agent(d: &mut Dash) {
    d.doc.by_agent = !d.doc.by_agent;
    d.doc.sel = 0;
    d.doc.top = 0;
    d.note(if d.doc.by_agent {
        "the doctor by agent"
    } else {
        "the doctor by machine"
    });
}

/// What `f` would do for the selected check.
pub(crate) fn selected_fix(d: &Dash) -> Option<Fix> {
    selected(&d.doc).and_then(|(_, l)| l.action.clone())
}

/// `✗ 1 problem · ! 1 warning · + 3 tips · ✓ 24 passed  in 1.3s`, or what's
/// still being checked.
fn summary(d: &Dash, w: usize) -> ratatui::text::Line<'static> {
    let mut v = vec![Span::raw(" ")];
    match (d.doc.sum(), d.doc.started) {
        (None, Some(t)) => {
            let waiting: Vec<&str> = d
                .doc
                .parts
                .iter()
                .filter(|p| p.1.is_none())
                .map(|p| p.0.as_str())
                .collect();
            v.push(Span::styled(
                format!("{} ", ui::spinner(t)),
                ui::fg(c::ACCENT()),
            ));
            v.push(Span::styled(
                format!("Checking {}… ", waiting.join(", ")),
                ui::muted(),
            ));
        }
        (Some(_), _) => {
            let n = Counts::of(d.doc.lines());
            let s = |k: usize, one: &str, many: &str| {
                format!("{k} {}", if k == 1 { one } else { many })
            };
            let mut parts: Vec<(Kind, String)> = Vec::new();
            if n.fail > 0 {
                parts.push((Kind::Fail, s(n.fail, "problem", "problems")));
            }
            if n.warn > 0 {
                parts.push((Kind::Warn, s(n.warn, "warning", "warnings")));
            }
            if n.tip > 0 {
                parts.push((Kind::Tip, s(n.tip, "tip", "tips")));
            }
            if n.fail + n.warn == 0 {
                parts.push((Kind::Ok, format!("all {} passed", n.ok)));
            } else {
                parts.push((Kind::Ok, format!("{} passed", n.ok)));
            }
            for (i, (k, t)) in parts.into_iter().enumerate() {
                if i > 0 {
                    v.push(Span::styled(" · ", ui::faint()));
                }
                let (m, st) = k.mark();
                v.push(Span::styled(
                    format!("{m} {t}"),
                    st.add_modifier(Modifier::BOLD),
                ));
            }
            let took = d
                .doc
                .took
                .map(|t| format!("  in {:.1}s ", t.as_secs_f32()))
                .unwrap_or_else(|| " ".into());
            let used: usize = v.iter().map(|s| ui::width(&s.content)).sum();
            if used + ui::width(&took) <= w {
                v.push(Span::styled(took, ui::faint()));
            } else {
                v.push(Span::raw(" "));
            }
        }
        _ => {}
    }
    ratatui::text::Line::from(v)
}

/// A group's colour: a fleet machine's own, else the accent.
fn group_color(d: &Dash, name: &str) -> Color {
    if d.cfg.fleet().iter().any(|h| h == name) {
        crate::theme::machine(&d.cfg, name)
    } else {
        c::ACCENT()
    }
}

/// `✗ gpu2   ✓ b31   ✓ This Mac   ⋅ dex42` — every machine's state at a glance.
fn strip(d: &Dash, rows: &[Row], w: usize) -> ratatui::text::Line<'static> {
    let mut v: Vec<Span> = Vec::new();
    for r in rows {
        let (mark, st, name, strong) = match r {
            Row::Head { name, n, .. } => {
                let k = n.worst();
                let (m, st) = k.mark();
                (m.to_string(), st, name.clone(), k != Kind::Ok)
            }
            Row::Waiting(what) => (
                ui::spinner(d.doc.started.unwrap_or(d.t0)).to_string(),
                ui::fg(c::ACCENT()),
                what.to_string(),
                false,
            ),
            _ => continue,
        };
        if !v.is_empty() {
            v.push(Span::raw("   "));
        }
        v.push(Span::styled(format!("{mark} "), st));
        v.push(Span::styled(
            name,
            if strong { ui::bold() } else { ui::muted() },
        ));
    }
    ratatui::text::Line::from(doctor::clip_spans(v, w))
}

/// A machine's head: `● gpu2  tester@gpu2.lan:2222 · herdr 0.8.0     ✗ 1  ! 1  ✓ 10`.
fn head_row(
    d: &Dash,
    name: &str,
    facts: &str,
    n: &Counts,
    w: usize,
) -> ratatui::text::Line<'static> {
    let tally = n.spans();
    let tw: usize = tally.iter().map(|s| ui::width(&s.content)).sum();
    let name_w = ui::width(name).min(w.saturating_sub(tw + 4));
    let facts_w = w.saturating_sub(2 + name_w + 2 + tw + 2);
    let facts = if facts_w >= 8 {
        ui::trunc(facts, facts_w)
    } else {
        String::new()
    };
    let used = 2
        + name_w
        + if facts.is_empty() {
            0
        } else {
            2 + ui::width(&facts)
        };
    // an agent's head: its glyph in its colour; a machine's: a dot in its own
    let (dot, col) = match crate::agents::find(name) {
        Some(a) => {
            let (g, col) = crate::icons::brand(a.id);
            (format!("{g} "), col)
        }
        None => ("● ".to_string(), group_color(d, name)),
    };
    let mut v = vec![
        Span::styled(dot, ui::fg(col)),
        Span::styled(ui::trunc(name, name_w), ui::bold()),
    ];
    if !facts.is_empty() {
        v.push(Span::styled(format!("  {facts}"), ui::faint()));
    }
    v.push(Span::raw(" ".repeat(w.saturating_sub(used + tw))));
    v.extend(tally);
    ratatui::text::Line::from(v)
}

/// Title column width.
const TW: usize = 13;
/// What comes before a row's fact: `┃✗ ` and the title, then two spaces.
const PRE: usize = 3 + TW + 2;

/// A version as the list shows it: an agent's with its glyph.
fn version_spans(head: &str, l: &Line, ver: &str) -> Vec<Span<'static>> {
    let agent = (doctor::area_of(head, &l.label) == "agents")
        .then(|| crate::agents::find(&l.label))
        .flatten();
    match agent {
        Some(a) => {
            let (g, col) = crate::icons::brand(a.id);
            vec![
                Span::styled(format!("{g} "), ui::fg(col)),
                Span::styled(ver.to_string(), ui::muted()),
            ]
        }
        None => vec![Span::styled(ver.to_string(), ui::muted())],
    }
}

/// A check's row: `┃✗ DNS            gpu2.lan doesn't resolve        0.8.0`.
fn check_row(
    head: &str,
    l: &Line,
    title: Option<&str>,
    w: usize,
    vw: usize,
    sel: bool,
) -> ratatui::text::Line<'static> {
    let mut b = doctor::brief(head, l);
    if let Some(t) = title {
        b.title = t.to_string();
    }
    let k = Kind::of(l);
    let (m, ms) = k.mark();
    let fact_st = match k {
        Kind::Fail | Kind::Warn | Kind::Tip => ui::fg(k.color()),
        Kind::Note => ui::muted(),
        Kind::Ok => Style::new(),
    };
    let mut fact = Vec::new();
    if b.fact.is_empty() {
        if let Some(p) = &b.path {
            fact.push(Span::styled(p.clone(), ui::faint()));
        }
    } else {
        fact.push(Span::styled(b.fact.clone(), fact_st));
    }
    if !b.extra.is_empty() {
        fact.push(Span::styled(
            format!(
                "{}{}",
                if fact.is_empty() { "" } else { " · " },
                b.extra.join(" · ")
            ),
            ui::faint(),
        ));
    }
    let ver = b.version.as_deref().unwrap_or("");
    let ver_spans = if ver.is_empty() {
        vec![]
    } else {
        version_spans(head, l, &ui::trunc(ver, vw.saturating_sub(2).max(4)))
    };
    let ver_w: usize = ver_spans.iter().map(|s| ui::width(&s.content)).sum();
    // ┃ m title  fact … version
    let fw = w.saturating_sub(PRE + if vw > 0 { vw + 1 } else { 0 });
    let fact = doctor::clip_spans(fact, fw);
    let used: usize = fact.iter().map(|s| ui::width(&s.content)).sum();
    let mut v = vec![
        Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
        Span::styled(format!("{m} "), ms),
        Span::styled(
            ui::pad(&b.title, TW),
            match k {
                Kind::Fail | Kind::Warn => ui::bold(),
                Kind::Note => ui::muted(),
                _ => Style::new(),
            },
        ),
        Span::raw("  "),
    ];
    v.extend(fact);
    let right = w.saturating_sub(PRE + used + ver_w);
    v.push(Span::raw(" ".repeat(right)));
    v.extend(ver_spans);
    if sel {
        v = selected_row(v, k.color());
    }
    ratatui::text::Line::from(v)
}

/// A row as selected: the bar stays as it is, the mark keeps its colour,
/// the rest on the selection.
fn selected_row(v: Vec<Span<'static>>, mark: Color) -> Vec<Span<'static>> {
    v.into_iter()
        .enumerate()
        .map(|(i, s)| match i {
            0 => s,
            1 if !c::mono() => s.patch_style(ui::selected().fg(mark)),
            _ => s.patch_style(ui::selected()),
        })
        .collect()
}

/// This terminal's row: its font, and how logos show.
fn terminal_row(d: &Dash, w: usize, sel: bool) -> ratatui::text::Line<'static> {
    let (nerd, font) = crate::icons::terminal_font();
    let tier = crate::logo::decide(&d.cfg, crate::logo::size(&d.cfg).1).tier;
    let k = if nerd == Some(false) {
        Kind::Tip
    } else {
        Kind::Note
    };
    let (m, ms) = k.mark();
    let fact = format!(
        "{} · logos as {}",
        font.unwrap_or_else(|| "font unknown".into()),
        tier.as_str()
    );
    let mut v = vec![
        Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
        Span::styled(format!("{m} "), ms),
        Span::styled(ui::pad("Fonts & logos", TW), ui::bold()),
        Span::raw("  "),
        Span::styled(ui::trunc(&fact, w.saturating_sub(PRE)), ui::muted()),
    ];
    if sel {
        let used: usize = v.iter().map(|s| ui::width(&s.content)).sum();
        v.push(Span::raw(" ".repeat(w.saturating_sub(used))));
        v = selected_row(v, k.color());
    }
    ratatui::text::Line::from(v)
}

/// Settings › Doctor: the list (with every machine's state on top) and the
/// selected check in detail — beside it when wide, under it when narrow.
pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect, focused: bool) {
    let wide = r.width >= 110;
    let (lr, dr) = if wide {
        let dw = (r.width * 2 / 5).clamp(40, 64);
        let [a, b] = Layout::horizontal([Constraint::Min(40), Constraint::Length(dw)]).areas(r);
        (a, b)
    } else {
        let dh = (r.height * 2).div_ceil(5).clamp(7, 11);
        let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(dh)]).areas(r);
        (a, b)
    };
    draw_list(d, f, lr, focused, wide);
    draw_detail(d, f, dr, wide);
}

fn draw_list(d: &mut Dash, f: &mut Frame, r: Rect, focused: bool, wide: bool) {
    // the title carries the arrangement as a strip: the one shown lit, v for the other
    let mut title = vec![Span::raw(match &d.flag {
        Some(h) => format!("Doctor · {h}"),
        None => "Doctor".into(),
    })];
    title.push(Span::styled("   ", ui::faint()));
    for (i, (name, on)) in [("Machines", !d.doc.by_agent), ("Agents", d.doc.by_agent)]
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            title.push(Span::styled(" │ ", ui::faint()));
        }
        title.push(Span::styled(
            name,
            if on {
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else {
                ui::muted().remove_modifier(Modifier::BOLD)
            },
        ));
    }
    title.push(Span::styled("  v", ui::faint()));
    let mut block = ui::panel(ratatui::text::Line::from(title), focused)
        .title_bottom(summary(d, r.width.saturating_sub(4) as usize));
    if !wide {
        block = block.padding(Padding::horizontal(1));
    }
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height < 2 || inner.width < 20 {
        return;
    }
    let w = inner.width as usize;
    let (top, page, chips) = {
        let rows = view(&d.doc);
        let n_sel = rows.iter().filter(|r| r.selectable()).count();
        let sel = d.doc.sel.min(n_sel.saturating_sub(1));
        let vw = rows
            .iter()
            .filter_map(|r| match r {
                Row::Check(h, l, _) => doctor::brief(h, l).version.map(|v| {
                    ui::width(&v)
                        + if doctor::area_of(h, &l.label) == "agents" {
                            2
                        } else {
                            0
                        }
                }),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            .min(14);
        let mut lines: Vec<ratatui::text::Line> = Vec::new();
        // which line each selectable row is on
        let mut at: Vec<usize> = Vec::new();
        for row in &rows {
            match row {
                Row::Head { name, facts, n, .. } => {
                    if !lines.is_empty() {
                        lines.push(ratatui::text::Line::raw(""));
                    }
                    at.push(lines.len());
                    // selected: the bar before it, the rest on the selection
                    let picked = at.len() - 1 == sel;
                    let head = head_row(d, name, facts, n, w.saturating_sub(1));
                    let mut v = vec![Span::styled(
                        if picked { "┃" } else { " " },
                        ui::fg(c::ACCENT()),
                    )];
                    v.extend(head.spans);
                    if picked {
                        v = selected_row(v, n.worst().color());
                    }
                    lines.push(ratatui::text::Line::from(v));
                }
                Row::Check(h, l, title) => {
                    at.push(lines.len());
                    lines.push(check_row(
                        h,
                        l,
                        title.as_deref(),
                        w,
                        vw,
                        at.len() - 1 == sel,
                    ));
                }
                Row::More(n) => {
                    lines.push(ratatui::text::Line::from(Span::styled(
                        format!("   · {n} more · ↵ on the name shows them"),
                        ui::faint(),
                    )));
                }
                Row::Waiting(what) => {
                    if !lines.is_empty() {
                        lines.push(ratatui::text::Line::raw(""));
                    }
                    lines.push(ratatui::text::Line::from(Span::styled(
                        format!(
                            "{} checking {what}…",
                            ui::spinner(d.doc.started.unwrap_or(d.t0))
                        ),
                        ui::muted(),
                    )));
                }
                Row::Terminal => {
                    lines.push(ratatui::text::Line::raw(""));
                    lines.push(ratatui::text::Line::from(vec![
                        Span::styled("● ", ui::fg(c::ACCENT())),
                        Span::styled("This terminal", ui::bold()),
                    ]));
                    at.push(lines.len());
                    lines.push(terminal_row(d, w, at.len() - 1 == sel));
                }
            }
        }
        // the strip on top, then the rows (scrolled to keep the selection in view)
        let strip_h = if inner.height >= 12 { 2 } else { 1 };
        f.render_widget(
            Paragraph::new(vec![strip(d, &rows, w)]),
            Rect { height: 1, ..inner },
        );
        let body = Rect {
            y: inner.y + strip_h,
            height: inner.height.saturating_sub(strip_h),
            ..inner
        };
        let page = body.height as usize;
        let mut top = d.doc.top.min(lines.len().saturating_sub(page));
        if let Some(&y) = at.get(sel) {
            // keep the machine's head in view above its first check
            let want = if y >= 2 && at.first() == Some(&y) {
                0
            } else {
                y.saturating_sub(1)
            };
            if want < top {
                top = want;
            }
            if y >= top + page {
                top = y + 1 - page;
            }
        }
        let mut chips = Vec::new();
        for (i, &y) in at.iter().enumerate() {
            if y >= top && y < top + page {
                chips.push((
                    Rect::new(body.x, body.y + (y - top) as u16, body.width, 1),
                    Chip::Check(i),
                ));
            }
        }
        f.render_widget(Paragraph::new(lines).scroll((top as u16, 0)), body);
        (top, page, chips)
    };
    d.doc.top = top;
    d.doc.page = page.max(1);
    d.hits.chips.extend(chips);
}

/// `label    value` lines, the value wrapped under itself.
fn kv(
    lines: &mut Vec<ratatui::text::Line<'static>>,
    label: &str,
    value: &str,
    st: Style,
    w: usize,
) {
    const LW: usize = 9;
    for (i, chunk) in ui::wrap(value, w.saturating_sub(LW).max(8))
        .into_iter()
        .enumerate()
    {
        lines.push(ratatui::text::Line::from(vec![
            Span::styled(
                format!("{:<LW$}", if i == 0 { label } else { "" }),
                ui::faint(),
            ),
            Span::styled(chunk, st),
        ]));
    }
}

/// ` problem ` on its colour, in dark ink (reversed without colour).
fn pill(text: &str, col: Color) -> Span<'static> {
    let st = if c::mono() {
        Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::new()
            .bg(col)
            .fg(c::BASE())
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(format!(" {text} "), st)
}

/// The selected check: where it is, what it found, why, its fix and what
/// `f` does — or this terminal's fonts and logos.
fn draw_detail(d: &mut Dash, f: &mut Frame, r: Rect, wide: bool) {
    let frame = |t: ratatui::text::Line<'static>| {
        let b = ui::panel(t, false);
        if wide {
            b
        } else {
            b.padding(Padding::horizontal(1))
        }
    };
    let w = (frame("".into()).inner(r).width as usize).max(10);
    let rows = view(&d.doc);
    let n_sel = rows.iter().filter(|r| r.selectable()).count();
    let sel = d.doc.sel.min(n_sel.saturating_sub(1));
    let picked = rows.iter().filter(|r| r.selectable()).nth(sel);
    let mut lines: Vec<ratatui::text::Line> = Vec::new();
    let title: ratatui::text::Line = match picked {
        Some(Row::Head { key, name, n, .. }) => {
            // the block's whole checklist, problems first
            let all: Vec<(String, Line)> = block_checks(&d.doc, key);
            for (h, l) in &all {
                let title = key
                    .starts_with("agent/")
                    .then(|| doctor::group_of(h).0.to_string());
                lines.push(check_row(h, l, title.as_deref(), w, 0, false));
            }
            let mut t = vec![Span::styled(format!("{name}  "), ui::bold())];
            t.extend(n.spans());
            ratatui::text::Line::from(t)
        }
        Some(Row::Check(head, l, _)) => {
            let b = doctor::brief(head, l);
            let k = Kind::of(l);
            let (m, ms) = k.mark();
            let (group, _) = doctor::group_of(head);
            let area = doctor::area_of(head, &l.label);
            let place = if area.is_empty() {
                group.to_string()
            } else {
                format!("{group} · {area}")
            };
            type Ls = Vec<ratatui::text::Line<'static>>;
            let fact_st = match k {
                Kind::Fail | Kind::Warn | Kind::Tip => {
                    ui::fg(k.color()).add_modifier(Modifier::BOLD)
                }
                _ => Style::new(),
            };
            let found_text = [b.fact.clone()]
                .into_iter()
                .chain(b.extra.iter().cloned())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" · ");
            let mut found: Ls = Vec::new();
            if !found_text.is_empty() {
                kv(&mut found, "found", &found_text, fact_st, w);
            }
            // its version and path, a line each (or one line, when it's tight)
            let mut at: Ls = Vec::new();
            if let Some(v) = &b.version {
                kv(&mut at, "version", v, Style::new(), w);
            }
            if let Some(p) = &b.path {
                kv(
                    &mut at,
                    "path",
                    &ui::trunc_left(p, w.saturating_sub(9)),
                    ui::muted(),
                    w,
                );
            }
            let mut at_one: Ls = Vec::new();
            if let (Some(v), Some(p)) = (&b.version, &b.path) {
                let p = ui::trunc_left(p, w.saturating_sub(12 + ui::width(v)));
                kv(
                    &mut at_one,
                    "version",
                    &format!("{v} · {p}"),
                    Style::new(),
                    w,
                );
            } else {
                at_one = at.clone();
            }
            let fixes: Vec<(String, Option<String>)> =
                l.fix.iter().map(|x| doctor::fix_parts(x)).collect();
            let mut why: Ls = Vec::new();
            if let Some(y) = b
                .why
                .clone()
                .or_else(|| fixes.iter().find_map(|(_, w)| w.clone()))
            {
                kv(&mut why, "why", &y, ui::muted(), w);
            }
            let mut fix: Ls = Vec::new();
            for (i, (cmd, _)) in fixes.iter().enumerate() {
                kv(
                    &mut fix,
                    if i == 0 { "fix" } else { "" },
                    cmd,
                    ui::fg(c::BLUE()),
                    w,
                );
            }
            let mut act: Ls = Vec::new();
            if let Some(a) = &l.action {
                for (i, chunk) in ui::wrap(&a.label(), w.saturating_sub(5).max(8))
                    .into_iter()
                    .enumerate()
                {
                    act.push(ratatui::text::Line::from(vec![
                        if i == 0 {
                            ui::keycap("f")
                        } else {
                            Span::raw("   ")
                        },
                        Span::styled(format!("  {chunk}"), ui::fg(c::ACCENT())),
                    ]));
                }
            }
            let room = frame("".into()).inner(r).height as usize;
            let all = found.len() + at.len() + why.len() + fix.len() + act.len();
            if wide {
                // where it is as a pill, then each part with room around it
                let blank = || ratatui::text::Line::raw("");
                lines.push(ratatui::text::Line::from(vec![
                    pill(k.word(), k.color()),
                    Span::styled(
                        format!("  {}", ui::trunc(&place, w.saturating_sub(12))),
                        ui::muted(),
                    ),
                ]));
                lines.push(blank());
                lines.extend(found);
                lines.extend(at);
                lines.extend(why);
                for part in [fix, act] {
                    if !part.is_empty() {
                        lines.push(blank());
                        lines.extend(part);
                    }
                }
            } else if all <= room {
                lines.extend([found, at, why, fix, act].into_iter().flatten());
            } else {
                // tight: what it found, why and the fix before where it is
                lines.extend([found, why, fix, act, at_one].into_iter().flatten());
            }
            let mut t = vec![Span::styled(format!("{m} "), ms), Span::raw(b.title)];
            if !wide {
                t.push(Span::styled(
                    format!("  {place}"),
                    ui::muted().remove_modifier(Modifier::BOLD),
                ));
            }
            ratatui::text::Line::from(t)
        }
        Some(Row::Terminal) => {
            lines.extend(crate::icons::preview_lines(&d.cfg.get_or("icons")));
            lines.extend(crate::icons::logo_lines(&d.cfg));
            lines.push(ratatui::text::Line::raw(""));
            lines.push(ratatui::text::Line::from(vec![
                Span::styled("aleph icons", ui::bold()),
                Span::styled(" shows every logo", ui::faint()),
            ]));
            ratatui::text::Line::from("This terminal")
        }
        _ => {
            lines.push(ratatui::text::Line::from(Span::styled(
                format!("{} checking…", ui::spinner(d.doc.started.unwrap_or(d.t0))),
                ui::muted(),
            )));
            ratatui::text::Line::from("Check")
        }
    };
    f.render_widget(Paragraph::new(lines).block(frame(title)), r);
}
