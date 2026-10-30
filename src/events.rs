//! The central notification inbox: an append-only
//! `~/.local/state/aleph/events.jsonl` that every process polling the
//! fleet (dashboard, `aleph watch`, `aleph ls`, …) writes to when a session
//! needs you, finishes, appears, disappears, or a machine drops off. Read by
//! `aleph inbox` (and the dashboard / watch).
//!
//! * One JSON object per line: `{t, host, name, kind, title}`.
//! * Capped at [`CAP`] lines: when the file grows past it, the newest lines are
//!   kept and written back atomically.
//! * Several processes may poll at once, so an event identical to one in the
//!   last [`DEDUP_SECS`] seconds is dropped.

use crate::cli::Args;
use crate::config;
use crate::fleet::Transition;
use crate::herdr::Session;
use crate::out::*;
use crate::theme;
use crate::ui::{self, c};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

pub const CAP: usize = 2000;
const DEDUP_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    Blocked,
    Done,
    New,
    Gone,
    #[default]
    Error,
    /// a rule answered the agent's question (`aleph auto`)
    Auto,
    /// a queued task went to a session (`aleph queue`)
    Task,
    /// herdr was restarted on a machine and its sessions restored
    Heal,
    /// an agent stopped at its plan's usage limit (`limits.rs`)
    Limit,
    /// a machine answers again
    HostUp,
    /// something done from a phone, Raycast or the web (the audit trail)
    Remote,
    /// a watch rule saw its text in a session's output (`aleph notify watch`)
    Match,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Blocked => "blocked",
            Kind::Done => "done",
            Kind::New => "new",
            Kind::Gone => "gone",
            Kind::Error => "error",
            Kind::Auto => "auto",
            Kind::Task => "task",
            Kind::Heal => "heal",
            Kind::Limit => "limit",
            Kind::HostUp => "up",
            Kind::Remote => "remote",
            Kind::Match => "match",
        }
    }
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "blocked" => Kind::Blocked,
            "done" => Kind::Done,
            "new" => Kind::New,
            "gone" => Kind::Gone,
            "error" => Kind::Error,
            "auto" => Kind::Auto,
            "task" => Kind::Task,
            "heal" => Kind::Heal,
            "limit" => Kind::Limit,
            "up" => Kind::HostUp,
            "remote" => Kind::Remote,
            "match" => Kind::Match,
            _ => return None,
        })
    }
    pub fn look(self) -> (&'static str, Color, &'static str) {
        match self {
            Kind::Blocked => ("▲", c::NEEDS(), "needs you"),
            Kind::Done => ("✓", c::TEAL(), "finished"),
            Kind::New => ("+", c::GREEN(), "started"),
            Kind::Gone => ("–", c::MUTED(), "gone"),
            Kind::Error => ("✕", c::YELLOW(), "unreachable"),
            Kind::Auto => ("↯", c::ACCENT(), "auto-approved"),
            Kind::Task => ("≡", c::BLUE(), "task sent"),
            Kind::Heal => ("↻", c::GREEN(), "herdr restarted"),
            Kind::Limit => ("◷", c::LIMIT(), "at its limit"),
            Kind::HostUp => ("●", c::GREEN(), "back up"),
            Kind::Remote => ("⇄", c::BLUE(), "remote action"),
            Kind::Match => ("◎", c::BLUE(), "matched"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Event {
    pub t: u64,
    pub host: String,
    /// session name (empty for machine events)
    pub name: String,
    pub kind: Kind,
    /// session title, folder or error text
    pub title: String,
    /// the agent (registry id or herdr's agent field); empty for machine events
    pub agent: String,
    /// who said so: empty (herdr's state), `agent` (its own hook), `aleph`
    pub source: String,
    /// more than the title, when there is more (an agent's own words)
    pub body: String,
    /// the pane, and herdr's state-change counter at the change (0: unknown):
    /// the same change seen by two hubs gets the same id
    pub pane: String,
    pub seq: u64,
    /// seconds it worked before this (a finished turn, a question); 0: unknown
    pub took: u64,
    /// the agent's own session id (herdr's `agent_session`, or the hook's
    /// `session_id`): which conversation it was, so its usage records can be
    /// found later (`turncost.rs`) even once the session has moved on or ended
    pub native: String,
    /// a finished turn that happened on a dashboard's screen, in front of
    /// you (`onscreen.rs`): no desktop banner, and `inbox.finished =
    /// unseen` leaves it out
    pub seen: bool,
}

impl Event {
    pub fn to_json(&self) -> Value {
        let mut v = json!({"t": self.t, "host": self.host, "name": self.name, "kind": self.kind.as_str(), "title": self.title, "agent": self.agent, "id": self.id()});
        if !self.source.is_empty() {
            v["source"] = json!(self.source);
        }
        if !self.body.is_empty() {
            v["body"] = json!(self.body);
        }
        if !self.pane.is_empty() && self.seq > 0 {
            v["pane"] = json!(self.pane);
            v["seq"] = json!(self.seq);
        }
        if self.took > 0 {
            v["took"] = json!(self.took);
        }
        if !self.native.is_empty() {
            v["native"] = json!(self.native);
        }
        if self.seen {
            v["seen"] = json!(true);
        }
        v
    }
    /// A stable id (the same event read twice is the same id). A state
    /// change herdr numbered is `host|pane|kind|seq`, so every hub that saw
    /// it agrees; others (and older events) are `t|host|name|kind`.
    pub fn id(&self) -> String {
        let key = if !self.pane.is_empty() && self.seq > 0 {
            format!(
                "{}|{}|{}|{}",
                self.host,
                self.pane,
                self.kind.as_str(),
                self.seq
            )
        } else {
            format!(
                "{}|{}|{}|{}",
                self.t,
                self.host,
                self.name,
                self.kind.as_str()
            )
        };
        let mut x: u64 = 0xcbf2_9ce4_8422_2325;
        for b in key.bytes() {
            x = (x ^ b as u64).wrapping_mul(0x100_0000_01b3);
        }
        format!("{x:016x}")
    }
    /// How loudly it should arrive.
    pub fn severity(&self) -> Severity {
        match self.kind {
            Kind::Blocked | Kind::Error => Severity::High,
            Kind::Limit | Kind::Done | Kind::HostUp | Kind::Remote | Kind::Match => {
                Severity::Normal
            }
            _ => Severity::Low,
        }
    }
    pub fn from_json(v: &Value) -> Option<Event> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        Some(Event {
            t: v.get("t").and_then(Value::as_u64)?,
            host: s("host"),
            name: s("name"),
            kind: Kind::parse(v.get("kind").and_then(Value::as_str)?)?,
            title: s("title"),
            agent: s("agent"),
            source: s("source"),
            body: s("body"),
            pane: s("pane"),
            seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
            took: v.get("took").and_then(Value::as_u64).unwrap_or(0),
            native: s("native"),
            seen: v.get("seen").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Normal,
    High,
}

pub fn file() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir()
            .join(format!("ch-events-test-{}", std::process::id()))
            .join("events.jsonl");
    }
    crate::config::world_state().join("events.jsonl")
}

/// Append events (deduplicated against the recent tail), trimming to [`CAP`].
pub fn append_all(evs: &[Event]) {
    append_to(&file(), evs, CAP);
}

pub fn append(e: Event) {
    append_all(&[e]);
}

fn append_to(path: &std::path::Path, evs: &[Event], cap: usize) {
    if evs.is_empty() {
        return;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let tail: Vec<Event> = existing
        .lines()
        .rev()
        .take(50)
        .filter_map(|l| serde_json::from_str(l).ok())
        .filter_map(|v| Event::from_json(&v))
        .collect();
    let fresh: Vec<&Event> = evs
        .iter()
        .filter(|e| {
            // herdr's and the agent's own word on the same thing both stay:
            // the router and the Inbox put them together (`pair`)
            !tail.iter().any(|o| {
                o.host == e.host
                    && o.name == e.name
                    && o.kind == e.kind
                    && o.source == e.source
                    // herdr numbers its state changes: another number is
                    // another question, however soon
                    && (o.seq == 0 || e.seq == 0 || o.seq == e.seq)
                    && e.t.abs_diff(o.t) < DEDUP_SECS
            })
        })
        .collect();
    if fresh.is_empty() {
        return;
    }
    let mut add = String::new();
    for e in fresh {
        add.push_str(&e.to_json().to_string());
        add.push('\n');
    }
    let lines = existing.lines().count() + add.lines().count();
    if lines > cap {
        // keep the newest `cap` lines, rewritten atomically
        let mut all: Vec<&str> = existing.lines().collect();
        all.extend(add.lines());
        let keep = &all[all.len() - cap..];
        let _ = config::write_atomic(path, (keep.join("\n") + "\n").as_bytes());
        return;
    }
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(add.as_bytes());
    }
}

// ------------------------------------------------------------------ inbox state
// (shared by the Inbox view and the API: read up to a time, dismissed ids)

fn seen_file() -> PathBuf {
    if cfg!(test) {
        return file().with_file_name("inbox_seen");
    }
    crate::config::world_state().join("inbox_seen")
}

fn dismissed_file() -> PathBuf {
    if cfg!(test) {
        return file().with_file_name("inbox_dismissed");
    }
    crate::config::world_state().join("inbox_dismissed")
}

/// The Inbox's own key for an event (what `inbox_dismissed` holds).
pub fn key(e: &Event) -> String {
    format!("{}|{}|{}|{}", e.t, e.host, e.name, e.kind.as_str())
}

/// When the Inbox's files last changed: the log, and its marks (read, archived,
/// kept). The hub, Raycast or another dashboard moving them shows here.
pub type Stamp = [Option<std::time::SystemTime>; 4];

pub fn stamp() -> Stamp {
    let at = |p: PathBuf| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    [
        at(file()),
        at(seen_file()),
        at(dismissed_file()),
        at(marks_file()),
    ]
}

/// Events up to this time are read.
pub fn seen() -> u64 {
    std::fs::read_to_string(seen_file())
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

pub fn set_seen(t: u64) {
    let _ = config::write_atomic(&seen_file(), format!("{t}\n").as_bytes());
}

/// Dismissed events (by `key`).
pub fn dismissed() -> std::collections::HashSet<String> {
    std::fs::read_to_string(dismissed_file())
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
}

/// Write the dismissed set, keeping only events still in the log.
pub fn save_dismissed(all: &std::collections::HashSet<String>, live: &[Event]) {
    let live: std::collections::HashSet<String> = live.iter().map(key).collect();
    let mut v: Vec<&String> = all.iter().filter(|k| live.contains(*k)).collect();
    v.sort();
    let body: String = v.iter().map(|s| format!("{s}\n")).collect();
    let _ = config::write_atomic(&dismissed_file(), body.as_bytes());
}

/// Archive events by their keys (`key`).
pub fn dismiss_keys(keys: &[String]) {
    if keys.is_empty() {
        return;
    }
    let evs = load(CAP);
    let mut all = dismissed();
    for k in keys {
        all.insert(k.clone());
    }
    save_dismissed(&all, &evs);
}

/// Once: the Inbox items the old rules filed that the new ones wouldn't —
/// every machine going down (whether or not an agent was at work) and
/// coming back up, and finished turns — go to the archive (nothing is
/// deleted; `A` shows them). A marker in the state folder keeps it once.
pub fn tidy_once() {
    let mark = if cfg!(test) {
        file().with_file_name("inbox-tidied-1")
    } else {
        crate::config::world_state().join("inbox-tidied-1")
    };
    if mark.exists() {
        return;
    }
    let evs = load(CAP);
    let keys: Vec<String> = evs
        .iter()
        .filter(|e| {
            e.kind == Kind::HostUp
                || e.kind == Kind::Done
                || (e.kind == Kind::Error && e.name.is_empty())
        })
        .map(key)
        .collect();
    dismiss_keys(&keys);
    let _ = config::write_atomic(&mark, format!("{}\n", keys.len()).as_bytes());
    if !keys.is_empty() {
        crate::log::info(
            "inbox",
            &format!(
                "{} old machine and finished items archived (once)",
                keys.len()
            ),
        );
    }
}

/// One outage row a machine a day: today's earlier ones from `host` are
/// archived (this one stands for them); their start times.
pub fn fold_outages(host: &str, now: u64) -> Vec<u64> {
    let today = crate::ui::local_day(now);
    let gone = dismissed();
    let earlier: Vec<Event> = load(CAP)
        .into_iter()
        .filter(|e| {
            e.kind == Kind::Error
                && e.host == host
                && e.name.is_empty()
                && crate::ui::local_day(e.t) == today
                && !gone.contains(&key(e))
        })
        .collect();
    dismiss_keys(&earlier.iter().map(key).collect::<Vec<_>>());
    earlier.iter().map(|e| e.t).collect()
}

/// Dismiss events by their ids (`Event::id`); how many matched.
pub fn dismiss_ids(ids: &[String]) -> usize {
    let evs = load(CAP);
    let mut all = dismissed();
    let mut n = 0;
    for e in &evs {
        if ids.contains(&e.id()) && all.insert(key(e)) {
            n += 1;
        }
    }
    save_dismissed(&all, &evs);
    n
}

/// "Needs you" events no longer worth a look (pure — tested), by `key`:
/// older than `ttl`, or — on a machine that just answered (`polled`) — its
/// session ended, was answered, or moved on to another question (which has
/// its own event). The live Needs-you lists still show a session that waits.
pub fn stale_questions(
    evs: &[Event],
    dismissed: &std::collections::HashSet<String>,
    sessions: &[&Session],
    polled: &dyn Fn(&str) -> bool,
    now: u64,
    ttl: Option<u64>,
) -> Vec<String> {
    evs.iter()
        .filter(|e| e.kind == Kind::Blocked && !dismissed.contains(&key(e)))
        .filter(|e| {
            if ttl.is_some_and(|ttl| now.saturating_sub(e.t) >= ttl) {
                return true;
            }
            if !polled(&e.host) {
                return false;
            }
            let s = sessions.iter().find(|s| {
                s.host == e.host
                    && ((!e.pane.is_empty() && s.pane_id == e.pane)
                        || (!e.name.is_empty() && name_of(s) == e.name))
            });
            match s {
                None => true,
                Some(s) if !s.blocked() => true,
                Some(s) => s.seq > 0 && e.seq > 0 && s.seq != e.seq,
            }
        })
        .map(key)
        .collect()
}

/// Delete events for good (the Inbox's `D`): the log is written back
/// without them. How many went.
pub fn remove(keys: &std::collections::HashSet<String>) -> usize {
    let path = file();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut n = 0;
    let keep: Vec<&str> = existing
        .lines()
        .filter(|l| {
            let gone = serde_json::from_str::<Value>(l)
                .ok()
                .and_then(|v| Event::from_json(&v))
                .is_some_and(|e| keys.contains(&key(&e)));
            n += gone as usize;
            !gone
        })
        .collect();
    if n > 0 {
        let body = if keep.is_empty() {
            String::new()
        } else {
            keep.join("\n") + "\n"
        };
        let _ = config::write_atomic(&path, body.as_bytes());
    }
    n
}

fn marks_file() -> PathBuf {
    if cfg!(test) {
        return file().with_file_name("inbox_marks");
    }
    crate::config::world_state().join("inbox_marks")
}

/// The Inbox's archive for what isn't an event in the log (launches, by
/// `history::id`), and what you brought back from the archive, which the
/// automatic archiving leaves alone from then on: (archived, kept).
pub fn marks() -> (
    std::collections::HashSet<String>,
    std::collections::HashSet<String>,
) {
    let mut a = std::collections::HashSet::new();
    let mut k = std::collections::HashSet::new();
    for l in std::fs::read_to_string(marks_file())
        .unwrap_or_default()
        .lines()
    {
        match l.split_once(' ') {
            Some(("archived", id)) => {
                a.insert(id.to_string());
            }
            Some(("kept", id)) => {
                k.insert(id.to_string());
            }
            _ => {}
        }
    }
    (a, k)
}

/// Write the marks back, keeping only ids `exists` still knows.
pub fn save_marks(
    archived: &std::collections::HashSet<String>,
    kept: &std::collections::HashSet<String>,
    exists: impl Fn(&str) -> bool,
) {
    let mut v: Vec<String> = archived
        .iter()
        .filter(|i| exists(i))
        .map(|i| format!("archived {i}"))
        .chain(
            kept.iter()
                .filter(|i| exists(i))
                .map(|i| format!("kept {i}")),
        )
        .collect();
    v.sort();
    let body: String = v.iter().map(|s| format!("{s}\n")).collect();
    let _ = config::write_atomic(&marks_file(), body.as_bytes());
}

/// The newest `limit` events, newest first.
pub fn load(limit: usize) -> Vec<Event> {
    load_from(&file(), limit)
}

/// The same question or finished turn, told by herdr and by the agent's own
/// hook (pure — tested)?
pub fn same_news(a: &Event, b: &Event) -> bool {
    a.host == b.host
        && a.kind == b.kind
        && matches!(a.kind, Kind::Blocked | Kind::Done)
        && (a.source == "agent") != (b.source == "agent")
        && ((!a.name.is_empty() && a.name == b.name) || (!a.pane.is_empty() && a.pane == b.pane))
        && a.t.abs_diff(b.t) <= 30
}

/// One event from both: the agent's words, herdr's numbers and id (pure).
pub fn pair(a: Event, b: Event) -> Event {
    let (mut agent, herdr) = if a.source == "agent" { (a, b) } else { (b, a) };
    if agent.name.is_empty() {
        agent.name = herdr.name.clone();
    }
    agent.t = herdr.t;
    agent.took = agent.took.max(herdr.took);
    if agent.native.is_empty() {
        agent.native = herdr.native;
    }
    agent.pane = herdr.pane;
    agent.seq = herdr.seq;
    agent
}

/// Events for people to read: each pair told twice shown once (`pair`).
pub fn merged(v: Vec<Event>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::with_capacity(v.len());
    for e in v {
        match out.iter().position(|o| same_news(o, &e)) {
            Some(i) => {
                let o = out.remove(i);
                out.insert(i, pair(o, e));
            }
            None => out.push(e),
        }
    }
    out
}

/// The Inbox's events: `load`, each pair shown once.
pub fn load_inbox(limit: usize) -> Vec<Event> {
    merged(load(limit))
}

fn load_from(path: &std::path::Path, limit: usize) -> Vec<Event> {
    let s = std::fs::read_to_string(path).unwrap_or_default();
    s.lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter_map(|v| Event::from_json(&v))
        .take(limit)
        .collect()
}

pub fn name_of(s: &Session) -> String {
    if s.name.is_empty() {
        s.display().to_string()
    } else {
        s.name.clone()
    }
}

/// The fleet's hook after a successful poll of `host`: `before` is what the
/// fleet knew (this run or the on-disk snapshot, fetched at `fetched`), `now`
/// the fresh list. A day-old snapshot isn't compared (too stale to say what
/// started or ended).
pub fn on_poll(
    host: &str,
    t: u64,
    fetched: Option<u64>,
    before: &[Session],
    now: &[Session],
    trans: &[Transition],
) {
    let fresh = fetched.is_some_and(|f| t.saturating_sub(f) < 86400);
    let (new, gone): (Vec<Session>, Vec<Session>) = if fresh {
        (
            now.iter()
                .filter(|s| !before.iter().any(|o| o.pane_id == s.pane_id))
                .cloned()
                .collect(),
            before
                .iter()
                .filter(|o| !now.iter().any(|s| s.pane_id == o.pane_id))
                .cloned()
                .collect(),
        )
    } else {
        (vec![], vec![])
    };
    append_all(&from_poll(host, t, trans, &new, &gone));
}

/// Inbox events for one poll: new sessions, transitions into blocked,
/// working → done/idle, and sessions that disappeared.
pub fn from_poll(
    host: &str,
    t: u64,
    trans: &[Transition],
    new: &[Session],
    gone: &[Session],
) -> Vec<Event> {
    let mut v = Vec::new();
    for s in new {
        v.push(Event {
            t,
            host: host.to_string(),
            name: name_of(s),
            agent: s.agent.clone(),
            kind: Kind::New,
            title: if s.title.is_empty() {
                s.cwd.clone()
            } else {
                s.title.clone()
            },
            native: s.native_id.clone().unwrap_or_default(),
            ..Default::default()
        });
    }
    for tr in trans {
        let s = &tr.session;
        let kind = match (tr.from.as_str(), s.status.as_str()) {
            ("", _) => continue,
            (_, "blocked") => Kind::Blocked,
            ("working", "done" | "idle") => Kind::Done,
            _ => continue,
        };
        v.push(Event {
            t,
            host: host.to_string(),
            name: name_of(s),
            agent: s.agent.clone(),
            kind,
            title: if s.title.is_empty() {
                s.cwd.clone()
            } else {
                s.title.clone()
            },
            pane: s.pane_id.clone(),
            seq: s.seq,
            took: if tr.from == "working" { tr.took } else { 0 },
            native: s.native_id.clone().unwrap_or_default(),
            seen: kind == Kind::Done && crate::onscreen::showing(host, &name_of(s)),
            ..Default::default()
        });
    }
    for s in gone {
        v.push(Event {
            t,
            host: host.to_string(),
            name: name_of(s),
            agent: s.agent.clone(),
            kind: Kind::Gone,
            title: s.cwd.clone(),
            ..Default::default()
        });
    }
    v
}

// ------------------------------------------------------------------ aleph inbox

pub fn cmd(a: Args) -> ExitCode {
    // `aleph inbox read [T]` / `dismiss ID…`: what the API's inbox.read and
    // inbox.dismiss do, for scripts and Raycast without a daemon
    match a.rest.first().map(String::as_str) {
        Some("read") => {
            let t = a
                .rest
                .get(1)
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(crate::cache::now);
            set_seen(t);
            if a.rest.iter().any(|x| x == "--json") {
                json(&json!({"seen": t}));
            } else {
                ok("marked read");
            }
            return ExitCode::SUCCESS;
        }
        Some("dismiss") => {
            let ids: Vec<String> = a
                .rest
                .iter()
                .skip(1)
                .filter(|x| !x.starts_with("--"))
                .cloned()
                .collect();
            if ids.is_empty() {
                return fail(
                    "usage: aleph inbox dismiss <event id>… (ids from aleph inbox --json)",
                );
            }
            let n = dismiss_ids(&ids);
            if a.rest.iter().any(|x| x == "--json") {
                json(&json!({"dismissed": n}));
            } else {
                ok(&format!("{n} archived"));
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let n = a
        .rest
        .iter()
        .position(|x| x == "-n")
        .and_then(|i| a.rest.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let host = a.flag.clone();
    // the archive (dismissed in the Inbox) stays out unless --all
    let all = a.rest.iter().any(|x| x == "--all");
    let gone = dismissed();
    let evs: Vec<Event> = load_inbox(CAP)
        .into_iter()
        .filter(|e| host.as_ref().is_none_or(|h| *h == e.host))
        .filter(|e| all || !gone.contains(&key(e)))
        .take(n)
        .collect();
    if a.rest.iter().any(|x| x == "--json") {
        // as the API's inbox.list says it: unread is after the read mark
        let seen = seen();
        json(&Value::Array(
            evs.iter()
                .map(|e| {
                    let mut v = e.to_json();
                    v["unread"] = json!(e.t > seen);
                    if gone.contains(&key(e)) {
                        v["archived"] = json!(true);
                    }
                    v
                })
                .collect(),
        ));
        return ExitCode::SUCCESS;
    }
    if !io::stdout().is_terminal() {
        for e in &evs {
            crate::out::raw(&format!(
                "{}\t{}\t{}\t{}\t{}",
                e.t,
                e.host,
                e.name,
                e.kind.as_str(),
                e.title
            ));
        }
        return ExitCode::SUCCESS;
    }
    if evs.is_empty() {
        say(Line::from(vec![
            Span::styled("⏺ ", ui::fg(c::ACCENT())),
            Span::raw("The inbox is empty"),
        ]));
        hint("events appear while the dashboard or aleph watch is running (sessions that need you, finish, start or end)");
        return ExitCode::SUCCESS;
    }
    let cfg = &a.cfg;
    let now = crate::cache::now();
    let wh = evs.iter().map(|e| ui::width(&e.host)).max().unwrap_or(4);
    let wn = evs
        .iter()
        .map(|e| ui::width(&e.name))
        .max()
        .unwrap_or(6)
        .min(28);
    out(Line::from(vec![
        Span::styled("⏺ ", ui::fg(c::ACCENT())),
        Span::styled(
            format!(
                "{} event{}",
                evs.len(),
                if evs.len() == 1 { "" } else { "s" }
            ),
            ui::bold(),
        ),
        Span::styled("  · newest first", ui::faint()),
    ]));
    for e in &evs {
        let (g, col, word) = e.kind.look();
        out(Line::from(vec![
            Span::styled(
                format!("  {:>8}  ", ui::ago(now.saturating_sub(e.t))),
                ui::faint(),
            ),
            Span::styled("▌", Style::new().fg(theme::machine(cfg, &e.host))),
            Span::styled(ui::pad(&e.host, wh), ui::muted()),
            Span::raw("  "),
            Span::styled(format!("{g} "), ui::fg(col).add_modifier(Modifier::BOLD)),
            Span::styled(ui::pad(word, 11), ui::fg(col)),
            Span::styled(ui::pad(&e.name, wn), ui::bold()),
            Span::styled(
                if e.title.is_empty() {
                    String::new()
                } else {
                    format!("  {}", ui::trunc(&e.title, 50))
                },
                ui::faint(),
            ),
        ]));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(t: u64, name: &str, kind: Kind) -> Event {
        Event {
            t,
            host: "b31".into(),
            name: name.into(),
            kind,
            title: "Fix tests".into(),
            agent: "claude".into(),
            ..Default::default()
        }
    }

    #[test]
    fn stale_questions_go() {
        let q = |t: u64, name: &str, pane: &str, seq: u64| Event {
            pane: pane.into(),
            seq,
            ..ev(t, name, Kind::Blocked)
        };
        let s = |name: &str, pane: &str, status: &str, seq: u64| Session {
            host: "b31".into(),
            name: name.into(),
            pane_id: pane.into(),
            status: status.into(),
            seq,
            ..Default::default()
        };
        let evs = vec![
            q(1000, "temper", "w1:p1", 7), // still waiting on that question
            q(1000, "ignite", "w1:p2", 3), // answered: working now
            q(1000, "weave", "w1:p3", 0),  // ended
            q(1000, "quill", "w1:p4", 5),  // a new question since (seq 6)
            q(100, "loom", "w1:p5", 2),    // old: past the ttl, still waiting
            ev(1000, "shell", Kind::Done), // not a question
        ];
        let live = [
            s("temper", "w1:p1", "blocked", 7),
            s("ignite", "w1:p2", "working", 0),
            s("quill", "w1:p4", "blocked", 6),
            s("loom", "w1:p5", "blocked", 2),
        ];
        let live: Vec<&Session> = live.iter().collect();
        let none = std::collections::HashSet::new();
        let names = |v: Vec<String>| -> Vec<String> {
            v.iter()
                .map(|k| k.split('|').nth(2).unwrap_or("").to_string())
                .collect()
        };
        let up = |_: &str| true;
        assert_eq!(
            names(stale_questions(&evs, &none, &live, &up, 1100, Some(300))),
            ["ignite", "weave", "quill", "loom"]
        );
        // a machine that didn't answer: only the age counts
        let down = |_: &str| false;
        assert_eq!(
            names(stale_questions(&evs, &none, &live, &down, 1100, Some(300))),
            ["loom"]
        );
        assert!(stale_questions(&evs, &none, &live, &down, 1100, None).is_empty());
        // already dismissed: not again
        let gone: std::collections::HashSet<String> = [key(&evs[1])].into();
        assert!(!names(stale_questions(&evs, &gone, &live, &up, 1100, None))
            .contains(&"ignite".to_string()));
    }

    #[test]
    fn ids_from_herdrs_counter() {
        let mut a = ev(100, "temper", Kind::Blocked);
        let old = a.id();
        // a numbered change: every hub agrees, whenever it saw it
        a.pane = "w1:p2".into();
        a.seq = 41;
        let mut b = a.clone();
        b.t = 107;
        assert_eq!(a.id(), b.id());
        assert_ne!(a.id(), old);
        b.seq = 42;
        assert_ne!(a.id(), b.id());
        // and it survives the log
        let back = Event::from_json(&a.to_json()).unwrap();
        assert_eq!(back.id(), a.id());
        // older events keep their ids (and the Inbox's keys never change)
        let mut c = a.clone();
        c.seq = 0;
        assert_eq!(c.id(), old);
        assert_eq!(key(&a), key(&c));
    }

    #[test]
    fn herdr_and_the_agent_both_kept_and_shown_once() {
        let p = std::env::temp_dir().join(format!("aleph-ev-pair-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        let herdr = Event {
            t: 100,
            host: "b31".into(),
            name: "heron".into(),
            kind: Kind::Done,
            title: "Fix the flaky test".into(),
            pane: "w1:p2".into(),
            seq: 9,
            took: 300,
            ..Default::default()
        };
        let agent = Event {
            t: 103,
            source: "agent".into(),
            title: "Fixed it. All 41 pass.".into(),
            body: "Fixed it. All 41 pass.".into(),
            seq: 0,
            took: 0,
            ..herdr.clone()
        };
        append_to(&p, std::slice::from_ref(&herdr), 50);
        append_to(&p, std::slice::from_ref(&agent), 50);
        // herdr's twice is once; the agent's word on it stays
        append_to(&p, std::slice::from_ref(&herdr), 50);
        let all = load_from(&p, 10);
        assert_eq!(all.len(), 2);
        // a second question soon after (herdr's next number) is news
        let next = Event {
            t: 130,
            kind: Kind::Blocked,
            seq: 11,
            ..herdr.clone()
        };
        let again = Event {
            t: 150,
            seq: 13,
            ..next.clone()
        };
        append_to(&p, &[next, again], 50);
        assert_eq!(load_from(&p, 10).len(), 4);
        let _ = std::fs::remove_file(&p);
        append_to(&p, std::slice::from_ref(&herdr), 50);
        append_to(&p, std::slice::from_ref(&agent), 50);
        let all = load_from(&p, 10);
        let shown = merged(all);
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].title.as_str(), shown[0].took, shown[0].id()),
            ("Fixed it. All 41 pass.", 300, herdr.id())
        );
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn append_dedup_trim_load() {
        let dir = std::env::temp_dir().join(format!("ch-ev-{}", std::process::id()));
        let p = dir.join("events.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        append_to(&p, &[ev(100, "ignite", Kind::Blocked)], 5);
        // same event seconds later from another process: dropped
        append_to(&p, &[ev(110, "ignite", Kind::Blocked)], 5);
        assert_eq!(load_from(&p, 10).len(), 1);
        for i in 0..8 {
            append_to(&p, &[ev(1000 + i * 100, &format!("s{i}"), Kind::Done)], 5);
        }
        let v = load_from(&p, 100);
        assert_eq!(v.len(), 5);
        assert_eq!(v[0].name, "s7"); // newest first
        assert_eq!(
            std::fs::read_to_string(&p).unwrap().lines().count(),
            5,
            "trimmed to the cap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn poll_events() {
        let s = |st: &str| Session {
            label: "claude_ignite:px".into(),
            status: st.into(),
            cwd: "/srv/px".into(),
            ..Default::default()
        };
        let tr = |from: &str, to: &str| Transition {
            session: s(to),
            from: from.into(),
            took: 0,
        };
        let v = from_poll(
            "b31",
            5,
            &[
                tr("working", "blocked"),
                tr("working", "idle"),
                tr("", "working"),
                tr("blocked", "working"),
            ],
            &[s("working")],
            &[s("idle")],
        );
        let kinds: Vec<Kind> = v.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::New, Kind::Blocked, Kind::Done, Kind::Gone]
        );
        assert_eq!(v[1].name, "claude_ignite");
        assert_eq!(v[1].title, "/srv/px");
    }

    #[test]
    fn json_roundtrip() {
        let e = ev(42, "x", Kind::Error);
        assert_eq!(Event::from_json(&e.to_json()), Some(e));
        assert!(Event::from_json(&json!({"t": 1, "kind": "weird"})).is_none());
        // the agent's session id: written only when known, read back, and
        // older lines without it still read
        let mut d = ev(43, "x", Kind::Done);
        assert!(d.to_json().get("native").is_none());
        d.native = "5f0c-uuid".into();
        assert_eq!(d.to_json()["native"], "5f0c-uuid");
        assert_eq!(Event::from_json(&d.to_json()), Some(d));
        let old = Event::from_json(&json!({"t": 1, "host": "b31", "name": "x", "kind": "done"}));
        assert_eq!(old.map(|e| e.native), Some(String::new()));
    }

    #[test]
    fn a_finished_turn_knows_its_conversation() {
        let s = Session {
            label: "claude_ignite".into(),
            status: "idle".into(),
            native_id: Some("5f0c-uuid".into()),
            ..Default::default()
        };
        let v = from_poll(
            "b31",
            5,
            &[Transition {
                session: s,
                from: "working".into(),
                took: 360,
            }],
            &[],
            &[],
        );
        assert_eq!(
            (v[0].kind, v[0].took, v[0].native.as_str()),
            (Kind::Done, 360, "5f0c-uuid")
        );
        // paired with the agent's own word: its id when it gave one, else herdr's
        let hook = Event {
            source: "agent".into(),
            ..v[0].clone()
        };
        let no_id = Event {
            native: String::new(),
            ..hook.clone()
        };
        assert_eq!(pair(no_id, v[0].clone()).native, "5f0c-uuid");
    }
}
