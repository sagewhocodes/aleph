//! Monitor (key 3): machine stats in depth. Sub-views — Fleet (every machine
//! side by side) · Overview (everything about one machine) · CPU · Memory ·
//! GPU · Processes · Network · Services · Timeline (every session's states,
//! `timechart`) · Usage — drawn by `monitor_draw`.
//!
//! Keys: `←`/`→` (or `h`/`l`) cycle sub-views, their letter jumps
//! (f o c m g p n s t u), `[`/`]` pick the machine (`↑`/`↓` too, except in
//! Processes and Timeline). In Fleet, `enter` opens that machine's Overview. In Overview,
//! `F` cycles the focus (all → CPU → memory → GPU → processes) and `P` shows
//! or hides the process list. In Processes: `↑`/`↓` select, `t` tree / list,
//! `←`/`→` fold / unfold (← on a leaf goes to its parent), `space` toggles a
//! fold, `+`/`-` unfold / fold everything, `s` sorts, `S` reverses, `/`
//! filters. Streams run for every fleet machine while this view is open.

use super::monitor_draw::{self as md, Focus, MachineRow, ProcUi, Sub};
use super::{Dash, Handoff};
use crate::fleet::Conn;
use crate::telemetry::Stats;
use crate::ui;
use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

#[derive(Default)]
pub(crate) struct State {
    pub sel: usize,
    pub sub: Sub,
    pub filtering: bool,
    /// what the Overview gives most room to
    focus: Focus,
    /// the Overview leaves the process list out
    procs_off: bool,
    /// the Processes table (sort, tree, folds, filter, selection)
    pv: ProcUi,
    hits: Vec<(Rect, Sub)>,
}

pub(crate) fn enter(d: &mut Dash) {
    if let Some(h) = d.fv.selected_host(d) {
        if let Some(i) = d.fleet.hosts.iter().position(|x| x.alias == h) {
            d.mon.sel = i;
        }
    }
}

fn n_hosts(d: &Dash) -> usize {
    d.fleet.hosts.len().max(1)
}

/// ⇥ / ⇧⇥: the next / previous view.
pub(crate) fn sub_step(d: &mut Dash, fwd: bool) {
    let pages = pages(d);
    let n = pages.len();
    let i = pages.iter().position(|s| *s == d.mon.sub).unwrap_or(0);
    let s = pages[if fwd { (i + 1) % n } else { (i + n - 1) % n }];
    set_sub(d, s);
}

/// Monitor's pages. Services only while looking for LLM servers is on (or
/// when there are some to show, as in showcase's demo world).
pub(crate) fn pages(d: &Dash) -> Vec<Sub> {
    Sub::shown(d.cfg.flag("services") || !d.services.is_empty())
}

pub(crate) fn set_sub(d: &mut Dash, s: Sub) {
    d.mon.sub = s;
    d.mon.filtering = false;
}

/// Keys while typing a process filter.
pub(crate) fn filter_key(d: &mut Dash, k: KeyEvent) {
    let pv = &mut d.mon.pv;
    match k.code {
        KeyCode::Esc => {
            pv.filter.clear();
            d.mon.filtering = false;
        }
        KeyCode::Enter => d.mon.filtering = false,
        KeyCode::Backspace => {
            pv.filter.pop();
        }
        KeyCode::Char(ch) => {
            pv.filter.push(ch);
            pv.jump(0);
            pv.scroll = 0;
        }
        _ => {}
    }
}

fn toast(d: &mut Dash, msg: String) {
    d.toast(super::Level::Info, msg);
}

/// Keys of the Processes sub-view; false when the key isn't one of them.
fn procs_key(d: &mut Dash, k: KeyEvent) -> bool {
    let pv = &mut d.mon.pv;
    let page = pv.page.max(1) as isize;
    match k.code {
        KeyCode::Down | KeyCode::Char('j') => pv.step(1),
        KeyCode::Up | KeyCode::Char('k') => pv.step(-1),
        KeyCode::PageDown => pv.step(page),
        KeyCode::PageUp => pv.step(-page),
        KeyCode::Home => pv.jump(0),
        KeyCode::End => pv.jump(usize::MAX),
        KeyCode::Left if pv.tree => pv.left(),
        KeyCode::Right if pv.tree => pv.right(),
        KeyCode::Char(' ') => pv.toggle(),
        KeyCode::Char('+') | KeyCode::Char('=') => pv.collapsed.clear(),
        KeyCode::Char('-') => pv.collapse_all = true,
        KeyCode::Char('t') => {
            pv.tree = !pv.tree;
            let m = if pv.tree { "process tree" } else { "flat list" };
            toast(d, m.into());
        }
        KeyCode::Char('s') => {
            pv.sort = pv.sort.next();
            pv.rev = false;
            let m = format!("sorted by {}", pv.sort.name());
            toast(d, m);
        }
        KeyCode::Char('S') => {
            pv.rev = !pv.rev;
            let m = format!(
                "sorted by {}{}",
                pv.sort.name(),
                if pv.rev { ", reversed" } else { "" }
            );
            toast(d, m);
        }
        KeyCode::Char('/') => d.mon.filtering = true,
        _ => return false,
    }
    true
}

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    let n = n_hosts(d);
    if d.mon.sub == Sub::Procs && procs_key(d, k) {
        return None;
    }
    if d.mon.sub == Sub::Usage && super::usage::on_key(d, k) {
        return None;
    }
    if d.mon.sub == Sub::Timeline && super::timechart::monitor_key(d, k) {
        return None;
    }
    if d.mon.sub == Sub::Overview {
        match k.code {
            KeyCode::Char('F') => {
                d.mon.focus = d.mon.focus.next();
                let m = format!("focus: {}", d.mon.focus.name());
                toast(d, m);
                return None;
            }
            KeyCode::Char('P') => {
                d.mon.procs_off = !d.mon.procs_off;
                let m = if d.mon.procs_off {
                    "processes hidden"
                } else {
                    "processes shown"
                };
                toast(d, m.into());
                return None;
            }
            _ => {}
        }
    }
    match k.code {
        KeyCode::Left | KeyCode::Char('h') => sub_step(d, false),
        KeyCode::Right | KeyCode::Char('l') => sub_step(d, true),
        KeyCode::Char(']') => {
            d.mon.sel = (d.mon.sel + 1) % n;
            super::usage::machine_changed(d);
        }
        KeyCode::Char('[') => {
            d.mon.sel = (d.mon.sel + n - 1) % n;
            super::usage::machine_changed(d);
        }
        KeyCode::Down | KeyCode::Char('j') => d.mon.sel = (d.mon.sel + 1) % n,
        KeyCode::Up | KeyCode::Char('k') => d.mon.sel = (d.mon.sel + n - 1) % n,
        KeyCode::Enter if d.mon.sub == Sub::Fleet => set_sub(d, Sub::Overview),
        KeyCode::Char(ch) => {
            if let Some(s) = Sub::from_key(ch).filter(|s| pages(d).contains(s)) {
                set_sub(d, s);
            }
        }
        _ => {}
    }
    None
}

pub(crate) fn mouse(d: &mut Dash, m: MouseEvent) {
    match m.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let hit = d.mon.hits.iter().find(|(r, _)| {
                m.column >= r.x
                    && m.column < r.x + r.width
                    && m.row >= r.y
                    && m.row < r.y + r.height
            });
            if let Some((_, s)) = hit {
                let s = *s;
                set_sub(d, s);
            }
        }
        MouseEventKind::ScrollDown if d.mon.sub == Sub::Procs => d.mon.pv.step(3),
        MouseEventKind::ScrollUp if d.mon.sub == Sub::Procs => d.mon.pv.step(-3),
        _ => {}
    }
}

pub(crate) fn hints(d: &Dash) -> Vec<(&'static str, &'static str)> {
    if d.mon.filtering {
        return vec![("enter", "keep filter"), ("esc", "clear")];
    }
    match d.mon.sub {
        Sub::Usage => super::usage::hints(),
        Sub::Timeline => super::timechart::monitor_hints(d),
        Sub::Services => vec![
            ("[ ]", "machine"),
            ("⇥ ←→", "view"),
            ("f o c m g p n s t u", "jump"),
            ("esc", "back"),
        ],
        Sub::Procs => {
            let mut v = vec![("↑↓", "select")];
            if d.mon.pv.tree {
                v.push(("←→ space", "fold"));
                v.push(("t", "flat list"));
            } else {
                v.push(("t", "tree"));
            }
            v.extend([
                ("s S", "sort"),
                ("/", "filter"),
                ("[ ]", "machine"),
                ("o c m g n", "views"),
                ("esc", "back"),
            ]);
            v
        }
        Sub::Fleet => vec![
            ("↑↓", "machine"),
            ("enter", "its overview"),
            ("⇥ ←→", "view"),
            ("f o c m g p n", "jump"),
            ("esc", "back"),
        ],
        Sub::Overview => vec![
            ("F", focus_hint(d.mon.focus)),
            (
                "P",
                if d.mon.procs_off {
                    "show processes"
                } else {
                    "hide processes"
                },
            ),
            ("⇥ ←→", "view"),
            ("[ ]", "machine"),
            ("f o c m g p n", "jump"),
            ("esc", "back"),
        ],
        _ => vec![
            ("⇥ ←→", "view"),
            ("[ ]", "machine"),
            ("f o c m g p n", "jump"),
            ("esc", "back"),
        ],
    }
}

/// What `F` does next.
fn focus_hint(f: Focus) -> &'static str {
    match f.next() {
        Focus::All => "focus: all",
        Focus::Cpu => "focus: CPU",
        Focus::Mem => "focus: memory",
        Focus::Gpu => "focus: GPU",
        Focus::Procs => "focus: processes",
    }
}

fn note_of(h: &crate::fleet::Host) -> String {
    match &h.conn {
        Conn::Up => String::new(),
        Conn::Down(e) if crate::fleet::not_running(e) => "herdr not running".into(),
        Conn::Down(e) => e.clone(),
        Conn::Unknown => "connecting…".into(),
    }
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    if d.fleet.hosts.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no machines yet — + in Fleet adds one",
                ui::muted(),
            )),
            r,
        );
        return;
    }
    let sel = d.mon.sel.min(d.fleet.hosts.len() - 1);
    d.mon.sel = sel;
    let (alias, color) = {
        let h = &d.fleet.hosts[sel];
        (h.alias.clone(), h.color)
    };
    let [strip_r, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(3)]).areas(r);
    let st: Option<Stats> = d.stats(&alias);
    let updated = st.as_ref().and_then(|s| s.updated);
    let shown = pages(d);
    d.mon.hits = md::strip(f, strip_r, d.mon.sub, &alias, color, updated, &shown);
    let empty = Stats::default();
    if d.mon.sub == Sub::Fleet {
        // every machine's stats, for the table and its charts
        let snaps: Vec<(String, Color, Option<Stats>, bool, String)> = d
            .fleet
            .hosts
            .iter()
            .map(|h| {
                // reachable = its stats arrive (herdr itself may not be running)
                let st = d.stats(&h.alias);
                let up = st.as_ref().is_some_and(|s| s.samples > 0) || h.conn == Conn::Up;
                let note = if up { String::new() } else { note_of(h) };
                (h.alias.clone(), h.color, st, up, note)
            })
            .collect();
        let rows: Vec<MachineRow> = snaps
            .iter()
            .map(|(a, col, s, up, note)| MachineRow {
                alias: a,
                color: *col,
                stats: s.as_ref(),
                up: *up,
                note,
            })
            .collect();
        md::fleet(f, body, &rows, sel, d.t0);
        return;
    }
    // every machine at once: no machine header
    if d.mon.sub == Sub::Usage {
        super::usage::draw(d, f, body);
        return;
    }
    // (turned off while it was open: Monitor's first page)
    if d.mon.sub == Sub::Services && !pages(d).contains(&Sub::Services) {
        d.mon.sub = Sub::Fleet;
    }
    if d.mon.sub == Sub::Services {
        draw_services(d, f, body);
        return;
    }
    if d.mon.sub == Sub::Timeline {
        super::timechart::draw_monitor(d, f, body);
        return;
    }
    let st = st.as_ref().unwrap_or(&empty);
    // the machine strip, then the sub-view
    let [head, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(2)]).areas(body);
    md::header(f, head, &alias, color, st);
    match d.mon.sub {
        Sub::Overview => {
            // the layout keys: p hides the processes, z maximises the charts
            // or the processes
            let shape = super::panes::shape(d, "monitor");
            use super::panes::Max;
            if shape.max == Max::Side {
                d.mon.pv.typing = false;
                md::procs(f, body, st, &mut d.mon.pv);
            } else {
                let procs = !d.mon.procs_off && !shape.hidden && shape.max != Max::Main;
                md::overview_body(f, body, st, d.t0, d.mon.focus, procs)
            }
        }
        Sub::Cpu => md::cpu(f, body, st),
        Sub::Memory => md::memory(f, body, st),
        Sub::Gpu => md::gpu(f, body, st),
        Sub::Net => md::net(f, body, st),
        Sub::Procs => {
            d.mon.pv.typing = d.mon.filtering;
            md::procs(f, body, st, &mut d.mon.pv);
        }
        Sub::Fleet | Sub::Services | Sub::Timeline | Sub::Usage => {}
    }
}

/// Monitor › Services: every machine's LLM servers and gateways, a panel
/// each (the selected machine first), with their logos.
fn draw_services(d: &Dash, f: &mut Frame, r: Rect) {
    if d.services.is_empty() {
        let lines = vec![
            Line::from(Span::styled("Looking for LLM servers is off.", ui::bold())),
            Line::from(vec![
                Span::styled("aleph settings set services on", ui::fg(crate::ui::c::ACCENT())),
                Span::styled(
                    " finds Ollama, LM Studio, vLLM, llama.cpp, LiteLLM and the others on every machine.",
                    ui::muted(),
                ),
            ]),
        ];
        f.render_widget(
            Paragraph::new(lines),
            r.inner(ratatui::layout::Margin::new(1, 1)),
        );
        return;
    }
    let sel = d.fleet.hosts.get(d.mon.sel).map(|h| h.alias.clone());
    let mut order: Vec<&(String, crate::services::Watcher)> = d.services.iter().collect();
    order.sort_by_key(|(a, _)| Some(a) != sel.as_ref());
    let scans: Vec<(String, Vec<crate::services::Found>)> =
        order.iter().map(|(a, w)| (a.clone(), w.found())).collect();
    // rows by how much each has: at least 3, the rest shared
    let need: Vec<u16> = scans
        .iter()
        .map(|(_, v)| (v.len() as u16 * 2 + 2).clamp(3, 12))
        .collect();
    let mut rects = Vec::new();
    let mut y = r.y;
    for (i, n) in need.iter().enumerate() {
        if y >= r.y + r.height {
            break;
        }
        let left = r.y + r.height - y;
        let h = if i + 1 == need.len() {
            left
        } else {
            (*n).min(left)
        };
        rects.push(Rect::new(r.x, y, r.width, h));
        y += h;
    }
    for ((alias, found), rr) in scans.iter().zip(rects) {
        crate::services::panel(f, rr, alias, found, d.t0);
    }
}
