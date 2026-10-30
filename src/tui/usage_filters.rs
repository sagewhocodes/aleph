//! Usage's filters: a period (rolling, calendar, or your own from–to) and
//! any of agents (and their accounts), machines and projects — a chip bar at
//! the top (click a chip to change it, its × to drop it), a picker for each
//! (`t` period · `a` agents and accounts · `m` machines · `p` projects:
//! type to search, space picks several, enter applies), a calendar for a
//! custom range, and `/` to type it all at once (`claude b31 aleph 30d`,
//! `acct:a3f2c1`, `2026-09-01..2026-10-15`). `x` clears.

use super::forms::{Field, Res};
use super::Dash;
use crate::ui::{self, c};
use crate::usage::dash::Opt;
use crate::usage::{Filter, Range, PERIODS};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Clear, Paragraph};
use std::collections::BTreeSet;

/// What Usage is narrowed to.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Filters {
    /// one of `PERIODS` (by index)
    pub period: usize,
    /// your own range (local days), over `period`
    pub custom: Option<(i64, i64)>,
    pub agents: Vec<String>,
    /// account ids
    pub accounts: Vec<String>,
    pub hosts: Vec<String>,
    /// project ids
    pub projects: Vec<String>,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            period: period_index("7d"),
            custom: None,
            agents: Vec::new(),
            accounts: Vec::new(),
            hosts: Vec::new(),
            projects: Vec::new(),
        }
    }
}

pub(crate) fn period_index(w: &str) -> usize {
    PERIODS.iter().position(|(x, _)| *x == w).unwrap_or(2)
}

impl Filters {
    pub fn range(&self) -> Range {
        match self.custom {
            Some((a, b)) => Range::between(a, b),
            None => Range::parse(PERIODS[self.period % PERIODS.len()].0)
                .unwrap_or_else(|| Range::days(7)),
        }
    }
    pub fn period_label(&self) -> String {
        match self.custom {
            // your own: its dates, as you picked them
            Some((a, b)) if a == b => crate::usage::day_name(a),
            Some((a, b)) => format!(
                "{} – {}",
                crate::usage::day_name(a),
                crate::usage::day_name(b)
            ),
            None => PERIODS[self.period % PERIODS.len()].1.to_string(),
        }
    }
    pub fn filter(&self) -> Filter {
        Filter {
            hosts: self.hosts.clone(),
            agents: self.agents.clone(),
            accounts: self.accounts.clone(),
            projects: self.projects.clone(),
            ..Default::default()
        }
    }
    /// Any filter but the period.
    pub fn narrowed(&self) -> bool {
        !(self.agents.is_empty()
            && self.accounts.is_empty()
            && self.hosts.is_empty()
            && self.projects.is_empty())
    }
    pub fn clear(&mut self) {
        self.agents.clear();
        self.accounts.clear();
        self.hosts.clear();
        self.projects.clear();
    }
}

// ------------------------------------------------------------------ the chip bar

/// What a chip changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dim {
    Period,
    Agents,
    Machines,
    Projects,
    /// Monitor › Timeline's picker (`timelist`): all of them on one list
    Timeline,
}

/// A chip's click: open its picker, or drop it (its ×).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UChip {
    Open(Dim),
    Drop(Dim),
    Clear,
    /// one of Usage's tabs
    Tab(usize),
}

/// The chip bar: `⏱ 7 days ▾ · claude (2 accounts) × · b31 × · aleph × · + filter`.
/// Its chips' rects go into the dashboard's hits.
pub(crate) fn chip_bar(d: &mut Dash, f: &mut Frame, r: Rect) {
    let fl = d.usage.f.clone();
    let names = |kind: char, ids: &[String]| -> String {
        let opts = d
            .usage
            .data
            .as_ref()
            .map(|x| x.options.clone())
            .unwrap_or_default();
        let one = |id: &String| {
            opts.iter()
                .find(|o| o.kind == kind && &o.id == id)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| match kind {
                    'a' => crate::agents::label(id),
                    'c' => crate::usage::accounts::name(id),
                    'p' => crate::usage::projects::name_of(id),
                    _ => id.clone(),
                })
        };
        match ids.len() {
            0 => String::new(),
            1 => one(&ids[0]),
            n => format!("{} +{}", one(&ids[0]), n - 1),
        }
    };
    let mut parts: Vec<(String, Style, Option<UChip>)> = vec![(
        format!(" ⏱ {} ▾ ", fl.period_label()),
        ui::selected().add_modifier(Modifier::BOLD),
        Some(UChip::Open(Dim::Period)),
    )];
    let chip = |text: String| {
        (
            format!(" {text} "),
            Style::new().fg(c::BASE()).bg(c::ACCENT()),
        )
    };
    let add = |parts: &mut Vec<(String, Style, Option<UChip>)>, dim: Dim, text: String| {
        let (t, st) = chip(text);
        parts.push((" ".into(), Style::new(), None));
        parts.push((t, st, Some(UChip::Open(dim))));
        parts.push(("× ".into(), ui::faint(), Some(UChip::Drop(dim))));
    };
    let agents = names('a', &fl.agents);
    let accts = names('c', &fl.accounts);
    let who = match (agents.is_empty(), accts.is_empty()) {
        (false, false) => format!("{agents} · {accts}"),
        (false, true) => agents,
        (true, false) => accts,
        _ => String::new(),
    };
    if !who.is_empty() {
        add(&mut parts, Dim::Agents, who);
    }
    if !fl.hosts.is_empty() {
        add(&mut parts, Dim::Machines, names('h', &fl.hosts));
    }
    if !fl.projects.is_empty() {
        add(&mut parts, Dim::Projects, names('p', &fl.projects));
    }
    parts.push((" ".into(), Style::new(), None));
    parts.push((
        if fl.narrowed() {
            "+ filter · x clears".into()
        } else {
            "+ filter (a m p /)".into()
        },
        ui::faint(),
        Some(if fl.narrowed() {
            UChip::Clear
        } else {
            UChip::Open(Dim::Agents)
        }),
    ));
    let mut x = r.x;
    let mut spans = Vec::new();
    for (t, st, hit) in parts {
        let w = ui::width(&t) as u16;
        if x + w > r.x + r.width {
            break;
        }
        if let Some(h) = hit {
            d.hits
                .chips
                .push((Rect::new(x, r.y, w, 1), super::Chip::Usage(h)));
        }
        spans.push(Span::styled(t, st));
        x += w;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

/// A chip clicked.
pub(crate) fn click(d: &mut Dash, c: UChip) {
    match c {
        UChip::Open(dim) => open(d, dim),
        UChip::Drop(dim) => {
            match dim {
                Dim::Period => d.usage.f.custom = None,
                Dim::Agents => {
                    d.usage.f.agents.clear();
                    d.usage.f.accounts.clear();
                }
                Dim::Machines => d.usage.f.hosts.clear(),
                Dim::Projects => d.usage.f.projects.clear(),
                Dim::Timeline => {}
            }
            changed(d);
        }
        UChip::Clear => {
            d.usage.f.clear();
            changed(d);
        }
        UChip::Tab(i) => {
            d.usage.view.tab = i;
            d.usage.sel = 0;
        }
    }
}

/// The filters moved: what's shown follows.
pub(crate) fn changed(d: &mut Dash) {
    d.usage.only = None;
    d.usage.sel = 0;
    super::usage::reload(d);
}

// ------------------------------------------------------------------ pickers

/// One line of a picker.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    /// what it sets: `a`/`c`/`h`/`p` an option's kind, `P` a period, `C` custom
    pub kind: char,
    pub id: String,
    pub label: String,
    pub depth: u8,
    pub cost: f64,
}

/// A picker: one of the filters, searchable, several at once.
#[derive(Clone)]
pub(crate) struct Picker {
    pub dim: Dim,
    pub items: Vec<Item>,
    pub q: Field,
    pub sel: usize,
    /// (kind, id) picked
    pub on: BTreeSet<(char, String)>,
}

fn items_of(dim: Dim, opts: &[Opt]) -> Vec<Item> {
    let it = |o: &Opt, depth: u8| Item {
        kind: o.kind,
        id: o.id.clone(),
        label: o.label.clone(),
        depth,
        cost: o.cost,
    };
    match dim {
        Dim::Period => {
            let mut v: Vec<Item> = PERIODS
                .iter()
                .map(|(w, l)| Item {
                    kind: 'P',
                    id: w.to_string(),
                    label: l.to_string(),
                    depth: 0,
                    cost: 0.0,
                })
                .collect();
            v.push(Item {
                kind: 'C',
                id: "custom".into(),
                label: "custom from – to…".into(),
                depth: 0,
                cost: 0.0,
            });
            v
        }
        Dim::Agents => opts
            .iter()
            .filter(|o| o.kind == 'a' || o.kind == 'c')
            .map(|o| it(o, u8::from(o.kind == 'c')))
            .collect(),
        Dim::Machines => opts
            .iter()
            .filter(|o| o.kind == 'h')
            .map(|o| it(o, 0))
            .collect(),
        Dim::Projects => opts
            .iter()
            .filter(|o| o.kind == 'p')
            .map(|o| it(o, 0))
            .collect(),
        Dim::Timeline => Vec::new(),
    }
}

/// Open a filter's picker (with what's picked now ticked).
pub(crate) fn open(d: &mut Dash, dim: Dim) {
    let opts = d
        .usage
        .data
        .as_ref()
        .map(|x| x.options.clone())
        .unwrap_or_default();
    let f = &d.usage.f;
    let mut on = BTreeSet::new();
    for (k, v) in [
        ('a', &f.agents),
        ('c', &f.accounts),
        ('h', &f.hosts),
        ('p', &f.projects),
    ] {
        for id in v {
            on.insert((k, id.clone()));
        }
    }
    let items = items_of(dim, &opts);
    let sel = match dim {
        Dim::Period => items
            .iter()
            .position(|i| i.id == PERIODS[f.period % PERIODS.len()].0)
            .unwrap_or(0),
        _ => 0,
    };
    d.mode = super::Mode::UsagePick(Picker {
        dim,
        items,
        q: Field::new(""),
        sel,
        on,
    });
}

impl Picker {
    /// The items the search keeps (an account stays with its agent).
    pub fn shown(&self) -> Vec<usize> {
        let q = self.q.text.trim().to_lowercase();
        if q.is_empty() {
            return (0..self.items.len()).collect();
        }
        (0..self.items.len())
            .filter(|&i| {
                let it = &self.items[i];
                it.label.to_lowercase().contains(&q) || it.id.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn key(&mut self, k: KeyEvent) -> Res {
        let shown = self.shown();
        match k.code {
            KeyCode::Esc => return Res::Cancel,
            KeyCode::Enter => return Res::Submit,
            KeyCode::Down => self.sel = (self.sel + 1).min(shown.len().saturating_sub(1)),
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char(' ') if self.dim != Dim::Period => {
                if let Some(it) = shown.get(self.sel).map(|&i| self.items[i].clone()) {
                    let k = (it.kind, it.id.clone());
                    if !self.on.remove(&k) {
                        self.on.insert(k);
                    }
                }
            }
            _ => {
                if self.q.key(&k) {
                    self.sel = 0;
                }
            }
        }
        Res::Keep
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let shown = self.shown();
        let h = (shown.len() as u16 + 5).clamp(8, area.height.saturating_sub(4).max(8));
        let r = ui::centered(area, 64.min(area.width.saturating_sub(4)), h);
        ui::clear(f, r);
        let title = match self.dim {
            Dim::Period => " Period ",
            Dim::Agents => " Agents and their accounts ",
            Dim::Machines => " Machines ",
            Dim::Projects => " Projects ",
            Dim::Timeline => " Narrow the timeline ",
        };
        let block = ui::panel(title, true);
        let inner = block.inner(r);
        f.render_widget(Clear, r);
        f.render_widget(block, r);
        let w = inner.width as usize;
        let mut lines = vec![Line::from(self.q.spans(true, "type to search"))];
        let room = (inner.height as usize).saturating_sub(2);
        let first = self.sel.saturating_sub(room.saturating_sub(1));
        for (n, &i) in shown.iter().enumerate().skip(first).take(room) {
            let it = &self.items[i];
            let on = self.on.contains(&(it.kind, it.id.clone()));
            let mark = match self.dim {
                Dim::Period => "  ".to_string(),
                _ => {
                    if on {
                        "◉ ".into()
                    } else {
                        "○ ".into()
                    }
                }
            };
            let cost = if it.cost > 0.0 {
                crate::timeline::money(it.cost)
            } else {
                String::new()
            };
            let pad = "  ".repeat(it.depth as usize);
            let name_w = w.saturating_sub(pad.len() + 4 + 10);
            let mut l = Line::from(vec![
                Span::styled(if n == self.sel { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
                Span::raw(pad),
                Span::styled(mark, ui::fg(c::ACCENT())),
                Span::styled(
                    ui::pad(&ui::trunc(&it.label, name_w), name_w),
                    if it.depth == 0 {
                        ui::bold()
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(format!("{cost:>8}"), ui::muted()),
            ]);
            if n == self.sel {
                l = l.style(ui::selected());
            }
            lines.push(l);
        }
        if shown.is_empty() {
            lines.push(Line::from(Span::styled("  nothing matches", ui::faint())));
        }
        f.render_widget(Paragraph::new(lines), inner);
        let keys = match self.dim {
            Dim::Period => vec![("↑↓", "pick"), ("enter", "use"), ("esc", "back")],
            _ => vec![
                ("space", "pick several"),
                ("enter", "apply"),
                ("esc", "back"),
            ],
        };
        let foot = Rect::new(r.x + 2, r.y + r.height - 1, r.width.saturating_sub(4), 1);
        f.render_widget(Paragraph::new(ui::hints(&keys)), foot);
    }
}

/// A picker's answer: the filters it sets.
pub(crate) fn apply(d: &mut Dash, p: Picker) {
    let shown = p.shown();
    let cur = shown.get(p.sel).map(|&i| p.items[i].clone());
    if p.dim == Dim::Period {
        match cur {
            Some(it) if it.kind == 'C' => {
                d.mode = super::Mode::UsageCal(Cal::new(d.usage.f.range()));
                return;
            }
            Some(it) => {
                d.usage.f.period = period_index(&it.id);
                d.usage.f.custom = None;
            }
            None => {}
        }
        changed(d);
        return;
    }
    // nothing ticked: the one under the cursor alone
    let mut on = p.on.clone();
    if p.items
        .iter()
        .all(|i| !on.contains(&(i.kind, i.id.clone())))
    {
        if let Some(it) = cur {
            on.insert((it.kind, it.id));
        }
    }
    let pick = |k: char| -> Vec<String> {
        on.iter()
            .filter(|(x, _)| *x == k)
            .map(|(_, id)| id.clone())
            .collect()
    };
    match p.dim {
        Dim::Agents => {
            d.usage.f.agents = pick('a');
            d.usage.f.accounts = pick('c');
        }
        Dim::Machines => d.usage.f.hosts = pick('h'),
        Dim::Projects => d.usage.f.projects = pick('p'),
        Dim::Period | Dim::Timeline => {}
    }
    changed(d);
}

// ------------------------------------------------------------------ the calendar

/// A from–to picker over a month grid.
#[derive(Clone, Debug)]
pub(crate) struct Cal {
    /// the day under the cursor (local day number)
    pub at: i64,
    /// the first end, once picked
    pub from: Option<i64>,
}

impl Cal {
    pub fn new(r: Range) -> Cal {
        Cal {
            at: r.to,
            from: None,
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> Option<(i64, i64)> {
        let today = ui::local_day(crate::cache::now());
        let month = |at: i64, n: i32| -> i64 {
            let (y, m, d) = ui::civil(at);
            let total = y as i32 * 12 + m as i32 - 1 + n;
            let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            let last = crate::usage::parse::days_from_civil(
                ny as i64 + (nm == 12) as i64,
                if nm == 12 { 1 } else { nm as u32 + 1 },
                1,
            ) - 1;
            (crate::usage::parse::days_from_civil(ny as i64, nm as u32, 1) + d as i64 - 1).min(last)
        };
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => self.at -= 1,
            KeyCode::Right | KeyCode::Char('l') => self.at += 1,
            KeyCode::Up | KeyCode::Char('k') => self.at -= 7,
            KeyCode::Down | KeyCode::Char('j') => self.at += 7,
            KeyCode::PageUp => self.at = month(self.at, -1),
            KeyCode::PageDown => self.at = month(self.at, 1),
            KeyCode::Char('t') => self.at = today,
            KeyCode::Enter => match self.from {
                None => self.from = Some(self.at),
                Some(a) => return Some((a.min(self.at), a.max(self.at))),
            },
            _ => {}
        }
        self.at = self.at.min(today);
        None
    }

    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let r = ui::centered(area, 34.min(area.width), 13.min(area.height));
        ui::clear(f, r);
        let title = match self.from {
            None => " From (enter) ",
            Some(_) => " To (enter) ",
        };
        let block = ui::panel(title, true);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let (y, m, _) = ui::civil(self.at);
        let first = crate::usage::parse::days_from_civil(y, m, 1);
        let next = if m == 12 {
            crate::usage::parse::days_from_civil(y + 1, 1, 1)
        } else {
            crate::usage::parse::days_from_civil(y, m + 1, 1)
        };
        const MONTHS: [&str; 12] = [
            "January",
            "February",
            "March",
            "April",
            "May",
            "June",
            "July",
            "August",
            "September",
            "October",
            "November",
            "December",
        ];
        let mut lines = vec![
            Line::from(Span::styled(
                format!("{} {y}", MONTHS[(m as usize + 11) % 12]),
                ui::bold(),
            ))
            .centered(),
            Line::from(Span::styled("Mo Tu We Th Fr Sa Su", ui::faint())),
        ];
        let lead = (first + 3).rem_euclid(7) as usize;
        let today = ui::local_day(crate::cache::now());
        let mut row: Vec<Span> = vec![Span::raw("   ".repeat(lead))];
        let (lo, hi) = match self.from {
            Some(a) => (a.min(self.at), a.max(self.at)),
            None => (self.at, self.at),
        };
        for day in first..next {
            let (_, _, dd) = ui::civil(day);
            let mut st = if day > today {
                ui::faint()
            } else {
                Style::new()
            };
            if (lo..=hi).contains(&day) {
                st = ui::selected();
            }
            if day == self.at {
                st = Style::new()
                    .fg(c::BASE())
                    .bg(c::ACCENT())
                    .add_modifier(Modifier::BOLD);
            }
            row.push(Span::styled(format!("{dd:>2}"), st));
            row.push(Span::raw(" "));
            if (day + 3).rem_euclid(7) == 6 {
                lines.push(Line::from(std::mem::take(&mut row)));
            }
        }
        if !row.is_empty() {
            lines.push(Line::from(row));
        }
        f.render_widget(Paragraph::new(lines), inner);
        let foot = Rect::new(r.x + 1, r.y + r.height - 1, r.width.saturating_sub(2), 1);
        f.render_widget(
            Paragraph::new(ui::hints(&[
                ("←→↑↓", "day"),
                ("pgup/dn", "month"),
                ("esc", "back"),
            ])),
            foot,
        );
    }
}

// ------------------------------------------------------------------ typed

/// `/`: the filters typed — a period (`30d`, `this-week`, `2026-09-01..`),
/// agents (by any name), machines, projects, accounts (`acct:` an id,
/// email or nickname), each word one of them. What it can't place is said.
pub(crate) fn parse_query(d: &Dash, q: &str) -> Result<Filters, String> {
    let mut f = Filters {
        period: d.usage.f.period,
        custom: d.usage.f.custom,
        ..Default::default()
    };
    let opts = d
        .usage
        .data
        .as_ref()
        .map(|x| x.options.clone())
        .unwrap_or_default();
    let hosts: Vec<String> = d.cfg.fleet();
    for w in q.split_whitespace() {
        let wl = w.to_lowercase();
        if let Some(p) = PERIODS.iter().position(|(x, _)| *x == wl) {
            f.period = p;
            f.custom = None;
            continue;
        }
        if let Some(a) = wl
            .strip_prefix("acct:")
            .or_else(|| wl.strip_prefix("account:"))
        {
            match crate::usage::accounts::Book::load().find(&d.cfg, a) {
                Some(id) => f.accounts.push(id),
                None => return Err(format!("no account {a}")),
            }
            continue;
        }
        if let Some(p) = wl.strip_prefix("project:") {
            match opts
                .iter()
                .find(|o| o.kind == 'p' && o.label.to_lowercase() == p)
            {
                Some(o) => f.projects.push(o.id.clone()),
                None => return Err(format!("no project {p}")),
            }
            continue;
        }
        if let Some(r) = Range::parse(&wl)
            .filter(|_| wl.contains("..") || wl.chars().next().is_some_and(|c| c.is_ascii_digit()))
        {
            if wl.contains("..") || wl.len() == 10 {
                f.custom = Some((r.from, r.to));
            } else if let Some(p) = PERIODS.iter().position(|(x, _)| *x == wl) {
                f.period = p;
            } else {
                f.custom = Some((r.from, r.to));
            }
            continue;
        }
        if hosts.iter().any(|h| h.to_lowercase() == wl) {
            f.hosts.push(
                hosts
                    .iter()
                    .find(|h| h.to_lowercase() == wl)
                    .cloned()
                    .unwrap_or_default(),
            );
            continue;
        }
        if let Ok(a) = crate::agents::resolve(&wl) {
            f.agents.push(a.to_string());
            continue;
        }
        if let Some(o) = opts
            .iter()
            .find(|o| o.kind == 'p' && o.label.to_lowercase() == wl)
        {
            f.projects.push(o.id.clone());
            continue;
        }
        if let Some(o) = opts
            .iter()
            .find(|o| o.kind == 'c' && o.label.to_lowercase().contains(&wl))
        {
            f.accounts.push(o.id.clone());
            continue;
        }
        return Err(format!(
            "{w}? (a period, agent, machine, project or acct:…)"
        ));
    }
    Ok(f)
}

/// The typed query's input.
pub(crate) fn ask(d: &mut Dash) {
    let mut now: Vec<String> = Vec::new();
    let f = &d.usage.f;
    if f.custom.is_none() {
        now.push(PERIODS[f.period % PERIODS.len()].0.to_string());
    }
    now.extend(f.agents.iter().cloned());
    now.extend(f.hosts.iter().cloned());
    d.mode = super::Mode::Input(super::forms::Input::new(
        "Usage of…".into(),
        "claude b31 aleph 30d · acct:work · 2026-09-01..2026-10-15 · enter",
        now.join(" "),
        super::forms::For::UsageQuery,
    ));
}

/// Keys the filters take in the Usage view (true: taken).
pub(crate) fn key(d: &mut Dash, k: &KeyEvent) -> bool {
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        return false;
    }
    match k.code {
        KeyCode::Char('t') => open(d, Dim::Period),
        KeyCode::Char('a') => open(d, Dim::Agents),
        KeyCode::Char('m') => open(d, Dim::Machines),
        KeyCode::Char('p') => open(d, Dim::Projects),
        KeyCode::Char('/') => ask(d),
        KeyCode::Char('x')
            if d.usage.f.narrowed() || d.usage.f.custom.is_some() || d.usage.only.is_some() =>
        {
            d.usage.f.clear();
            d.usage.f.custom = None;
            changed(d);
            d.toast(super::Level::Info, "usage of everything");
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periods_and_filters() {
        let f = Filters::default();
        assert_eq!(f.period_label(), "7 days");
        assert!(!f.narrowed());
        let g = Filters {
            custom: Some((20_700, 20_710)),
            agents: vec!["claude".into()],
            ..Default::default()
        };
        assert_eq!(g.range(), Range::between(20_700, 20_710));
        assert!(g.narrowed());
        assert_eq!(g.filter().agents, vec!["claude".to_string()]);
    }

    #[test]
    fn the_calendar_moves_and_picks_two_ends() {
        let today = ui::local_day(crate::cache::now());
        let mut c = Cal::new(Range::days(7));
        let k = |code| KeyEvent::new(code, KeyModifiers::NONE);
        c.key(k(KeyCode::Right));
        assert_eq!(c.at, today, "never past today");
        c.key(k(KeyCode::Up));
        assert_eq!(c.at, today - 7);
        assert_eq!(c.key(k(KeyCode::Enter)), None);
        c.key(k(KeyCode::PageUp));
        let got = c.key(k(KeyCode::Enter)).unwrap();
        assert!(got.0 < got.1 && got.1 == today - 7);
    }

    #[test]
    fn pickers_search_and_pick_several() {
        let opts = vec![
            Opt {
                kind: 'a',
                id: "claude".into(),
                label: "Claude Code".into(),
                cost: 3.0,
                ..Default::default()
            },
            Opt {
                kind: 'c',
                id: "aaaa".into(),
                label: "me@work.com · Max 20x".into(),
                parent: "claude".into(),
                cost: 2.0,
                ..Default::default()
            },
            Opt {
                kind: 'a',
                id: "codex".into(),
                label: "Codex".into(),
                cost: 1.0,
                ..Default::default()
            },
        ];
        let mut p = Picker {
            dim: Dim::Agents,
            items: items_of(Dim::Agents, &opts),
            q: Field::new(""),
            sel: 0,
            on: BTreeSet::new(),
        };
        assert_eq!(p.items[1].depth, 1, "an account under its agent");
        let k = |code| KeyEvent::new(code, KeyModifiers::NONE);
        for ch in "work".chars() {
            p.key(k(KeyCode::Char(ch)));
        }
        assert_eq!(p.shown(), vec![1]);
        p.key(k(KeyCode::Char(' ')));
        assert!(p.on.contains(&('c', "aaaa".into())));
        assert_eq!(p.key(k(KeyCode::Enter)), Res::Submit);
        let per = items_of(Dim::Period, &[]);
        assert_eq!(per.last().unwrap().kind, 'C');
        assert_eq!(per.len(), PERIODS.len() + 1);
    }
}
