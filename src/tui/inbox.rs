//! Inbox: what needs you, then everything that happened — events and the
//! sessions you started (a launch joins its `started` event, so one row has
//! both what happened and the recipe to start it again).
//!
//! * **Now** — live from the fleet, not from history: sessions waiting for
//!   your answer, sessions stopped at a usage limit, machines that dropped
//!   off. Each says what you can do about it, right there.
//! * **The timeline** — every machine's events newest first, by day; a run
//!   of identical events folds into one row (`×5`). Each row says what's true
//!   *now* (still waiting / answered, still down / back up), so stale items
//!   look stale.
//! * Dots mark what's unread or needs you: on rows, on the filter chips that
//!   hold them, in the summary, and on the tab.
//! * One aligned table, grouped by machine (or day, agent, type, session:
//!   `G`). Every row says whether its agent is still alive on its machine,
//!   from the fleet's live poll: the logo breathes in the state's colour
//!   while it works or waits, and turns into a grey ○ once it's gone. Live
//!   rows come first; ended ones are dimmed (`e` hides them).
//! * The archive: `x` puts a row there (and brings it back from there), a
//!   launch whose session has ended goes by itself after `inbox.auto_archive`,
//!   `A` shows it, `D` deletes for good.
//!
//! Keys: ↑↓ move · enter the session in Focus (or its machine) · a attach ·
//! o open it in a new terminal tab · X close it · y approve · s reply · Q
//! queue a message for when it's free · r start it again · P save as a
//! preset · x archive · D delete for good · A the archive · e ended rows ·
//! G group · v its layout (overview, cards, table) · u mark all read · U dismiss… (pick kinds; archive or delete) ·
//! c copy (a launch: its command) · f kind · g
//! agent · m machine · / search · R reload · J the session journal.

use super::fleet::RowId;
use super::{forms, Dash, Handoff, Level, Mode, Pending, View};
use crate::events::{self, Event, Kind};
use crate::fleet::Conn;
use crate::herdr::{Herdr, Session};
use crate::history::Entry;
use crate::ui::{self, c};
use crate::{icons, theme};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Layout as Layout_;
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// The kind filter (`f` cycles).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Show {
    #[default]
    All,
    Attention,
    Finished,
    Problems,
    Automation,
    /// sessions you started (the old History)
    Launches,
}

impl Show {
    const ALL: [Show; 6] = [
        Show::All,
        Show::Attention,
        Show::Finished,
        Show::Problems,
        Show::Automation,
        Show::Launches,
    ];
    fn name(self) -> &'static str {
        match self {
            Show::All => "All",
            Show::Attention => "Needs you",
            Show::Finished => "Finished",
            Show::Problems => "Problems",
            Show::Automation => "Automation",
            Show::Launches => "Launches",
        }
    }
    fn keeps_item(self, it: &Item) -> bool {
        match self {
            Show::Launches => it.launch.is_some(),
            s => s.keeps(it.e.kind),
        }
    }
    fn keeps(self, k: Kind) -> bool {
        match self {
            Show::All => true,
            Show::Attention => k == Kind::Blocked,
            Show::Finished => matches!(k, Kind::Done | Kind::Gone),
            Show::Problems => matches!(k, Kind::Error | Kind::Limit),
            Show::Automation => matches!(k, Kind::Auto | Kind::Task | Kind::Heal),
            Show::Launches => k == Kind::New,
        }
    }
}

mod look;

/// How the Inbox is laid out (`L` cycles; `inbox.layout` is where it starts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Layout {
    /// KPI tiles, a panel per group, the day's activity
    #[default]
    Overview,
    /// live sessions as cards, the ended ones on a shelf
    Cards,
    /// one table, two lines a row
    Table,
}

impl Layout {
    const ALL: [Layout; 3] = [Layout::Overview, Layout::Cards, Layout::Table];
    fn name(self) -> &'static str {
        match self {
            Layout::Overview => "overview",
            Layout::Cards => "cards",
            Layout::Table => "table",
        }
    }
    fn parse(s: &str) -> Layout {
        Layout::ALL
            .into_iter()
            .find(|l| l.name() == s)
            .unwrap_or_default()
    }
}

/// How the timeline is grouped (`G` cycles; `inbox.group` is where it starts).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Group {
    #[default]
    Machine,
    Day,
    Agent,
    Type,
    Session,
}

impl Group {
    const ALL: [Group; 5] = [
        Group::Machine,
        Group::Day,
        Group::Agent,
        Group::Type,
        Group::Session,
    ];
    fn name(self) -> &'static str {
        match self {
            Group::Machine => "machine",
            Group::Day => "day",
            Group::Agent => "agent",
            Group::Type => "type",
            Group::Session => "session",
        }
    }
    fn parse(s: &str) -> Group {
        Group::ALL
            .into_iter()
            .find(|g| g.name() == s)
            .unwrap_or_default()
    }
}

/// Is a row's agent still alive on its machine? From the fleet's live poll:
/// its session's state now, or why nobody can say.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    Needs,
    Limit,
    Working,
    Done,
    Idle,
    /// a machine's own event, and the machine answers now
    Up,
    /// the machine hasn't answered yet (or it's paused)
    Checking,
    /// its machine doesn't answer, so nobody knows
    Down,
    /// gone from its machine (or the machine left the fleet)
    Ended,
}

impl Life {
    fn alive(self) -> bool {
        matches!(
            self,
            Life::Needs | Life::Limit | Life::Working | Life::Done | Life::Idle
        )
    }
    /// What needs you first, the dead last.
    fn rank(self) -> u8 {
        self as u8
    }
    /// (glyph, label, colour, breathing period in ms; 0 holds still)
    fn look(self, t0: Instant) -> (&'static str, &'static str, Color, u64) {
        match self {
            Life::Needs => (crate::look::needs(), "needs you", c::NEEDS(), 900),
            Life::Limit => ("◷", "at limit", c::LIMIT(), 2400),
            Life::Working => (ui::spinner(t0), "working", c::WORKING(), 1400),
            Life::Done => ("●", "done", c::DONE(), 0),
            Life::Idle => ("●", "idle", c::IDLE(), 0),
            Life::Up => ("●", "up", c::GREEN(), 0),
            Life::Checking => ("…", "checking", c::FAINT(), 0),
            Life::Down => ("✕", "machine down", c::RED(), 0),
            Life::Ended => ("○", "ended", c::FAINT(), 0),
        }
    }
}

#[derive(Default)]
pub(crate) struct State {
    rows: Vec<Event>,
    sel: usize,
    scroll: usize,
    loaded: Option<Instant>,
    /// the Inbox's files as last read, and when they were last looked at
    stamp: events::Stamp,
    looked: Option<Instant>,
    /// unix time the inbox was last looked at (persisted)
    pub(crate) seen: u64,
    pub(crate) show: Show,
    host: Option<String>,
    /// only this agent (`g`)
    agent: Option<String>,
    search: String,
    /// sessions you started (history.jsonl), newest first
    launches: Vec<Entry>,
    /// events and launches joined (rebuilt when either is reloaded)
    merged: Vec<Item>,
    pub searching: bool,
    /// archived event ids (persisted; the API calls them dismissed)
    pub(crate) dismissed: HashSet<String>,
    /// archived launches, and what was brought back from the archive (which
    /// the automatic archiving then leaves alone); persisted
    archived: HashSet<String>,
    kept: HashSet<String>,
    /// a session's folder as its launch or `started` event saw it
    dirs: HashMap<(String, String), String>,
    group: Group,
    layout: Layout,
    /// cards across, as last drawn (↑↓ move a row of cards)
    card_cols: usize,
    /// hide rows whose session has ended (`e`)
    hide_ended: bool,
    /// show the archive instead (`A`)
    archive_view: bool,
    /// an ended launch is archived after this many seconds (None: never)
    auto_after: Option<u64>,
    /// when the inbox was opened with something unread (it's read a moment later)
    opened: Option<Instant>,
    /// finished turns' numbers, worked out when shown (`turncost.rs`)
    pub(crate) turns: crate::turncost::Cache,
}

/// One shown item: something live from the fleet (`now`), or a run of `n`
/// identical events (the newest stands for them; `first` is the oldest).
#[derive(Clone, Debug)]
struct Item {
    e: Event,
    n: usize,
    first: u64,
    now: bool,
    /// how it was started (a launch from history.jsonl)
    launch: Option<Entry>,
    /// it's in the event log (a launch on its own isn't: never unread)
    logged: bool,
    /// its agent now (set as the list is built)
    life: Life,
    /// the group it's listed under (empty: Needs you now)
    group: String,
}

fn id(e: &Event) -> String {
    events::key(e)
}

/// A session's events, oldest first (Focus's timeline).
pub(crate) fn of_session(d: &Dash, host: &str, name: &str) -> Vec<Event> {
    let mut v: Vec<Event> = d
        .inbox
        .rows
        .iter()
        .filter(|e| e.host == host && e.name == name)
        .cloned()
        .collect();
    v.reverse();
    v
}

pub(crate) fn init(d: &mut Dash) {
    read_marks(d);
    apply_settings(d);
    reload(d);
}

/// The marks as they are on disk: the hub archives what's answered or expired
/// there, and Raycast or another dashboard may have moved them too.
fn read_marks(d: &mut Dash) {
    d.inbox.stamp = events::stamp();
    d.inbox.seen = events::seen();
    d.inbox.dismissed = events::dismissed();
    (d.inbox.archived, d.inbox.kept) = events::marks();
}

/// Pick up what other processes changed: new events from the log, and the
/// marks, so a question the hub archived leaves the list without a restart.
pub(crate) fn look_again(d: &mut Dash) {
    let now = events::stamp();
    if now == d.inbox.stamp {
        return;
    }
    let log_moved = now[0] != d.inbox.stamp[0];
    read_marks(d);
    if log_moved {
        reload(d);
    }
}

/// `inbox.group`, `inbox.hide_ended` and `inbox.auto_archive`, as set now.
pub(crate) fn apply_settings(d: &mut Dash) {
    d.inbox.group = Group::parse(&d.cfg.get_or("inbox.group"));
    d.inbox.layout = Layout::parse(&d.cfg.get_or("inbox.layout"));
    d.inbox.hide_ended = d.cfg.flag("inbox.hide_ended");
    d.inbox.auto_after = match d.cfg.get_or("inbox.auto_archive").trim() {
        "off" | "never" | "" | "0" => None,
        v => crate::log::parse_duration(v).map(|x| x.as_secs()),
    };
}

#[cfg(test)]
pub(crate) fn set_rows(d: &mut Dash, v: Vec<Event>) {
    d.inbox.rows = v;
    rebuild(d);
    d.inbox.loaded = Some(Instant::now());
}

#[cfg(test)]
pub(crate) fn set_launches(d: &mut Dash, v: Vec<Entry>) {
    d.inbox.launches = v;
    rebuild(d);
}

fn reload(d: &mut Dash) {
    let mut v = events::load_inbox(1000);
    v.sort_by_key(|e| std::cmp::Reverse(e.t));
    d.inbox.rows = v;
    let mut l = crate::history::load(500);
    l.sort_by_key(|e| std::cmp::Reverse(e.t));
    d.inbox.launches = l;
    rebuild(d);
    d.inbox.loaded = Some(Instant::now());
}

/// The finished turns the Inbox lists (`inbox.finished`): none, the ones
/// not seen on a dashboard's screen, those and long, or all — one row a
/// session a day, the newest, saying how many (pure — tested).
pub(crate) fn finished_shown(rows: &[Event], mode: &str, done_min: u64) -> Vec<Event> {
    let keep = |e: &Event| match mode {
        "all" => true,
        "unseen" => !e.seen,
        "unseen-long" => !e.seen && e.took >= done_min,
        _ => false,
    };
    let mut out: Vec<Event> = Vec::new();
    let mut count: HashMap<(String, String, i64), (usize, usize)> = HashMap::new();
    for e in rows {
        if e.kind != Kind::Done {
            out.push(e.clone());
            continue;
        }
        if !keep(e) {
            continue;
        }
        let k = (e.host.clone(), e.name.clone(), crate::ui::local_day(e.t));
        match count.get_mut(&k) {
            // rows are newest first: the first one stands for the day
            Some((_, n)) => *n += 1,
            None => {
                count.insert(k, (out.len(), 1));
                out.push(e.clone());
            }
        }
    }
    for (i, n) in count.into_values() {
        if n > 1 {
            out[i].title = format!("{n}× · {}", out[i].title);
        }
    }
    out
}

/// The joined timeline and the folder of each session, after a load.
fn rebuild(d: &mut Dash) {
    let done_min =
        crate::log::parse_duration(&d.cfg.get_or("notify.done_min")).map_or(120, |x| x.as_secs());
    let shown = finished_shown(&d.inbox.rows, &d.cfg.get_or("inbox.finished"), done_min);
    d.inbox.merged = merged(&shown, &d.inbox.launches);
    let mut dirs: HashMap<(String, String), String> = HashMap::new();
    // oldest first, so the newest start of a name wins
    for e in d.inbox.rows.iter().rev() {
        let t = e.title.trim();
        if e.kind == Kind::New && (t.starts_with('/') || t.starts_with('~')) {
            dirs.insert((e.host.clone(), e.name.clone()), t.to_string());
        }
    }
    for l in d.inbox.launches.iter().rev() {
        if !l.dir.is_empty() {
            dirs.insert((l.host.clone(), l.name.clone()), l.dir.clone());
        }
    }
    d.inbox.dirs = dirs;
}

fn matches_search(d: &Dash, e: &Event) -> bool {
    matches_text(
        d,
        &format!("{} {} {} {}", e.host, e.name, e.title, e.agent),
        &e.host,
        &e.name,
    )
}

/// The search matches this text, or the session's note and tags.
fn matches_text(d: &Dash, hay: &str, host: &str, name: &str) -> bool {
    d.inbox.search.is_empty() || {
        let q = d.inbox.search.to_lowercase();
        let note = crate::notes::search_text(host, name);
        format!("{hay} {note}").to_lowercase().contains(&q)
    }
}

/// The machine / agent / search filters (the archive is `archived`'s).
fn kept(d: &Dash, e: &Event) -> bool {
    d.inbox.host.as_deref().is_none_or(|h| h == e.host)
        && d.inbox.agent.as_deref().is_none_or(|a| a == e.agent)
        && matches_search(d, e)
}

/// A whole item passes the machine / agent / search filters (a launch also
/// matches on its folder, prompt and arguments).
fn kept_item(d: &Dash, it: &Item) -> bool {
    let Some(l) = &it.launch else {
        return kept(d, &it.e);
    };
    d.inbox.host.as_deref().is_none_or(|h| h == l.host)
        && d.inbox.agent.as_deref().is_none_or(|a| a == l.agent)
        && matches_text(
            d,
            &format!(
                "{} {} {} {} {} {} {}",
                l.host,
                l.name,
                l.agent,
                l.dir,
                l.prompt.as_deref().unwrap_or(""),
                l.args.join(" "),
                it.e.title
            ),
            &l.host,
            &l.name,
        )
}

fn same(a: &Event, b: &Event) -> bool {
    a.host == b.host && a.name == b.name && a.kind == b.kind && a.title == b.title
}

/// What needs you right now, from the fleet's live state: sessions waiting
/// for an answer, sessions stopped at a limit, machines that dropped off.
fn live(d: &Dash) -> Vec<Item> {
    let now = crate::cache::now();
    let mut out: Vec<Item> = Vec::new();
    for h in &d.fleet.hosts {
        if d.inbox.host.as_deref().is_some_and(|x| x != h.alias) {
            continue;
        }
        for s in &h.sessions {
            let kind = if s.blocked() {
                Kind::Blocked
            } else if s.limit.is_some() {
                Kind::Limit
            } else {
                continue;
            };
            let e = Event {
                t: s.since.unwrap_or(now),
                host: h.alias.clone(),
                name: s.name.clone(),
                kind,
                title: match (&s.limit, kind) {
                    (Some(r), Kind::Limit) if !r.is_empty() => format!("resets {r}"),
                    _ => s.title.clone(),
                },
                agent: s.agent.clone(),
                ..Default::default()
            };
            out.push(Item {
                first: e.t,
                e,
                n: 1,
                now: true,
                launch: None,
                logged: false,
                life: if kind == Kind::Blocked {
                    Life::Needs
                } else {
                    Life::Limit
                },
                group: String::new(),
            });
        }
        if let Conn::Down(err) = &h.conn {
            if !crate::fleet::not_running(err) {
                // since when: the last time it answered
                let t = h.fetched.unwrap_or(now);
                let e = Event {
                    t,
                    host: h.alias.clone(),
                    name: String::new(),
                    kind: Kind::Error,
                    title: err.clone(),
                    agent: String::new(),
                    ..Default::default()
                };
                out.push(Item {
                    first: t,
                    e,
                    n: 1,
                    now: true,
                    launch: None,
                    logged: false,
                    life: Life::Down,
                    group: String::new(),
                });
            }
        }
    }
    out.retain(|i| {
        matches_search(d, &i.e)
            && d.inbox.show.keeps_item(i)
            && d.inbox.agent.as_deref().is_none_or(|a| a == i.e.agent)
    });
    // what blocks work first: questions, then limits, then machines
    out.sort_by_key(|i| {
        (
            match i.e.kind {
                Kind::Blocked => 0,
                Kind::Limit => 1,
                _ => 2,
            },
            i.e.t,
        )
    });
    out
}

/// Does launch `l` belong to `started` event `e`: the same session, and the
/// event seen from 5 s before the launch was written to 120 s after it.
fn joins(l: &Entry, e: &Event) -> bool {
    e.kind == Kind::New
        && l.host == e.host
        && l.name == e.name
        && e.t + 5 >= l.t
        && e.t <= l.t + 120
}

/// Events and launches as one timeline, newest first: a launch joins its
/// `started` event; a run of identical events (same machine, session, kind
/// and text) folds into its newest.
fn merged(rows: &[Event], launches: &[Entry]) -> Vec<Item> {
    let mut used = vec![false; launches.len()];
    // launches by session, so the join is one lookup per `started` event
    let mut by: std::collections::HashMap<(&str, &str), Vec<usize>> = Default::default();
    for (i, l) in launches.iter().enumerate() {
        by.entry((l.host.as_str(), l.name.as_str()))
            .or_default()
            .push(i);
    }
    let mut all: Vec<Item> = rows
        .iter()
        .map(|e| {
            let launch = (e.kind == Kind::New)
                .then(|| by.get(&(e.host.as_str(), e.name.as_str())))
                .flatten()
                .and_then(|ix| {
                    ix.iter()
                        .copied()
                        .find(|i| !used[*i] && joins(&launches[*i], e))
                })
                .map(|i| {
                    used[i] = true;
                    launches[i].clone()
                });
            Item {
                e: e.clone(),
                n: 1,
                first: e.t,
                now: false,
                launch,
                logged: true,
                life: Life::Checking,
                group: String::new(),
            }
        })
        .collect();
    // launches whose event isn't in the log (older, or started elsewhere)
    for (l, _) in launches.iter().zip(&used).filter(|(_, u)| !**u) {
        all.push(Item {
            e: Event {
                t: l.t,
                host: l.host.clone(),
                name: l.name.clone(),
                kind: Kind::New,
                title: l.dir.clone(),
                agent: l.agent.clone(),
                ..Default::default()
            },
            n: 1,
            first: l.t,
            now: false,
            launch: Some(l.clone()),
            logged: false,
            life: Life::Checking,
            group: String::new(),
        });
    }
    all.sort_by_key(|i| std::cmp::Reverse(i.e.t));
    all
}

/// Is the agent behind this event still alive on its machine?
fn life(d: &Dash, e: &Event) -> Life {
    let Some(h) = d.fleet.host(&e.host) else {
        return Life::Ended;
    };
    if h.paused {
        return Life::Checking;
    }
    if e.name.is_empty() {
        return match &h.conn {
            Conn::Up => Life::Up,
            Conn::Down(_) => Life::Down,
            Conn::Unknown => Life::Checking,
        };
    }
    match &h.conn {
        // herdr stopped there: its sessions went with it
        Conn::Down(x) if crate::fleet::not_running(x) => return Life::Ended,
        Conn::Down(_) => return Life::Down,
        Conn::Unknown => return Life::Checking,
        Conn::Up => {}
    }
    match h
        .sessions
        .iter()
        .find(|s| s.name == e.name || s.display() == e.name)
    {
        None => Life::Ended,
        Some(s) if s.limit.is_some() => Life::Limit,
        Some(s) if s.blocked() => Life::Needs,
        Some(s) => match s.status.as_str() {
            "working" => Life::Working,
            "done" => Life::Done,
            _ => Life::Idle,
        },
    }
}

/// A session's start: its launch, or its `started` event.
fn is_start(it: &Item) -> bool {
    it.launch.is_some() || it.e.kind == Kind::New
}

/// The ids a row is archived by: its events (the whole folded run) and its
/// launch.
fn ids_of(d: &Dash, it: &Item) -> Vec<String> {
    let mut v: Vec<String> = if it.logged {
        d.inbox
            .rows
            .iter()
            .filter(|x| same(x, &it.e))
            .map(id)
            .collect()
    } else {
        Vec::new()
    };
    if let Some(l) = &it.launch {
        v.push(crate::history::id(l));
    }
    v
}

/// In the archive: put there, or a start whose session ended longer ago than
/// `inbox.auto_archive` (unless it was brought back).
fn archived(d: &Dash, it: &Item, life: Life) -> bool {
    if it.now {
        return false;
    }
    if it.logged && d.inbox.dismissed.contains(&id(&it.e)) {
        return true;
    }
    let launch = it.launch.as_ref().map(crate::history::id);
    if launch
        .as_ref()
        .is_some_and(|l| d.inbox.archived.contains(l))
    {
        return true;
    }
    life == Life::Ended
        && is_start(it)
        && d.inbox
            .auto_after
            .is_some_and(|a| crate::cache::now().saturating_sub(it.e.t) >= a)
        && !d.inbox.kept.contains(&id(&it.e))
        && !launch.is_some_and(|l| d.inbox.kept.contains(&l))
}

/// What the list shows of the timeline: the filters, the archive (or only
/// it), ended rows unless hidden; a run of identical events folds into one.
fn timeline(d: &Dash) -> Vec<Item> {
    let mut out: Vec<Item> = Vec::new();
    for it in d
        .inbox
        .merged
        .iter()
        .filter(|it| d.inbox.show.keeps_item(it) && kept_item(d, it))
    {
        let lf = life(d, &it.e);
        if archived(d, it, lf) != d.inbox.archive_view
            || (d.inbox.hide_ended && !d.inbox.archive_view && lf == Life::Ended)
        {
            continue;
        }
        match out.last_mut() {
            Some(p) if p.launch.is_none() && it.launch.is_none() && same(&p.e, &it.e) => {
                p.n += 1;
                p.first = it.e.t;
            }
            _ => {
                let mut it = it.clone();
                it.life = lf;
                out.push(it);
            }
        }
    }
    grouped(d, out)
}

/// Kinds by weight, for grouping by type.
fn kind_rank(k: Kind) -> u8 {
    match k {
        Kind::Blocked => 0,
        Kind::Limit => 1,
        Kind::Error => 2,
        Kind::Done => 3,
        Kind::Gone => 4,
        Kind::New => 5,
        Kind::Remote => 6,
        Kind::Match => 7,
        Kind::Auto => 8,
        Kind::Task => 9,
        Kind::Heal => 10,
        Kind::HostUp => 11,
    }
}

fn group_key(d: &Dash, it: &Item) -> String {
    let e = &it.e;
    match d.inbox.group {
        Group::Machine => e.host.clone(),
        Group::Day => ui::local_day(e.t).to_string(),
        Group::Agent => agent_of(d, e),
        Group::Type => kind_rank(e.kind).to_string(),
        Group::Session if e.name.is_empty() => e.host.clone(),
        Group::Session => format!("{}:{}", e.host, e.name),
    }
}

/// The timeline in its groups. By day it stays newest first; otherwise each
/// group lists its live rows first (what needs you, then working, …, the
/// ended last), newest first within each. Machines come in fleet order,
/// types by weight, agents and sessions by their liveliest row.
fn grouped(d: &Dash, mut v: Vec<Item>) -> Vec<Item> {
    for it in &mut v {
        it.group = group_key(d, it);
    }
    if d.inbox.group == Group::Day {
        return v;
    }
    let fleet_at = |h: &str| {
        d.fleet
            .hosts
            .iter()
            .position(|x| x.alias == h)
            .unwrap_or(usize::MAX)
    };
    // a group's place: (fixed order, its liveliest row, its newest row)
    let mut place: HashMap<String, (usize, u8, std::cmp::Reverse<u64>)> = HashMap::new();
    for it in &v {
        let fixed = match d.inbox.group {
            Group::Machine => fleet_at(&it.e.host),
            Group::Type => kind_rank(it.e.kind) as usize,
            _ => 0,
        };
        let p = place
            .entry(it.group.clone())
            .or_insert((fixed, u8::MAX, std::cmp::Reverse(0)));
        p.1 = p.1.min(it.life.rank());
        p.2 = p.2.min(std::cmp::Reverse(it.e.t));
    }
    v.sort_by(|a, b| {
        place[&a.group]
            .cmp(&place[&b.group])
            .then_with(|| a.group.cmp(&b.group))
            .then_with(|| a.life.rank().cmp(&b.life.rank()))
            .then_with(|| b.e.t.cmp(&a.e.t))
    });
    v
}

/// Everything shown, in order: the live items, then the timeline (cards:
/// the live sessions, then the ended ones).
fn items(d: &Dash) -> Vec<Item> {
    if d.inbox.layout == Layout::Cards && !d.inbox.archive_view {
        return look::card_items(d);
    }
    let mut v = if d.inbox.archive_view {
        Vec::new()
    } else {
        live(d)
    };
    v.extend(timeline(d));
    v
}

/// The newest `n` events from one machine (the Fleet's machine view).
pub(crate) fn for_host(d: &Dash, host: &str, n: usize) -> Vec<Event> {
    d.inbox
        .rows
        .iter()
        .filter(|e| e.host == host && !d.inbox.dismissed.contains(&id(e)))
        .take(n)
        .cloned()
        .collect()
}

/// The alerts that count as new, newest first: unseen, wanting attention, and
/// really in the list (`inbox.finished` can leave finished turns out, and a run
/// of identical events is one row). Every count and dot reads this, so none
/// can promise rows that aren't there.
fn attention(d: &Dash) -> Vec<&Event> {
    let mut out = Vec::new();
    let mut prev: Option<&Event> = None;
    for it in &d.inbox.merged {
        let e = &it.e;
        let repeat = prev.is_some_and(|p| same(p, e));
        prev = Some(e);
        if !repeat
            && it.logged
            && e.t > d.inbox.seen
            && matches!(
                e.kind,
                Kind::Blocked | Kind::Done | Kind::Error | Kind::Limit
            )
            && !d.inbox.dismissed.contains(&id(e))
        {
            out.push(e);
        }
    }
    out
}

/// New alerts from one machine (a dot on its Fleet card).
pub(crate) fn unread_for(d: &Dash, host: &str) -> usize {
    attention(d).iter().filter(|e| e.host == host).count()
}

/// New alerts (for the tab's dot).
pub(crate) fn unread(d: &Dash) -> usize {
    attention(d).len()
}

/// The newest `n` alerts that came after the Inbox was last looked at
/// (ctrl+n's "missed"), newest first.
pub(crate) fn missed(d: &Dash, n: usize) -> Vec<Event> {
    attention(d).into_iter().take(n).cloned().collect()
}

/// Everything up to now counts as read (ctrl+n's `r`).
pub(crate) fn mark_read(d: &mut Dash) {
    mark_seen(d);
}

pub(crate) fn enter(d: &mut Dash) {
    reload(d);
    d.inbox.opened = None;
}

/// Everything up to now counts as read.
fn mark_seen(d: &mut Dash) {
    d.inbox.seen = crate::cache::now();
    events::set_seen(d.inbox.seen);
}

/// Pick up new events as the fleet changes; what's on screen is read a few
/// seconds after it's been seen (so the dots are noticed first).
pub(crate) fn pump(d: &mut Dash) {
    if !matches!(d.inbox.loaded, Some(t) if t >= d.fleet.changed_at) {
        reload(d);
    }
    // (not in tests: their dashboards share these files)
    if !cfg!(test) && d.inbox.looked.is_none_or(|t| t.elapsed().as_secs() >= 2) {
        d.inbox.looked = Some(Instant::now());
        look_again(d);
    }
    if d.view == View::Inbox && unread(d) > 0 {
        match d.inbox.opened {
            None => d.inbox.opened = Some(Instant::now()),
            Some(t) if t.elapsed().as_secs() >= 4 => {
                d.inbox.opened = None;
                mark_seen(d);
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------ reasons

/// What an event means, in words: (headline, detail).
pub(crate) fn reason(e: &Event) -> (String, String) {
    let t = e.title.trim().to_string();
    match e.kind {
        Kind::Error => explain_error(&t),
        Kind::Blocked => ("is waiting for your answer".into(), t),
        Kind::Done => ("finished its turn".into(), t),
        Kind::New => ("started".into(), t),
        Kind::Gone => ("ended".into(), t),
        Kind::Auto => (
            "was answered by an auto-approve rule".into(),
            t.trim_start_matches("rule ").to_string(),
        ),
        Kind::Task => ("was sent a queued task".into(), t),
        Kind::Heal => ("herdr was restarted to recover".into(), t),
        Kind::Limit => ("stopped at its usage limit".into(), t),
        Kind::HostUp => ("answers again".into(), t),
        Kind::Remote => ("was acted on remotely".into(), t),
        Kind::Match => ("said what a watch rule looks for".into(), t),
    }
}

/// ssh / herdr error text → (plain explanation, `host:port · first line`).
pub(crate) fn explain_error(t: &str) -> (String, String) {
    let l = t.to_lowercase();
    let target = t
        .split("connect to host ")
        .nth(1)
        .and_then(|r| {
            let h = r.split_whitespace().next()?;
            let p = r.split(" port ").nth(1)?.split(':').next()?;
            Some(format!("{h}:{p}"))
        })
        .unwrap_or_default();
    let head = if l.contains("timed out") {
        "unreachable — ssh timed out"
    } else if l.contains("exited with status 255") {
        // ssh's own failure: it couldn't connect (or the link dropped)
        "unreachable — ssh couldn't connect"
    } else if l.contains("could not resolve hostname") || l.contains("name or service not known") {
        "unknown host — the name doesn't resolve"
    } else if l.contains("no route to host") || l.contains("network is unreachable") {
        "unreachable — no route (VPN off? asleep?)"
    } else if l.contains("connection reset") || l.contains("broken pipe") {
        "the connection dropped"
    } else if l.contains("permission denied") {
        "key login refused"
    } else if l.contains("host key verification failed") || l.contains("identification has changed")
    {
        "host key isn't trusted"
    } else if l.starts_with("ssh") && l.contains("connection refused") {
        "ssh refused — sshd isn't listening"
    } else if crate::fleet::not_running(&l) {
        "herdr stopped there"
    } else if l.contains("unknown machine") {
        "not saved in herdr"
    } else {
        "error"
    };
    let first = t.lines().next().unwrap_or("").trim();
    let first = first.strip_prefix("ssh: ").unwrap_or(first);
    let detail = if target.is_empty() {
        first.to_string()
    } else {
        format!("{target} · {first}")
    };
    (head.to_string(), detail)
}

fn look(k: Kind) -> (&'static str, Color, &'static str) {
    match k {
        Kind::Blocked => (crate::look::needs(), c::NEEDS(), "needs you"),
        Kind::Done => ("✓", c::GREEN(), "finished"),
        Kind::New => ("+", c::BLUE(), "started"),
        Kind::Gone => ("–", c::MUTED(), "ended"),
        Kind::Error => ("✕", c::YELLOW(), "problem"),
        Kind::Auto => ("↯", c::ACCENT(), "auto"),
        Kind::Task => ("≡", c::BLUE(), "task"),
        Kind::Heal => ("↻", c::GREEN(), "healed"),
        Kind::Limit => ("◷", c::LIMIT(), "at limit"),
        Kind::HostUp => ("●", c::GREEN(), "back up"),
        Kind::Remote => ("⇄", c::BLUE(), "remote"),
        Kind::Match => ("◎", c::BLUE(), "matched"),
    }
}

/// "Linux · Ubuntu 24.04" (from herdr info) or the stats' platform.
fn host_os(d: &Dash, h: &str) -> String {
    d.fleet
        .host(h)
        .map(|x| x.os().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            d.stats(h)
                .map(|s| format!("{} {}", s.platform, s.os).trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_default()
}

/// The device's own name (`NG-PC-1`) when the machine's stats know it.
fn device(d: &Dash, h: &str) -> Option<String> {
    d.stats(h)
        .map(|s| s.hostname.clone())
        .filter(|n| !n.is_empty() && n != h)
}

fn live_session(d: &Dash, e: &Event) -> Option<Session> {
    if e.name.is_empty() {
        return None;
    }
    d.fleet
        .host(&e.host)?
        .sessions
        .iter()
        .find(|s| s.name == e.name || s.display() == e.name)
        .cloned()
}

/// The event's agent, or the live session's when the event predates it.
fn agent_of(d: &Dash, e: &Event) -> String {
    if !e.agent.is_empty() {
        return e.agent.clone();
    }
    live_session(d, e).map(|s| s.agent).unwrap_or_default()
}

/// What's true about an event now: (text, colour), e.g. "still waiting 4m"
/// or "answered". Empty when there's nothing to add.
fn now_state(d: &Dash, it: &Item) -> (String, Color) {
    let now = crate::cache::now();
    let e = &it.e;
    let s = live_session(d, e);
    let host = d.fleet.host(&e.host);
    match e.kind {
        Kind::Blocked => match &s {
            Some(s) if s.blocked() => (
                format!(
                    "{} still waiting {}",
                    crate::look::needs(),
                    ui::dur(now.saturating_sub(s.since.unwrap_or(e.t)))
                ),
                c::NEEDS(),
            ),
            Some(_) => ("✓ answered".into(), c::GREEN()),
            None => ("– session ended".into(), c::FAINT()),
        },
        Kind::Limit => match &s {
            Some(s) if s.limit.is_some() => ("◷ still stopped".into(), c::LIMIT()),
            Some(_) => ("● back at work".into(), c::GREEN()),
            None => ("– session ended".into(), c::FAINT()),
        },
        Kind::Error if e.name.is_empty() => match host.map(|h| &h.conn) {
            Some(Conn::Up) => ("● back up".into(), c::GREEN()),
            Some(Conn::Down(x)) if crate::fleet::not_running(x) => {
                ("○ reachable, herdr stopped".into(), c::MUTED())
            }
            Some(Conn::Down(_)) => (
                format!(
                    "✕ still down{}",
                    host.and_then(|h| h.fetched)
                        .map(|t| format!(" · {}", ui::dur(now.saturating_sub(t))))
                        .unwrap_or_default()
                ),
                c::RED(),
            ),
            _ => (String::new(), c::FAINT()),
        },
        _ => match &s {
            Some(s) => {
                let (t, col) = super::fleet::status_for(d, s);
                (format!("now {t}"), col)
            }
            None if !e.name.is_empty() => ("– not running now".into(), c::FAINT()),
            None => (String::new(), c::FAINT()),
        },
    }
}

/// A one-line nudge: what to do about a live item.
fn nudge(d: &Dash, it: &Item) -> String {
    let now = crate::cache::now();
    let e = &it.e;
    let mut s = match e.kind {
        Kind::Blocked => format!(
            "waiting {} — y approves its yes option · s reply · a attach",
            ui::dur(now.saturating_sub(e.t))
        ),
        Kind::Limit => {
            let r = e.title.trim_start_matches("resets ").to_string();
            if r.is_empty() {
                "out of quota — Q queues a message for when it's back".into()
            } else {
                format!("back {r} — Q queues a message to send then")
            }
        }
        Kind::Error => {
            let (h, _) = explain_error(&e.title);
            format!(
                "{h} — {} · {} › Doctor checks it",
                super::fleet::hint_for(&e.title),
                super::digit(View::Settings)
            )
        }
        _ => String::new(),
    };
    if e.kind != Kind::Error {
        if let Some(Conn::Down(_)) = d.fleet.host(&e.host).map(|h| &h.conn) {
            s.push_str(" (its machine is down)");
        }
    }
    s
}

// ------------------------------------------------------------------ keys

fn current(d: &Dash) -> Option<Item> {
    items(d).into_iter().nth(d.inbox.sel)
}

/// The newest launch of the session an event is about (an event outside
/// the join window still knows how its session began).
fn launch_of(d: &Dash, e: &Event) -> Option<Entry> {
    if e.name.is_empty() {
        return None;
    }
    d.inbox
        .launches
        .iter()
        .find(|l| l.host == e.host && l.name == e.name && l.t <= e.t + 120)
        .cloned()
}

/// The new-session form, filled in from a launch.
fn again_form(d: &Dash, l: &Entry) -> forms::NewForm {
    let mut nf = forms::NewForm::new(d, &l.host, Some(&l.dir));
    nf.set_agent(&l.agent);
    nf.set_prompt(l.prompt.as_deref().unwrap_or(""));
    nf.set_worktree(l.worktree);
    nf
}

/// enter: the session in Focus (esc comes back here), else its machine.
fn jump(d: &mut Dash) -> Option<Handoff> {
    let e = current(d)?.e;
    let hit = live_session(d, &e).map(|s| s.key());
    if let Some(k) = &hit {
        d.fv.sel = Some(RowId::Sess(k.clone()));
        super::wall::focus_on(d, k, View::Inbox);
        return None;
    }
    d.switch(View::Fleet);
    match hit {
        Some(k) => d.fv.sel = Some(RowId::Sess(k)),
        None => {
            d.fv.sel = Some(RowId::Host(e.host.clone()));
            if !e.name.is_empty() {
                d.toast(
                    Level::Info,
                    format!("{} isn't running on {} any more", e.name, e.host),
                );
            }
        }
    }
    None
}

/// Keys while typing a search.
pub(crate) fn search_key(d: &mut Dash, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            d.inbox.search.clear();
            d.inbox.searching = false;
        }
        KeyCode::Enter => d.inbox.searching = false,
        KeyCode::Backspace => {
            d.inbox.search.pop();
        }
        KeyCode::Char(ch) => {
            d.inbox.search.push(ch);
            d.inbox.sel = 0;
        }
        _ => {}
    }
}

pub(crate) fn click(d: &mut Dash, i: usize, double: bool) -> Option<Handoff> {
    if let Some(Row::Item(ix, _)) = rowmap(d).get(i) {
        d.inbox.sel = *ix;
        if double {
            return jump(d);
        }
    }
    None
}

/// `x`: into the archive, or (looking at the archive) back out of it.
fn archive(d: &mut Dash, it: &Item) {
    // from the files as they are now: what the hub archived meanwhile stays
    read_marks(d);
    let ids = ids_of(d, it);
    let back = d.inbox.archive_view;
    for i in &ids {
        if back {
            d.inbox.dismissed.remove(i);
            d.inbox.archived.remove(i);
            // the automatic archiving leaves it alone from now on
            if is_start(it) {
                d.inbox.kept.insert(i.clone());
            }
        } else if i.starts_with("launch|") {
            d.inbox.archived.insert(i.clone());
            d.inbox.kept.remove(i);
        } else {
            d.inbox.dismissed.insert(i.clone());
            d.inbox.kept.remove(i);
        }
    }
    // keep the files small: only ids still in the logs matter
    events::save_dismissed(&d.inbox.dismissed, &events::load(events::CAP));
    let known: HashSet<String> = d
        .inbox
        .rows
        .iter()
        .map(id)
        .chain(d.inbox.launches.iter().map(crate::history::id))
        .collect();
    events::save_marks(&d.inbox.archived, &d.inbox.kept, |i| known.contains(i));
    let who = if it.e.name.is_empty() {
        it.e.host.clone()
    } else {
        it.e.name.clone()
    };
    d.note(if back {
        format!("{who} is back in the inbox")
    } else {
        format!("{who} archived — A shows the archive")
    });
    d.inbox.sel = d.inbox.sel.min(items(d).len().saturating_sub(1));
}

/// What `D` deletes for good, once you've said yes.
pub(crate) struct Forget {
    events: HashSet<String>,
    launches: Vec<String>,
    what: String,
}

/// `D`: ask first, then the row's events and launch go from the logs.
fn ask_forget(d: &mut Dash, it: &Item) {
    let ids = ids_of(d, it);
    let (launches, evs): (Vec<String>, Vec<String>) =
        ids.into_iter().partition(|i| i.starts_with("launch|"));
    let what = if it.e.name.is_empty() {
        format!("{} on {}", look(it.e.kind).2, it.e.host)
    } else {
        format!("{} on {}", it.e.name, it.e.host)
    };
    let mut text = format!(
        "{what}: {} alert{}",
        evs.len(),
        if evs.len() == 1 { "" } else { "s" }
    );
    if !launches.is_empty() {
        text.push_str(" and how it was started (r can't start it again after this)");
    }
    text.push_str(". This can't be undone.");
    d.mode = Mode::Confirm {
        title: "Delete for good?".into(),
        text,
        on_yes: Pending::Forget(Forget {
            events: evs.into_iter().collect(),
            launches,
            what,
        }),
    };
}

pub(crate) fn forget(d: &mut Dash, f: Forget) {
    let n = events::remove(&f.events) + crate::history::remove(&f.launches);
    reload(d);
    d.note(if n > 0 {
        format!("{} deleted for good", f.what)
    } else {
        "nothing left to delete".into()
    });
    d.inbox.sel = d.inbox.sel.min(items(d).len().saturating_sub(1));
}

/// ⇥ / ⇧⇥ (and `f`): the next / previous kind filter.
pub(crate) fn filter_step(d: &mut Dash, fwd: bool) {
    let n = Show::ALL.len();
    let i = Show::ALL
        .iter()
        .position(|s| *s == d.inbox.show)
        .unwrap_or(0);
    d.inbox.show = Show::ALL[if fwd { (i + 1) % n } else { (i + n - 1) % n }];
    d.inbox.sel = 0;
}

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    let n = items(d).len();
    let last = n.saturating_sub(1);
    // cards: ←→ a card, ↑↓ a row of them
    if d.inbox.layout == Layout::Cards && !d.inbox.archive_view {
        let cols = d.inbox.card_cols.max(1);
        match k.code {
            KeyCode::Left | KeyCode::Char('h') => {
                d.inbox.sel = d.inbox.sel.saturating_sub(1);
                return None;
            }
            KeyCode::Right | KeyCode::Char('l') => {
                d.inbox.sel = (d.inbox.sel + 1).min(last);
                return None;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                d.inbox.sel = d.inbox.sel.saturating_sub(cols);
                return None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                d.inbox.sel = (d.inbox.sel + cols).min(last);
                return None;
            }
            _ => {}
        }
    }
    match k.code {
        // its own layouts (L is the panels' layout mode, as everywhere)
        KeyCode::Char('v') => {
            let i = Layout::ALL
                .iter()
                .position(|l| *l == d.inbox.layout)
                .unwrap_or(0);
            d.inbox.layout = Layout::ALL[(i + 1) % Layout::ALL.len()];
            d.inbox.sel = 0;
            d.inbox.scroll = 0;
            d.note(format!(
                "{} layout — inbox.layout keeps one",
                d.inbox.layout.name()
            ));
        }
        KeyCode::Up | KeyCode::Char('k') => d.inbox.sel = d.inbox.sel.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => d.inbox.sel = (d.inbox.sel + 1).min(last),
        KeyCode::PageDown => d.inbox.sel = (d.inbox.sel + 10).min(last),
        KeyCode::PageUp => d.inbox.sel = d.inbox.sel.saturating_sub(10),
        KeyCode::Home => d.inbox.sel = 0,
        KeyCode::Char('g') => {
            // the agents in the inbox, most frequent first
            let mut used: Vec<(String, usize)> = Vec::new();
            for it in &d.inbox.merged {
                let a = &it.e.agent;
                if a.is_empty() {
                    continue;
                }
                match used.iter_mut().find(|(x, _)| x == a) {
                    Some(x) => x.1 += 1,
                    None => used.push((a.clone(), 1)),
                }
            }
            used.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
            let used: Vec<String> = used.into_iter().map(|x| x.0).collect();
            d.inbox.agent = match &d.inbox.agent {
                None => used.first().cloned(),
                Some(a) => {
                    let i = used.iter().position(|x| x == a).map_or(0, |i| i + 1);
                    used.get(i).cloned()
                }
            };
            d.inbox.sel = 0;
        }
        KeyCode::End => d.inbox.sel = last,
        KeyCode::Char('G') => {
            let i = Group::ALL
                .iter()
                .position(|g| *g == d.inbox.group)
                .unwrap_or(0);
            d.inbox.group = Group::ALL[(i + 1) % Group::ALL.len()];
            d.inbox.sel = 0;
            d.note(format!("grouped by {}", d.inbox.group.name()));
        }
        KeyCode::Char('e') => {
            d.inbox.hide_ended = !d.inbox.hide_ended;
            d.inbox.sel = 0;
            d.note(if d.inbox.hide_ended {
                "ended sessions hidden — e shows them"
            } else {
                "ended sessions shown, dimmed"
            });
        }
        KeyCode::Char('A') => {
            d.inbox.archive_view = !d.inbox.archive_view;
            d.inbox.sel = 0;
        }
        // the session journal: what each session did today
        KeyCode::Char('J') => {
            super::journal::open(d, ui::local_day(crate::cache::now()));
        }
        KeyCode::Char('o') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e) else {
                d.toast(Level::Info, "that session isn't running now");
                return None;
            };
            super::open_in_herdr_tab(d, &s);
        }
        KeyCode::Char('X') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e) else {
                d.toast(Level::Info, "that session isn't running now");
                return None;
            };
            d.mode = Mode::Confirm {
                title: "Close session?".into(),
                text: format!(
                    "{} ({}) — {}",
                    s.display(),
                    s.host,
                    super::fleet::close_what(d)
                ),
                on_yes: Pending::Close(vec![s]),
            };
        }
        KeyCode::Char('D') => {
            let it = current(d)?;
            if it.now {
                d.toast(
                    Level::Info,
                    "that one is live — it clears itself once it's dealt with",
                );
                return None;
            }
            ask_forget(d, &it);
        }
        KeyCode::Char('f') => filter_step(d, true),
        KeyCode::Char('m') => {
            let hosts: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
            d.inbox.host = match &d.inbox.host {
                None => hosts.first().cloned(),
                Some(h) => {
                    let i = hosts.iter().position(|x| x == h).map_or(0, |i| i + 1);
                    hosts.get(i).cloned()
                }
            };
            d.inbox.sel = 0;
        }
        KeyCode::Char('/') => d.inbox.searching = true,
        KeyCode::Char('R') => {
            reload(d);
            d.note("reloaded");
        }
        // start it again: the new-session form, filled in from how it began
        KeyCode::Char('r') => {
            let it = current(d)?;
            let Some(l) = it.launch.or_else(|| launch_of(d, &it.e)) else {
                d.toast(
                    Level::Info,
                    "no record of how that one started — n starts a new session",
                );
                return None;
            };
            d.mode = Mode::NewForm(again_form(d, &l));
        }
        KeyCode::Char('P') => {
            let it = current(d)?;
            let Some(l) = it.launch.or_else(|| launch_of(d, &it.e)) else {
                d.toast(
                    Level::Info,
                    "only sessions started from Aleph can become presets",
                );
                return None;
            };
            let name = crate::names::from_folder(
                &crate::path::show(crate::path::basename(l.dir.as_bytes())),
                &d.cfg
                    .presets()
                    .iter()
                    .map(|p| p.name.clone())
                    .collect::<Vec<_>>(),
            );
            d.mode = Mode::Input(forms::Input::new(
                "Save as preset".into(),
                &format!(
                    "{} on {} in {} — start it again with {} new @name",
                    crate::agents::label(&l.agent),
                    l.host,
                    l.dir,
                    crate::doctor::me()
                ),
                name,
                forms::For::SavePreset(Box::new(l)),
            ));
        }
        // at its limit: the same folder, another agent
        KeyCode::Char('C') => {
            let e = current(d)?.e;
            let s = live_session(d, &e);
            let dir = s.as_ref().map(|s| s.cwd.clone()).unwrap_or_default();
            let avoid = s
                .as_ref()
                .map(|s| s.agent.clone())
                .unwrap_or_else(|| e.agent.clone());
            super::quick::open(d, &e.host);
            if let Mode::Quick(q) = &mut d.mode {
                q.prefer(&dir, &avoid);
            }
        }
        KeyCode::Char('N') => {
            let e = current(d)?.e;
            if e.name.is_empty() {
                d.toast(Level::Info, "notes go on sessions");
                return None;
            }
            let cur = crate::notes::get(&e.host, &e.name).map(|n| n.text);
            d.mode = Mode::Input(forms::Input::new(
                format!("Note on {}", e.name),
                "kept after it ends · #tags are searchable · empty clears it",
                cur.unwrap_or_default(),
                forms::For::Note(e.host.clone(), e.name.clone()),
            ));
        }
        KeyCode::Char('U') => dismiss_list(d),
        KeyCode::Char('u') => {
            let n = unread(d);
            mark_seen(d);
            d.note(if n > 0 {
                format!("{n} marked read")
            } else {
                "nothing unread".into()
            });
        }
        KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete => {
            let it = current(d)?;
            if it.now {
                d.toast(
                    Level::Info,
                    "that one is live — it clears itself once it's dealt with",
                );
            } else {
                archive(d, &it);
            }
        }
        KeyCode::Char('c') => {
            let it = current(d)?;
            if let Some(l) = &it.launch {
                if super::copy(&crate::history::command(l)) {
                    d.note("command copied");
                } else {
                    d.error(super::copy_failed());
                }
                return None;
            }
            let e = it.e;
            let (h, det) = reason(&e);
            let who = if e.name.is_empty() {
                e.host.clone()
            } else {
                format!("{}:{}", e.host, e.name)
            };
            let text = format!(
                "{} {} {who} {h}{}",
                ui::day_label(e.t, crate::cache::now()),
                ui::clock(e.t),
                if det.is_empty() {
                    String::new()
                } else {
                    format!(" — {det}")
                }
            );
            if super::copy(&text) {
                d.note("copied");
            } else {
                d.error(super::copy_failed());
            }
        }
        KeyCode::Char('a') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e) else {
                d.toast(Level::Info, "that session isn't running now");
                return None;
            };
            return Some(Handoff::Attach {
                target: d.target_of(&s.host),
                pane: s.pane_id.clone(),
                label: s.display().to_string(),
            });
        }
        KeyCode::Char('y') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e).filter(|s| s.blocked()) else {
                d.toast(Level::Info, "nothing is asking there now");
                return None;
            };
            let t = d.target_of(&s.host);
            d.spawn(Some(s.host.clone()), move || {
                Herdr::new(&t)
                    .approve(&s.pane_id)
                    .map(|k| format!("approved {} ({})", s.display(), k.join(" ")))
            });
        }
        KeyCode::Char('s') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e) else {
                d.toast(Level::Info, "that session isn't running now");
                return None;
            };
            d.mode = Mode::Input(forms::Input::new(
                format!("Reply to {}", s.display()),
                "enter sends it as if you typed it · tab queues it for when it's free",
                String::new(),
                forms::For::Send(vec![s]),
            ));
        }
        KeyCode::Char('Q') => {
            let e = current(d)?.e;
            let Some(s) = live_session(d, &e) else {
                d.toast(Level::Info, "that session isn't running now");
                return None;
            };
            d.mode = Mode::Input(forms::Input::new(
                format!("Queue for {}", s.display()),
                "sent once it's free (answered, or back from its limit)",
                String::new(),
                forms::For::QueueTo(s.host.clone(), s.name.clone()),
            ));
        }
        KeyCode::Enter => return jump(d),
        _ => {}
    }
    None
}

pub(crate) fn hints(d: &Dash) -> Vec<(&'static str, &'static str)> {
    if d.inbox.searching {
        return vec![("enter", "keep"), ("esc", "clear")];
    }
    let mut v = vec![("enter", "go to")];
    if let Some(it) = current(d) {
        if let Some(s) = live_session(d, &it.e) {
            if s.blocked() {
                v.push(("y", "approve"));
            }
            v.extend([("s", "reply"), ("o", "open in a tab"), ("X", "close")]);
        }
        if it.launch.is_some() || launch_of(d, &it.e).is_some() {
            v.push(("r", "start again"));
        }
        if !it.now {
            v.push(if d.inbox.archive_view {
                ("x", "bring back")
            } else {
                ("x", "archive")
            });
            v.push(("D", "delete"));
        }
    }
    if !d.inbox.archive_view {
        v.push(("U", "dismiss…"));
    }
    v.extend([
        ("v", "layout"),
        ("G", "group"),
        ("e", "ended"),
        (
            "A",
            if d.inbox.archive_view {
                "inbox"
            } else {
                "archive"
            },
        ),
        ("⇥", "kind"),
        ("/", "search"),
        ("J", "journal"),
    ]);
    v
}

// ------------------------------------------------------------------ drawing

/// A screen line of the list.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Row {
    /// a group's head ("Needs you now", a machine, a day, …): its first item
    Head(usize),
    /// the column titles, under each head
    Cols(usize),
    /// an item, and which of its lines
    Item(usize, u8),
    /// the end of a group's panel (overview): its last item
    Foot(usize),
    Gap,
}

fn rowmap(d: &Dash) -> Vec<Row> {
    rows_of(&items(d), d.inbox.layout == Layout::Overview)
}

/// Lines per row: the event, then what it's about (one when compact).
fn per_item() -> u8 {
    if ui::scale() == 0 {
        1
    } else {
        2
    }
}

fn rows_of(its: &[Item], panels: bool) -> Vec<Row> {
    let mut out = Vec::new();
    let mut last: Option<(bool, &str)> = None;
    for (i, it) in its.iter().enumerate() {
        let g = (it.now, it.group.as_str());
        if last != Some(g) {
            if last.is_some() {
                if panels {
                    out.push(Row::Foot(i - 1));
                }
                out.push(Row::Gap);
            }
            out.push(Row::Head(i));
            out.push(Row::Cols(i));
            last = Some(g);
        }
        for p in 0..per_item() {
            out.push(Row::Item(i, p));
        }
    }
    if panels && !its.is_empty() {
        out.push(Row::Foot(its.len() - 1));
    }
    out
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    let r = r.intersection(f.area());
    if r.width < 12 || r.height < 4 {
        return;
    }
    // the tiles when there's room for them, else one summary line
    let tiles = r.height >= 24 && r.width >= 80;
    let [top, bar, body] = Layout_::vertical([
        Constraint::Length(if tiles { 4 } else { 1 }),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(r);
    if tiles {
        look::tiles(d, f, top);
    } else {
        draw_summary(d, f, top);
    }
    draw_filters(d, f, bar);
    let its = items(d);
    if its.is_empty() {
        draw_empty(d, f, body);
        return;
    }
    d.inbox.sel = d.inbox.sel.min(its.len() - 1);
    // the layout keys: p hides the detail, z maximises the list or the detail
    let shape = super::panes::shape(d, "inbox");
    let (list_r, detail_r) = if body.width >= 120 {
        let dw = (body.width * 36 / 100).clamp(56, 92);
        let (a, b) = shape.columns(body, dw, 1);
        (a, (b.width > 0).then_some(b))
    } else if body.height >= 30 {
        use super::panes::Max;
        let none = Rect { height: 0, ..body };
        match shape.max {
            Max::Side => (none, Some(body)),
            Max::Main => (body, None),
            _ if shape.hidden => (body, None),
            _ => {
                let [a, b] =
                    Layout_::vertical([Constraint::Min(8), Constraint::Length(16)]).areas(body);
                (a, Some(b))
            }
        }
    } else {
        (body, None)
    };
    // the day's activity under the list when it's tall enough
    let (list_r, chart_r) = if list_r.height >= 22 && !d.inbox.archive_view {
        let [a, b] = Layout_::vertical([Constraint::Min(8), Constraint::Length(7)]).areas(list_r);
        (a, Some(b))
    } else {
        (list_r, None)
    };
    if list_r.width == 0 || list_r.height == 0 {
        d.hits.list = Rect::default();
        d.hits.list_len = 0;
    } else if d.inbox.layout == Layout::Cards && !d.inbox.archive_view {
        look::cards(d, f, list_r, &its);
        d.hits.list = Rect::default();
        d.hits.list_len = 0;
    } else {
        draw_list(d, f, list_r, &its);
    }
    if let Some(cr) = chart_r {
        look::activity(d, f, cr);
    }
    if let Some(dr) = detail_r {
        draw_detail(d, f, dr, &its[d.inbox.sel]);
    }
}

fn draw_empty(d: &Dash, f: &mut Frame, r: Rect) {
    let lines = if d.inbox.archive_view {
        vec![
            Line::from(Span::styled("The archive is empty", ui::bold())),
            Line::raw(""),
            Line::from(Span::styled(
                "x in the inbox puts a row here; ended launches come by themselves (inbox.auto_archive) · A goes back",
                ui::muted(),
            )),
        ]
    } else if d.inbox.rows.is_empty() && d.inbox.launches.is_empty() {
        vec![
            Line::from(Span::styled("All quiet", ui::bold())),
            Line::raw(""),
            Line::from(Span::styled(
                "Nothing needs you. Questions, finished turns, limits and dropped machines land here.",
                ui::muted(),
            )),
            Line::from(Span::styled(
                format!(
                    "{} watch --install sends them as desktop (and phone) notifications too.",
                    crate::doctor::me()
                ),
                ui::faint(),
            )),
        ]
    } else {
        vec![
            Line::from(Span::styled("Nothing matches", ui::bold())),
            Line::raw(""),
            Line::from(Span::styled(
                "f changes the kind · m the machine · / searches (esc clears) · e shows ended sessions · A the archive",
                ui::muted(),
            )),
        ]
    };
    let h = (lines.len() as u16).min(r.height);
    let y = r.y + (r.height - h) / 3;
    f.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        Rect { y, height: h, ..r },
    );
    // showcase mode: stars around it
    crate::showcase::starfield(&d.cfg, f.buffer_mut(), r);
}

/// `▲ 1 need you now   ◷ 1 at their limit   ✕ 1 machine down   9 today   ● 2 unread`
fn draw_summary(d: &Dash, f: &mut Frame, r: Rect) {
    let lv = live(d);
    let count = |k: Kind| lv.iter().filter(|i| i.e.kind == k).count();
    let (b, l, down) = (count(Kind::Blocked), count(Kind::Limit), count(Kind::Error));
    let now = crate::cache::now();
    let today = d
        .inbox
        .rows
        .iter()
        .filter(|e| ui::local_day(e.t) == ui::local_day(now))
        .count();
    let unread_n = unread(d);
    let mut v: Vec<Span> = Vec::new();
    let chip = |v: &mut Vec<Span<'static>>, n: usize, g: &str, label: String, col: Color| {
        if n > 0 {
            v.push(Span::styled(
                format!("{g} {n} {label}"),
                ui::fg(col).add_modifier(Modifier::BOLD),
            ));
            v.push(Span::raw("    "));
        }
    };
    chip(
        &mut v,
        b,
        crate::look::needs(),
        "need you now".into(),
        c::NEEDS(),
    );
    chip(&mut v, l, "◷", "at their limit".into(), c::LIMIT());
    chip(
        &mut v,
        down,
        "✕",
        format!("machine{} down", if down == 1 { "" } else { "s" }),
        c::RED(),
    );
    if b + l + down == 0 {
        v.push(Span::styled("✓ nothing needs you", ui::fg(c::GREEN())));
        v.push(Span::raw("    "));
    }
    v.push(Span::styled(format!("{today} today"), ui::muted()));
    if unread_n > 0 {
        v.push(Span::raw("    "));
        v.push(Span::styled("● ", ui::fg(c::ACCENT())));
        v.push(Span::styled(
            format!("{unread_n} unread"),
            ui::fg(c::ACCENT()),
        ));
        v.push(Span::styled("  u marks read", ui::faint()));
    }
    f.render_widget(Paragraph::new(Line::from(v)), r);
}

/// A click on a filter chip.
pub(crate) fn set_show(d: &mut Dash, i: usize) {
    if let Some(s) = Show::ALL.get(i) {
        d.inbox.show = *s;
        d.inbox.sel = 0;
    }
}

fn draw_filters(d: &mut Dash, f: &mut Frame, r: Rect) {
    let lv = live(d);
    let mut l: Vec<Span> = Vec::new();
    let mut x = r.x;
    let mut chips: Vec<(Rect, super::Chip)> = Vec::new();
    // what each chip would show: the filters, and the archive as now
    let shown: Vec<&Item> = d
        .inbox
        .merged
        .iter()
        .filter(|it| kept_item(d, it))
        .filter(|it| {
            let lf = life(d, &it.e);
            archived(d, it, lf) == d.inbox.archive_view
                && !(d.inbox.hide_ended && !d.inbox.archive_view && lf == Life::Ended)
        })
        .collect();
    for (ci, s) in Show::ALL.into_iter().enumerate() {
        let on = s == d.inbox.show;
        let n = shown.iter().filter(|it| s.keeps_item(it)).count();
        // a dot when it holds something unread or live (launches never are)
        let hot = s != Show::Launches
            && (attention(d).iter().any(|e| s.keeps(e.kind) && kept(d, e))
                || (s != Show::All && lv.iter().any(|i| s.keeps(i.e.kind))));
        let st = if on {
            ui::selected().fg(c::ACCENT())
        } else {
            ui::muted()
        };
        l.push(Span::styled(format!(" {} ", s.name()), st));
        l.push(Span::styled(
            format!("{n}"),
            if on { st } else { ui::faint() },
        ));
        l.push(Span::styled(
            if hot { " •" } else { "  " },
            ui::fg(c::ACCENT()),
        ));
        l.push(Span::raw("  "));
        // the chip's clickable span: its name, count and dot
        let w = (ui::width(s.name()) + 2 + n.to_string().len() + 2) as u16;
        chips.push((Rect::new(x, r.y, w, 1), super::Chip::Inbox(ci)));
        x += w + 2;
    }
    d.hits.chips.extend(chips);
    l.push(Span::styled("  machine ", ui::faint()));
    match &d.inbox.host {
        Some(h) => {
            l.push(icons::os_glyph(&host_os(d, h)));
            l.push(Span::raw(" "));
            l.push(Span::styled(
                h.clone(),
                ui::fg(theme::machine(&d.cfg, h)).add_modifier(Modifier::BOLD),
            ));
        }
        None => l.push(Span::styled("all", ui::muted())),
    }
    l.push(Span::styled("   group ", ui::faint()));
    l.push(Span::styled(d.inbox.group.name(), ui::fg(c::ACCENT())));
    l.push(Span::styled(
        if d.inbox.hide_ended {
            "   ended hidden"
        } else {
            "   ended dimmed"
        },
        ui::faint(),
    ));
    if d.inbox.archive_view {
        l.push(Span::styled(
            "   ▣ archive",
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ));
    }
    if d.inbox.searching || !d.inbox.search.is_empty() {
        l.push(Span::styled("   / ", ui::fg(c::ACCENT())));
        l.push(Span::styled(d.inbox.search.clone(), ui::bold()));
        if d.inbox.searching {
            l.push(Span::styled("▏", ui::fg(c::ACCENT())));
        }
    }
    f.render_widget(Paragraph::new(Line::from(l)), r);
}

/// The table's columns at this width: 0 hides one.
#[derive(Clone, Copy, Debug)]
struct Cols {
    time: usize,
    what: usize,
    agent: usize,
    session: usize,
    machine: usize,
    folder: usize,
    now: usize,
}

const GAP: usize = 2;

fn cols(d: &Dash, its: &[Item], w: usize) -> Cols {
    let by = d.inbox.group;
    let longest = |f: &dyn Fn(&Item) -> usize, lo: usize, hi: usize| {
        its.iter().map(f).max().unwrap_or(lo).clamp(lo, hi)
    };
    let mut c = Cols {
        time: if by == Group::Day { 5 } else { 10 },
        // pills: a space either side
        what: 13,
        agent: 2 + longest(&|i| ui::width(&short_agent(&agent_of(d, &i.e))), 4, 9),
        session: longest(
            &|i| ui::width(&i.e.name) + if i.n > 1 { 4 } else { 0 },
            7,
            24,
        ),
        machine: if by == Group::Machine {
            0
        } else {
            longest(&|i| ui::width(&machine_text(d, &i.e.host)), 3, 22)
        },
        folder: 0,
        now: 16,
    };
    let fixed = |c: &Cols| {
        let v = [c.time, c.what, c.agent, c.session, c.machine, c.now];
        1 + v.iter().sum::<usize>() + GAP * v.iter().filter(|x| **x > 0).count()
    };
    // narrower: the agent's name, then the machine, then the age
    if fixed(&c) + 12 > w {
        c.agent = 1;
    }
    if fixed(&c) + 12 > w && c.machine > 0 {
        c.machine = 0;
    }
    if fixed(&c) > w {
        c.time = 5;
        c.now = 10;
    }
    c.folder = w.saturating_sub(fixed(&c));
    if c.folder < 8 {
        c.folder = 0;
    }
    c
}

/// `b31 · Ubuntu`: the machine and its system, in words.
fn machine_text(d: &Dash, h: &str) -> String {
    match icons::distro(&host_os(d, h)) {
        Some(x) => format!("{h} · {}", x.name),
        None => h.to_string(),
    }
}

/// `Claude Code` → `Claude`, `GitHub Copilot` → `Copilot`.
fn short_agent(id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    let l = crate::agents::label(id);
    match l.as_str() {
        "GitHub Copilot" => "Copilot".into(),
        _ => l.split(' ').next().unwrap_or(&l).to_string(),
    }
}

/// When, as short as the grouping allows: `15:26` today (or grouped by
/// day), `yest 23:48`, `Mon 12:53` within the week, `21 Oct` before.
fn when(t: u64, now: u64, by_day: bool) -> String {
    let days = ui::local_day(now) - ui::local_day(t);
    if by_day || days <= 0 {
        return ui::clock(t);
    }
    let label = ui::day_label(t, now);
    match days {
        1 => format!("yest {}", ui::clock(t)),
        2..=6 => format!("{} {}", label.split(' ').next().unwrap_or(""), ui::clock(t)),
        _ => label
            .split_once(' ')
            .map(|x| x.1.to_string())
            .unwrap_or(label),
    }
}

/// The session's folder, `~`-short: where it runs now, else where it was
/// started.
fn folder_of(d: &Dash, it: &Item) -> String {
    let e = &it.e;
    let host = d.fleet.host(&e.host);
    let raw = live_session(d, e)
        .map(|s| s.cwd)
        .filter(|c| !c.is_empty())
        .or_else(|| it.launch.as_ref().map(|l| l.dir.clone()))
        .or_else(|| d.inbox.dirs.get(&(e.host.clone(), e.name.clone())).cloned())
        .unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    host.map_or_else(|| ui::tilde_path(&raw), |h| h.short_path(&raw))
}

/// The agent's logo, as alive as the agent: breathing in its state's colour
/// while it works or waits, still in its own colour when idle or done, a
/// grey ○ once it has ended.
fn logo(d: &Dash, agent: &str, life: Life) -> Span<'static> {
    if agent.is_empty() {
        return Span::raw(" ");
    }
    let (g, brand) = icons::brand(agent);
    let (_, _, col, period) = life.look(d.t0);
    match life {
        Life::Ended => Span::styled("○", ui::faint()),
        Life::Down | Life::Checking => Span::styled(g, ui::faint()),
        _ if period > 0 => Span::styled(g, ui::pulse(col, d.t0, period)),
        _ if ui::c::mono() => Span::styled(g, ui::bold()),
        _ => Span::styled(g, ui::fg(brand).add_modifier(Modifier::BOLD)),
    }
}

/// NOW: a pill while the agent lives (`▲ waiting 4m`, `● working`), grey
/// words once it's gone (`○ ended`, `✕ machine down`).
fn badge(d: &Dash, it: &Item, w: usize) -> Span<'static> {
    let now = crate::cache::now();
    let (g, label, col, _) = it.life.look(d.t0);
    let text = match it.life {
        Life::Needs => {
            let since = live_session(d, &it.e)
                .and_then(|s| s.since)
                .unwrap_or(it.e.t);
            format!("{g} waiting {}", ui::dur(now.saturating_sub(since)))
        }
        Life::Up if it.e.kind == Kind::Error => format!("{g} back up"),
        _ => format!("{g} {label}"),
    };
    if it.life.alive() {
        look::pill(&ui::trunc(&text, w.saturating_sub(2)), col)
    } else {
        Span::styled(ui::trunc(&text, w), ui::fg(col))
    }
}

/// A session's own colour: its hue while it runs, else a steady one from
/// its name.
fn session_color(d: &Dash, e: &Event) -> Color {
    live_session(d, e)
        .map(|s| s.color())
        .unwrap_or_else(|| theme::session("", "", &format!("{}|{}", e.host, e.name)))
}

fn table_head(c: &Cols) -> Line<'static> {
    let mut v = vec![Span::raw(" ")];
    for (title, w) in [
        ("TIME", c.time),
        ("WHAT", c.what),
        ("AGENT", c.agent),
        ("SESSION", c.session),
        ("MACHINE", c.machine),
        ("FOLDER", c.folder),
    ] {
        if w > 0 {
            v.push(Span::styled(
                ui::pad(&ui::trunc(title, w), w + GAP),
                ui::faint().add_modifier(Modifier::BOLD),
            ));
        }
    }
    v.push(Span::styled(
        format!("{:>w$}", "NOW", w = c.now),
        ui::faint().add_modifier(Modifier::BOLD),
    ));
    Line::from(v)
}

/// `text ──────── right ─` across `w`.
fn rule(left: Vec<Span<'static>>, right: Vec<Span<'static>>, w: usize) -> Line<'static> {
    let lw: usize = left.iter().map(|s| ui::width(&s.content)).sum();
    let rw: usize = right.iter().map(|s| ui::width(&s.content)).sum();
    let mut v = vec![Span::raw(" ")];
    v.extend(left);
    v.push(Span::raw(" "));
    let fill = w.saturating_sub(lw + 2 + if rw > 0 { rw + 3 } else { 0 });
    v.push(Span::styled("─".repeat(fill), ui::fg(c::LINE())));
    if rw > 0 {
        v.push(Span::raw(" "));
        v.extend(right);
        v.push(Span::styled(" ─", ui::fg(c::LINE())));
    }
    Line::from(v)
}

/// `2 live · 9 ended` over a group's sessions.
fn tally_of(its: &[Item], group: &str) -> Vec<Span<'static>> {
    let mut live: HashSet<&str> = HashSet::new();
    let mut ended: HashSet<&str> = HashSet::new();
    let mut alerts = 0;
    for it in its.iter().filter(|i| !i.now && i.group == group) {
        alerts += it.n;
        if it.e.name.is_empty() {
            continue;
        }
        if it.life.alive() {
            live.insert(&it.e.name);
        } else if it.life == Life::Ended {
            ended.insert(&it.e.name);
        }
    }
    let mut v = Vec::new();
    if !live.is_empty() {
        v.push(Span::styled(
            format!("● {} live", live.len()),
            ui::fg(c::GREEN()),
        ));
        v.push(Span::styled(" · ", ui::faint()));
    }
    if !ended.is_empty() {
        v.push(Span::styled(
            format!("○ {} ended", ended.len()),
            ui::faint(),
        ));
        v.push(Span::styled(" · ", ui::faint()));
    }
    v.push(Span::styled(
        format!("{alerts} alert{}", if alerts == 1 { "" } else { "s" }),
        ui::faint(),
    ));
    v
}

/// A group's head: Needs you now, or the machine / day / agent / type /
/// session it holds, with what's alive in it.
fn head_line(d: &Dash, its: &[Item], i: usize, w: usize) -> Line<'static> {
    let (l, r) = head_parts(d, its, i);
    rule(l, r, w)
}

/// A group's head as (left, right): a bar in the group's colour, what the
/// group is, and on the right what's alive in it.
fn head_parts(d: &Dash, its: &[Item], i: usize) -> (Vec<Span<'static>>, Vec<Span<'static>>) {
    let it = &its[i];
    let now = crate::cache::now();
    if it.now {
        let n = its.iter().filter(|x| x.now).count();
        return (
            vec![
                Span::styled("▌", ui::fg(c::RED())),
                Span::styled(
                    format!("Needs you now · {n}"),
                    ui::fg(c::RED()).add_modifier(Modifier::BOLD),
                ),
            ],
            vec![Span::styled("y approves · s replies", ui::faint())],
        );
    }
    let e = &it.e;
    let right = tally_of(its, &it.group);
    let left: Vec<Span<'static>> = match d.inbox.group {
        Group::Day => vec![Span::styled(
            ui::day_label(e.t, now),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        )],
        Group::Machine => {
            let os = host_os(d, &e.host);
            let mut v = vec![Span::styled("▌", ui::fg(theme::machine(&d.cfg, &e.host)))];
            // the logo where the font has one (the system's name follows anyway)
            if icons::style() == icons::Style_::Nerd && icons::distro(&os).is_some() {
                v.push(icons::os_glyph(&os));
                v.push(Span::raw(" "));
            }
            v.push(Span::styled(
                e.host.clone(),
                ui::fg(theme::machine(&d.cfg, &e.host)).add_modifier(Modifier::BOLD),
            ));
            if !os.is_empty() {
                v.push(Span::styled(
                    format!(" · {}", super::fleet::os_pretty(&os)),
                    ui::muted(),
                ));
            }
            if let Some(dev) = device(d, &e.host) {
                v.push(Span::styled(format!(" · {dev}"), ui::faint()));
            }
            let (st, col) = match d.fleet.host(&e.host).map(|h| &h.conn) {
                Some(Conn::Up) => ("  ● up", c::GREEN()),
                Some(Conn::Down(_)) => ("  ✕ down", c::RED()),
                Some(Conn::Unknown) => ("  … connecting", c::FAINT()),
                None => ("  not in the fleet", c::FAINT()),
            };
            v.push(Span::styled(st, ui::fg(col)));
            v
        }
        Group::Agent => {
            let a = agent_of(d, e);
            if a.is_empty() {
                vec![Span::styled("Machines", ui::bold())]
            } else {
                vec![
                    icons::agent(&a),
                    Span::styled(format!(" {}", crate::agents::label(&a)), ui::bold()),
                ]
            }
        }
        Group::Type => {
            let (g, col, what) = look(e.kind);
            vec![Span::styled(
                format!("{g} {}", what[..1].to_uppercase() + &what[1..]),
                ui::fg(col).add_modifier(Modifier::BOLD),
            )]
        }
        Group::Session => {
            let mut v = vec![Span::styled(
                e.host.clone(),
                ui::fg(theme::machine(&d.cfg, &e.host)).add_modifier(Modifier::BOLD),
            )];
            if !e.name.is_empty() {
                let a = agent_of(d, e);
                v.push(Span::styled(":", ui::faint()));
                v.push(Span::styled(e.name.clone(), ui::bold()));
                if !a.is_empty() {
                    v.push(Span::raw("  "));
                    v.push(logo(d, &a, it.life));
                    v.push(Span::styled(format!(" {}", short_agent(&a)), ui::muted()));
                }
                let f = folder_of(d, it);
                if !f.is_empty() {
                    v.push(Span::styled(
                        format!("  {}", ui::trunc_left(&f, 40)),
                        ui::faint(),
                    ));
                }
            }
            v
        }
    };
    let mut left = left;
    if d.inbox.group != Group::Machine {
        left.insert(0, Span::styled("▌", ui::fg(c::ACCENT())));
    }
    (left, right)
}

fn draw_list(d: &mut Dash, f: &mut Frame, r: Rect, its: &[Item]) {
    let panels = d.inbox.layout == Layout::Overview;
    // the table sits in one panel; the overview draws a panel per group
    let area = if panels {
        r
    } else {
        let total: usize = its.iter().filter(|i| !i.now).map(|i| i.n).sum();
        let mut title = vec![Span::raw(if d.inbox.archive_view {
            "Archive "
        } else {
            "Inbox "
        })];
        title.push(Span::styled(
            format!("{total} alert{}", if total == 1 { "" } else { "s" }),
            ui::muted(),
        ));
        title.push(Span::styled(
            format!(" · by {}", d.inbox.group.name()),
            ui::faint(),
        ));
        if d.inbox.archive_view {
            title.push(Span::styled(
                "  x brings one back · D deletes for good · A back",
                ui::faint(),
            ));
        }
        let block = ui::panel(Line::from(title), true);
        let inner = block.inner(r);
        f.render_widget(block, r);
        inner
    };
    if area.height < 2 || area.width < 16 {
        return;
    }
    let w = area.width as usize;
    // inside a panel: "│ " … " │"
    let iw = if panels { w.saturating_sub(4) } else { w };
    let cl = cols(d, its, iw);
    let map = rows_of(its, panels);
    let at = map
        .iter()
        .position(|x| *x == Row::Item(d.inbox.sel, 0))
        .unwrap_or(0);
    let h = area.height as usize;
    if at <= d.inbox.scroll {
        // keep its group's head and column titles in view too
        d.inbox.scroll = at.saturating_sub(3);
    } else if at + per_item() as usize > d.inbox.scroll + h {
        d.inbox.scroll = (at + per_item() as usize).saturating_sub(h);
    }
    let sel_group = its.get(d.inbox.sel).map(|x| (x.now, x.group.clone()));
    let edge = |i: usize| -> Color {
        let it = &its[i];
        if it.now {
            c::RED()
        } else if sel_group.as_ref() == Some(&(it.now, it.group.clone())) {
            c::ACCENT()
        } else {
            c::LINE()
        }
    };
    let mut lines: Vec<Line> = Vec::new();
    for row in map.iter().skip(d.inbox.scroll).take(h) {
        let line = match *row {
            Row::Gap => Line::raw(""),
            Row::Head(i) if panels => {
                let (l, rt) = head_parts(d, its, i);
                panel_top(l, rt, w, edge(i))
            }
            Row::Head(i) => head_line(d, its, i, w),
            Row::Cols(i) if panels => boxed(table_head(&cl), iw, edge(i)),
            Row::Cols(_) => table_head(&cl),
            Row::Foot(i) => Line::from(Span::styled(
                format!("╰{}╯", "─".repeat(w.saturating_sub(2))),
                ui::fg(edge(i)),
            )),
            Row::Item(i, part) => {
                let selected = i == d.inbox.sel;
                let mut l = item_line(d, &its[i], selected, &cl, part);
                if selected {
                    l = Line::from(
                        l.spans
                            .into_iter()
                            .map(|s| s.patch_style(ui::selected()))
                            .chain(std::iter::once(Span::styled(
                                " ".repeat(iw.saturating_sub(0)),
                                ui::selected(),
                            )))
                            .collect::<Vec<_>>(),
                    );
                }
                if panels {
                    boxed(l, iw, edge(i))
                } else {
                    l
                }
            }
        };
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), area);
    d.hits.list = area;
    d.hits.list_first = area.y;
    d.hits.list_offset = d.inbox.scroll;
    d.hits.list_len = map.len();
}

/// `╭ head ──────── right ╮` in the group's edge colour.
fn panel_top(
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
    w: usize,
    edge: Color,
) -> Line<'static> {
    let lw: usize = left.iter().map(|s| ui::width(&s.content)).sum();
    let rw: usize = right.iter().map(|s| ui::width(&s.content)).sum();
    let mut v = vec![Span::styled("╭ ", ui::fg(edge))];
    v.extend(left);
    v.push(Span::raw(" "));
    let fill = w.saturating_sub(lw + rw + 7);
    v.push(Span::styled("─".repeat(fill), ui::fg(edge)));
    if rw > 0 {
        v.push(Span::raw(" "));
        v.extend(right);
    }
    v.push(Span::styled(" ─╮", ui::fg(edge)));
    Line::from(v)
}

/// A line inside a group's panel: `│ ` … ` │`, cut or padded to `iw`.
fn boxed(l: Line<'static>, iw: usize, edge: Color) -> Line<'static> {
    let mut v = vec![Span::styled("│ ", ui::fg(edge))];
    let mut used = 0;
    for s in l.spans {
        let sw = ui::width(&s.content);
        if used + sw > iw {
            let room = iw - used;
            if room > 0 {
                v.push(Span::styled(ui::trunc(&s.content, room), s.style));
                used += ui::width(&ui::trunc(&s.content, room));
            }
            break;
        }
        used += sw;
        v.push(s);
    }
    v.push(Span::raw(" ".repeat(iw.saturating_sub(used))));
    v.push(Span::styled(" │", ui::fg(edge)));
    Line::from(v)
}

/// A row: line 0 is time · what · agent · session · machine · folder · now;
/// line 1, under it, what it's about (the task, the question, the prompt)
/// and how long.
fn item_line(d: &Dash, it: &Item, selected: bool, cl: &Cols, part: u8) -> Line<'static> {
    let now = crate::cache::now();
    let e = &it.e;
    let alive = it.now || it.life.alive();
    let gone = !it.now && it.life == Life::Ended;
    // ended rows stay readable, only quieter
    let text_st = if gone { ui::muted() } else { ui::fg(c::TEXT()) };
    if part == 1 {
        let indent = 1 + cl.time + GAP;
        let live = live_session(d, e);
        let about = match (&live, e.kind) {
            // (only a question's own row: what the session finished or
            // started before it asked is told as such)
            (Some(s), Kind::Blocked) if s.blocked() => {
                format!(
                    "asks: {}",
                    if e.title.is_empty() {
                        &s.title
                    } else {
                        &e.title
                    }
                )
            }
            (Some(s), Kind::New | Kind::Done) if !s.title.is_empty() => s.title.clone(),
            (_, Kind::New) => match it
                .launch
                .as_ref()
                .and_then(|l| l.prompt.clone())
                .filter(|p| !p.is_empty())
            {
                Some(p) => format!("“{p}”"),
                None => format!("started {}", crate::agents::label(&agent_of(d, e))),
            },
            _ => {
                let (h, det) = reason(e);
                if det.is_empty() {
                    h
                } else {
                    format!("{h} · {det}")
                }
            }
        };
        let meta = if it.n > 1 {
            format!("×{} since {}", it.n, ui::clock(it.first))
        } else if let Some(s) = &live {
            let word = match it.life {
                Life::Needs => "waiting",
                Life::Limit => "at its limit",
                l => l.look(d.t0).1,
            };
            match s.since {
                Some(t) => format!("{word} for {}", ui::dur(now.saturating_sub(t))),
                None => String::new(),
            }
        } else {
            ui::ago(now.saturating_sub(e.t))
        };
        let total = 1
            + [
                cl.time, cl.what, cl.agent, cl.session, cl.machine, cl.folder,
            ]
            .iter()
            .filter(|x| **x > 0)
            .map(|x| x + GAP)
            .sum::<usize>()
            + cl.now;
        let mw = ui::width(&meta);
        let room = total.saturating_sub(indent + mw + 2);
        let t = ui::trunc(&about, room);
        let tw = ui::width(&t);
        return Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(t, if gone { ui::faint() } else { ui::muted() }),
            Span::raw(" ".repeat(total.saturating_sub(indent + tw + mw))),
            Span::styled(meta, ui::faint()),
        ]);
    }
    let (g, col, what) = look(e.kind);
    let agent = agent_of(d, e);
    let cell = |text: String, w: usize, st: Style| -> Span<'static> {
        Span::styled(ui::pad(&ui::trunc(&text, w), w + GAP), st)
    };
    // the mark: selection, live in its colour, unread in the accent
    let mark = if selected {
        Span::styled("┃", ui::fg(c::ACCENT()))
    } else if it.now {
        Span::styled("●", ui::fg(col).add_modifier(Modifier::BOLD))
    } else if it.logged && e.t > d.inbox.seen {
        Span::styled("●", ui::fg(c::ACCENT()))
    } else {
        Span::raw(" ")
    };
    let mut l = vec![mark];
    let today = ui::local_day(e.t) == ui::local_day(now);
    l.push(cell(
        when(e.t, now, d.inbox.group == Group::Day),
        cl.time,
        if today && !gone {
            ui::fg(c::TEXT()).add_modifier(Modifier::BOLD)
        } else {
            ui::muted()
        },
    ));
    // what happened, as a pill in its colour
    let p = look::pill(&ui::trunc(&format!("{g} {what}"), cl.what - 2), col);
    let pw = ui::width(&p.content);
    l.push(p);
    l.push(Span::raw(" ".repeat((cl.what + GAP).saturating_sub(pw))));
    // the agent: its logo as alive as it is, then its name
    l.push(logo(d, &agent, it.life));
    if cl.agent > 1 {
        l.push(cell(
            format!(" {}", short_agent(&agent)),
            cl.agent - 1,
            if alive {
                ui::fg(c::TEXT())
            } else {
                ui::muted()
            },
        ));
    } else {
        l.push(Span::raw(" ".repeat(GAP)));
    }
    let mut name = if e.name.is_empty() {
        "—".to_string()
    } else {
        e.name.clone()
    };
    if crate::notes::get(&e.host, &e.name).is_some() {
        name.push_str(" ✎");
    }
    let fold = if it.n > 1 {
        format!(" ×{}", it.n)
    } else {
        String::new()
    };
    let nw = cl.session.saturating_sub(ui::width(&fold));
    let shown = ui::trunc(&name, nw);
    l.push(Span::styled(
        shown.clone(),
        if gone {
            ui::muted().add_modifier(Modifier::BOLD)
        } else {
            ui::fg(session_color(d, e)).add_modifier(Modifier::BOLD)
        },
    ));
    l.push(Span::styled(fold.clone(), ui::fg(col)));
    l.push(Span::raw(" ".repeat(
        (cl.session + GAP).saturating_sub(ui::width(&shown) + ui::width(&fold)),
    )));
    if cl.machine > 0 {
        let m = machine_text(d, &e.host);
        let (host, sys) = m.split_once(" · ").unwrap_or((&m, ""));
        let mut used = ui::width(host);
        l.push(Span::styled(
            host.to_string(),
            ui::fg(theme::machine(&d.cfg, &e.host)).add_modifier(Modifier::BOLD),
        ));
        if !sys.is_empty() && used + 3 < cl.machine {
            let t = ui::trunc(&format!(" · {sys}"), cl.machine - used);
            used += ui::width(&t);
            l.push(Span::styled(t, ui::muted()));
        }
        l.push(Span::raw(
            " ".repeat((cl.machine + GAP).saturating_sub(used)),
        ));
    }
    if cl.folder > 0 {
        let folder = folder_of(d, it);
        let (text, st) = if folder.is_empty() {
            // nothing to say where: say what it was about
            let (head, detail) = reason(e);
            let t = if e.kind == Kind::New || detail.is_empty() {
                head
            } else {
                format!("{head} · {detail}")
            };
            (t, ui::faint())
        } else {
            (ui::trunc_left(&folder, cl.folder), text_st)
        };
        l.push(cell(text, cl.folder, st));
    }
    let used: usize = l.iter().map(|s| ui::width(&s.content)).sum();
    let total = 1 + [
        cl.time, cl.what, cl.agent, cl.session, cl.machine, cl.folder,
    ]
    .iter()
    .filter(|x| **x > 0)
    .map(|x| x + GAP)
    .sum::<usize>();
    if used < total {
        l.push(Span::raw(" ".repeat(total - used)));
    }
    let b = badge(d, it, cl.now);
    let bw = ui::width(&b.content);
    l.push(Span::raw(" ".repeat(cl.now.saturating_sub(bw))));
    l.push(b);
    Line::from(l)
}

/// A section of the detail pane: its title at the left edge, a rule after;
/// what's in it is indented under it.
fn section(t: &str, w: usize) -> Line<'static> {
    let fill = w.saturating_sub(ui::width(t) + 3);
    Line::from(vec![
        Span::raw(" "),
        Span::styled(
            t.to_string(),
            ui::fg(c::TEXT()).add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled("─".repeat(fill), ui::fg(c::LINE())),
    ])
}

/// A session's latest finished turn and what it took (Fleet's preview), as
/// far as it's known.
pub(crate) fn last_turn(d: &Dash, s: &Session) -> Option<(u64, crate::turncost::Turn)> {
    let e = d
        .inbox
        .rows
        .iter()
        .filter(|e| {
            e.kind == Kind::Done
                && e.host == s.host
                && (e.name == s.name || (!e.pane.is_empty() && e.pane == s.pane_id))
        })
        .max_by_key(|e| e.t)?;
    let native = crate::turncost::native_of(e, Some(s));
    d.inbox.turns.get(&d.cfg, e, &native).map(|t| (e.t, t))
}

/// `412k tok · $1.20* · 6 min`, the agent's own timing marked, and
/// `reading…` while its machine's usage hasn't been read since it ended.
pub(crate) fn turn_spans(t: &crate::turncost::Turn) -> Vec<Span<'static>> {
    let mut v: Vec<Span> = Vec::new();
    for (i, p) in crate::turncost::parts(t).into_iter().enumerate() {
        if i > 0 {
            v.push(Span::styled(" · ", ui::faint()));
        }
        let money = p.starts_with('$') || p.starts_with("<$");
        v.push(Span::styled(
            p,
            if money {
                ui::fg(c::ACCENT())
            } else {
                ui::bold()
            },
        ));
    }
    if !t.complete {
        v.push(Span::styled("  reading…", ui::faint()));
    } else if t.agent_timed {
        v.push(Span::styled("  its own timing", ui::faint()));
    }
    v
}

fn kv(k: &str, v: Vec<Span<'static>>) -> Line<'static> {
    let mut l = vec![Span::styled(format!("   {k:<10}"), ui::faint())];
    l.extend(v);
    Line::from(l)
}

/// Keycaps for what to do now, in aligned columns that fit `w`.
fn actions(v: &[(&str, &str)], w: usize) -> Vec<Line<'static>> {
    let cell = v
        .iter()
        .map(|(k, l)| ui::width(&ui::keycap(k).content) + 1 + ui::width(l))
        .max()
        .unwrap_or(0)
        + 3;
    let per = ((w.saturating_sub(3)) / cell.max(1)).max(1);
    v.chunks(per)
        .map(|row| {
            let mut l: Vec<Span> = vec![Span::raw("   ")];
            for (k, label) in row {
                let cap = ui::keycap(k);
                let used = ui::width(&cap.content) + 1 + ui::width(label);
                l.push(cap);
                l.push(Span::styled(format!(" {label}"), ui::bold()));
                l.push(Span::raw(" ".repeat(cell.saturating_sub(used))));
            }
            Line::from(l)
        })
        .collect()
}

fn draw_detail(d: &Dash, f: &mut Frame, r: Rect, it: &Item) {
    let e = &it.e;
    let (g, col, what) = look(e.kind);
    let (head, detail) = reason(e);
    let (lg, ll, lcol, _) = it.life.look(d.t0);
    let block = Block::bordered()
        .border_set(crate::ui::border_set(false))
        .border_style(ui::fg(col))
        .title(Span::styled(
            format!(" {g} {what}{} ", if it.now { " · now" } else { "" }),
            ui::fg(col).add_modifier(Modifier::BOLD),
        ))
        .title(
            Line::from(if it.now {
                vec![]
            } else {
                vec![Span::styled(format!(" {lg} {ll} now "), ui::fg(lcol))]
            })
            .right_aligned(),
        );
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height == 0 || inner.width < 10 {
        return;
    }
    let now = crate::cache::now();
    let w = inner.width as usize;
    let tw = w.saturating_sub(4);
    let os = host_os(d, &e.host);
    let hcol = theme::machine(&d.cfg, &e.host);
    let host = d.fleet.host(&e.host);
    let st = d.stats(&e.host);
    let live_s = live_session(d, e);
    let agent = agent_of(d, e);
    let who = if e.name.is_empty() {
        e.host.clone()
    } else {
        e.name.clone()
    };
    let mut lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw(" "),
            Span::styled(who, ui::bold()),
            Span::raw(" "),
            Span::styled(head.clone(), ui::fg(col).add_modifier(Modifier::BOLD)),
        ]),
        Line::from(vec![
            Span::styled(
                format!(" {} {}", ui::day_label(e.t, now), ui::clock(e.t)),
                ui::muted(),
            ),
            Span::styled(
                format!("  ·  {}", ui::ago(now.saturating_sub(e.t))),
                ui::faint(),
            ),
            Span::styled(
                if it.n > 1 {
                    format!(
                        "  ·  ×{} since {} ({})",
                        it.n,
                        ui::clock(it.first),
                        ui::ago(now.saturating_sub(it.first))
                    )
                } else {
                    String::new()
                },
                ui::fg(col),
            ),
        ]),
        Line::raw(""),
    ];

    // how it started (a launch, or the newest one of this session)
    let launch = it.launch.clone().or_else(|| launch_of(d, e));

    // ---- what you can do
    let mut todo: Vec<(&str, &str)> = Vec::new();
    let mut said: Vec<String> = Vec::new();
    match (&live_s, e.kind) {
        (Some(s), _) if s.blocked() => {
            said.push("It's waiting for your answer.".into());
            todo.extend([("y", "approve"), ("s", "reply"), ("a", "attach")]);
        }
        (Some(s), _) if s.limit.is_some() => {
            let r = s.limit.clone().unwrap_or_default();
            said.push(if r.is_empty() {
                "It's out of quota.".into()
            } else {
                format!("It's out of quota until {r}.")
            });
            todo.extend([
                ("Q", "queue for then"),
                ("C", "carry on with another agent"),
                ("a", "attach"),
            ]);
        }
        (Some(_), Kind::Done) => {
            said.push("It's done — its reply is in Fleet's preview.".into());
            todo.extend([
                ("enter", "read it"),
                ("s", "follow up"),
                ("Q", "queue next"),
            ]);
        }
        (Some(_), _) => todo.extend([("enter", "go to"), ("s", "message"), ("a", "attach")]),
        (None, Kind::Error) => {
            let (now_t, _) = now_state(d, it);
            said.push(format!(
                "{}{}",
                super::fleet::hint_for(&e.title),
                if now_t.is_empty() {
                    String::new()
                } else {
                    format!("  ({now_t})")
                }
            ));
            if it.n > 2 {
                said.push(format!(
                    "It dropped {} times in {} — a flaky link or VPN?",
                    it.n,
                    ui::dur(e.t.saturating_sub(it.first).max(60))
                ));
            }
            todo.extend([
                (super::digit(View::Settings), "settings › doctor"),
                ("enter", "machine"),
            ]);
        }
        (None, _) if !e.name.is_empty() => {
            said.push("That session isn't running any more.".into());
        }
        _ => todo.push(("enter", "go to")),
    }
    if live_s.is_some() {
        todo.extend([("o", "open in a new tab"), ("X", "close it")]);
    }
    if launch.is_some() {
        todo.extend([("r", "start again"), ("P", "save as preset")]);
    }
    if !e.name.is_empty() {
        todo.push(("N", "note"));
    }
    if !it.now {
        todo.push(if d.inbox.archive_view {
            ("x", "bring back")
        } else {
            ("x", "archive")
        });
        todo.push(("D", "delete for good"));
    }
    lines.push(section("What you can do", w));
    for s in said {
        for l in ui::wrap(&s, tw) {
            lines.push(Line::from(Span::raw(format!("   {l}"))));
        }
    }
    lines.extend(actions(&todo, w));
    lines.push(Line::raw(""));

    // ---- what happened
    let body = if detail.is_empty() { &e.title } else { &detail };
    // a finished turn: its tokens, $ and time (filled in once its usage is read)
    let turn = (e.kind == Kind::Done)
        .then(|| {
            let native = crate::turncost::native_of(e, live_s.as_ref());
            d.inbox.turns.get(&d.cfg, e, &native)
        })
        .flatten();
    if !body.trim().is_empty() || turn.is_some() {
        lines.push(section("What happened", w));
        for l in ui::wrap(body, tw).into_iter().take(4) {
            lines.push(Line::from(Span::styled(format!("   {l}"), ui::muted())));
        }
        if let Some(t) = &turn {
            lines.push(kv("this turn", turn_spans(t)));
        }
        lines.push(Line::raw(""));
    }

    // ---- your note
    if let Some(n) = crate::notes::get(&e.host, &e.name) {
        lines.push(section("Note", w));
        for l in ui::wrap(&n.text, tw).into_iter().take(3) {
            lines.push(Line::from(Span::styled(
                format!("   {l}"),
                ui::fg(c::ACCENT()),
            )));
        }
        lines.push(Line::raw(""));
    }

    // ---- how it started
    if let Some(l) = &launch {
        lines.push(section("Started with", w));
        let short = host.map_or(l.dir.clone(), |h| h.short_path(&l.dir));
        if !l.dir.is_empty() {
            lines.push(kv(
                "folder",
                vec![Span::raw(ui::trunc_left(&short, tw.saturating_sub(12)))],
            ));
        }
        let mut a = vec![
            icons::agent(&l.agent),
            Span::raw(format!(" {}", crate::agents::label(&l.agent))),
        ];
        if l.worktree {
            a.push(Span::styled("  in a new worktree", ui::muted()));
        }
        lines.push(kv("agent", a));
        if !l.args.is_empty() {
            lines.push(kv(
                "args",
                vec![Span::styled(
                    ui::trunc(&l.args.join(" "), tw.saturating_sub(12)),
                    ui::muted(),
                )],
            ));
        }
        if let Some(p) = l.prompt.as_deref().filter(|p| !p.is_empty()) {
            lines.push(kv(
                "prompt",
                vec![Span::styled(
                    ui::trunc(&format!("“{p}”"), tw.saturating_sub(12)),
                    ui::muted(),
                )],
            ));
        }
        lines.push(Line::from(vec![
            Span::styled("   $ ", ui::fg(c::ACCENT())),
            Span::styled(
                ui::trunc(&crate::history::command(l), tw.saturating_sub(3)),
                ui::bold(),
            ),
        ]));
        lines.push(Line::raw(""));
    }

    // ---- the machine
    lines.push(section("Machine", w));
    let mut m: Vec<Span> = vec![Span::raw("   ")];
    m.extend(icons::os_badge(&os, 0));
    m.push(Span::raw(" "));
    m.push(Span::styled(
        e.host.clone(),
        Style::new().fg(hcol).add_modifier(Modifier::BOLD),
    ));
    if let Some(dev) = device(d, &e.host) {
        m.push(Span::styled(format!("  ·  {dev}"), ui::muted()));
    }
    lines.push(Line::from(m));
    if !os.is_empty() {
        lines.push(kv("system", vec![Span::raw(super::fleet::os_pretty(&os))]));
    }
    if let Some(h) = host {
        if !h.addr.is_empty() {
            lines.push(kv(
                "address",
                vec![Span::styled(h.addr.clone(), ui::muted())],
            ));
        }
        let (state, scol) = match &h.conn {
            Conn::Up => (
                format!(
                    "● herdr {}{}",
                    h.herdr_version.clone().unwrap_or_default(),
                    h.latency
                        .map(|l| format!("  ·  {} ms", l.as_millis()))
                        .unwrap_or_default()
                ),
                c::GREEN(),
            ),
            Conn::Down(x) if crate::fleet::not_running(x) => {
                ("○ herdr not running".into(), c::MUTED())
            }
            Conn::Down(_) => ("✕ unreachable now".into(), c::RED()),
            Conn::Unknown => ("connecting…".into(), c::YELLOW()),
        };
        lines.push(kv("now", vec![Span::styled(state, ui::fg(scol))]));
        if !h.sessions.is_empty() {
            let t = super::fleet::tally(h.sessions.iter());
            let mut v = vec![Span::raw(format!("{}  ", h.sessions.len()))];
            for (g, n, label, col) in [
                (crate::look::needs(), t.blocked, "need you", c::NEEDS()),
                ("◷", t.limit, "at limit", c::LIMIT()),
                (ui::spinner(d.t0), t.working, "working", c::WORKING()),
                ("●", t.idle + t.done, "idle", c::MUTED()),
            ] {
                if n > 0 {
                    v.push(Span::styled(format!("{g} {n} {label}   "), ui::fg(col)));
                }
            }
            lines.push(kv("sessions", v));
        }
    }
    if let Some(st) = &st {
        if !st.gpus.is_empty() {
            let mut v = super::fleet::gpu_badge(st);
            if let Some(g) = st.gpus.first() {
                v.push(Span::styled(
                    format!(
                        "  ·  {:.0}% · {} VRAM",
                        g.util * 100.0,
                        ui::bytes(g.mem_total)
                    ),
                    ui::faint(),
                ));
            }
            lines.push(kv("gpu", v));
        }
    }
    lines.push(Line::raw(""));

    // ---- the agent and its session
    if !agent.is_empty() || !e.name.is_empty() {
        lines.push(section("Agent", w));
        if !agent.is_empty() {
            let mut v = vec![
                Span::raw("   "),
                icons::agent(&agent),
                Span::styled(format!(" {}", crate::agents::label(&agent)), ui::bold()),
            ];
            if let Some(ver) = d
                .agents
                .snapshot(&e.host)
                .and_then(|f| f.into_iter().find(|x| x.id == agent).map(|x| x.version))
                .filter(|v| !v.is_empty())
            {
                v.push(Span::styled(format!("  {ver}"), ui::faint()));
            }
            lines.push(Line::from(v));
        }
        if !e.name.is_empty() {
            let (t, c_) = now_state(d, it);
            lines.push(kv(
                "session",
                vec![
                    Span::styled(e.name.clone(), ui::bold()),
                    Span::styled(format!("   {t}"), ui::fg(c_)),
                ],
            ));
        }
        if let Some(s) = &live_s {
            if !s.cwd.is_empty() {
                let short = host.map_or(s.cwd.clone(), |h| h.short_path(&s.cwd));
                lines.push(kv(
                    "folder",
                    vec![Span::raw(ui::trunc_left(&short, tw.saturating_sub(12)))],
                ));
            }
            if !s.title.is_empty() {
                lines.push(kv(
                    "task",
                    vec![Span::styled(
                        ui::trunc(&s.title, tw.saturating_sub(12)),
                        ui::muted(),
                    )],
                ));
            }
            let q = crate::queue::for_session(&s.host, &s.name).len();
            if q > 0 {
                lines.push(kv(
                    "queued",
                    vec![Span::styled(
                        format!("≡ {q} message{} waiting", if q == 1 { "" } else { "s" }),
                        ui::fg(c::BLUE()),
                    )],
                ));
            }
        }
        lines.push(Line::raw(""));
    }

    // ---- the same from a shell
    let me = crate::doctor::me();
    let cmd = match (&live_s, e.kind) {
        (Some(s), _) if s.blocked() => format!("{me} approve {}:{}", s.host, s.name),
        (Some(s), _) => format!("{me} resume {}:{}", s.host, s.name),
        (None, Kind::Error) => format!("{me} --host {} doctor", e.host),
        _ => String::new(),
    };
    if !cmd.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("   $ ", ui::fg(c::ACCENT())),
            Span::styled(ui::trunc(&cmd, tw.saturating_sub(3)), ui::bold()),
        ]));
        lines.push(Line::raw(""));
    }

    // ---- earlier, for this session (or machine)
    let room = inner.height as usize;
    let earlier: Vec<&Event> = d
        .inbox
        .rows
        .iter()
        .filter(|x| x.host == e.host && x.name == e.name && x.t < it.first.min(e.t))
        .take(10)
        .collect();
    if !earlier.is_empty() && lines.len() + 3 <= room {
        lines.push(section("Earlier", w));
        for x in earlier {
            if lines.len() >= room {
                break;
            }
            let (g, col, what) = look(x.kind);
            let (h2, _) = reason(x);
            lines.push(Line::from(vec![
                Span::styled(format!("   {} ", ui::clock(x.t)), ui::faint()),
                Span::styled(ui::pad(&format!("{g} {what}"), 13), ui::fg(col)),
                Span::styled(ui::trunc(&h2, tw.saturating_sub(21)), ui::muted()),
            ]));
        }
    }
    lines.truncate(room);
    f.render_widget(Paragraph::new(lines), inner);
}

// ------------------------------------------------------------------ dismiss all

/// A row of `U`'s checklist.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Pick {
    All,
    Kind(Kind),
    /// whatever kind, from a session that has ended (ticked to begin with)
    Ended,
}

/// `U`: what to clear from the Inbox — All, each kind it has (with how
/// many), alerts from ended sessions (ticked to begin with) — then archive
/// them (`A` brings them back) or delete them for good.
pub(crate) struct DismissList {
    /// (what, how many, ticked)
    rows: Vec<(Pick, usize, bool)>,
    /// every row the Inbox has outside the archive: (its ids, kind, ended)
    items: Vec<(Vec<String>, Kind, bool)>,
    /// the cursor: a row, or `rows.len()` for the Archive / Delete switch
    at: usize,
    pub delete: bool,
}

/// What `U` clears, once you've said yes.
pub(crate) struct Dismiss {
    ids: Vec<String>,
    delete: bool,
    what: String,
}

impl DismissList {
    fn new(items: Vec<(Vec<String>, Kind, bool)>) -> DismissList {
        let mut rows = vec![(Pick::All, items.len(), false)];
        let order = [
            Kind::Blocked,
            Kind::Done,
            Kind::Limit,
            Kind::Error,
            Kind::HostUp,
            Kind::New,
            Kind::Gone,
            Kind::Auto,
            Kind::Task,
            Kind::Heal,
            Kind::Remote,
            Kind::Match,
        ];
        for k in order {
            let n = items.iter().filter(|x| x.1 == k).count();
            if n > 0 {
                rows.push((Pick::Kind(k), n, false));
            }
        }
        let ended = items.iter().filter(|x| x.2).count();
        if ended > 0 {
            rows.push((Pick::Ended, ended, true));
        }
        DismissList {
            rows,
            items,
            at: 0,
            delete: false,
        }
    }

    fn ticked(&self, p: Pick) -> bool {
        self.rows.iter().any(|r| r.0 == p && r.2)
    }

    /// The rows the ticks cover, and how many.
    fn chosen(&self) -> Vec<&(Vec<String>, Kind, bool)> {
        let all = self.ticked(Pick::All);
        let ended = self.ticked(Pick::Ended);
        self.items
            .iter()
            .filter(|(_, k, e)| all || self.ticked(Pick::Kind(*k)) || (ended && *e))
            .collect()
    }

    pub fn key(&mut self, k: KeyEvent) -> forms::Res {
        use forms::Res;
        let last = self.rows.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Res::Cancel,
            KeyCode::Enter => {
                return if self.chosen().is_empty() {
                    Res::Keep
                } else {
                    Res::Submit
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.at = self.at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => self.at = (self.at + 1).min(last),
            KeyCode::BackTab => self.at = self.at.saturating_sub(1),
            KeyCode::Home | KeyCode::Char('g') => self.at = 0,
            KeyCode::End | KeyCode::Char('G') => self.at = last,
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l')
                if self.at == last =>
            {
                self.delete = !self.delete
            }
            KeyCode::Char('d') => self.delete = !self.delete,
            KeyCode::Char(' ') if self.at == last => self.delete = !self.delete,
            KeyCode::Char(' ') => {
                let on = !self.rows[self.at].2;
                if self.rows[self.at].0 == Pick::All {
                    for r in &mut self.rows {
                        r.2 = on;
                    }
                } else {
                    self.rows[self.at].2 = on;
                    // All stays ticked only while everything is
                    let every = self.rows[1..].iter().all(|r| r.2);
                    self.rows[0].2 = every;
                }
            }
            KeyCode::Char('a') => {
                let on = !self.rows[0].2;
                for r in &mut self.rows {
                    r.2 = on;
                }
            }
            _ => {}
        }
        Res::Keep
    }

    pub fn draw(&self, _d: &Dash, f: &mut Frame, area: Rect) {
        let w = 76u16.min(area.width.saturating_sub(2));
        let h = (self.rows.len() as u16 + 8).min(area.height.saturating_sub(2));
        let r = ui::centered(area, w, h);
        crate::ui::clear(f, r);
        let block = ui::panel(Line::from("Dismiss alerts"), true);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let iw = inner.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        for (i, (p, n, on)) in self.rows.iter().enumerate() {
            let sel = i == self.at;
            let (glyph, col, name) = match p {
                Pick::All => ("◆", c::ACCENT(), "All"),
                Pick::Kind(k) => look(*k),
                Pick::Ended => ("○", c::MUTED(), "from ended sessions"),
            };
            let name = match p {
                Pick::Kind(_) => {
                    let mut c = name.chars();
                    c.next()
                        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                        .unwrap_or_default()
                }
                _ => name.to_string(),
            };
            let label = format!("{glyph} {name}");
            let count = format!("{n}");
            let pad = iw.saturating_sub(6 + ui::width(&label) + count.len() + 1);
            lines.push(Line::from(vec![
                Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
                Span::styled(
                    if *on { "[✓] " } else { "[ ] " },
                    if *on {
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                    } else {
                        ui::faint()
                    },
                ),
                Span::styled(format!("{glyph} "), ui::fg(col)),
                Span::styled(
                    name,
                    if sel {
                        ui::bold().fg(c::TEXT()).bg(c::SEL())
                    } else if *p == Pick::All {
                        ui::bold()
                    } else {
                        Style::new()
                    },
                ),
                Span::raw(" ".repeat(pad)),
                Span::styled(count, ui::muted()),
            ]));
            if *p == Pick::All {
                lines.push(Line::from(Span::styled(
                    format!(" {}", "─".repeat(iw.saturating_sub(2))),
                    ui::faint(),
                )));
            }
        }
        lines.push(Line::raw(""));
        // one button: what enter does — archive, or (d / ←→ / space on it)
        // delete for good instead
        let sel = self.at == self.rows.len();
        let n = self.chosen().len();
        let (label, col) = match (n, self.delete) {
            (0, _) => ("tick something first".to_string(), c::MUTED()),
            (n, false) => (
                format!("  ▣ Archive {n} alert{}  ", if n == 1 { "" } else { "s" }),
                c::ACCENT(),
            ),
            (n, true) => (format!("  ✕ Delete {n} for good  ",), c::RED()),
        };
        let button = if n == 0 {
            ui::faint()
        } else if sel {
            ui::bold().fg(c::BASE()).bg(col)
        } else {
            ui::fg(col).add_modifier(Modifier::BOLD).bg(c::SEL())
        };
        lines.push(Line::from(vec![
            Span::styled(if sel { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(label, button),
            Span::styled(
                if self.delete {
                    "   d: archive instead"
                } else {
                    "   d: delete instead"
                },
                ui::faint(),
            ),
        ]));
        lines.push(Line::raw(""));
        // the hints that fit
        let all: [(&str, &str); 4] = [
            ("space", "tick"),
            ("a", "all"),
            ("enter", "do it"),
            ("esc", "cancel"),
        ];
        let mut keep: Vec<(&str, &str)> = Vec::new();
        for h in all {
            keep.push(h);
            if ui::line_width(&ui::hints(&keep)) > iw {
                keep.pop();
                break;
            }
        }
        lines.push(ui::hints(&keep));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// `U`: the checklist (an empty Inbox → a note).
pub(crate) fn dismiss_list(d: &mut Dash) {
    let items: Vec<(Vec<String>, Kind, bool)> = d
        .inbox
        .merged
        .iter()
        .filter(|it| !it.now)
        .filter_map(|it| {
            let lf = life(d, &it.e);
            if archived(d, it, lf) {
                return None;
            }
            let ids = ids_of(d, it);
            (!ids.is_empty()).then_some((ids, it.e.kind, lf == Life::Ended))
        })
        .collect();
    if items.is_empty() {
        d.note("nothing to dismiss");
        return;
    }
    d.mode = Mode::DismissList(DismissList::new(items));
}

/// enter in the checklist: one confirm for all of them.
pub(crate) fn confirm_dismiss(d: &mut Dash, l: DismissList) {
    let chosen = l.chosen();
    let n = chosen.len();
    let mut by: Vec<(String, usize)> = Vec::new();
    for (_, k, _) in &chosen {
        let name = look(*k).2.to_string();
        match by.iter_mut().find(|x| x.0 == name) {
            Some(x) => x.1 += 1,
            None => by.push((name, 1)),
        }
    }
    let list = by
        .iter()
        .map(|(k, n)| format!("{n} {k}"))
        .collect::<Vec<_>>()
        .join(", ");
    let ids: Vec<String> = chosen.iter().flat_map(|x| x.0.clone()).collect();
    let what = format!("{n} alert{}", if n == 1 { "" } else { "s" });
    d.mode = Mode::Confirm {
        title: if l.delete {
            format!("Delete {what} for good?")
        } else {
            format!("Archive {what}?")
        },
        text: if l.delete {
            format!("{list}. This can't be undone.")
        } else {
            format!("{list} — A shows the archive, and x brings one back.")
        },
        on_yes: Pending::Dismiss(Dismiss {
            ids,
            delete: l.delete,
            what,
        }),
    };
}

/// After yes: archived (the dismissed set and launch marks) or deleted.
pub(crate) fn dismiss(d: &mut Dash, x: Dismiss) {
    let (launches, evs): (Vec<String>, Vec<String>) =
        x.ids.into_iter().partition(|i| i.starts_with("launch|"));
    if x.delete {
        let evs: HashSet<String> = evs.into_iter().collect();
        events::remove(&evs);
        crate::history::remove(&launches);
        reload(d);
        d.note(format!("{} deleted for good", x.what));
    } else {
        read_marks(d);
        for i in evs {
            d.inbox.kept.remove(&i);
            d.inbox.dismissed.insert(i);
        }
        for i in launches {
            d.inbox.kept.remove(&i);
            d.inbox.archived.insert(i);
        }
        events::save_dismissed(&d.inbox.dismissed, &events::load(events::CAP));
        let known: HashSet<String> = d
            .inbox
            .rows
            .iter()
            .map(id)
            .chain(d.inbox.launches.iter().map(crate::history::id))
            .collect();
        events::save_marks(&d.inbox.archived, &d.inbox.kept, |i| known.contains(i));
        rebuild(d);
        d.note(format!("{} archived — A shows the archive", x.what));
    }
    d.inbox.sel = d.inbox.sel.min(items(d).len().saturating_sub(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finished_turns_listed_as_inbox_finished_says() {
        let done = |t: u64, name: &str, took: u64, seen: bool| Event {
            t,
            host: "b31".into(),
            name: name.into(),
            kind: Kind::Done,
            title: "work".into(),
            took,
            seen,
            ..Default::default()
        };
        let q = Event {
            t: 900,
            host: "b31".into(),
            name: "a".into(),
            kind: Kind::Blocked,
            ..Default::default()
        };
        // newest first, as the Inbox holds them
        let rows = vec![
            q.clone(),
            done(800, "a", 600, false),
            done(700, "a", 30, true),
            done(600, "a", 30, false),
            done(500, "b", 900, true),
        ];
        let kinds = |v: &[Event]| {
            v.iter()
                .map(|e| (e.name.clone(), e.title.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            kinds(&finished_shown(&rows, "none", 120)),
            [("a".into(), String::new())]
        );
        // unseen: a's two unseen fold into one row, b's seen one is left out
        assert_eq!(
            kinds(&finished_shown(&rows, "unseen", 120)),
            [
                ("a".into(), String::new()),
                ("a".into(), "2× · work".into())
            ]
        );
        assert_eq!(
            kinds(&finished_shown(&rows, "unseen-long", 120)),
            [("a".into(), String::new()), ("a".into(), "work".into())]
        );
        assert_eq!(finished_shown(&rows, "all", 120).len(), 3);
    }

    #[test]
    fn errors_read_like_sentences() {
        let (h, d) =
            explain_error("ssh: connect to host 172.16.200.16 port 22: Operation timed out");
        assert_eq!(h, "unreachable — ssh timed out");
        assert!(d.starts_with("172.16.200.16:22 · connect"), "{d}");
        assert_eq!(
            explain_error("ssh: Could not resolve hostname gpu2: nodename nor servname provided").0,
            "unknown host — the name doesn't resolve"
        );
        assert_eq!(
            explain_error("ssh: connect to host h port 22: Connection refused").0,
            "ssh refused — sshd isn't listening"
        );
        assert_eq!(
            explain_error("Connection reset by 172.16.200.16 port 22").0,
            "the connection dropped"
        );
        assert_eq!(
            explain_error("tester@h: Permission denied (publickey).").0,
            "key login refused"
        );
        assert_eq!(explain_error("something odd").0, "error");
    }

    #[test]
    fn filters() {
        assert!(Show::Attention.keeps(Kind::Blocked) && !Show::Attention.keeps(Kind::Done));
        assert!(Show::Automation.keeps(Kind::Heal) && Show::Automation.keeps(Kind::Auto));
        assert!(Show::Problems.keeps(Kind::Error) && Show::Problems.keeps(Kind::Limit));
    }
}
