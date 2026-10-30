//! Session timelines: each session's states — working, needs you, done,
//! idle, at its limit — over the last `ui.timeline_hours` (6), one row of
//! cells per session under an hour axis.
//!
//! * Fleet: `g` (`fleet.timeline`) turns each session row's folder, title
//!   and usage into its timeline, under an axis pinned to the list's top (the
//!   rows, keys and layout stay as they are); the preview shows the selected
//!   session's strip either way.
//! * Monitor › Timeline (`t`): every session by machine, ended ones too, with
//!   how long each worked and how often it needed you.
//!
//! The stretches that ended come from the state log (`statelog`, written by
//! the hub); the one going on now from the fleet (`Session::since`). Time
//! nobody watched (no hub ran) stays blank: nothing is made up.

use super::Dash;
use crate::herdr::Session;
use crate::statelog::{self, St, Stretch};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

/// How often the log is looked at for news.
const LOOK: Duration = Duration::from_secs(3);
/// The window's choices for `+` / `-` (hours).
const HOURS: [u64; 7] = [1, 2, 3, 6, 12, 24, 48];

#[derive(Default)]
pub(crate) struct State {
    /// the log as last read
    log: Vec<Stretch>,
    /// `host|pane` → its stretches in `log`
    by_key: HashMap<String, Vec<usize>>,
    /// the file as last read: (modified, length)
    stamp: Option<(SystemTime, u64)>,
    looked: Option<Instant>,
    /// Monitor › Timeline: the selected session row, the first row shown,
    /// and ended sessions left out (`e`)
    pub sel: usize,
    pub scroll: usize,
    pub hide_ended: bool,
    /// the conversations of the last weeks (`timeline::sessions`), read off
    /// the UI thread while it's shown
    pub hist: Vec<crate::timeline::Brief>,
    hist_at: Option<Instant>,
    hist_rx: Option<std::sync::mpsc::Receiver<Vec<crate::timeline::Brief>>>,
}

impl State {
    fn load(&mut self, v: Vec<Stretch>) {
        self.by_key.clear();
        for (i, s) in v.iter().enumerate() {
            self.by_key.entry(s.key()).or_default().push(i);
        }
        self.log = v;
    }
    /// Tests: these earlier conversations, as if just read.
    #[cfg(test)]
    pub(crate) fn set_earlier(&mut self, v: Vec<crate::timeline::Brief>) {
        self.hist = v;
        self.hist_at = Some(Instant::now());
    }
    /// Tests: the log as given.
    #[cfg(test)]
    pub(crate) fn set_log(&mut self, v: Vec<Stretch>) {
        self.load(v);
        self.looked = Some(Instant::now());
        self.stamp = Some((SystemTime::now(), u64::MAX));
    }
}

/// Every frame: read the log again when it changed (looked at every few
/// seconds; a small local file).
pub(crate) fn pump(d: &mut Dash) {
    let tl = &mut d.tl;
    if tl.looked.is_some_and(|t| t.elapsed() < LOOK) {
        return;
    }
    tl.looked = Some(Instant::now());
    let p = statelog::file();
    let stamp = std::fs::metadata(&p)
        .ok()
        .and_then(|m| Some((m.modified().ok()?, m.len())));
    if stamp.is_some() && stamp == tl.stamp {
        return;
    }
    tl.stamp = stamp;
    tl.load(statelog::read_from(&p));
}

/// The window, in hours (`ui.timeline_hours`, 1–48).
pub(crate) fn hours(cfg: &crate::config::Config) -> u64 {
    cfg.num("ui.timeline_hours").clamp(1, 48)
}

/// (from, to) of the window now.
pub(crate) fn window(d: &Dash) -> (u64, u64) {
    let now = crate::cache::now();
    (now.saturating_sub(hours(&d.cfg) * 3600), now)
}

/// A live session's stretches, in order: the log's, then the one going on
/// now (to now, or to the last poll while its machine doesn't answer).
pub(crate) fn of_session(d: &Dash, s: &Session) -> Vec<(u64, u64, St)> {
    let now = crate::cache::now();
    let mut v: Vec<(u64, u64, St)> = logged(d, &s.key(), &s.name);
    let end = match d.fleet.host(&s.host) {
        Some(h) if h.conn != crate::fleet::Conn::Up => h.fetched.unwrap_or(now),
        _ => now,
    };
    let start = s.since.unwrap_or(now).min(end);
    v.push((start, end, St::of(s)));
    statelog::tidy(v)
}

/// What the log has for `host|pane` under that name.
fn logged(d: &Dash, key: &str, name: &str) -> Vec<(u64, u64, St)> {
    d.tl.by_key
        .get(key)
        .into_iter()
        .flatten()
        .filter_map(|i| d.tl.log.get(*i))
        .filter(|x| x.name == name)
        .map(|x| (x.start, x.end, x.st))
        .collect()
}

/// `n` cells across `from..to` (pure — tested): each the state it spent most
/// time in (a question counts thrice: a short wait still shows), none when
/// less than a third of it was seen.
pub(crate) fn cells(v: &[(u64, u64, St)], from: u64, to: u64, n: usize) -> Vec<Option<St>> {
    let n = n.max(1);
    let span = to.saturating_sub(from).max(1) as f64;
    let cw = span / n as f64;
    let slot = |st: St| match st {
        St::Working => 0,
        St::Needs => 1,
        St::Done => 2,
        St::Idle => 3,
        St::Limit => 4,
        St::Compacting => 5,
    };
    let mut acc = vec![[0f64; 6]; n];
    for &(s, e, st) in v {
        let (s, e) = (s.max(from), e.min(to));
        if e <= s {
            continue;
        }
        let (fs, fe) = ((s - from) as f64, (e - from) as f64);
        let i0 = ((fs / cw) as usize).min(n - 1);
        let i1 = ((fe / cw).ceil() as usize).clamp(i0 + 1, n);
        for (i, a) in acc.iter_mut().enumerate().take(i1).skip(i0) {
            let (a0, a1) = (i as f64 * cw, (i + 1) as f64 * cw);
            let ov = fe.min(a1) - fs.max(a0);
            if ov > 0.0 {
                a[slot(st)] += ov;
            }
        }
    }
    const ALL: [St; 6] = [
        St::Working,
        St::Needs,
        St::Done,
        St::Idle,
        St::Limit,
        St::Compacting,
    ];
    acc.iter()
        .map(|a| {
            let seen: f64 = a.iter().sum();
            if seen < cw / 3.0 && a[1] == 0.0 {
                return None;
            }
            ALL.iter()
                .copied()
                .filter(|st| a[slot(*st)] > 0.0)
                .max_by(|x, y| {
                    let wx = a[slot(*x)] * if *x == St::Needs { 3.0 } else { 1.0 };
                    let wy = a[slot(*y)] * if *y == St::Needs { 3.0 } else { 1.0 };
                    wx.total_cmp(&wy).then(x.rank().cmp(&y.rank()))
                })
        })
        .collect()
}

/// A state's cell: its glyph and colour. Busy states are full height, done
/// half, idle a baseline; without colour each has a shape of its own.
pub(crate) fn look(st: St) -> (&'static str, Color) {
    let mono = c::mono();
    match st {
        St::Working => ("█", c::WORKING()),
        St::Needs => (if mono { "▓" } else { "█" }, c::NEEDS()),
        St::Limit => (if mono { "▒" } else { "█" }, c::LIMIT()),
        St::Compacting => (if mono { "░" } else { "█" }, c::MAUVE()),
        St::Done => ("▄", c::DONE()),
        St::Idle => ("▁", c::IDLE()),
    }
}

/// What a state is called in the legend.
fn word(st: St) -> &'static str {
    match st {
        St::Working => "working",
        St::Needs => "needs you",
        St::Done => "done",
        St::Idle => "idle",
        St::Limit => "at limit",
        St::Compacting => "compacting",
    }
}

/// The cells as spans (a run of one state is one span; nothing seen is blank).
pub(crate) fn bar(cells: &[Option<St>]) -> Vec<Span<'static>> {
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut run: Option<(Option<St>, String)> = None;
    let flush = |out: &mut Vec<Span<'static>>, r: Option<(Option<St>, String)>| {
        if let Some((st, text)) = r {
            out.push(match st {
                Some(st) => Span::styled(text, ui::fg(look(st).1)),
                None => Span::raw(text),
            });
        }
    };
    for cell in cells {
        let g = cell.map_or(" ", |st| look(st).0);
        match &mut run {
            Some((st, text)) if st == cell => text.push_str(g),
            _ => {
                flush(&mut out, run.take());
                run = Some((*cell, g.to_string()));
            }
        }
    }
    flush(&mut out, run);
    out
}

/// One session's bar, `w` cells across the window now.
pub(crate) fn session_bar(d: &Dash, s: &Session, w: usize) -> Vec<Span<'static>> {
    let (from, to) = window(d);
    bar(&cells(&of_session(d, s), from, to, w))
}

/// Has a session anything to draw in the window?
pub(crate) fn has_any(d: &Dash, s: &Session) -> bool {
    let (from, _) = window(d);
    of_session(d, s).iter().any(|x| x.1 > from)
}

/// The hour axis for a `w`-cell window (pure — tested): a label at each
/// tick — every hour, or every 2, 3, 6, 12 or 24 as the cells get fewer —
/// `14:00` where there's room, `14` where not; `tz` is the local offset.
pub(crate) fn axis(from: u64, to: u64, w: usize, tz: i64) -> String {
    let mut buf: Vec<char> = vec![' '; w];
    let span = to.saturating_sub(from).max(1) as f64;
    let per_hour = w as f64 * 3600.0 / span;
    let pick = |need: f64| {
        [1u64, 2, 3, 6, 12, 24]
            .into_iter()
            .find(|s| per_hour * *s as f64 >= need)
    };
    // `14:00` every so often, or `14` twice as often (or more)
    let (step, long) = match (pick(7.0), pick(4.0)) {
        (Some(l), Some(s)) if s * 2 <= l => (s, false),
        (Some(l), _) => (l, true),
        (None, s) => (s.unwrap_or(24), false),
    };
    let unit = step as i64 * 3600;
    let local = from as i64 + tz;
    let mut t = local.div_euclid(unit) * unit + unit - tz;
    let mut free = 0usize;
    while t <= to as i64 {
        let x = ((t - from as i64) as f64 / span * w as f64).round() as usize;
        let h = (t + tz).rem_euclid(86_400) / 3600;
        let label = if long {
            format!("{h:02}:00")
        } else {
            format!("{h:02}")
        };
        if x >= free && x + label.len() <= w {
            for (i, ch) in label.chars().enumerate() {
                buf[x + i] = ch;
            }
            free = x + label.len() + 1;
        }
        t += unit;
    }
    // `now` at the right end, where the last label left room
    if w >= 3 && free + 3 <= w && buf[w - 3..].iter().all(|c| *c == ' ') {
        for (i, ch) in "now".chars().enumerate() {
            buf[w - 3 + i] = ch;
        }
    }
    buf.into_iter().collect()
}

/// `█ working  █ needs you  ▄ done  ▁ idle  █ at limit`
pub(crate) fn legend() -> Vec<Span<'static>> {
    let mut v = Vec::new();
    for st in [
        St::Working,
        St::Compacting,
        St::Needs,
        St::Done,
        St::Idle,
        St::Limit,
    ] {
        let (g, col) = look(st);
        v.push(Span::styled(format!("{g} "), ui::fg(col)));
        v.push(Span::styled(format!("{}  ", word(st)), ui::faint()));
    }
    v
}

// ------------------------------------------------------------------ Fleet

/// Fleet's rows as timelines (`g`, `fleet.timeline`)?
pub(crate) fn fleet_rows(d: &Dash) -> bool {
    d.cfg.flag("fleet.timeline")
}

/// `g` in Fleet: the rows' timelines on or off (kept in `fleet.timeline`).
pub(crate) fn toggle_rows(d: &mut Dash) {
    let on = !fleet_rows(d);
    if let Err(e) = d
        .cfg
        .set("fleet.timeline", if on { "true" } else { "false" })
    {
        d.error(format!("couldn't save: {e}"));
        return;
    }
    let h = hours(&d.cfg);
    d.note(if on {
        format!("timeline: each session's last {h}h · g for folders and titles again")
    } else {
        "folders and titles · g for the timeline".to_string()
    });
}

/// The axis line over Fleet's timeline rows: `lead` columns in, `w` wide.
pub(crate) fn fleet_axis(d: &Dash, lead: usize, w: usize) -> Line<'static> {
    let (from, to) = window(d);
    let label = format!("last {}h", hours(&d.cfg));
    let pad = if lead > ui::width(&label) + 2 {
        format!("  {}", ui::pad(&label, lead - 2))
    } else {
        " ".repeat(lead)
    };
    Line::from(vec![
        Span::styled(pad, ui::faint()),
        Span::styled(axis(from, to, w, ui::utc_offset()), ui::faint()),
    ])
}

// ------------------------------------------------------------------ Monitor

/// How many earlier conversations Monitor › Timeline lists.
const EARLIER: usize = 300;
/// How far back it looks for them (days; `timeline.keep` when shorter).
const EARLIER_DAYS: u64 = 60;

/// A row of Monitor › Timeline.
enum Row {
    Host(String),
    /// the earlier conversations' heading
    Head(String),
    /// a project's heading among them
    Project(String),
    /// one of them (`State::hist`)
    Past(usize),
    /// a live session (`host|pane`)
    Live(String),
    /// an ended one: its last stretch's fields
    Ended {
        host: String,
        name: String,
        agent: String,
        key: String,
    },
}

/// Sessions that ended within the window, by machine: (host, name, agent,
/// key), the latest first.
fn ended(d: &Dash, from: u64) -> Vec<(String, String, String, String)> {
    let live: std::collections::HashSet<(String, String)> = d
        .fleet
        .sessions()
        .iter()
        .map(|s| (s.key(), s.name.clone()))
        .collect();
    let mut last: HashMap<(String, String), (u64, String, String)> = HashMap::new();
    for x in d.tl.log.iter().filter(|x| x.end > from) {
        let k = (x.key(), x.name.clone());
        if live.contains(&k) {
            continue;
        }
        let e = last
            .entry(k)
            .or_insert((x.end, x.host.clone(), x.agent.clone()));
        if x.end > e.0 {
            *e = (x.end, x.host.clone(), x.agent.clone());
        }
    }
    let mut v: Vec<(u64, String, String, String, String)> = last
        .into_iter()
        .map(|((key, name), (end, host, agent))| (end, host, name, agent, key))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.2.cmp(&b.2)));
    v.into_iter().map(|(_, h, n, a, k)| (h, n, a, k)).collect()
}

fn rows(d: &Dash, from: u64) -> Vec<Row> {
    let gone = if d.tl.hide_ended {
        Vec::new()
    } else {
        ended(d, from)
    };
    let mut v = Vec::new();
    for h in &d.fleet.hosts {
        let old: Vec<&(String, String, String, String)> =
            gone.iter().filter(|x| x.0 == h.alias).collect();
        if h.sessions.is_empty() && old.is_empty() {
            continue;
        }
        v.push(Row::Host(h.alias.clone()));
        v.extend(h.sessions.iter().map(|s| Row::Live(s.key())));
        v.extend(old.into_iter().map(|(host, name, agent, key)| Row::Ended {
            host: host.clone(),
            name: name.clone(),
            agent: agent.clone(),
            key: key.clone(),
        }));
    }
    if d.tl.hide_ended {
        return v;
    }
    // the conversations before: the ones not running now, by project (the
    // project worked on last first)
    let live: std::collections::HashSet<(&str, &str, &str)> = d
        .fleet
        .sessions()
        .iter()
        .filter_map(|s| Some((s.host.as_str(), s.agent.as_str(), s.native_id.as_deref()?)))
        .collect();
    let mut groups: Vec<(&str, Vec<usize>)> = Vec::new();
    let past = d
        .tl
        .hist
        .iter()
        .enumerate()
        .filter(|(_, b)| !live.contains(&(b.host.as_str(), b.agent.as_str(), b.session.as_str())))
        .take(EARLIER);
    for (i, b) in past {
        match groups.iter_mut().find(|g| g.0 == b.project) {
            Some(g) => g.1.push(i),
            None => groups.push((&b.project, vec![i])),
        }
    }
    if !groups.is_empty() {
        let n: usize = groups.iter().map(|g| g.1.len()).sum();
        v.push(Row::Head(format!(
            "earlier · {n} conversation{}",
            if n == 1 { "" } else { "s" }
        )));
        for (p, is) in groups {
            v.push(Row::Project(p.to_string()));
            v.extend(is.into_iter().map(Row::Past));
        }
    }
    v
}

/// The selectable rows' places in `rows`.
fn picks(v: &[Row]) -> Vec<usize> {
    v.iter()
        .enumerate()
        .filter(|(_, r)| !matches!(r, Row::Host(_) | Row::Head(_) | Row::Project(_)))
        .map(|(i, _)| i)
        .collect()
}

/// A row's conversation: (host, agent, the agent's session id, working now).
fn conversation(d: &Dash, r: &Row) -> Option<(String, String, String, bool)> {
    match r {
        Row::Live(k) => {
            let s = d.fleet.find(k)?;
            let id = s.native_id.clone()?;
            Some((s.host.clone(), s.agent.clone(), id, s.status == "working"))
        }
        Row::Past(i) => {
            let b = d.tl.hist.get(*i)?;
            Some((b.host.clone(), b.agent.clone(), b.session.clone(), false))
        }
        _ => None,
    }
}

/// The earlier conversations, read again every minute while it's shown.
fn earlier(d: &mut Dash) {
    if let Some(rx) = &d.tl.hist_rx {
        if let Ok(v) = rx.try_recv() {
            d.tl.hist = v;
            d.tl.hist_rx = None;
        }
    }
    if d.tl.hist_rx.is_some()
        || d.tl
            .hist_at
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
    {
        return;
    }
    d.tl.hist_at = Some(Instant::now());
    let (tx, rx) = std::sync::mpsc::channel();
    d.tl.hist_rx = Some(rx);
    let cfg = d.cfg.clone();
    std::thread::spawn(move || {
        let keep = crate::usage::parse_days(&cfg.get_or("timeline.keep")).unwrap_or(EARLIER_DAYS);
        let _ = tx.send(crate::timeline::sessions(&cfg, keep.min(EARLIER_DAYS)));
    });
}

/// How long it worked in the window, and how often it asked (from `v`).
fn tally(v: &[(u64, u64, St)], from: u64, to: u64) -> (u64, usize) {
    let mut busy = 0;
    let mut asks = 0;
    for &(s, e, st) in v {
        let (a, b) = (s.max(from), e.min(to));
        if st.busy() && b > a {
            busy += b - a;
        }
        if st == St::Needs && s >= from && s <= to {
            asks += 1;
        }
    }
    (busy, asks)
}

/// Keys of Monitor › Timeline; false when it isn't one of them.
pub(crate) fn monitor_key(d: &mut Dash, k: KeyEvent) -> bool {
    let (from, _) = window(d);
    let n = picks(&rows(d, from)).len();
    match k.code {
        KeyCode::Char('t') => open_turns(d, from),
        // [ ]: session by session, round and round
        KeyCode::Char(']') if n > 0 && super::machine_step(&k).is_none() => {
            d.tl.sel = (d.tl.sel + 1) % n
        }
        KeyCode::Char('[') if n > 0 && super::machine_step(&k).is_none() => {
            d.tl.sel = (d.tl.sel + n - 1) % n
        }
        KeyCode::Down | KeyCode::Char('j') => d.tl.sel = (d.tl.sel + 1).min(n.saturating_sub(1)),
        KeyCode::Up | KeyCode::Char('k') => d.tl.sel = d.tl.sel.saturating_sub(1),
        KeyCode::Home => d.tl.sel = 0,
        KeyCode::End => d.tl.sel = n.saturating_sub(1),
        KeyCode::Char('e') => {
            d.tl.hide_ended = !d.tl.hide_ended;
            d.note(if d.tl.hide_ended {
                "ended sessions hidden · e shows them"
            } else {
                "ended sessions shown"
            });
        }
        KeyCode::Char(ch @ ('+' | '-')) => {
            let now = hours(&d.cfg);
            let i = HOURS
                .iter()
                .position(|h| *h >= now)
                .unwrap_or(HOURS.len() - 1);
            let next = if ch == '+' {
                // `+` shows more hours
                HOURS[(i + 1).min(HOURS.len() - 1)]
            } else {
                HOURS[i.saturating_sub(1)]
            };
            if next != now {
                if let Err(e) = d.cfg.set("ui.timeline_hours", &next.to_string()) {
                    d.error(format!("couldn't save: {e}"));
                } else {
                    d.note(format!("timeline: the last {next}h"));
                }
            }
        }
        KeyCode::Enter => {
            let v = rows(d, from);
            let p = picks(&v);
            match p.get(d.tl.sel).and_then(|i| v.get(*i)) {
                Some(Row::Live(key)) => {
                    let key = key.clone();
                    super::wall::focus_on(d, &key, super::View::Monitor);
                }
                Some(Row::Ended { .. }) => d.note("it has ended — its story is in the Inbox (4)"),
                Some(Row::Past(_)) => open_turns(d, from),
                _ => {}
            }
        }
        _ => return false,
    }
    true
}

/// `t`: the selected conversation's whole turn view.
fn open_turns(d: &mut Dash, from: u64) {
    let v = rows(d, from);
    let p = picks(&v);
    let row = p.get(d.tl.sel).and_then(|i| v.get(*i));
    match row.and_then(|r| conversation(d, r)) {
        Some((host, agent, session, running)) => {
            let title = crate::agents::label(&agent);
            super::tree_view::open_full(d, &host, &agent, &session, running, title);
        }
        None => d.note(
            "no conversation to show for it: its agent didn't say which (or it's not one Aleph reads)",
        ),
    }
}

pub(crate) fn monitor_hints(_d: &Dash) -> Vec<(&'static str, &'static str)> {
    vec![
        ("↑↓ [ ]", "session"),
        ("ctrl+[ ]", "machine"),
        ("enter", "focus"),
        ("t", "turn view"),
        ("+ -", "hours"),
        ("e", "ended"),
        ("←→", "view"),
        ("f o c m g p n s t u", "jump"),
        ("esc", "back"),
    ]
}

/// Monitor › Timeline.
pub(crate) fn draw_monitor(d: &mut Dash, f: &mut Frame, r: Rect) {
    earlier(d);
    let (from, to) = window(d);
    let v = rows(d, from);
    let p = picks(&v);
    d.tl.sel = d.tl.sel.min(p.len().saturating_sub(1));
    // the list, and the selected conversation's timeline beside it (wide)
    // or in its place (narrow, while it has the keys)
    let pick = p
        .get(d.tl.sel)
        .and_then(|i| v.get(*i))
        .and_then(|row| conversation(d, row));
    let (r, side) = if r.width >= 140 {
        let lw = r.width - (r.width * 2 / 5).clamp(60, 90);
        (
            Rect { width: lw, ..r },
            Some(Rect {
                x: r.x + lw,
                width: r.width - lw,
                ..r
            }),
        )
    } else {
        (r, None)
    };
    if let Some(side) = side {
        draw_conversation(d, f, side, pick.as_ref());
    }
    if r.width == 0 {
        return;
    }
    let live_n = d.fleet.sessions().len();
    let mut title = vec![
        Span::raw(" Timeline "),
        Span::styled(format!("· last {}h ", hours(&d.cfg)), ui::faint()),
    ];
    if live_n > 0 {
        title.push(Span::styled(
            format!("· {live_n} session{} ", if live_n == 1 { "" } else { "s" }),
            ui::faint(),
        ));
    }
    let block = ui::panel(Line::from(title), true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height < 3 || inner.width < 30 {
        return;
    }
    let w = inner.width as usize;
    if v.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                format!("No sessions in the last {}h.", hours(&d.cfg)),
                ui::bold(),
            )),
            Line::from(Span::styled(
                "Each session's working, waiting, done and idle stretches show up here as they happen.",
                ui::muted(),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }
    // columns: name · the bar · what it did
    let names = v.iter().filter_map(|r| match r {
        Row::Live(k) => d.fleet.find(k).map(|s| ui::width(s.display())),
        Row::Ended { name, .. } => Some(ui::width(name)),
        _ => None,
    });
    let nw = names.max().unwrap_or(8).clamp(8, 22);
    let lead = 2 + 2 + 2 + nw + 2;
    let right = if w >= lead + 40 + 22 { 20 } else { 0 };
    let bw = w.saturating_sub(lead + right + usize::from(right > 0) * 2);
    // the legend and the axis, pinned
    let mut head = vec![Line::from(legend()), Line::raw("")];
    head[1] = Line::from(vec![
        Span::raw(" ".repeat(lead)),
        Span::styled(axis(from, to, bw, ui::utc_offset()), ui::faint()),
    ]);
    f.render_widget(Paragraph::new(head), Rect { height: 2, ..inner });
    let list = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(2),
        ..inner
    };
    let h = list.height as usize;
    // keep the selection in view (its machine's heading too, when it fits)
    let at = p.get(d.tl.sel).copied().unwrap_or(0);
    if at < d.tl.scroll + 1 {
        d.tl.scroll = at.saturating_sub(1);
    } else if h > 0 && at >= d.tl.scroll + h {
        d.tl.scroll = at + 1 - h;
    }
    d.tl.scroll = d.tl.scroll.min(v.len().saturating_sub(h.max(1)));
    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in v.iter().enumerate().skip(d.tl.scroll).take(h) {
        let on = at == i;
        let line = match row {
            Row::Host(a) => {
                let col = d.fleet.host(a).map_or(c::MUTED(), |x| x.color);
                let label = crate::showcase::shown(a).into_owned();
                let fill = w.saturating_sub(ui::width(&label) + 4);
                Line::from(vec![
                    Span::styled("● ", ui::fg(col)),
                    Span::styled(label, ui::fg(col).add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled("─".repeat(fill), ui::fg(c::LINE())),
                ])
            }
            Row::Head(t) => {
                let fill = w.saturating_sub(ui::width(t) + 4);
                Line::from(vec![
                    Span::styled("◇ ", ui::faint()),
                    Span::styled(t.clone(), ui::muted().add_modifier(Modifier::BOLD)),
                    Span::raw(" "),
                    Span::styled("─".repeat(fill), ui::fg(c::LINE())),
                ])
            }
            Row::Project(name) => Line::from(vec![
                Span::raw("  "),
                Span::styled("▪ ", ui::fg(c::ACCENT())),
                Span::styled(
                    if name.is_empty() {
                        "no project".to_string()
                    } else {
                        crate::showcase::text(name).into_owned()
                    },
                    ui::bold(),
                ),
            ]),
            Row::Past(i) => past_line(d, &d.tl.hist[*i], w),
            Row::Live(k) => {
                let Some(s) = d.fleet.find(k) else {
                    lines.push(Line::raw(""));
                    continue;
                };
                let st = of_session(d, s);
                let (g, col, _) = ui::status(&s.status, d.t0);
                let (g, col) = if s.limit.is_some() {
                    ("◷", c::LIMIT())
                } else {
                    (g, col)
                };
                let mut l = vec![
                    Span::raw("  "),
                    Span::styled(format!("{g} "), ui::fg(col)),
                    crate::icons::agent(&s.agent),
                    Span::raw(" "),
                    Span::styled(
                        ui::pad(&ui::trunc(s.display(), nw), nw),
                        Style::new().fg(s.color()).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                ];
                l.extend(bar(&cells(&st, from, to, bw)));
                if right > 0 {
                    l.push(Span::raw("  "));
                    l.extend(summary(&st, from, to, right));
                }
                Line::from(l)
            }
            Row::Ended {
                name, agent, key, ..
            } => {
                let st = statelog::tidy(logged(d, key, name));
                let mut l = vec![
                    Span::raw("  "),
                    Span::styled("○ ", ui::faint()),
                    crate::icons::agent(agent),
                    Span::raw(" "),
                    Span::styled(ui::pad(&ui::trunc(name, nw), nw), ui::faint()),
                    Span::raw("  "),
                ];
                l.extend(bar(&cells(&st, from, to, bw)));
                if right > 0 {
                    l.push(Span::raw("  "));
                    let mut sm = summary(&st, from, to, right);
                    if let Some(first) = sm.first_mut() {
                        if first.content.trim().is_empty() {
                            *first = Span::styled(ui::pad("ended", right), ui::faint());
                        }
                    }
                    l.extend(sm);
                }
                Line::from(l)
            }
        };
        lines.push(if on { line.style(ui::selected()) } else { line });
    }
    f.render_widget(Paragraph::new(lines), list);
    // the accent bar on the selected row
    if at >= d.tl.scroll && at < d.tl.scroll + h {
        let y = list.y + (at - d.tl.scroll) as u16;
        if let Some(cell) = f.buffer_mut().cell_mut((list.x, y)) {
            cell.set_symbol("┃").set_fg(c::ACCENT());
        }
    }
}

/// An earlier conversation's row: its agent, machine, when, its prompts and $.
fn past_line(d: &Dash, b: &crate::timeline::Brief, w: usize) -> Line<'static> {
    let col = d.fleet.host(&b.host).map_or(c::MUTED(), |x| x.color);
    let now = crate::cache::now();
    let when = ui::ago(now.saturating_sub(b.last));
    let left = vec![
        Span::raw("    "),
        crate::icons::agent(&b.agent),
        Span::raw(" "),
        Span::styled(crate::showcase::shown(&b.host).into_owned(), ui::fg(col)),
        Span::styled(format!(" · {when}"), ui::faint()),
    ];
    let mut right = Vec::new();
    if b.prompts > 0 {
        right.push(format!(
            "{} prompt{}",
            b.prompts,
            if b.prompts == 1 { "" } else { "s" }
        ));
    }
    right.push(format!("{} req", b.requests));
    right.push(crate::usage::dash::money_short(b.cost, b.estimated));
    let right = right.join(" · ");
    let lw: usize = left.iter().map(|s| ui::width(&s.content)).sum();
    let mut spans = left;
    if lw + ui::width(&right) + 2 <= w {
        spans.push(Span::raw(" ".repeat(w - lw - ui::width(&right))));
        spans.push(Span::styled(right, ui::muted()));
    }
    Line::from(spans)
}

/// The selected conversation's timeline, in its own panel.
fn draw_conversation(
    d: &mut Dash,
    f: &mut Frame,
    r: Rect,
    pick: Option<&(String, String, String, bool)>,
) {
    let mut title = vec![Span::raw(" Conversation ")];
    if let Some((host, agent, _, _)) = pick {
        title.push(Span::styled(
            format!("· {agent} · {} ", crate::showcase::shown(host)),
            ui::faint(),
        ));
        let m = match d.tv.mode(&d.cfg) {
            crate::timeline::Fold::Smart => "smart",
            crate::timeline::Fold::Open => "open",
            crate::timeline::Fold::Folded => "folded",
        };
        title.push(Span::styled(format!("· {m} "), ui::faint()));
    }
    let block = ui::panel(Line::from(title), false);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let Some((host, agent, session, running)) = pick else {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("Pick a conversation on the left.", ui::muted())),
                Line::from(Span::styled(
                    "A session whose agent said which conversation it is, or one from earlier: its prompts, turns and tool calls, with their time and cost.",
                    ui::faint(),
                )),
            ])
            .wrap(ratatui::widgets::Wrap { trim: true }),
            inner,
        );
        return;
    };
    super::tree_view::show(d, host, agent, session, *running);
    let body = if inner.height < 4 {
        inner
    } else {
        // a line saying how to get its keys
        let hint = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                "t: the turn view, with its keys · the wheel scrolls, a click folds",
                ui::faint(),
            )),
            hint,
        );
        Rect {
            height: inner.height - 1,
            ..inner
        }
    };
    super::tree_view::draw(d, f, body, false);
}

/// `3h 10m busy · ▲ 4`, `w` wide.
fn summary(v: &[(u64, u64, St)], from: u64, to: u64, w: usize) -> Vec<Span<'static>> {
    let (busy, asks) = tally(v, from, to);
    let mut out = Vec::new();
    let mut used = 0;
    if busy > 0 {
        let t = format!("{} busy", ui::dur(busy));
        used += ui::width(&t);
        out.push(Span::styled(t, ui::fg(c::WORKING())));
    }
    if asks > 0 {
        let t = format!(
            "{}{} {asks}",
            if used > 0 { " · " } else { "" },
            crate::look::needs()
        );
        used += ui::width(&t);
        out.push(Span::styled(t, ui::fg(c::NEEDS())));
    }
    if used == 0 {
        out.push(Span::raw(" ".repeat(w)));
    } else if used < w {
        out.push(Span::raw(" ".repeat(w - used)));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use St::*;

    #[test]
    fn cells_take_the_state_with_the_most_time() {
        // 0..600 in 6 cells of 100 s
        let v = [(0, 150, Working), (150, 160, Needs), (160, 400, Done)];
        let c = cells(&v, 0, 600, 6);
        assert_eq!(c[0], Some(Working));
        // 50 s working, 10 s asking (×3 = 30), 40 s done: working
        assert_eq!(c[1], Some(Working));
        assert_eq!(c[2], Some(Done));
        assert_eq!(c[3], Some(Done));
        // nothing seen after 400
        assert_eq!(c[4], None);
        assert_eq!(c[5], None);
        // a short question in a quiet cell still shows
        let c = cells(&[(0, 5, Needs)], 0, 600, 6);
        assert_eq!(c[0], Some(Needs));
        // before the window and empty input
        assert_eq!(cells(&[(0, 50, Idle)], 100, 200, 2), vec![None, None]);
        assert_eq!(cells(&[], 0, 10, 3), vec![None, None, None]);
    }

    #[test]
    fn bars_join_runs_and_leave_gaps_blank() {
        let b = bar(&[Some(Working), Some(Working), None, Some(Idle)]);
        let text: Vec<String> = b.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(text, vec!["██", " ", "▁"]);
    }

    #[test]
    fn the_axis_ticks_hours() {
        // six hours from 08:00 over 60 cells: a label every hour, 10 apart
        let from = 8 * 3600;
        let a = axis(from, from + 6 * 3600, 60, 0);
        assert_eq!(a.chars().count(), 60);
        assert!(a.starts_with("          09:00"), "{a:?}");
        assert!(a.contains("10:00") && a.contains("13:00"), "{a:?}");
        assert!(a.ends_with("now"), "{a:?}");
        // 48 hours over 40 cells: every 12 hours, short labels
        let a = axis(from, from + 48 * 3600, 40, 0);
        assert!(a.contains("12") && !a.contains(":00"), "{a:?}");
        // the local offset moves the ticks (UTC+05:30: 08:00 UTC is 13:30)
        let a = axis(from, from + 6 * 3600, 60, 19800);
        assert!(a.starts_with("     14:00"), "{a:?}");
        // too narrow for anything
        assert_eq!(axis(0, 3600, 2, 0), "  ");
    }

    #[test]
    fn tally_counts_work_and_questions() {
        let v = [
            (0, 100, Working),
            (100, 110, Needs),
            (110, 300, Working),
            (300, 320, Needs),
        ];
        assert_eq!(tally(&v, 50, 1000), (240, 2));
        assert_eq!(tally(&v, 200, 1000), (100, 1));
    }
}
