//! Fleet's board: the machines list maximised (`z`), in bands. Each machine
//! is a band across the screen: who it is, then its CPU, memory and every
//! GPU as meters with a sparkline each; under that its network, disks,
//! usage today and who uses its GPUs; then its sessions (in
//! `fleet.board_order` within the machine: needs you first, working first,
//! most recent, or herdr's own), each with its state, this turn, model,
//! folder and branch, the day's timeline and its totals, and under it the
//! agent's latest real output, read live for the rows on screen (every 5 s,
//! every 2 s while it works) and never kept. Keys and clicks are the list's:
//! j/k move, ←→ fold a machine, `,` changes the order.

use super::fleet::{self, RowId};
use super::monitor_draw::clean;
use super::{Dash, View};
use crate::fleet::{Conn, Host};
use crate::herdr::{Herdr, Session};
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// The orders a machine's sessions can take (`fleet.board_order`; `,`).
pub(crate) const ORDERS: [(&str, &str); 4] = [
    ("needs", "needs you first"),
    ("working", "working first"),
    ("recent", "most recent first"),
    ("machine", "herdr's order"),
];

enum Msg {
    /// a machine's panes read: pane → its screen's last lines
    Lines(String, HashMap<String, String>),
    /// a machine's folders' branches: (folder, branch)
    Branches(String, Vec<(String, String)>),
}

pub(crate) struct State {
    /// drawn last frame (the list is maximised and the screen big enough)
    pub on: bool,
    /// each session's latest output line (`host|pane`), and when it was asked
    lines: HashMap<String, String>,
    asked: HashMap<String, Instant>,
    /// machines whose panes are being read now
    reading: HashSet<String>,
    /// `host\tfolder` → its git branch, and when each machine was asked
    branches: HashMap<String, String>,
    branch_asked: HashMap<String, Instant>,
    /// the sessions on screen last frame (only theirs are read)
    visible: Vec<String>,
    /// the first line shown (the band list scrolls)
    scroll: usize,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
}

impl Default for State {
    fn default() -> State {
        let (tx, rx) = channel();
        State {
            on: false,
            lines: HashMap::new(),
            asked: HashMap::new(),
            reading: HashSet::new(),
            branches: HashMap::new(),
            branch_asked: HashMap::new(),
            visible: Vec::new(),
            scroll: 0,
            tx,
            rx,
        }
    }
}

/// Does Fleet want the board: its list maximised (`z`), alone or with the
/// resources under it?
pub(crate) fn wanted(d: &Dash) -> bool {
    use super::panes::Max;
    d.view == View::Fleet
        && !d.fleet.hosts.is_empty()
        && matches!(super::panes::shape(d, "fleet").max, Max::Main | Max::Top)
}

fn order_of(d: &Dash) -> &'static str {
    let v = d.cfg.get_or("fleet.board_order");
    ORDERS
        .iter()
        .find(|(k, _)| *k == v)
        .map_or(ORDERS[0].0, |o| o.0)
}

/// `,`: the next order, kept.
pub(crate) fn next_order(d: &mut Dash) {
    let now = order_of(d);
    let i = ORDERS.iter().position(|(k, _)| *k == now).unwrap_or(0);
    let (k, words) = ORDERS[(i + 1) % ORDERS.len()];
    let _ = d.cfg.set("fleet.board_order", k);
    d.note(format!("sessions: {words}"));
}

/// The list's rows (machines, their sessions, agents outside herdr, the
/// machines not in the fleet), each machine's sessions in the board's order.
pub(crate) fn rows(d: &Dash) -> Vec<RowId> {
    let first = order_of(d);
    let rank = |s: &Session| -> u8 {
        let st = if s.limit.is_some() {
            "limit"
        } else {
            s.status.as_str()
        };
        match (first, st) {
            ("working", "working") | ("needs", "blocked") => 0,
            ("working", "blocked") | ("needs", "limit") => 1,
            ("working", "limit") | ("needs", "working") => 2,
            (_, "done") => 3,
            (_, "idle") => 4,
            _ => 5,
        }
    };
    let session = |k: &str| d.fleet.sessions().into_iter().find(|s| s.key() == k);
    let mut out: Vec<RowId> = Vec::new();
    let mut group: Vec<(usize, RowId)> = Vec::new();
    let flush = |group: &mut Vec<(usize, RowId)>, out: &mut Vec<RowId>| {
        if first != "machine" {
            group.sort_by_key(|(i, r)| match r {
                RowId::Sess(k) => match session(k) {
                    Some(s) if first == "recent" => {
                        (0, std::cmp::Reverse(s.since.unwrap_or(0)), 0, *i)
                    }
                    Some(s) => (
                        rank(s),
                        std::cmp::Reverse(0),
                        s.since.unwrap_or(u64::MAX),
                        *i,
                    ),
                    None => (9, std::cmp::Reverse(0), 0, *i),
                },
                _ => (8, std::cmp::Reverse(0), 0, *i),
            });
        }
        out.extend(group.drain(..).map(|x| x.1));
    };
    for (i, r) in fleet::rows(d).into_iter().enumerate() {
        match r {
            RowId::Sess(_) | RowId::Outside(_) => group.push((i, r)),
            other => {
                flush(&mut group, &mut out);
                out.push(other);
            }
        }
    }
    flush(&mut group, &mut out);
    out
}

/// j/k on the board: the next row in its order.
pub(crate) fn move_by(d: &mut Dash, delta: isize) {
    let rows = rows(d);
    if rows.is_empty() {
        return;
    }
    let at =
        d.fv.sel
            .as_ref()
            .and_then(|s| rows.iter().position(|r| r == s));
    let i = match at {
        Some(i) => (i as isize + delta).clamp(0, rows.len() as isize - 1) as usize,
        None => 0,
    };
    fleet::select(d, rows[i].clone());
}

// ------------------------------------------------------------------ reading

/// Every loop: what was read comes in; the rows on screen are read again
/// when due, one round trip per machine; each machine's branches once a
/// minute.
pub(crate) fn pump(d: &mut Dash) {
    while let Ok(m) = d.fb.rx.try_recv() {
        match m {
            Msg::Lines(host, got) => {
                d.fb.reading.remove(&host);
                for (pane, text) in got {
                    let line = last_line(&text);
                    d.fb.lines.insert(format!("{host}|{pane}"), line);
                }
            }
            Msg::Branches(host, v) => {
                for (dir, b) in v {
                    d.fb.branches.insert(format!("{host}\t{dir}"), b);
                }
            }
        }
    }
    if !d.fb.on || d.view != View::Fleet {
        return;
    }
    let mut due: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut dirs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for key in &d.fb.visible {
        let Some(s) = d.fleet.sessions().into_iter().find(|s| s.key() == *key) else {
            continue;
        };
        dirs.entry(s.host.clone()).or_default().push(s.cwd.clone());
        let every = Duration::from_secs(if s.status == "working" { 2 } else { 5 });
        if d.fb.reading.contains(&s.host)
            || d.fb.asked.get(key).is_some_and(|t| t.elapsed() < every)
        {
            continue;
        }
        due.entry(s.host.clone())
            .or_default()
            .push(s.pane_id.clone());
    }
    let now = Instant::now();
    for (host, panes) in due {
        for p in &panes {
            d.fb.asked.insert(format!("{host}|{p}"), now);
        }
        d.fb.reading.insert(host.clone());
        let (t, tx) = (d.target_of(&host), d.fb.tx.clone());
        std::thread::spawn(move || {
            let got = Herdr::new(&t).read_many(&panes, 12);
            let _ = tx.send(Msg::Lines(host, got));
        });
    }
    for (host, mut v) in dirs {
        if d.fb
            .branch_asked
            .get(&host)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            continue;
        }
        d.fb.branch_asked.insert(host.clone(), now);
        v.sort();
        v.dedup();
        let tx = d.fb.tx.clone();
        std::thread::spawn(move || {
            let s = branch_script(&v);
            let Ok(o) = crate::proc::run(
                &mut crate::sshcfg::shell(&host, "sh -s"),
                Duration::from_secs(20),
                Some(s.as_bytes()),
            ) else {
                return;
            };
            let _ = tx.send(Msg::Branches(host, parse_branches(&o.text(), &v)));
        });
    }
}

/// The branch of each folder (`B <i> <branch>`): one `sh -s` for a machine,
/// git's own lock-free reads; nothing on a Mac without the developer tools
/// (its git would open an install dialog).
fn branch_script(folders: &[String]) -> String {
    let mut s = String::from(crate::chan::PATH_PRELUDE);
    s.push_str(
        "\nLC_ALL=C; export LC_ALL; GIT_OPTIONAL_LOCKS=0; export GIT_OPTIONAL_LOCKS\n\
         command -v git >/dev/null 2>&1 || exit 0\n\
         if [ \"$(uname)\" = Darwin ] && ! xcode-select -p >/dev/null 2>&1; then exit 0; fi\n\
         b() {\n\
         \x20 [ -d \"$2\" ] || return\n\
         \x20 x=$(git -C \"$2\" symbolic-ref --short -q HEAD 2>/dev/null || git -C \"$2\" rev-parse --short HEAD 2>/dev/null) || return\n\
         \x20 printf 'B\\t%s\\t%s\\n' \"$1\" \"$x\"\n\
         }\n",
    );
    for (i, f) in folders.iter().enumerate() {
        if !f.is_empty() && !f.contains('\n') {
            s.push_str(&format!("b {i} {}\n", crate::usage::projects::sh_path(f)));
        }
    }
    s
}

fn parse_branches(text: &str, folders: &[String]) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            (f.next()? == "B").then_some(())?;
            let i: usize = f.next()?.parse().ok()?;
            let b = f.next()?.trim();
            let dir = folders.get(i)?.clone();
            (!b.is_empty()).then(|| (dir, b.to_string()))
        })
        .collect()
}

/// A screen's latest real output: from the bottom up, past blank lines, an
/// agent's input box and its rules (a rule with words in it, too), prompt
/// lines, spinners, tips and key hints.
pub(crate) fn last_line(text: &str) -> String {
    let lines: Vec<String> = crate::ansi::lines(text)
        .iter()
        .map(crate::ansi::plain)
        .collect();
    // Claude Code marks what it says and does (⏺) and their results (⎿):
    // its latest such line, never its footer's notices
    if let Some(l) = lines.iter().rev().find(|l| {
        let t = l.trim_start();
        (t.starts_with('⏺') || t.starts_with('⎿')) && !t.contains("Tip:")
    }) {
        return l.trim().to_string();
    }
    generic_last(&lines)
}

/// Any other agent's latest real output: from the bottom up, past what
/// is plainly chrome.
fn generic_last(lines: &[String]) -> String {
    const RULE: &str = "─━═╌┄╍┈┉-_╭╮╰╯┌┐└┘▔▁▏▕";
    let chrome = |l: &str| {
        let t = l.trim_matches(|ch: char| ch.is_whitespace() || "│┃║|".contains(ch));
        let n = t.chars().filter(|c| !c.is_whitespace()).count();
        let rule = t.chars().filter(|c| RULE.contains(*c)).count();
        t.is_empty()
            // mostly rule: a border, or a border with a title in it
            || rule * 3 >= n
            || t.starts_with(['>', '❯', '›', '$'])
            || t.starts_with("Tip:")
            || t.starts_with("⎿  Tip")
            || [
                "for shortcuts",
                "esc to interrupt",
                "shift+tab",
                "auto mode",
                "⏵⏵",
                "ctrl+c to exit",
                "bypass permissions",
                "accept edits",
                "to manage",
                "tokens ·",
                "Auto-update",
                "Update installed",
                "claude doctor",
                "to start fresh",
            ]
            .iter()
            .any(|h| t.contains(h))
            // a spinner's line: "✻ Hyperspacing… (2m 8s · ↓ 7.4k tokens)"
            || (t.contains('…') && t.contains("(") && t.ends_with(')'))
    };
    lines
        .iter()
        .rev()
        .find(|l| !chrome(l))
        .map(|l| {
            l.trim_matches(|ch: char| ch.is_whitespace() || "│┃║".contains(ch))
                .to_string()
        })
        .unwrap_or_default()
}

// ------------------------------------------------------------------ drawing

/// The board in `r` (the strip above it is the list's own).
pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    d.fb.on = true;
    let words = ORDERS
        .iter()
        .find(|(k, _)| *k == order_of(d))
        .map_or("", |o| o.1);
    let title = Line::from(vec![
        Span::raw("Machines & sessions"),
        Span::styled(format!(" · {words}"), ui::fg(c::ACCENT())),
        Span::styled("  , order", ui::faint()),
    ]);
    let block = ui::panel(title, true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let rows = rows(d);
    let all = fleet::rows(d);
    let cw = cols(d, &rows, w.saturating_sub(4));
    let sel = d.fv.sel.clone();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut words_at: Vec<usize> = Vec::new();
    let mut keys: Vec<(usize, String)> = Vec::new();
    let mut bands: Vec<(usize, Option<usize>, String, bool)> = Vec::new();
    for (j, row) in rows.iter().enumerate() {
        let on = sel.as_ref() == Some(row);
        let idx = all.iter().position(|r| r == row);
        let last_child = rows
            .get(j + 1)
            .is_none_or(|n| !matches!(n, RowId::Sess(_) | RowId::Outside(_)));
        let add = |ls: Vec<Line<'static>>,
                   lines: &mut Vec<Line<'static>>,
                   map: &mut Vec<Option<usize>>| {
            for l in ls {
                lines.push(l);
                map.push(idx);
            }
        };
        match row {
            RowId::Host(a) => {
                let Some(h) = d.fleet.host(a) else { continue };
                if j > 0 {
                    lines.push(Line::raw(""));
                    map.push(None);
                }
                add(band(d, h, w, on), &mut lines, &mut map);
                if !folded(d, h) {
                    bands.push((lines.len(), idx, a.clone(), on));
                }
            }
            RowId::Sess(k) => {
                let Some(s) = d.fleet.sessions().into_iter().find(|s| s.key() == *k) else {
                    continue;
                };
                keys.push((lines.len(), k.clone()));
                let (a, b) = sess_lines(d, s, &cw, w, on, last_child);
                add(vec![a], &mut lines, &mut map);
                if let Some(b) = b {
                    words_at.push(lines.len());
                    add(vec![b], &mut lines, &mut map);
                }
            }
            RowId::Outside(k) => {
                if let Some(o) = super::outside::find(d, k) {
                    add(
                        vec![outside_line(d, &o, &cw, w, on, last_child)],
                        &mut lines,
                        &mut map,
                    );
                }
            }
            RowId::Others => {
                lines.push(Line::raw(""));
                map.push(None);
                let n = fleet::available(d).len();
                add(
                    vec![paint1(
                        vec![
                            Span::styled(
                                if d.fv.others_open { "  ▾ " } else { "  ▸ " },
                                ui::fg(c::MUTED()),
                            ),
                            Span::styled("Not in fleet", if on { ui::bold() } else { ui::muted() }),
                            Span::styled(
                                format!(
                                    "  {n} machine{} in ~/.ssh/config{}",
                                    if n == 1 { "" } else { "s" },
                                    if d.fv.others_open {
                                        ""
                                    } else {
                                        " · enter shows them"
                                    }
                                ),
                                ui::faint(),
                            ),
                        ],
                        on,
                        w,
                    )],
                    &mut lines,
                    &mut map,
                );
            }
            RowId::Avail(a) => add(vec![fleet::avail_line(d, a, w, on)], &mut lines, &mut map),
        }
    }
    let h = inner.height as usize;
    // what's left of the screen goes to what the machines can say
    for (at, idx, more) in room_for_more(d, &bands, w, h.saturating_sub(lines.len()))
        .into_iter()
        .rev()
    {
        let n = more.len();
        for x in words_at.iter_mut().filter(|x| **x >= at) {
            *x += n;
        }
        for (x, _) in keys.iter_mut().filter(|(x, _)| *x >= at) {
            *x += n;
        }
        map.splice(at..at, std::iter::repeat_n(idx, n));
        lines.splice(at..at, more);
    }
    // keep the selected row in view
    let first = map
        .iter()
        .position(|x| {
            x.is_some() && *x == sel.as_ref().and_then(|s| all.iter().position(|r| r == s))
        })
        .unwrap_or(0);
    let last = map
        .iter()
        .rposition(|x| {
            x.is_some() && *x == sel.as_ref().and_then(|s| all.iter().position(|r| r == s))
        })
        .unwrap_or(first);
    if first < d.fb.scroll {
        d.fb.scroll = first.saturating_sub(1);
    } else if h > 0 && last >= d.fb.scroll + h {
        d.fb.scroll = last + 1 - h;
    }
    d.fb.scroll = d.fb.scroll.min(lines.len().saturating_sub(h.max(1)));
    let top = d.fb.scroll;
    let shown: Vec<Line> = lines.into_iter().skip(top).take(h).collect();
    f.render_widget(Paragraph::new(shown), inner);
    // the agents' words: no stars there, and the showcase lens redacts them
    for at in words_at.iter().filter(|a| **a >= top && **a < top + h) {
        let lr = Rect::new(inner.x, inner.y + (*at - top) as u16, inner.width, 1);
        ui::agent_screen(lr);
        crate::showcase::screen_area(f.buffer_mut(), lr);
    }
    d.fb.visible = keys
        .into_iter()
        .filter(|(at, _)| *at >= top && *at < top + h)
        .map(|x| x.1)
        .collect();
    // clicks: the list's own bookkeeping (lines → its rows)
    d.hits.list = inner;
    d.hits.list_first = inner.y;
    d.hits.list_offset = top;
    d.hits.list_len = map.len();
    d.fv.linemap = map;
}

/// A machine folded away: its band alone, as small as it goes.
fn folded(d: &Dash, h: &Host) -> bool {
    h.collapsed && d.fv.filter.is_empty()
}

/// A machine's band: who it is and its meters, a line per GPU with its whole
/// model, then a line of what else. Folded, the GPUs join the meters.
fn band(d: &Dash, h: &Host, w: usize, on: bool) -> Vec<Line<'static>> {
    let small = folded(d, h);
    let fold = if small { "▸ " } else { "▾ " };
    let (st, col) = fleet::host_state(h, d.t0);
    let os = fleet::host_os(d, h);
    let mut l: Vec<Span<'static>> = vec![Span::styled(fold, ui::faint())];
    l.extend(crate::icons::os_badge(&os, 9));
    l.push(Span::styled(
        format!(" {:<8}", crate::showcase::shown(&h.alias)),
        Style::new().fg(h.color).add_modifier(Modifier::BOLD),
    ));
    l.push(Span::styled(format!(" {st}"), ui::fg(col)));
    if let (Conn::Up, Some(lat)) = (&h.conn, h.latency) {
        l.push(Span::styled(format!(" {}ms", lat.as_millis()), ui::faint()));
    }
    let t = fleet::tally(h.sessions.iter());
    for (n, g, colr) in [
        (t.blocked, crate::look::needs().to_string(), c::NEEDS()),
        (t.working, ui::spinner(d.t0).to_string(), c::WORKING()),
        (t.limit, "◷".to_string(), c::LIMIT()),
    ] {
        if n > 0 {
            l.push(Span::styled(
                format!("  {g} {n}"),
                ui::fg(colr).add_modifier(Modifier::BOLD),
            ));
        }
    }
    // the meters, from a fixed column so the machines line up
    let lead: usize = 2 + 9 + 9 + 22;
    let used: usize = l.iter().map(|s| ui::width(&s.content)).sum();
    l.push(Span::raw(" ".repeat(lead.saturating_sub(used).max(2))));
    let room = w.saturating_sub(lead.max(used + 2));
    let stats = d
        .stats(&h.alias)
        .filter(|s| s.live() || !s.cpu_hist.is_empty());
    let mut more: Vec<Span<'static>> = vec![Span::raw("    ")];
    let mut gpus: Vec<Line<'static>> = Vec::new();
    match &stats {
        Some(st) => {
            l.extend(meters(st, room, small));
            more.extend(details(d, &h.alias, st));
            if !small {
                gpus = st
                    .gpus
                    .iter()
                    .map(|g| gpu_line(g, st.gpus.len() > 1, w, on))
                    .collect();
                if let Some(why) = gpu_trouble(st) {
                    gpus.push(paint1(fit(detail_line("gpu", vec![warned(why)]), w), on, w));
                }
            }
        }
        None => {
            let why = match &h.conn {
                Conn::Down(e) => ui::trunc(e, room),
                _ if d.cfg.get_or("stats") == "off" => "stats off".into(),
                _ => "reading its stats…".into(),
            };
            l.push(Span::styled(why, ui::faint()));
            if let Some(r) = super::usage::today_for(d, &h.alias).filter(|r| r.requests > 0) {
                more.extend(super::usage::today_line(r));
            }
        }
    }
    if !h.addr.is_empty() {
        more.push(Span::styled(
            format!("  ·  {}", crate::showcase::shown(&h.addr)),
            ui::faint(),
        ));
    }
    let mut v = vec![paint1(l, on, w)];
    v.extend(gpus);
    if more.len() > 1 {
        v.push(paint1(fit(more, w), on, w));
    }
    v
}

/// A column for a whole GPU model, the same on every machine: a card's name
/// and number run to about 28.
fn model_col(w: usize) -> usize {
    (w * 2 / 7).clamp(14, 32)
}

/// One GPU: its badge and whole model, how busy, then what fits, the most
/// telling first. On a machine with several, its number as well.
fn gpu_line(g: &crate::telemetry::Gpu, many: bool, w: usize, on: bool) -> Line<'static> {
    let frac = g.util.clamp(0.0, 1.0);
    let badge = crate::icons::gpu(&g.vendor);
    let bw = ui::width(&badge.content);
    let mut model = clean(&g.name.replace("NVIDIA ", ""));
    if many && !g.index.trim().is_empty() {
        model = format!("{} {model}", clean(g.index.trim()));
    }
    let mc = model_col(w);
    let mut l = vec![
        Span::raw("    "),
        badge,
        Span::raw(" ".repeat(10usize.saturating_sub(bw).max(1))),
        Span::styled(ui::pad(&ui::trunc(&model, mc), mc + 1), ui::bold()),
    ];
    l.extend(ui::bar(frac, if w >= 150 { 14 } else { 8 }, ui::heat(frac)));
    l.push(Span::styled(
        format!(" {:>3.0}%", frac * 100.0),
        Style::new().fg(ui::heat(frac)),
    ));
    if w >= 110 && !g.hist.is_empty() {
        l.push(Span::raw(" "));
        l.push(spark(&g.hist, if w >= 150 { 16 } else { 8 }, c::ACCENT()));
    }
    let mut parts: Vec<Span<'static>> = Vec::new();
    if g.mem_total > 0.0 && g.renderer.is_none() {
        let fr = g.mem_used / g.mem_total;
        parts.push(Span::styled(
            format!("{}/{}", ui::bytes(g.mem_used), ui::bytes(g.mem_total)),
            Style::new().fg(ui::heat(fr)),
        ));
    }
    if let Some(t) = g.temp.filter(|t| t.is_finite()) {
        parts.push(Span::styled(
            format!("{t:.0}°"),
            Style::new().fg(ui::heat(t / 100.0)),
        ));
    }
    if let Some(p) = g.power.filter(|p| p.is_finite()) {
        parts.push(Span::styled(
            match g.power_limit.filter(|x| x.is_finite() && *x > 0.0) {
                Some(lim) => format!("{p:.0}/{lim:.0} W"),
                None => format!("{p:.0} W"),
            },
            ui::muted(),
        ));
    }
    if let Some(fan) = g.fan.filter(|x| x.is_finite()) {
        parts.push(Span::styled(format!("fan {fan:.0}%"), ui::faint()));
    }
    for t in g.throttle.iter().take(2) {
        parts.push(warned(t));
    }
    if let Some(e) = g.ecc.filter(|e| *e > 0) {
        parts.push(warned(&format!("{e} ECC errors")));
    }
    let mut used: usize = l.iter().map(|s| ui::width(&s.content)).sum();
    for p in parts {
        let pw = ui::width(&p.content) + 2;
        if used + pw > w {
            break;
        }
        used += pw;
        l.push(Span::raw("  "));
        l.push(p);
    }
    paint1(l, on, w)
}

/// Something wrong with a GPU, in the colour that says so.
fn warned(text: &str) -> Span<'static> {
    Span::styled(
        clean(text),
        ui::fg(c::YELLOW()).add_modifier(Modifier::BOLD),
    )
}

/// Why the GPUs are missing, when that's news: a tool that failed is, a
/// machine with no GPU tool at all isn't (`gpu_group`, when there's room).
fn gpu_trouble(st: &crate::telemetry::Stats) -> Option<&str> {
    let note = st.gpu_note.as_deref()?.trim();
    (st.gpus.is_empty() && !note.is_empty() && note != crate::telemetry::NO_GPU_TOOL)
        .then_some(note)
}

/// A detail line: a label in its own column, then what it says.
fn detail_line(label: &str, mut spans: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::raw("    "),
        Span::styled(ui::pad(label, 9), ui::fg(c::MUTED())),
    ];
    v.append(&mut spans);
    v
}

/// `a · b · c`.
fn dotted(parts: Vec<Vec<Span<'static>>>) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    for (i, p) in parts.into_iter().filter(|p| !p.is_empty()).enumerate() {
        if i > 0 {
            v.push(Span::styled(" · ", ui::faint()));
        }
        v.extend(p);
    }
    v
}

fn muted(text: String) -> Vec<Span<'static>> {
    vec![Span::styled(text, ui::muted())]
}

/// Each GPU's clocks, link and encoders, and who is using it. No GPU: why.
fn gpu_group(st: &crate::telemetry::Stats) -> Vec<Vec<Span<'static>>> {
    let ok = |x: Option<f64>| x.filter(|v| v.is_finite());
    let many = st.gpus.len() > 1;
    let mut out = Vec::new();
    for g in &st.gpus {
        let mut parts = Vec::new();
        match (ok(g.clock_sm), ok(g.clock_sm_max)) {
            (Some(c), Some(m)) if m > 0.0 => parts.push(muted(format!("{c:.0}/{m:.0} MHz"))),
            (Some(c), _) => parts.push(muted(format!("{c:.0} MHz"))),
            _ => {}
        }
        if let Some(c) = ok(g.clock_mem) {
            parts.push(muted(format!("memory {c:.0} MHz")));
        }
        if !g.pstate.trim().is_empty() {
            parts.push(muted(clean(g.pstate.trim())));
        }
        if !g.pcie.trim().is_empty() {
            parts.push(muted(format!("PCIe {}", clean(g.pcie.trim()))));
        }
        if let Some(m) = ok(g.mem_util) {
            parts.push(muted(format!("memory bus {:.0}%", m * 100.0)));
        }
        if let Some(m) = ok(g.temp_mem) {
            parts.push(muted(format!("memory {m:.0}°")));
        }
        for (name, v) in [("enc", ok(g.enc)), ("dec", ok(g.dec))] {
            if let Some(v) = v.filter(|v| *v > 0.0) {
                parts.push(muted(format!("{name} {:.0}%", v * 100.0)));
            }
        }
        for (name, v) in [("renderer", ok(g.renderer)), ("tiler", ok(g.tiler))] {
            if let Some(v) = v {
                parts.push(muted(format!("{name} {:.0}%", v * 100.0)));
            }
        }
        if let Some(a) = ok(g.mem_alloc).filter(|a| *a > 0.0) {
            parts.push(muted(format!("{} allocated", ui::bytes(a))));
        }
        let mut users: Vec<&crate::telemetry::GpuProc> = st
            .gpu_procs
            .iter()
            .filter(|p| p.gpu == g.index || (p.gpu == g.bus && !g.bus.is_empty()))
            .collect();
        users.sort_by(|a, b| b.mem.total_cmp(&a.mem));
        for p in users.iter().take(2) {
            let who = if p.owner.is_empty() {
                &p.user
            } else {
                &p.owner
            };
            let name = if p.cmd.is_empty() { &p.name } else { &p.cmd };
            let name = name.rsplit('/').next().unwrap_or(name);
            parts.push(muted(format!(
                "{} {} {}",
                ui::trunc(&clean(name), 20),
                ui::bytes(p.mem),
                clean(who)
            )));
        }
        if users.len() > 2 {
            parts.push(muted(format!("+{} more", users.len() - 2)));
        }
        let parts = dotted(parts);
        if !parts.is_empty() {
            let label = if many {
                format!("gpu {}", clean(g.index.trim()))
            } else {
                "gpu".to_string()
            };
            out.push(detail_line(&label, parts));
        }
    }
    if st.gpus.is_empty() && gpu_trouble(st).is_none() {
        if let Some(n) = st.gpu_note.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push(detail_line(
                "gpu",
                vec![Span::styled(clean(n.trim()), ui::faint())],
            ));
        }
    }
    out
}

/// The computer itself: its CPU, how loaded, how long up, what it runs.
fn machine_group(st: &crate::telemetry::Stats) -> Vec<Vec<Span<'static>>> {
    let mut parts = Vec::new();
    if !st.cpu_model.trim().is_empty() {
        parts.push(vec![Span::styled(
            clean(st.cpu_model.trim()),
            Style::new().fg(c::TEXT()),
        )]);
    }
    if st.ncpu > 0 {
        parts.push(muted(format!("{} threads", st.ncpu)));
    }
    if st.sockets > 1 {
        parts.push(muted(format!("{} sockets", st.sockets)));
    }
    if let Some(m) = st.cpu_mhz.filter(|m| m.is_finite() && *m > 0.0) {
        parts.push(muted(format!("{:.1} GHz", m / 1000.0)));
    }
    if let Some(t) = st.cpu_temp.filter(|t| t.is_finite()) {
        parts.push(vec![Span::styled(
            format!("{t:.0}°"),
            Style::new().fg(ui::heat(t / 100.0)),
        )]);
    }
    if st.load != [0.0; 3] {
        parts.push(muted(format!(
            "load {:.1} {:.1} {:.1}",
            st.load[0], st.load[1], st.load[2]
        )));
    }
    if st.swap_total > 0.0 {
        parts.push(muted(format!(
            "swap {}/{}",
            ui::bytes(st.swap_used),
            ui::bytes(st.swap_total)
        )));
    }
    if st.uptime > 0 {
        parts.push(muted(format!("up {}", ui::dur(st.uptime))));
    }
    if st.procs_total > 0 {
        parts.push(muted(format!("{} processes", st.procs_total)));
    }
    if let Some(n) = st.users_logged.filter(|n| *n > 0) {
        parts.push(muted(format!("{n} user{}", if n == 1 { "" } else { "s" })));
    }
    if !st.kernel.trim().is_empty() {
        parts.push(muted(clean(st.kernel.trim())));
    }
    let parts = dotted(parts);
    if parts.is_empty() {
        return Vec::new();
    }
    vec![detail_line("machine", parts)]
}

/// Each interface's traffic; then how full the disks are and how busy.
fn net_disk_group(st: &crate::telemetry::Stats) -> Vec<Vec<Span<'static>>> {
    let mut out = Vec::new();
    let mut nets: Vec<&crate::telemetry::Net> = st.nets.iter().collect();
    nets.sort_by(|a, b| (b.rx + b.tx).total_cmp(&(a.rx + a.tx)));
    // the ones doing nothing say nothing, unless they're all there is
    if nets.iter().any(|n| n.rx + n.tx > 0.0) {
        nets.retain(|n| n.rx + n.tx > 0.0);
    }
    let nets: Vec<Vec<Span<'static>>> = nets
        .into_iter()
        .take(5)
        .map(|n| {
            muted(format!(
                "{} ↓{}/s ↑{}/s",
                clean(&n.name),
                ui::bytes(n.rx),
                ui::bytes(n.tx)
            ))
        })
        .collect();
    if !nets.is_empty() {
        out.push(detail_line("network", dotted(nets)));
    }
    let mut parts: Vec<Vec<Span<'static>>> = st
        .disks
        .iter()
        .filter(|(_, t, _)| *t > 0.0)
        .take(4)
        .map(|(m, t, u)| {
            let fr = u / t;
            vec![
                Span::styled(format!("{} ", clean(m)), ui::muted()),
                Span::styled(format!("{:.0}%", fr * 100.0), Style::new().fg(ui::heat(fr))),
                Span::styled(format!(" of {}", ui::bytes(*t)), ui::faint()),
            ]
        })
        .collect();
    let mut io: Vec<&crate::telemetry::DiskIo> = st.disk_io.iter().collect();
    io.sort_by(|a, b| (b.read + b.write).total_cmp(&(a.read + a.write)));
    for x in io.into_iter().take(3) {
        let busy = x
            .busy
            .filter(|b| b.is_finite())
            .map_or(String::new(), |b| format!(" {:.0}% busy", b * 100.0));
        parts.push(muted(format!(
            "{} read {}/s write {}/s{busy}",
            clean(&x.name),
            ui::bytes(x.read),
            ui::bytes(x.write)
        )));
    }
    let parts = dotted(parts);
    if !parts.is_empty() {
        out.push(detail_line("disks", parts));
    }
    out
}

/// The agents running here, the sessions they've had today, its local servers.
fn agents_group(d: &Dash, alias: &str, st: &crate::telemetry::Stats) -> Vec<Vec<Span<'static>>> {
    let mut parts: Vec<Vec<Span<'static>>> = st
        .agent_procs
        .iter()
        .map(|(id, n, rss)| {
            vec![
                crate::icons::agent(id),
                Span::styled(
                    format!(" {} {n} · {}", clean(id), ui::bytes(*rss)),
                    ui::muted(),
                ),
            ]
        })
        .collect();
    if !st.sessions_today.is_empty() {
        let today: Vec<String> = st
            .sessions_today
            .iter()
            .map(|(id, n)| format!("{} {n}", clean(id)))
            .collect();
        parts.push(muted(format!("today {}", today.join(" "))));
    }
    if let Some((_, w)) = d.services.iter().find(|(a, _)| a == alias) {
        let found = w.snapshot().found;
        if !found.is_empty() {
            parts.push(crate::services::summary(&found));
        }
    }
    let parts = dotted(parts);
    if parts.is_empty() {
        return Vec::new();
    }
    vec![detail_line("agents", parts)]
}

/// What the screen's `spare` lines can hold: a group at a time, the same one
/// for every machine, the group that doesn't fit left out and the next one
/// tried. Lines to add after each band, in the bands' order.
fn room_for_more(
    d: &Dash,
    bands: &[(usize, Option<usize>, String, bool)],
    w: usize,
    spare: usize,
) -> Vec<(usize, Option<usize>, Vec<Line<'static>>)> {
    if spare == 0 || bands.is_empty() {
        return Vec::new();
    }
    let stats: Vec<_> = bands
        .iter()
        .map(|b| d.stats(&b.2).filter(|s| s.live() || !s.cpu_hist.is_empty()))
        .collect();
    let mut left = spare;
    let mut out: Vec<Vec<Vec<Span<'static>>>> = vec![Vec::new(); bands.len()];
    for group in 0..4 {
        let got: Vec<Vec<Vec<Span<'static>>>> = bands
            .iter()
            .zip(&stats)
            .map(|(b, st)| match st {
                None => Vec::new(),
                Some(st) => match group {
                    0 => gpu_group(st),
                    1 => machine_group(st),
                    2 => net_disk_group(st),
                    _ => agents_group(d, &b.2, st),
                },
            })
            .collect();
        let need: usize = got.iter().map(Vec::len).sum();
        if need > left {
            continue;
        }
        left -= need;
        for (o, g) in out.iter_mut().zip(got) {
            o.extend(g);
        }
    }
    bands
        .iter()
        .zip(out)
        .filter(|(_, more)| !more.is_empty())
        .map(|(b, more)| {
            let lines = more
                .into_iter()
                .map(|l| paint1(fit(l, w), b.3, w))
                .collect();
            (b.0, b.1, lines)
        })
        .collect()
}

/// CPU, memory and (with `gpus`) each GPU in `room` columns: bar, figure,
/// sparkline; the sparklines go first when it's tight, then the bars shrink.
fn meters(st: &crate::telemetry::Stats, room: usize, gpus: bool) -> Vec<Span<'static>> {
    let mut items: Vec<(String, f64, String, Vec<f64>, Color)> = vec![
        (
            "cpu".into(),
            st.cpu,
            String::new(),
            st.cpu_hist.clone(),
            c::BLUE(),
        ),
        (
            "mem".into(),
            st.mem_frac(),
            String::new(),
            st.mem_hist.clone(),
            c::GREEN(),
        ),
    ];
    for g in st.gpus.iter().filter(|_| gpus) {
        let name = g
            .name
            .replace("NVIDIA ", "")
            .replace("GeForce ", "")
            .replace("Apple ", "")
            .replace(" Workstation Edition", "")
            .replace(" Blackwell", "")
            .replace(" Max-Q", "");
        let name = name.split(" · ").next().unwrap_or("").to_string();
        let mut extra = Vec::new();
        if g.mem_total > 0.0 && g.renderer.is_none() {
            extra.push(format!(
                "{}/{}",
                ui::bytes(g.mem_used),
                ui::bytes(g.mem_total)
            ));
        }
        if let Some(t) = g.temp {
            extra.push(format!("{t:.0}°"));
        }
        items.push((
            ui::trunc(&name, 10),
            g.util,
            extra.join(" "),
            g.hist.clone(),
            c::ACCENT(),
        ));
    }
    let n = items.len().max(1);
    let each = room / n;
    let (bw, sw) = match each {
        e if e >= 50 => (12, 16),
        e if e >= 40 => (10, 10),
        e if e >= 30 => (8, 6),
        e if e >= 22 => (6, 0),
        _ => (4, 0),
    };
    let mut v = Vec::new();
    for (name, frac, extra, hist, hc) in items {
        let mut part: Vec<Span<'static>> = vec![Span::styled(
            format!("{name} "),
            ui::fg(c::MUTED()).add_modifier(Modifier::BOLD),
        )];
        part.extend(ui::bar(frac, bw, ui::heat(frac)));
        part.push(Span::styled(
            format!(" {:>3.0}%", frac * 100.0),
            Style::new().fg(ui::heat(frac)),
        ));
        if sw > 0 && !hist.is_empty() {
            part.push(Span::raw(" "));
            part.push(spark(&hist, sw, hc));
        }
        let pw: usize = part.iter().map(|s| ui::width(&s.content)).sum();
        if !extra.is_empty() && pw + ui::width(&extra) + 1 < each {
            part.push(Span::styled(format!(" {extra}"), ui::faint()));
        }
        let pw: usize = part.iter().map(|s| ui::width(&s.content)).sum();
        part.push(Span::raw(" ".repeat(each.saturating_sub(pw).max(2))));
        v.extend(part);
    }
    v
}

/// The band's second line: network, the fullest disks, today's usage, the
/// biggest GPU user and whose session it is.
fn details(d: &Dash, alias: &str, st: &crate::telemetry::Stats) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::styled("net ", ui::faint()),
        Span::styled(
            format!("↓{}/s ↑{}/s", ui::bytes(st.net_rx), ui::bytes(st.net_tx)),
            ui::muted(),
        ),
    ];
    let mut disks: Vec<(String, f64)> = st
        .disks
        .iter()
        .filter(|(_, t, _)| *t > 0.0)
        .map(|(m, t, u)| (m.clone(), u / t))
        .collect();
    disks.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (m, fr) in disks.into_iter().take(2) {
        v.push(Span::styled(
            format!("  disk {} ", if m == "/" { "/" } else { "~" }),
            ui::faint(),
        ));
        v.push(Span::styled(
            format!("{:.0}%", fr * 100.0),
            Style::new().fg(ui::heat(fr)),
        ));
    }
    if let Some(r) = super::usage::today_for(d, alias).filter(|r| r.requests > 0) {
        v.push(Span::styled("  ·  ", ui::faint()));
        v.extend(super::usage::today_line(r));
    }
    let mut procs: Vec<&crate::telemetry::GpuProc> = st.gpu_procs.iter().collect();
    procs.sort_by(|a, b| b.mem.total_cmp(&a.mem));
    if let Some(p) = procs.first() {
        let who = if p.owner.is_empty() {
            &p.user
        } else {
            &p.owner
        };
        let name = if p.cmd.is_empty() { &p.name } else { &p.cmd };
        let name = name.rsplit('/').next().unwrap_or(name);
        v.push(Span::styled("  ·  gpu ", ui::faint()));
        v.push(Span::styled(ui::trunc(name, 28), ui::muted()));
        v.push(Span::styled(
            format!(" {} · {who}", ui::bytes(p.mem)),
            ui::faint(),
        ));
        if procs.len() > 1 {
            v.push(Span::styled(format!(" +{}", procs.len() - 1), ui::faint()));
        }
    }
    v
}

/// The latest samples, up to `w`, from the left (a new dashboard has few).
fn spark(hist: &[f64], w: usize, col: Color) -> Span<'static> {
    let start = hist.len().saturating_sub(w);
    let n = hist.len() - start;
    ui::spark(&hist[start..], n, col)
}

/// Spans cut to `w` columns.
fn fit(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for s in spans {
        let sw = ui::width(&s.content);
        if used + sw <= w {
            used += sw;
            out.push(s);
        } else {
            let room = w.saturating_sub(used);
            if room > 1 {
                out.push(Span::styled(ui::trunc(&s.content, room), s.style));
            }
            break;
        }
    }
    out
}

/// The session columns' widths for `w`: the timeline, model, turn and
/// folder give way, in that order, when it's tight.
struct Cols {
    name: usize,
    state: usize,
    turn: usize,
    model: usize,
    folder: usize,
    tl: usize,
    use_: usize,
}

fn cols(d: &Dash, rows: &[RowId], w: usize) -> Cols {
    let sessions: Vec<&Session> = rows
        .iter()
        .filter_map(|r| match r {
            RowId::Sess(k) => d.fleet.sessions().into_iter().find(|s| s.key() == *k),
            _ => None,
        })
        .collect();
    let name = sessions
        .iter()
        .map(|s| ui::width(s.display()))
        .max()
        .unwrap_or(10)
        .clamp(10, 22);
    let state = sessions
        .iter()
        .map(|s| ui::width(&fleet::status_for(d, s).0))
        .max()
        .unwrap_or(14)
        .clamp(14, 26);
    let mut c = Cols {
        name,
        state,
        turn: 22,
        model: 16,
        folder: 30,
        tl: 0,
        use_: 22,
    };
    let lead = 4 + 2 + 2;
    let fixed = |c: &Cols| lead + c.name + 1 + c.state + 1 + c.turn + c.model + c.folder + c.use_;
    let room = w.saturating_sub(fixed(&c));
    if room >= 14 {
        c.tl = room.min(60);
    }
    c.folder += w.saturating_sub(fixed(&c) + c.tl).min(10);
    for drop in ["model", "turn", "folder"] {
        if fixed(&c) + c.tl <= w {
            break;
        }
        match drop {
            "model" => c.model = 0,
            "turn" => c.turn = 0,
            _ => c.folder = 0,
        }
    }
    c
}

/// A session: its line, and under it the agent's latest real output.
fn sess_lines(
    d: &Dash,
    s: &Session,
    cw: &Cols,
    w: usize,
    on: bool,
    last: bool,
) -> (Line<'static>, Option<Line<'static>>) {
    let (st, scol) = fleet::status_for(d, s);
    let marked = d.fv.marked.contains(&s.key());
    let mut l = vec![
        Span::styled(if last { "    └ " } else { "    ├ " }, ui::faint()),
        Span::styled(if marked { "● " } else { "" }, ui::fg(c::ACCENT())),
        Span::styled(format!("{} ", s.glyph()), ui::fg(s.color())),
        crate::icons::agent(&s.agent),
        Span::raw(" "),
        Span::styled(
            ui::pad(&ui::trunc(s.display(), cw.name), cw.name + 1),
            Style::new().fg(s.color()).add_modifier(if on {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
        ),
        Span::styled(
            ui::pad(&ui::trunc(&st, cw.state), cw.state + 1),
            ui::fg(scol),
        ),
    ];
    let live = super::usage::live_for(d, s);
    if cw.turn > 0 {
        l.push(Span::styled(
            ui::pad(&ui::trunc(&turn(d, s), cw.turn - 1), cw.turn),
            ui::muted(),
        ));
    }
    if cw.model > 0 {
        let m = live
            .map(|x| {
                let m = x.model.trim_start_matches("claude-").to_string();
                if x.effort.is_empty() {
                    m
                } else {
                    format!("{m} {}", x.effort)
                }
            })
            .unwrap_or_default();
        l.push(Span::styled(
            ui::pad(&ui::trunc(&m, cw.model - 1), cw.model),
            ui::muted(),
        ));
    }
    if cw.folder > 0 {
        let folder = s.cwd.rsplit('/').find(|p| !p.is_empty()).unwrap_or("~");
        let text = match d.fb.branches.get(&format!("{}\t{}", s.host, s.cwd)) {
            Some(b) => format!("{folder} ⎇ {b}"),
            None => folder.to_string(),
        };
        l.push(Span::styled(
            ui::pad(
                &ui::trunc(&crate::showcase::shown(&text), cw.folder - 1),
                cw.folder,
            ),
            Style::new().fg(c::TEXT()),
        ));
    }
    if cw.tl > 0 {
        l.extend(super::timechart::session_bar(d, s, cw.tl.saturating_sub(1)));
        l.push(Span::raw(" "));
    }
    let chip: Vec<Span> = live
        .map(|x| super::usage::chip(x, cw.use_))
        .unwrap_or_default();
    let used: usize = l.iter().map(|x| ui::width(&x.content)).sum();
    let cwid: usize = chip.iter().map(|x| ui::width(&x.content)).sum();
    l.push(Span::raw(" ".repeat(w.saturating_sub(used + cwid))));
    l.extend(chip);
    // under it: the agent's latest real output, when there is one
    let b = d.fb.lines.get(&s.key()).filter(|x| !x.is_empty()).map(|x| {
        let lead = if last { "        " } else { "    │   " };
        paint1(
            vec![
                Span::styled(lead, ui::faint()),
                Span::styled(ui::trunc(x, w.saturating_sub(10)), ui::muted()),
            ],
            on,
            w,
        )
    });
    (paint1(l, on, w), b)
}

/// An agent outside herdr, under its machine.
fn outside_line(
    d: &Dash,
    o: &super::outside::Outside,
    cw: &Cols,
    w: usize,
    on: bool,
    last: bool,
) -> Line<'static> {
    let mut v = vec![
        Span::styled(if last { "    └ " } else { "    ├ " }, ui::faint()),
        Span::styled("○ ", ui::muted()),
        crate::icons::agent(&o.p.agent),
        Span::raw(" "),
        Span::styled(
            ui::pad(
                &ui::trunc(&crate::agents::label(&o.p.agent), cw.name),
                cw.name + 1,
            ),
            ui::muted(),
        ),
        Span::styled(ui::pad("outside herdr", cw.state + 1), ui::faint()),
        Span::styled(
            format!("{}  ", ui::trunc(&o.cwd.clone().unwrap_or_default(), 48)),
            ui::faint(),
        ),
    ];
    // its numbers, from its own logs
    match super::outside::usage_for(d, o) {
        Some(l) => v.extend(super::usage::chip(l, 34)),
        None => v.push(Span::styled("· enter reopens it there", ui::faint())),
    }
    paint1(v, on, w)
}

/// A selected line on the selection's colour, with the accent bar.
fn paint1(mut a: Vec<Span<'static>>, on: bool, w: usize) -> Line<'static> {
    if on {
        for s in a.iter_mut() {
            s.style = s.style.bg(c::SEL());
        }
        let used: usize = a.iter().map(|s| ui::width(&s.content)).sum();
        a.push(Span::styled(
            " ".repeat(w.saturating_sub(used)),
            Style::new().bg(c::SEL()),
        ));
        if let Some(first) = a.first_mut() {
            let t = first.content.to_string();
            if let Some(rest) = t.strip_prefix(' ') {
                *first = Span::styled(format!("┃{rest}"), first.style.fg(c::ACCENT()));
            }
        }
    }
    Line::from(a)
}

/// This turn: how long it's been working and the tokens since (its
/// latest requests), else what its last finished turn took.
fn turn(d: &Dash, s: &Session) -> String {
    let now = crate::cache::now();
    if s.status == "working" {
        let since = s.since.unwrap_or(now);
        let mut t = format!("turn {}", ui::dur(now.saturating_sub(since)));
        if let Some(l) = super::usage::live_for(d, s) {
            let tok: u64 = l
                .recent
                .iter()
                .filter(|(at, _)| *at >= since)
                .map(|x| x.1)
                .sum();
            if tok > 0 {
                t.push_str(&format!(" · {} tok", crate::usage::dash::tokens(tok)));
            }
        }
        return t;
    }
    match super::inbox::last_turn(d, s) {
        Some((_, t)) => format!("last {}", crate::turncost::parts(&t).join(" · ")),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_latest_line_is_real_output() {
        let screen = "⏺ Running cargo test -q\n  ⎿ 12 passed\n\n─────────── usage-explorer-agent-timeline ─\n> \n────────────\n  ⏵⏵ auto mode on · 1 shell · esc to interrupt\n";
        assert_eq!(last_line(screen), "⎿ 12 passed");
        let spin = "⏺ Done.\n✻ Hyperspacing… (2m 8s · ↓ 7.4k tokens)\n  ⎿  Tip: Use /btw to ask\n";
        assert_eq!(last_line(spin), "⏺ Done.");
        assert_eq!(last_line(""), "");
        assert_eq!(last_line("\x1b[1mbuilt\x1b[0m\n"), "built");
        // Claude Code's footer notices never pass for its output
        let notices = "⏺ Fixed the test.\n  ⎿  41 passed\n\n✘ Auto-update failed · Run claude doctor\n~62k uncached · /clear to start fresh ✓ Update installed · Restart to update\n";
        assert_eq!(last_line(notices), "⎿  41 passed");
        // a box with words in it is the agent's input
        assert_eq!(
            last_line("result 42\n╭────╮\n│ > fix it │\n╰────╯\n"),
            "result 42"
        );
    }

    #[test]
    fn branches_come_back_by_folder() {
        let dirs = vec!["~/a".to_string(), "/srv/b c".to_string()];
        let s = branch_script(&dirs);
        assert!(
            s.contains("b 0 \"$HOME\"/'a'") && s.contains("b 1 '/srv/b c'"),
            "{s}"
        );
        assert!(s.contains("xcode-select -p"), "{s}");
        let v = parse_branches("B\t1\tmain\nB\t0\t\nnoise\nB\t9\tx\n", &dirs);
        assert_eq!(v, vec![("/srv/b c".to_string(), "main".to_string())]);
    }
}
