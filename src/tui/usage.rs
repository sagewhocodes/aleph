//! Usage in the dashboard: Monitor › Usage (`u`) and the numbers other views
//! put beside a session (Fleet rows, Focus, Inbox) or a machine (its card
//! and panel).
//!
//! Loaded off the UI thread ([`crate::usage::dash::load`]) every minute and
//! whenever the range, breakdown or machine filter changes, from the local
//! store that the daemon fills (or this dashboard, while no daemon runs).
//!
//! Keys in the view: `t` period (`r`/`R` the next / the one before) · `a`
//! agents and accounts · `m` machines · `p` projects · `/` typed · `x`
//! clears (`usage_filters`) · `b`/`B` breakdown · `↑`/`↓` select · `enter`
//! opens a session's Focus · `S` reads every machine's logs now.

use super::Dash;
use crate::herdr::Session;
use crate::ui::{self, c};
use crate::usage::dash::{self, Data, Live};
use crate::usage::{By, Row};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the numbers are read again.
const RELOAD: Duration = Duration::from_secs(60);

const BYS: [(By, &str); 8] = [
    (By::Agent, "agent"),
    (By::Account, "account"),
    (By::Model, "model"),
    (By::Host, "machine"),
    (By::Project, "project"),
    (By::Folder, "folder"),
    (By::Session, "session"),
    (By::Effort, "effort"),
];

pub(crate) struct State {
    pub data: Option<Data>,
    gen: u64,
    loading: bool,
    at: Option<Instant>,
    rx: Option<Receiver<(u64, Data)>>,
    /// the period and what it's narrowed to
    pub f: super::usage_filters::Filters,
    /// the screens: tab, layout, splits
    pub view: super::usage_tabs::View,
    pub by: usize,
    pub sel: usize,
    /// only the machine picked in Monitor (`[` `]`)
    pub mine: bool,
    /// only one machine or one agent (the palette's "usage b31", "usage
    /// claude"; `x` shows everything again)
    pub only: Option<Only>,
    syncing: Arc<AtomicBool>,
    /// reads the logs in the background while this dashboard runs the hub
    pub bg: crate::usage::Background,
    /// the OpenRouter account's spend (its API, every 5 minutes off this
    /// thread): the card here and the header's chip
    pub or: crate::openrouter::Watch,
}

impl Default for State {
    fn default() -> Self {
        State {
            data: None,
            gen: 0,
            loading: false,
            at: None,
            rx: None,
            f: super::usage_filters::Filters::default(),
            view: super::usage_tabs::View::default(),
            by: 0,
            sel: 0,
            mine: false,
            only: None,
            syncing: Arc::new(AtomicBool::new(false)),
            bg: crate::usage::Background::new(),
            or: crate::openrouter::Watch::default(),
        }
    }
}

/// What Usage can narrow to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Only {
    Host(String),
    Agent(String),
}

fn host_filter(d: &Dash) -> Option<String> {
    if let Some(Only::Host(h)) = &d.usage.only {
        return Some(h.clone());
    }
    if !d.usage.mine {
        return None;
    }
    d.fleet.hosts.get(d.mon.sel).map(|h| h.alias.clone())
}

fn agent_filter(d: &Dash) -> Option<String> {
    match &d.usage.only {
        Some(Only::Agent(a)) => Some(a.clone()),
        _ => None,
    }
}

/// Monitor › Usage for one machine or one agent (None: everything).
pub(crate) fn show_only(d: &mut Dash, only: Option<Only>) {
    d.usage.f.clear();
    match &only {
        Some(Only::Host(h)) => d.usage.f.hosts = vec![h.clone()],
        Some(Only::Agent(a)) => d.usage.f.agents = vec![a.clone()],
        None => {}
    }
    d.usage.only = only;
    d.usage.mine = false;
    d.usage.sel = 0;
    // one machine: its agents side by side; one agent: its accounts (and so its machines' logins)
    let at = |b: By| BYS.iter().position(|(x, _)| *x == b).unwrap_or(0);
    match &d.usage.only {
        Some(Only::Host(_)) => d.usage.by = at(By::Agent),
        Some(Only::Agent(_)) => d.usage.by = at(By::Account),
        None => {}
    }
    d.switch(super::View::Monitor);
    super::monitor::set_sub(d, super::monitor_draw::Sub::Usage);
    reload(d);
}

/// Read again now (the next `pump` starts it).
pub(crate) fn reload(d: &mut Dash) {
    d.usage.at = None;
}

/// Call every frame: start a load when one is due; take a finished one.
pub(crate) fn pump(d: &mut Dash) {
    d.usage.or.pump(&d.cfg);
    if let Some(rx) = &d.usage.rx {
        if let Ok((g, data)) = rx.try_recv() {
            if g == d.usage.gen {
                // showcase: the logs' folders (sessions outside herdr among
                // them) are masked like herdr sessions'
                if crate::showcase::on() {
                    let folders = data.live.values().map(|l| l.project.as_str());
                    let keys = data.breakdown.iter().map(|r| r.key.as_str());
                    for f in folders.chain(keys).filter(|k| k.starts_with(['~', '/'])) {
                        crate::showcase::learn_folder(f);
                    }
                }
                d.usage.data = Some(data);
                d.usage.loading = false;
                let n = d.usage.data.as_ref().map_or(0, |x| x.breakdown.len());
                d.usage.sel = d.usage.sel.min(n.saturating_sub(1));
            }
        }
    }
    let due = d.usage.at.is_none_or(|t| t.elapsed() > RELOAD);
    if !due || (d.usage.loading && d.usage.at.is_some()) {
        return;
    }
    d.usage.gen += 1;
    d.usage.loading = true;
    d.usage.at = Some(Instant::now());
    let (tx, rx) = channel();
    d.usage.rx = Some(rx);
    let mut filter = d.usage.f.filter();
    // `a`'s old meaning, kept: the machine picked in Monitor
    if let (true, Some(h)) = (d.usage.mine, host_filter(d)) {
        filter.hosts = vec![h];
    }
    let (cfg, g, range, by) = (
        d.cfg.clone(),
        d.usage.gen,
        d.usage.f.range(),
        BYS[d.usage.by % BYS.len()].0,
    );
    // the breakup and the daily stacks split by what the rows are (`b`)
    let split = by;
    let tree =
        super::usage_tabs::SPLITS[d.usage.view.pivot_split % super::usage_tabs::SPLITS.len()].0;
    std::thread::spawn(move || {
        let data = dash::load_split(&cfg, range, by, &filter, split, tree);
        let _ = tx.send((g, data));
    });
}

// ------------------------------------------------------------------ for other views

/// A session's numbers (by the agent's own session id, which herdr reports).
pub(crate) fn live_for<'a>(d: &'a Dash, s: &Session) -> Option<&'a Live> {
    let id = s.native_id.as_deref()?;
    d.usage.data.as_ref()?.live.get(id)
}

/// A machine's totals today.
pub(crate) fn today_for<'a>(d: &'a Dash, host: &str) -> Option<&'a Row> {
    d.usage.data.as_ref()?.today.get(host)
}

/// `12.4k tok · ctx 63% · $0.42` (what fits in `room` columns, dropped from
/// the right).
pub(crate) fn chip(l: &Live, room: usize) -> Vec<Span<'static>> {
    let mut parts: Vec<(String, Style)> =
        vec![(format!("{} tok", dash::tokens(l.tokens)), ui::faint())];
    if let Some(p) = l.ctx_pct() {
        let col = if p >= 85 {
            c::RED()
        } else if p >= 65 {
            c::YELLOW()
        } else {
            c::MUTED()
        };
        let t = if p == 0 && l.ctx.unwrap_or(0) > 0 {
            "ctx <1%".to_string()
        } else {
            format!("ctx {p}%")
        };
        parts.push((t, ui::fg(col)));
    }
    if l.cost > 0.0 {
        parts.push((dash::money(l.cost, l.estimated), ui::faint()));
    }
    while !parts.is_empty() && parts.iter().map(|(s, _)| ui::width(s) + 3).sum::<usize>() > room {
        parts.pop();
    }
    let mut v = Vec::new();
    for (i, (s, st)) in parts.into_iter().enumerate() {
        if i > 0 {
            v.push(Span::styled(" · ", ui::faint()));
        }
        v.push(Span::styled(s, st));
    }
    v
}

/// `today 1.2M tok · $4.20 est.` for a machine.
pub(crate) fn today_line(r: &Row) -> Vec<Span<'static>> {
    let mut v = vec![
        Span::styled("today ", ui::faint()),
        Span::styled(format!("{} tok", dash::tokens(r.tokens())), ui::muted()),
    ];
    if r.cost() > 0.0 {
        v.push(Span::styled(" · ", ui::faint()));
        v.push(Span::styled(dash::row_money(r), ui::muted()));
    }
    if r.active_secs() > 0 {
        v.push(Span::styled(" · ", ui::faint()));
        v.push(Span::styled(
            format!("{} active", ui::dur(r.active_secs())),
            ui::faint(),
        ));
    }
    v
}

// ------------------------------------------------------------------ keys

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> bool {
    let n = d
        .usage
        .data
        .as_ref()
        .map_or(0, |x| x.more.tree.len().max(x.breakdown.len()));
    if super::usage_filters::key(d, &k) || super::usage_tabs::key(d, &k) {
        return true;
    }
    let periods = crate::usage::PERIODS.len();
    match k.code {
        KeyCode::Char('r') => {
            d.usage.f.custom = None;
            d.usage.f.period = (d.usage.f.period + 1) % periods;
            reload(d);
        }
        KeyCode::Char('R') => {
            d.usage.f.custom = None;
            d.usage.f.period = (d.usage.f.period + periods - 1) % periods;
            reload(d);
        }
        KeyCode::Char('b') => step_by(d, 1),
        KeyCode::Char('B') => step_by(d, BYS.len() - 1),

        KeyCode::Char('[') | KeyCode::Char(']') if d.usage.mine => return false,
        KeyCode::Down | KeyCode::Char('j') => {
            d.usage.sel = (d.usage.sel + 1).min(n.saturating_sub(1))
        }
        KeyCode::Up | KeyCode::Char('k') => d.usage.sel = d.usage.sel.saturating_sub(1),
        KeyCode::Char('S') => sync_now(d),
        // back out of the last narrowing
        KeyCode::Backspace if d.usage.f.narrowed() => {
            let f = &mut d.usage.f;
            if !f.projects.is_empty() {
                f.projects.clear();
            } else if !f.hosts.is_empty() {
                f.hosts.clear();
            } else if !f.accounts.is_empty() {
                f.accounts.clear();
            } else {
                f.agents.clear();
            }
            super::usage_filters::changed(d);
        }
        KeyCode::Enter => open_selected(d),
        _ => return false,
    }
    true
}

/// `[ ]` moved the machine: the filtered numbers follow.
pub(crate) fn machine_changed(d: &mut Dash) {
    if d.usage.mine {
        reload(d);
    }
}

fn sync_now(d: &mut Dash) {
    // OpenRouter's numbers too, from its API
    d.usage.or.again();
    if d.usage.syncing.swap(true, Ordering::Relaxed) {
        d.toast(super::Level::Info, "already reading the logs…");
        return;
    }
    d.toast(
        super::Level::Info,
        "reading every machine's agent logs (only numbers leave them)…",
    );
    let (cfg, flag) = (d.cfg.clone(), d.usage.syncing.clone());
    let tx = d.tx.clone();
    std::thread::spawn(move || {
        let reps = crate::usage::sync_all(&cfg);
        flag.store(false, Ordering::Relaxed);
        let bad: Vec<String> = reps
            .iter()
            .filter(|r| !r.ok())
            .map(|r| format!("{}: {}", r.host, r.error.clone().unwrap_or_default()))
            .collect();
        let msg = if bad.is_empty() {
            format!("usage read on {} machine(s)", reps.len())
        } else {
            format!("usage: {}", bad.join(" · "))
        };
        let _ = tx.send(super::Bg::Done {
            ok: bad.is_empty(),
            msg,
            refresh: None,
        });
    });
    d.usage.at = Some(Instant::now() - RELOAD + Duration::from_secs(20));
}

fn open_selected(d: &mut Dash) {
    let Some(data) = &d.usage.data else { return };
    let by = BYS[d.usage.by % BYS.len()].0;
    // a row of the breakdown: narrow to it (backspace / x go back)
    if by != By::Session {
        let Some(key) = data.more.tree.get(d.usage.sel).map(|(r, _)| r.key.clone()) else {
            return;
        };
        let f = &mut d.usage.f;
        match by {
            By::Agent => f.agents = vec![key],
            By::Account => f.accounts = vec![key],
            By::Host => f.hosts = vec![key],
            By::Project => f.projects = vec![key],
            _ => return,
        }
        super::usage_filters::changed(d);
        return;
    }
    let Some(r) = data.breakdown.get(d.usage.sel) else {
        return;
    };
    let id = r.key.rsplit(':').next().unwrap_or("");
    let s = d
        .fleet
        .sessions()
        .into_iter()
        .find(|s| s.native_id.as_deref() == Some(id))
        .cloned();
    match s {
        Some(s) => super::wall::focus_on(d, &s.key(), super::View::Monitor),
        None => d.toast(super::Level::Info, "that session isn't running any more"),
    }
}

/// What the breakdown's rows are (`b`); the breakup and the daily stacks
/// split by it too.
pub(crate) fn by_of(d: &Dash) -> By {
    BYS[d.usage.by % BYS.len()].0
}

/// Its name, for the charts' titles.
pub(crate) fn by_name(d: &Dash) -> &'static str {
    BYS[d.usage.by % BYS.len()].1
}

/// The next (or, `n` = len − 1, the previous) "by", read again.
pub(crate) fn step_by(d: &mut Dash, n: usize) {
    d.usage.by = (d.usage.by + n) % BYS.len();
    d.usage.sel = 0;
    reload(d);
}

pub(crate) fn hints() -> Vec<(&'static str, &'static str)> {
    vec![
        ("t", "period"),
        ("a m p", "agents · machines · projects"),
        ("/", "type"),
        ("b", "breakdown"),
        ("↑↓", "select"),
        ("S", "read logs now"),
        ("←→", "view"),
        ("esc", "back"),
    ]
}

// ------------------------------------------------------------------ drawing

/// A read this old is shown as late (the daemon reads every minute).
const READ_LATE: u64 = 15 * 60;

/// One line: each machine, and how its logs were last read. A read that failed
/// says why, in red, so numbers that stopped moving have an explanation.
pub(crate) fn reads_line(
    cfg: &crate::config::Config,
    reads: &[crate::usage::ReadState],
    now: u64,
    width: usize,
) -> Line<'static> {
    let failed = reads.iter().filter(|r| r.error.is_some()).count();
    let mut l: Vec<Span<'static>> = Vec::new();
    for r in reads {
        let col = crate::theme::machine(cfg, &r.host);
        l.push(Span::styled("▌", ui::fg(col)));
        l.push(Span::styled(format!("{} ", r.host), ui::fg(col)));
        let age = now.saturating_sub(r.at);
        let (text, style) = match &r.error {
            // one failure gets the room to say why; several, the first words
            Some(e) => (
                format!("✕ {}", ui::trunc(e, if failed == 1 { 72 } else { 24 })),
                ui::fg(c::RED()),
            ),
            None if r.at == 0 => ("never read".to_string(), ui::muted()),
            None if r.waiting > 0 => (
                format!("⟳ {} files waiting", r.waiting),
                ui::fg(c::YELLOW()),
            ),
            None if age > READ_LATE => (format!("✓ {}", ui::ago(age)), ui::fg(c::YELLOW())),
            None => (format!("✓ {}", ui::ago(age)), ui::faint()),
        };
        l.push(Span::styled(text, style));
        l.push(Span::raw("  "));
    }
    if failed > 0 {
        l.push(Span::styled("aleph usage sources", ui::faint()));
    }
    // never past the edge: the last machines give way
    let mut used = 0;
    let keep = l
        .iter()
        .take_while(|s| {
            used += ui::width(&s.content);
            used <= width
        })
        .count();
    l.truncate(keep);
    Line::from(l)
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    if r.height < 4 || r.width < 30 {
        return;
    }
    // the by-chips beside the filters' bar
    let mut chips: Vec<Span> = vec![Span::styled("by ", ui::faint())];
    for (i, (_, name)) in BYS.iter().enumerate() {
        let on = i == d.usage.by;
        chips.push(Span::styled(
            format!(" {name} "),
            if on {
                ui::selected().add_modifier(Modifier::BOLD)
            } else {
                ui::muted()
            },
        ));
    }
    if d.usage.mine {
        if let Some(h) = host_filter(d) {
            chips.push(Span::styled(
                format!("   {h} ([ ] machine)"),
                ui::fg(c::ACCENT()),
            ));
        }
    }
    if d.usage.loading || d.usage.syncing.load(Ordering::Relaxed) {
        chips.push(Span::styled(
            format!("  {} reading", ui::spinner(d.t0)),
            ui::faint(),
        ));
    }
    // how each machine's logs were last read, whenever there's room for a line
    let strip_h: u16 = if r.height >= 12 { 1 } else { 0 };
    let [bar, top, strip, body] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(strip_h),
        Constraint::Min(1),
    ])
    .areas(r);
    super::usage_filters::chip_bar(d, f, bar);
    f.render_widget(Paragraph::new(Line::from(chips)), top);
    if let Some(data) = &d.usage.data {
        let line = reads_line(
            &d.cfg,
            &data.reads,
            crate::cache::now(),
            strip.width as usize,
        );
        f.render_widget(Paragraph::new(line), strip);
    }
    // the OpenRouter account's own card, when there's a key
    let or = d
        .usage
        .or
        .spend
        .clone()
        .filter(|_| d.usage.or.has_key != Some(false));
    let or_h: u16 = if or.is_some() { OR_H } else { 0 };
    let or_busy = d.usage.or.busy;
    let Some(data) = d.usage.data.clone() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{} reading the usage store…", ui::spinner(d.t0)),
                ui::muted(),
            )),
            body,
        );
        return;
    };
    if data.total.requests == 0 && data.kpis.iter().all(|(_, k)| k.requests == 0) {
        let lines = vec![
            Line::from(Span::styled("No usage in this range yet.", ui::bold())),
            Line::from(Span::styled(
                "Aleph reads the agents' own logs on each machine (only ids, times, models and numbers leave them).",
                ui::muted(),
            )),
            Line::from(vec![
                Span::styled("S", ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)),
                Span::styled(" reads them now · ", ui::muted()),
                Span::styled("aleph usage sources", ui::fg(c::ACCENT())),
                Span::styled(" shows what was read", ui::muted()),
            ]),
        ];
        let [msg, card] =
            Layout::vertical([Constraint::Length(5), Constraint::Length(or_h)]).areas(body);
        f.render_widget(Paragraph::new(lines), msg.inner(Margin::new(1, 1)));
        if let Some(s) = &or {
            draw_openrouter(f, card.intersection(body), s, or_busy, d.t0);
        }
        return;
    }
    // the OpenRouter account's own card heads the screens, when there's room
    let body = match &or {
        Some(s) if body.height >= OR_H + 16 => {
            let [card, rest] =
                Layout::vertical([Constraint::Length(OR_H), Constraint::Min(3)]).areas(body);
            draw_openrouter(f, card, s, or_busy, d.t0);
            rest
        }
        _ => body,
    };
    super::usage_tabs::draw(d, f, body, &data);
}

/// The OpenRouter card's height (four lines in a panel).
const OR_H: u16 = 6;

/// The OpenRouter card: what the account spent (today, this month, in all),
/// credits left as a bar, the key's limit, and how old the numbers are —
/// from OpenRouter's own API, not the logs.
fn draw_openrouter(f: &mut Frame, r: Rect, s: &crate::openrouter::Spend, busy: bool, t0: Instant) {
    if r.height < 3 || r.width < 20 {
        return;
    }
    let mut title = vec![Span::styled(" OpenRouter ", ui::bold())];
    if busy {
        title.push(Span::styled(
            format!("{} reading ", ui::spinner(t0)),
            ui::faint(),
        ));
    }
    let block = ui::panel(Line::from(title), false);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let lines = openrouter_card(s, crate::cache::now(), inner.width as usize);
    f.render_widget(Paragraph::new(lines), inner);
}

/// The card's lines, `w` columns wide (pure — tested).
pub(crate) fn openrouter_card(
    s: &crate::openrouter::Spend,
    now: u64,
    w: usize,
) -> Vec<Line<'static>> {
    use dash::money;
    let lab = |t: &str| Span::styled(format!("{t:<9}"), ui::faint());
    let level = |left: f64, of: f64| {
        let share = if of > 0.0 { left / of } else { 1.0 };
        if share < 0.1 {
            c::RED()
        } else if share < 0.25 {
            c::YELLOW()
        } else {
            c::GREEN()
        }
    };
    let mut v = Vec::new();
    // spent
    let mut l = vec![lab("spent")];
    let mut first = true;
    for (what, x) in [
        ("today", s.today),
        ("this month", s.month),
        ("total", s.usage),
    ] {
        let Some(x) = x else { continue };
        if !first {
            l.push(Span::styled("  ·  ", ui::faint()));
        }
        first = false;
        l.push(Span::styled(format!("{what} "), ui::muted()));
        l.push(Span::styled(
            money(x, false),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ));
    }
    if first {
        l.push(Span::styled("nothing read yet", ui::faint()));
    }
    v.push(Line::from(l));
    // credits, as a bar of what's left
    let mut l = vec![lab("credits")];
    match (s.left(), s.credits) {
        (Some(left), Some(total)) if total > 0.0 => {
            let col = level(left, total);
            l.extend(ui::bar(left / total, (w / 4).clamp(6, 16), col));
            l.push(Span::styled(
                format!(" {}", money(left, false)),
                ui::fg(col).add_modifier(Modifier::BOLD),
            ));
            l.push(Span::styled(
                format!(" left of {}", money(total, false)),
                ui::muted(),
            ));
        }
        (Some(_), Some(_)) => l.push(Span::styled("none bought yet", ui::muted())),
        _ => l.push(Span::styled(
            "only a management key can read them",
            ui::faint(),
        )),
    }
    v.push(Line::from(l));
    // the key's own limit
    let mut l = vec![lab("limit")];
    match s.limit.filter(|x| *x > 0.0) {
        Some(lim) => {
            let left = s.limit_left.unwrap_or(lim);
            l.push(Span::styled(
                money(left, false),
                ui::fg(level(left, lim)).add_modifier(Modifier::BOLD),
            ));
            l.push(Span::styled(
                format!(
                    " left of {}{}",
                    money(lim, false),
                    s.limit_reset
                        .as_deref()
                        .map(|r| format!(" · resets {r}"))
                        .unwrap_or_default()
                ),
                ui::muted(),
            ));
        }
        None => l.push(Span::styled("none on this key", ui::faint())),
    }
    v.push(Line::from(l));
    // how old, or what went wrong
    let age = now.saturating_sub(s.at);
    let l = match &s.error {
        Some(e) => vec![
            Span::styled("! ", ui::fg(c::RED()).add_modifier(Modifier::BOLD)),
            Span::styled(e.clone(), ui::fg(c::RED())),
        ],
        None if s.stale(now) => vec![Span::styled(
            format!("read {} — stale (S reads it now)", ui::ago(age)),
            ui::fg(c::YELLOW()),
        )],
        None => vec![Span::styled(format!("read {}", ui::ago(age)), ui::faint())],
    };
    v.push(Line::from(l));
    v
}

/// Plan windows from the logs (Codex's rate limits): `b31 codex 5h 42%`.
pub(crate) fn limit_lines(v: &serde_json::Value) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let Some(hosts) = v.as_object() else {
        return out;
    };
    for (host, agents) in hosts {
        for (agent, ids) in agents.as_object().into_iter().flatten() {
            for (_, l) in ids.as_object().into_iter().flatten() {
                let mut sp = vec![Span::styled(
                    format!("{:<16}", format!("{host} {agent}")),
                    ui::faint(),
                )];
                for (i, w) in l
                    .get("windows")
                    .and_then(|w| w.as_array())
                    .into_iter()
                    .flatten()
                    .enumerate()
                {
                    let pct = w
                        .get("used_percent")
                        .and_then(|x| x.as_f64())
                        .unwrap_or(0.0);
                    let mins = w.get("minutes").and_then(|x| x.as_u64()).unwrap_or(0);
                    let span = match mins {
                        0 => "window".to_string(),
                        m if m % 10_080 == 0 => format!("{}w", m / 10_080),
                        m if m % 1440 == 0 => format!("{}d", m / 1440),
                        m if m % 60 == 0 => format!("{}h", m / 60),
                        m => format!("{m}m"),
                    };
                    if i > 0 {
                        sp.push(Span::styled(" · ", ui::faint()));
                    }
                    let col = if pct >= 90.0 {
                        c::RED()
                    } else if pct >= 70.0 {
                        c::YELLOW()
                    } else {
                        c::MUTED()
                    };
                    sp.push(Span::styled(format!("{span} {pct:.0}%"), ui::fg(col)));
                    if let Some(t) = w.get("resets_at").and_then(|x| x.as_u64()) {
                        sp.push(Span::styled(
                            format!(" (resets {})", ui::clock(t)),
                            ui::faint(),
                        ));
                    }
                }
                out.push(Line::from(sp));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(l: &Line) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn chips_drop_from_the_right() {
        let l = Live {
            tokens: 12_400,
            ctx: Some(126_000),
            window: Some(200_000),
            cost: 0.42,
            ..Default::default()
        };
        let all: String = chip(&l, 60).iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(all, "12k tok · ctx 63% · $0.42");
        let some: String = chip(&l, 22).iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(some, "12k tok · ctx 63%");
    }
}
