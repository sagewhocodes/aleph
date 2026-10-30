//! Closing a session after a grace (`close.grace`, 5 s by default): the
//! agent keeps running until then, so an undo loses nothing.
//!
//! Every client asks the same way. The dashboard (`x`), `aleph close` and
//! the API's `session.close` (Raycast, the web console) put the session in
//! `state/closing.json`: its deadline, machine, pane, name and whether its
//! worktree goes too. Whoever holds the hub lock carries the closes out
//! once their deadline passes (`Closer::tick`, from `Hub::tick`). While
//! nobody holds it, a process that looks (a dashboard that isn't the hub, a
//! daemon whose `hub.machine` is another computer, `aleph close` waiting
//! out its grace) takes the lock for the moment it claims them, so two
//! processes never both close. An undo (`u`, `aleph close --undo`,
//! `session.close.undo`) takes it out of the file before then.
//!
//! * a session that ends by itself during the grace is just dropped;
//! * closing one that's already pending sets its deadline again;
//! * the file outlives whoever asked: a dashboard that quits with a close
//!   pending and nobody to hand it to waits it out first ([`on_quit`]);
//! * a close nobody carried out within [`STALE_MS`] of its deadline is
//!   dropped, not run late (the Inbox says so).

use crate::cli::Args;
use crate::config::{write_atomic, Config, Target};
use crate::fleet::{Conn, Fleet};
use crate::herdr::{self, Herdr, Session};
use crate::out::{err, fail, hint, json, ok, warn};
use serde_json::{json, Value};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

/// The grace when `close.grace` doesn't parse.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(5);
/// The longest grace there is.
const MAX_GRACE: Duration = Duration::from_secs(3600);
/// A close still waiting this long after its deadline (nothing ran to carry
/// it out, or its machine didn't answer) is dropped rather than run late.
pub const STALE_MS: u64 = 10 * 60 * 1000;
/// A close under way stays listed (the dashboard says "closing…") until its
/// session is gone from the fleet, or this long.
const CLAIM_KEEP_MS: u64 = 30_000;
/// The longest a quitting dashboard waits out a grace (`on_quit`).
const QUIT_WAIT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Entry {
    /// the machine (ssh alias)
    pub host: String,
    pub pane: String,
    /// herdr's name for it (`aleph close --undo <name>`)
    pub name: String,
    /// what we call it everywhere
    pub display: String,
    pub agent: String,
    /// herdr's workspace (its worktree, with `worktree`)
    pub workspace: String,
    /// its git worktree checkout goes too (`aleph close --worktree`)
    pub worktree: bool,
    /// unix ms: it closes then
    pub deadline: u64,
    /// unix ms: when it was asked (the last time)
    pub added: u64,
    /// the closes asked together share it (an undo keeps the last batch)
    pub batch: u64,
    /// who asked: dashboard · cli · api (who)
    pub by: String,
    /// the process that asked
    pub pid: u32,
    /// unix ms: when an executor took it (the close is under way)
    pub claimed: Option<u64>,
}

impl Entry {
    pub fn of(s: &Session, deadline: u64, now: u64, worktree: bool, by: &str) -> Entry {
        Entry {
            host: s.host.clone(),
            pane: s.pane_id.clone(),
            name: s.name.clone(),
            display: s.display().to_string(),
            agent: s.agent.clone(),
            workspace: s.workspace_id.clone(),
            worktree,
            deadline,
            added: now,
            batch: now,
            by: by.to_string(),
            pid: std::process::id(),
            claimed: None,
        }
    }

    /// Whole seconds left (rounded up; 0 once it's due).
    pub fn left(&self, now: u64) -> u64 {
        self.deadline.saturating_sub(now).div_ceil(1000)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "host": self.host, "pane": self.pane, "name": self.name, "display": self.display,
            "agent": self.agent, "workspace": self.workspace, "worktree": self.worktree,
            "deadline": self.deadline, "added": self.added, "batch": self.batch, "by": self.by,
            "pid": self.pid, "claimed": self.claimed,
        })
    }

    fn from_json(v: &Value) -> Option<Entry> {
        let s = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let n = |k: &str| v.get(k).and_then(Value::as_u64);
        let e = Entry {
            host: s("host"),
            pane: s("pane"),
            name: s("name"),
            display: s("display"),
            agent: s("agent"),
            workspace: s("workspace"),
            worktree: v.get("worktree").and_then(Value::as_bool).unwrap_or(false),
            deadline: n("deadline")?,
            added: n("added").unwrap_or(0),
            batch: n("batch").unwrap_or(0),
            by: s("by"),
            pid: n("pid").unwrap_or(0) as u32,
            claimed: n("claimed"),
        };
        (!e.host.is_empty() && !e.pane.is_empty()).then_some(e)
    }

    /// Does `q` name it: its name, what we call it, its pane, or any of
    /// those after `host:`?
    pub fn matches(&self, q: &str) -> bool {
        let q = q.trim();
        let mine = |x: &str| x == self.name || x == self.display || x == self.pane;
        match q.split_once(':') {
            Some((h, rest)) if h == self.host => mine(rest),
            // herdr's own pane ids have a colon (`w1:p4`)
            _ => mine(q),
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `close.grace` as a duration: `5s`, `500ms`, `1m`; `0` / `off` / `now`
/// close at once. None: not one of those.
pub fn parse_grace(v: &str) -> Option<Duration> {
    let v = v.trim();
    if matches!(v, "" | "0" | "off" | "now" | "none") {
        return Some(Duration::ZERO);
    }
    crate::log::parse_duration(v).map(|d| d.min(MAX_GRACE))
}

/// How long a close waits (zero: it closes at once).
pub fn grace(cfg: &Config) -> Duration {
    parse_grace(&cfg.get_or("close.grace")).unwrap_or(DEFAULT_GRACE)
}

/// "5s", "1m 30s", "500ms": a grace for people.
pub fn grace_text(g: Duration) -> String {
    let ms = g.as_millis() as u64;
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let s = ms.div_ceil(1000);
    match (s / 60, s % 60) {
        (0, s) => format!("{s}s"),
        (m, 0) => format!("{m}m"),
        (m, s) => format!("{m}m {s}s"),
    }
}

// ------------------------------------------------------------------ the file

pub fn file() -> PathBuf {
    crate::config::world_state().join("closing.json")
}

/// The file's shape: `{"v": 1, "closing": […]}`.
const VERSION: u64 = 1;

fn parse(text: &str) -> Vec<Entry> {
    serde_json::from_str::<Value>(text)
        .ok()
        .as_ref()
        .and_then(|v| v.get("closing")?.as_array().cloned())
        .map(|a| a.iter().filter_map(Entry::from_json).collect())
        .unwrap_or_default()
}

fn load_at(p: &Path) -> Vec<Entry> {
    std::fs::read_to_string(p)
        .map(|t| parse(&t))
        .unwrap_or_default()
}

pub fn load() -> Vec<Entry> {
    load_at(&file())
}

/// Read-modify-write under a lock file (the dashboard, the daemon and the
/// CLI all write it). An empty list leaves no file behind.
fn update_at<R>(p: &Path, f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    let lock = p.with_extension("lock");
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    for _ in 0..40 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
        {
            Ok(_) => break,
            Err(_) => {
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|e| e > Duration::from_secs(10));
                if stale {
                    let _ = std::fs::remove_file(&lock);
                } else {
                    std::thread::sleep(Duration::from_millis(25));
                }
            }
        }
    }
    let mut v = load_at(p);
    let before = v.clone();
    let r = f(&mut v);
    if v != before {
        if v.is_empty() {
            let _ = std::fs::remove_file(p);
        } else {
            let data =
                json!({"v": VERSION, "closing": v.iter().map(Entry::to_json).collect::<Vec<_>>()});
            let _ = write_atomic(p, data.to_string().as_bytes());
        }
    }
    let _ = std::fs::remove_file(&lock);
    r
}

fn update<R>(f: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
    update_at(&file(), f)
}

/// The closes pending now — cheap enough for every frame (read again only
/// when the file changes).
pub fn current() -> Vec<Entry> {
    /// (the file, its modification time, what it said)
    type Seen = Option<(PathBuf, Option<SystemTime>, Vec<Entry>)>;
    static CACHE: Mutex<Seen> = Mutex::new(None);
    let p = file();
    let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
    let mut g = CACHE.lock().unwrap();
    let fresh = g
        .as_ref()
        .is_some_and(|(cp, t, _)| *cp == p && *t == mtime && mtime.is_some());
    if !fresh {
        let v = if mtime.is_some() {
            load_at(&p)
        } else {
            Vec::new()
        };
        *g = Some((p, mtime, v));
    }
    g.as_ref().map(|x| x.2.clone()).unwrap_or_default()
}

/// The close pending for this session, if any.
pub fn pending(host: &str, pane: &str) -> Option<Entry> {
    current()
        .into_iter()
        .find(|e| e.host == host && e.pane == pane)
}

// ------------------------------------------------------------------ asking, undoing (pure parts)

/// Put `e` in the list; one already there for the same session gets its
/// deadline set again (unless its close is under way).
pub fn put(list: &mut Vec<Entry>, e: Entry) {
    match list
        .iter_mut()
        .find(|x| x.host == e.host && x.pane == e.pane)
    {
        Some(x) if x.claimed.is_some() => {}
        Some(x) => *x = e,
        None => list.push(e),
    }
}

/// Which closes an undo means.
#[derive(Clone, Copy, Debug)]
pub enum Sel<'a> {
    /// this session (machine, pane)
    Session(&'a str, &'a str),
    /// by name: `ignite`, `b31:ignite`, a pane id
    Query(&'a str),
    /// the last ones asked (one batch: sessions closed together)
    Latest,
}

/// The indices `sel` picks in `list`.
pub fn pick(list: &[Entry], sel: Sel) -> Vec<usize> {
    let idx = |f: &dyn Fn(&Entry) -> bool| -> Vec<usize> {
        list.iter()
            .enumerate()
            .filter(|(_, e)| f(e))
            .map(|(i, _)| i)
            .collect()
    };
    match sel {
        Sel::Session(h, p) => idx(&|e| e.host == h && e.pane == p),
        Sel::Query(q) => idx(&|e| e.matches(q)),
        Sel::Latest => {
            let Some(b) = list
                .iter()
                .filter(|e| e.claimed.is_none())
                .map(|e| e.batch)
                .max()
            else {
                return Vec::new();
            };
            idx(&|e| e.claimed.is_none() && e.batch == b)
        }
    }
}

/// Close these sessions after `grace`: each goes in the file (or has its
/// deadline set again). The entries as written.
pub fn request(sessions: &[Session], grace: Duration, worktree: bool, by: &str) -> Vec<Entry> {
    let now = now_ms();
    let deadline = now + grace.as_millis() as u64;
    let v: Vec<Entry> = sessions
        .iter()
        .map(|s| Entry::of(s, deadline, now, worktree, by))
        .collect();
    let out = v.clone();
    update(move |list| {
        for e in v {
            put(list, e);
        }
    });
    out
}

/// Keep the closes `sel` picks: (kept, too late — their close is under way).
pub fn undo(sel: Sel) -> (Vec<Entry>, Vec<Entry>) {
    update(|list| {
        let at = pick(list, sel);
        let (late, kept): (Vec<Entry>, Vec<Entry>) = at
            .iter()
            .map(|i| list[*i].clone())
            .partition(|e| e.claimed.is_some());
        list.retain(|e| !kept.contains(e));
        (kept, late)
    })
}

/// Take one out of the file (its close is done, or failed).
fn forget(e: &Entry) {
    update(|list| list.retain(|x| !(x.host == e.host && x.pane == e.pane && x.batch == e.batch)));
}

// ------------------------------------------------------------------ carrying them out

/// Is the session still there?
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Alive {
    Yes,
    /// a poll since the close was asked doesn't list it: it ended
    Gone,
    /// its machine hasn't answered since
    Unknown,
}

/// What to do with one pending close now.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Wait,
    Close,
    /// out of the file without closing anything (why)
    Drop(String),
    /// out of the file quietly (its close was done)
    Forget,
}

/// The one rule for every pending close — pure.
pub fn step(e: &Entry, now: u64, alive: Alive) -> Step {
    if let Some(t) = e.claimed {
        return if alive == Alive::Gone || now.saturating_sub(t) > CLAIM_KEEP_MS {
            Step::Forget
        } else {
            Step::Wait
        };
    }
    if alive == Alive::Gone {
        return Step::Drop(format!("{} ended by itself before its close", e.display));
    }
    if now < e.deadline {
        return Step::Wait;
    }
    if now.saturating_sub(e.deadline) > STALE_MS {
        return Step::Drop(format!(
            "didn't close {} on {}: its close was due {} ago and nothing was running to carry it out — close it again if it should go",
            e.display,
            e.host,
            crate::ui::dur((now - e.deadline) / 1000)
        ));
    }
    match alive {
        Alive::Yes => Step::Close,
        _ => Step::Wait,
    }
}

/// Is `s` the session `e` closes? (Its pane, and the same agent: herdr
/// could give a new session the pane of one that ended.)
fn same(e: &Entry, s: &Session) -> bool {
    s.pane_id == e.pane && (e.agent.is_empty() || s.agent == e.agent)
}

/// Is the session still in the fleet?
pub fn alive_in(fleet: &Fleet, e: &Entry) -> Alive {
    let Some(h) = fleet.host(&e.host) else {
        return Alive::Unknown;
    };
    if h.sessions.iter().any(|s| same(e, s)) {
        return Alive::Yes;
    }
    // gone only by a live poll that came after the close was asked
    let since = h.conn == Conn::Up
        && !h.stale
        && !h.paused
        && h.fetched.is_some_and(|t| t * 1000 > e.added);
    if since {
        Alive::Gone
    } else {
        Alive::Unknown
    }
}

/// A claim: the closes to carry out now (marked under way in the file), and
/// those dropped, with why.
pub type Claim = (Vec<Entry>, Vec<(Entry, String)>);

/// What to close now, under the hub lock: `holder` holds it; otherwise
/// it's taken for the moment of the claim while nobody holds it. None:
/// another process runs the hub (it carries them out).
pub fn claim_due(holder: bool, now: u64, alive: &dyn Fn(&Entry) -> Alive) -> Option<Claim> {
    let list = current();
    if list.iter().all(|e| step(e, now, alive(e)) == Step::Wait) {
        return Some((Vec::new(), Vec::new()));
    }
    let _lock = if holder {
        None
    } else {
        Some(crate::auto::HubLock::try_borrow()?)
    };
    Some(claim_at(&file(), now, alive))
}

/// The claim itself, judged again on the file as it is under its lock.
fn claim_at(p: &Path, now: u64, alive: &dyn Fn(&Entry) -> Alive) -> Claim {
    update_at(p, |list| {
        let (mut close, mut dropped) = (Vec::new(), Vec::new());
        list.retain_mut(|e| match step(e, now, alive(e)) {
            Step::Wait => true,
            Step::Close => {
                e.claimed = Some(now);
                close.push(e.clone());
                true
            }
            Step::Drop(why) => {
                dropped.push((e.clone(), why));
                false
            }
            Step::Forget => false,
        });
        (close, dropped)
    })
}

/// Close one now (blocking): its pane, then its worktree when asked.
pub fn execute(e: &Entry, t: &Target) -> Result<String, String> {
    let h = Herdr::new(t);
    h.close_pane(&e.pane)
        .map_err(|x| format!("couldn't close {}: {x}", e.display))?;
    crate::restore::mark_closed(&e.host, &e.name);
    let mut m = format!("closed {}", e.display);
    if e.worktree {
        // (herdr refuses when the checkout has changes; never forced)
        match h.remove_worktree(&e.workspace) {
            Ok(()) => m.push_str(" and removed its worktree checkout"),
            Err(x) => m.push_str(&format!(" — kept its worktree: {x}")),
        }
    }
    Ok(m)
}

/// A close carried out: the log says so; a failure goes in the Inbox too
/// (whoever asked may be gone), and the session stays.
fn finished(e: &Entry, r: &Result<String, String>) {
    match r {
        Ok(m) => crate::log::info("closing", &format!("{m} on {} (asked by {})", e.host, e.by)),
        Err(x) => {
            forget(e);
            crate::log::warn("closing", x);
            crate::events::append(crate::events::Event {
                t: crate::cache::now(),
                host: e.host.clone(),
                name: e.name.clone(),
                agent: e.agent.clone(),
                kind: crate::events::Kind::Error,
                title: x.clone(),
                source: "aleph".into(),
                ..Default::default()
            });
        }
    }
}

/// A close dropped: an ended session only in the log; one that went stale
/// in the Inbox too.
fn dropped(e: &Entry, why: &str) {
    crate::log::info("closing", why);
    if why.starts_with("didn't close") {
        crate::events::append(crate::events::Event {
            t: crate::cache::now(),
            host: e.host.clone(),
            name: e.name.clone(),
            agent: e.agent.clone(),
            kind: crate::events::Kind::Error,
            title: why.to_string(),
            source: "aleph".into(),
            ..Default::default()
        });
    }
}

/// The closes, carried out by whoever runs the hub (and by a process that
/// looks while nobody does).
#[derive(Default)]
pub struct Closer {
    last: Option<Instant>,
    /// per machine: the agents' panes (pane, label) at the last good look
    agents: std::collections::HashMap<String, Vec<(String, String)>>,
}

/// The panes that held an agent and don't any more, from tabs Aleph opened
/// (`<agent>_<name>`, not a shell of ours). Pure — tested.
pub fn exited(before: &[(String, String)], now: &[(String, String)]) -> Vec<String> {
    before
        .iter()
        .filter(|(pane, _)| !now.iter().any(|(p, _)| p == pane))
        .filter(|(_, label)| {
            label.split_once('_').is_some_and(|(agent, _)| {
                agent != crate::agents::SHELL && crate::agents::find(agent).is_some()
            })
        })
        .map(|(pane, _)| pane.clone())
        .collect()
}

impl Closer {
    /// An agent that exited by itself leaves its shell behind, in a pane Fleet
    /// no longer lists (`close.exited`): that pane closes, once herdr says only
    /// the shell is left. A pane running anything else is never touched.
    pub fn close_exited(&mut self, cfg: &Config, fleet: &Fleet) {
        let on = cfg.get_or("close.exited") == "close" && !crate::showcase::demo();
        for h in &fleet.hosts {
            if h.conn != crate::fleet::Conn::Up || h.stale || h.paused {
                continue;
            }
            let now: Vec<(String, String)> = h
                .sessions
                .iter()
                .filter(|s| s.agent != crate::agents::SHELL)
                .map(|s| (s.pane_id.clone(), s.label.clone()))
                .collect();
            let gone = match self.agents.get(&h.alias) {
                Some(before) if on => exited(before, &now),
                _ => Vec::new(),
            };
            self.agents.insert(h.alias.clone(), now);
            for pane in gone {
                if pending(&h.alias, &pane).is_some() {
                    continue;
                }
                let (t, alias) = (h.target.clone(), h.alias.clone());
                std::thread::spawn(move || {
                    let herdr = Herdr::new(&t);
                    if herdr.at_prompt(&pane) == Ok(true) && herdr.close_pane(&pane).is_ok() {
                        crate::log::info(
                            "close",
                            &format!("{alias}: closed {pane}, its agent had exited"),
                        );
                    }
                });
            }
        }
    }

    /// After every fleet poll. `holder`: this process holds the hub lock.
    pub fn tick(&mut self, cfg: &Config, fleet: &Fleet, holder: bool) {
        if self
            .last
            .is_some_and(|t| t.elapsed() < Duration::from_millis(200))
        {
            return;
        }
        self.last = Some(Instant::now());
        let alive = |e: &Entry| alive_in(fleet, e);
        let Some((close, drop)) = claim_due(holder, now_ms(), &alive) else {
            return;
        };
        for (e, why) in drop {
            dropped(&e, &why);
        }
        for e in close {
            let t = fleet
                .host(&e.host)
                .map(|h| h.target.clone())
                .unwrap_or_else(|| cfg.target_for(&e.host, herdr::machine_for));
            std::thread::spawn(move || {
                let r = execute(&e, &t);
                finished(&e, &r);
            });
        }
    }
}

// ------------------------------------------------------------------ the CLI

const USAGE: &str = "usage: aleph close <name> [--yes] [--worktree] [--now] [--json] · aleph close --undo [name] · aleph close --pending";

/// `aleph close`: after `close.grace` — handed to the hub when one runs,
/// else waited out here.
pub fn cmd(a: Args) -> ExitCode {
    if a.has("--undo") {
        return undo_cmd(&a);
    }
    if a.has("--pending") {
        return pending_cmd(&a);
    }
    let p = a.positional();
    let Some(q) = p.first() else {
        return fail(USAGE);
    };
    let (t, s) = match crate::cli::find(&a.cfg, &a.flag, q) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let g = grace(&a.cfg);
    let at_once = a.has("--now") || g.is_zero();
    let worktree = a.has("--worktree");
    let as_json = a.has("--json");
    if !a.has("--yes")
        && !a.has("-y")
        && !crate::out::yes(&format!(
            "Close {} on {}? {} stops and the pane closes{}",
            s.display(),
            t.ssh,
            crate::agents::label(&s.agent),
            if at_once {
                String::new()
            } else {
                format!(" in {} (ctrl+c or --undo keeps it)", grace_text(g))
            }
        ))
    {
        return ExitCode::from(1);
    }
    if at_once {
        // (a close pending for it goes: this is it)
        let _ = undo(Sel::Session(&s.host, &s.pane_id));
        let e = Entry::of(&s, now_ms(), now_ms(), worktree, "cli");
        return match execute(&e, &t) {
            Ok(m) => {
                if as_json {
                    json(&json!({"closed": true, "host": s.host, "name": s.name}));
                } else {
                    say_closed(&m, worktree);
                }
                ExitCode::SUCCESS
            }
            Err(e) => {
                err(&e);
                ExitCode::from(1)
            }
        };
    }
    let e = request(std::slice::from_ref(&s), g, worktree, "cli").remove(0);
    let undo_hint = format!("{} close --undo {}", crate::doctor::me(), e.display);
    if crate::auto::HubLock::held() {
        // a daemon or a dashboard runs the hub: it carries the close out
        if as_json {
            json(&pending_json(&e, g));
        } else {
            ok(&format!(
                "closing {} in {} — {undo_hint} keeps it",
                e.display,
                grace_text(g)
            ));
        }
        return ExitCode::SUCCESS;
    }
    wait_out(&a.cfg, e, &undo_hint, as_json)
}

fn say_closed(m: &str, worktree: bool) {
    match m.split_once(" — ") {
        Some((done, kept)) if worktree => {
            ok(done);
            warn(kept);
            hint("herdr worktree remove --workspace <id> --force removes it anyway");
        }
        _ => ok(m),
    }
}

/// What `session.close` and `aleph close --json` say about a close that waits.
pub fn pending_json(e: &Entry, g: Duration) -> Value {
    json!({
        "closed": false, "pending": true, "host": e.host, "name": e.name, "display": e.display,
        "deadline": e.deadline / 1000, "deadline_ms": e.deadline, "grace_s": g.as_secs_f64(),
        "worktree": e.worktree, "undo": "session.close.undo",
    })
}

/// No process runs the hub: wait the grace here, with a countdown (ctrl+c
/// keeps the session), then close it under the hub lock — or let a hub that
/// started meanwhile do it.
fn wait_out(cfg: &Config, e: Entry, undo_hint: &str, as_json: bool) -> ExitCode {
    let stop = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone());
    let tty = std::io::stderr().is_terminal();
    if !tty && !as_json {
        ok(&format!(
            "closing {} in {} — ctrl+c or {undo_hint} keeps it",
            e.display,
            grace_text(Duration::from_millis(e.deadline.saturating_sub(now_ms())))
        ));
    }
    let t0 = Instant::now();
    let clear = || {
        if tty {
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    };
    let kept = |how: &str| {
        clear();
        if as_json {
            json(&json!({"closed": false, "kept": true, "host": e.host, "name": e.name}));
        } else {
            ok(&format!("kept {} — {how}", e.display));
        }
        ExitCode::from(1)
    };
    let mut handed: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            let (k, _) = undo(Sel::Session(&e.host, &e.pane));
            if !k.is_empty() {
                return kept("it won't close");
            }
        }
        let now = now_ms();
        let Some(cur) = load()
            .into_iter()
            .find(|x| x.host == e.host && x.pane == e.pane)
        else {
            return kept("its close was undone");
        };
        if cur.claimed.is_some() {
            clear();
            if as_json {
                json(
                    &json!({"closed": false, "pending": true, "host": e.host, "name": e.name, "by": "hub"}),
                );
            } else {
                ok(&format!(
                    "{} is closing (a hub that started meanwhile took it over)",
                    e.display
                ));
            }
            return ExitCode::SUCCESS;
        }
        if now < cur.deadline {
            if tty {
                countdown(
                    t0,
                    &format!("closing {} on {} in {}s", e.display, e.host, cur.left(now)),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        // due: claim what's due (one left behind by another process too) and
        // close it
        let due: Vec<Entry> = load().into_iter().filter(|x| x.deadline <= now).collect();
        let seen = listed(cfg, &due);
        let alive = |x: &Entry| match seen.get(&x.host) {
            Some(Some(v)) if v.iter().any(|s| same(x, s)) => Alive::Yes,
            Some(Some(_)) => Alive::Gone,
            // ours: try anyway (the error says why); others wait for a hub
            _ if x.host == e.host && x.pane == e.pane => Alive::Yes,
            _ => Alive::Unknown,
        };
        match claim_due(false, now, &alive) {
            // a hub holds the lock now: it closes it (a moment to see it claimed)
            None => {
                let t = *handed.get_or_insert_with(Instant::now);
                if t.elapsed() > Duration::from_secs(5) {
                    clear();
                    ok(&format!(
                        "closing {} — the hub carries it out ({undo_hint} keeps it until then)",
                        e.display
                    ));
                    return ExitCode::SUCCESS;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Some((close, drop)) => {
                let mut mine: Option<ExitCode> = None;
                for (x, why) in drop {
                    dropped(&x, &why);
                    if x.host == e.host && x.pane == e.pane {
                        clear();
                        if as_json {
                            json(
                                &json!({"closed": false, "ended": true, "host": e.host, "name": e.name}),
                            );
                        } else {
                            ok(&format!("{} ended by itself — nothing to close", e.display));
                        }
                        mine = Some(ExitCode::SUCCESS);
                    }
                }
                for x in close {
                    let r = execute(&x, &cfg.target_for(&x.host, herdr::machine_for));
                    if r.is_ok() {
                        forget(&x);
                    }
                    finished(&x, &r);
                    let ours = x.host == e.host && x.pane == e.pane;
                    clear();
                    match (&r, ours) {
                        (Ok(m), true) => {
                            if as_json {
                                json(
                                    &json!({"closed": true, "host": e.host, "name": e.name, "waited_s": t0.elapsed().as_secs()}),
                                );
                            } else {
                                say_closed(m, e.worktree);
                            }
                            mine = Some(ExitCode::SUCCESS);
                        }
                        (Err(m), true) => {
                            err(m);
                            mine = Some(ExitCode::from(1));
                        }
                        // one asked earlier, left for a hub that never came
                        (Ok(m), false) => hint(&format!("{m} too (asked by {} earlier)", x.by)),
                        (Err(m), false) => warn(m),
                    }
                }
                if let Some(c) = mine {
                    return c;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// The countdown line on stderr (a terminal): spinner, what, ctrl+c.
fn countdown(t0: Instant, what: &str) {
    use crate::ui::{self, c};
    use ratatui::text::{Line, Span};
    let l = Line::from(vec![
        Span::styled(format!("{} ", ui::spinner(t0)), ui::fg(c::ACCENT())),
        Span::styled(what.to_string(), ui::muted()),
        Span::styled(" · ctrl+c keeps it", ui::faint()),
    ]);
    eprint!("\r\x1b[2K{}", ui::to_ansi(&l, true));
    let _ = std::io::stderr().flush();
}

/// The sessions on the machines of `v`, each asked once (None: it didn't
/// answer).
fn listed(cfg: &Config, v: &[Entry]) -> std::collections::HashMap<String, Option<Vec<Session>>> {
    let mut seen = std::collections::HashMap::new();
    for e in v {
        seen.entry(e.host.clone()).or_insert_with(|| {
            Herdr::new(&cfg.target_for(&e.host, herdr::machine_for))
                .sessions()
                .ok()
        });
    }
    seen
}

/// `aleph close --undo [name]`: keep it (the last close without a name).
fn undo_cmd(a: &Args) -> ExitCode {
    let p = a.positional();
    let sel = match p.first() {
        Some(q) => Sel::Query(q),
        None => Sel::Latest,
    };
    let (kept, late) = undo(sel);
    if a.has("--json") {
        json(&json!({
            "kept": kept.iter().map(Entry::to_json).collect::<Vec<_>>(),
            "late": late.iter().map(Entry::to_json).collect::<Vec<_>>(),
        }));
        return if kept.is_empty() {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        };
    }
    for e in &kept {
        ok(&format!(
            "kept {} on {} — it won't close",
            e.display, e.host
        ));
    }
    for e in &late {
        warn(&format!(
            "too late: {} on {} is closing already",
            e.display, e.host
        ));
    }
    if kept.is_empty() && late.is_empty() {
        match p.first() {
            Some(q) => err(&format!("{q} isn't closing")),
            None => err("nothing is closing"),
        }
        let left = load();
        if !left.is_empty() {
            hint(&format!(
                "closing now: {}",
                left.iter()
                    .map(|e| format!("{}:{}", e.host, e.display))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }
    if kept.is_empty() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// `aleph close --pending`: what's waiting out its grace.
fn pending_cmd(a: &Args) -> ExitCode {
    let v = load();
    let now = now_ms();
    if a.has("--json") {
        json(&Value::Array(v.iter().map(Entry::to_json).collect()));
        return ExitCode::SUCCESS;
    }
    if !std::io::stdout().is_terminal() {
        for e in &v {
            crate::out::raw(&format!(
                "{}\t{}\t{}\t{}\t{}",
                e.host,
                e.display,
                e.pane,
                if e.claimed.is_some() {
                    "closing".to_string()
                } else {
                    e.left(now).to_string()
                },
                e.by
            ));
        }
        return ExitCode::SUCCESS;
    }
    if v.is_empty() {
        ok("nothing is closing");
        return ExitCode::SUCCESS;
    }
    let pad = crate::ui::pad;
    let w = v
        .iter()
        .map(|e| crate::ui::width(&e.display))
        .max()
        .unwrap_or(0);
    let hw = v
        .iter()
        .map(|e| crate::ui::width(&e.host))
        .max()
        .unwrap_or(0);
    for e in &v {
        let when = if e.claimed.is_some() {
            "closing now".to_string()
        } else {
            format!("in {}s", e.left(now))
        };
        crate::out::raw(&format!(
            "  {}  {}  {}  asked by {}",
            pad(&e.display, w),
            pad(&e.host, hw),
            pad(&when, 11),
            e.by
        ));
    }
    hint(&format!(
        "{} close --undo <name> keeps one",
        crate::doctor::me()
    ));
    ExitCode::SUCCESS
}

// ------------------------------------------------------------------ the dashboard quitting

/// The dashboard quits: closes still pending that nobody else will carry
/// out (no daemon) are waited out here, on the shell, with a countdown
/// (ctrl+c keeps them). With a daemon running, it takes them over.
pub fn on_quit(cfg: &Config, lock: Option<crate::auto::HubLock>) {
    let mine: Vec<Entry> = load().into_iter().filter(|e| e.claimed.is_none()).collect();
    if mine.is_empty() {
        return;
    }
    if crate::queue::watch_running(cfg) {
        drop(lock);
        hint(&format!(
            "{} close{} pending — the daemon carries {} out ({} close --undo keeps {})",
            mine.len(),
            if mine.len() == 1 { "" } else { "s" },
            if mine.len() == 1 { "it" } else { "them" },
            crate::doctor::me(),
            if mine.len() == 1 { "it" } else { "the last" }
        ));
        return;
    }
    // another dashboard runs the hub: it carries them out
    if lock.is_none() && crate::auto::HubLock::held() {
        return;
    }
    // a long grace isn't waited out at the door: the next hub carries it out
    let first = mine.iter().map(|e| e.deadline).min().unwrap_or(0);
    if first.saturating_sub(now_ms()) > QUIT_WAIT.as_millis() as u64 {
        hint(&format!(
            "closing {} later — the next dashboard or daemon carries it out ({} close --undo keeps it)",
            mine.iter()
                .map(|e| e.display.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            crate::doctor::me()
        ));
        return;
    }
    let holder = lock.is_some();
    let stop = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone());
    let tty = std::io::stderr().is_terminal();
    let t0 = Instant::now();
    loop {
        if stop.load(Ordering::Relaxed) {
            if tty {
                eprint!("\r\x1b[2K");
            }
            for e in &mine {
                if !undo(Sel::Session(&e.host, &e.pane)).0.is_empty() {
                    ok(&format!("kept {} — it won't close", e.display));
                }
            }
            return;
        }
        let now = now_ms();
        let left: Vec<Entry> = load().into_iter().filter(|e| e.claimed.is_none()).collect();
        let Some(first) = left.iter().min_by_key(|e| e.deadline) else {
            return;
        };
        if now < first.deadline {
            if tty {
                let names: Vec<&str> = left.iter().map(|e| e.display.as_str()).collect();
                countdown(
                    t0,
                    &format!("closing {} in {}s", names.join(", "), first.left(now)),
                );
            }
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if tty {
            eprint!("\r\x1b[2K");
        }
        let seen = listed(cfg, &left);
        let alive = |x: &Entry| match seen.get(&x.host) {
            Some(Some(v)) if v.iter().any(|s| same(x, s)) => Alive::Yes,
            Some(Some(_)) => Alive::Gone,
            _ => Alive::Yes,
        };
        let Some((close, drop)) = claim_due(holder, now, &alive) else {
            // a hub took the lock meanwhile: it carries them out
            return;
        };
        for (e, why) in drop {
            dropped(&e, &why);
        }
        for e in close {
            let r = execute(&e, &cfg.target_for(&e.host, herdr::machine_for));
            if r.is_ok() {
                forget(&e);
            }
            finished(&e, &r);
            match r {
                Ok(m) => ok(&m),
                Err(m) => err(&m),
            }
        }
        // (machines that didn't answer: left for the next hub)
        if load()
            .iter()
            .all(|e| e.claimed.is_some() || e.deadline > now_ms())
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
        if t0.elapsed() > Duration::from_secs(30) {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_agent_of_ours_that_left_counts_as_exited() {
        let row = |p: &str, l: &str| (p.to_string(), l.to_string());
        let before = vec![
            row("w1:p1", "claude_ignite:px"),
            row("w1:p2", "codex_tune"),
            row("w1:p3", "my own tab"),
            row("w1:p4", "shell_frost"),
            row("w1:p5", "claude_stays"),
        ];
        let now = vec![row("w1:p5", "claude_stays")];
        assert_eq!(exited(&before, &now), ["w1:p1", "w1:p2"]);
        assert!(exited(&[], &now).is_empty());
    }

    fn s(host: &str, pane: &str, name: &str) -> Session {
        Session {
            host: host.into(),
            pane_id: pane.into(),
            name: name.into(),
            label: format!("claude_{name}"),
            agent: "claude".into(),
            workspace_id: "w1".into(),
            ..Default::default()
        }
    }

    fn tmp(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("aleph-closing-{tag}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn grace_values() {
        assert_eq!(parse_grace("5s"), Some(Duration::from_secs(5)));
        assert_eq!(parse_grace("0"), Some(Duration::ZERO));
        assert_eq!(parse_grace("off"), Some(Duration::ZERO));
        assert_eq!(parse_grace("500ms"), Some(Duration::from_millis(500)));
        assert_eq!(parse_grace("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_grace("2d"), Some(MAX_GRACE), "an hour at most");
        assert_eq!(parse_grace("soon"), None);
        assert_eq!(grace(&Config::in_memory(&[])), DEFAULT_GRACE);
        assert_eq!(
            grace(&Config::in_memory(&["close.grace=0"])),
            Duration::ZERO
        );
        assert_eq!(
            grace(&Config::in_memory(&["close.grace=bogus"])),
            DEFAULT_GRACE
        );
        assert_eq!(grace_text(Duration::from_secs(5)), "5s");
        assert_eq!(grace_text(Duration::from_secs(90)), "1m 30s");
        assert_eq!(grace_text(Duration::from_millis(400)), "400ms");
    }

    #[test]
    fn the_seconds_left_round_up() {
        let e = Entry {
            deadline: 10_000,
            ..Default::default()
        };
        assert_eq!(e.left(5_000), 5);
        assert_eq!(e.left(5_001), 5);
        assert_eq!(e.left(9_999), 1);
        assert_eq!(e.left(10_000), 0);
        assert_eq!(e.left(20_000), 0);
    }

    #[test]
    fn closing_again_sets_the_deadline_again() {
        let mut v = Vec::new();
        put(
            &mut v,
            Entry::of(&s("b31", "w1:p1", "ignite"), 5_000, 0, false, "cli"),
        );
        put(
            &mut v,
            Entry::of(&s("b31", "w1:p2", "temper"), 5_000, 0, false, "cli"),
        );
        put(
            &mut v,
            Entry::of(
                &s("b31", "w1:p1", "ignite"),
                9_000,
                4_000,
                true,
                "dashboard",
            ),
        );
        assert_eq!(v.len(), 2, "one entry per session");
        assert_eq!(v[0].deadline, 9_000);
        assert!(v[0].worktree && v[0].by == "dashboard");
        // one under way isn't touched
        v[1].claimed = Some(5_000);
        put(
            &mut v,
            Entry::of(&s("b31", "w1:p2", "temper"), 99_000, 6_000, false, "cli"),
        );
        assert_eq!(v[1].deadline, 5_000);
    }

    #[test]
    fn undo_picks_the_session_a_name_or_the_last_batch() {
        let mut v = Vec::new();
        put(
            &mut v,
            Entry::of(&s("b31", "w1:p1", "ignite"), 5_000, 100, false, "cli"),
        );
        let mut b = Entry::of(&s("gpu2", "w1:p4", "weave"), 6_000, 200, false, "dashboard");
        let mut c = Entry::of(&s("b31", "w1:p2", "temper"), 6_000, 200, false, "dashboard");
        b.batch = 200;
        c.batch = 200;
        put(&mut v, b);
        put(&mut v, c);
        assert_eq!(pick(&v, Sel::Session("b31", "w1:p1")), vec![0]);
        assert_eq!(pick(&v, Sel::Query("ignite")), vec![0]);
        assert_eq!(pick(&v, Sel::Query("b31:ignite")), vec![0]);
        assert_eq!(pick(&v, Sel::Query("gpu2:ignite")), Vec::<usize>::new());
        assert_eq!(pick(&v, Sel::Query("w1:p4")), vec![1]);
        assert_eq!(pick(&v, Sel::Query("gpu2:w1:p4")), vec![1]);
        assert_eq!(pick(&v, Sel::Latest), vec![1, 2], "the last batch, whole");
        // the last batch under way: the one before it
        v[1].claimed = Some(7_000);
        v[2].claimed = Some(7_000);
        assert_eq!(pick(&v, Sel::Latest), vec![0]);
    }

    #[test]
    fn the_rule_for_each_close() {
        let mut e = Entry::of(&s("b31", "w1:p1", "ignite"), 10_000, 5_000, false, "cli");
        assert_eq!(step(&e, 6_000, Alive::Yes), Step::Wait);
        assert_eq!(step(&e, 10_000, Alive::Yes), Step::Close);
        // its machine doesn't answer: wait, but not for ever
        assert_eq!(step(&e, 11_000, Alive::Unknown), Step::Wait);
        assert!(
            matches!(step(&e, 10_000 + STALE_MS + 1, Alive::Unknown), Step::Drop(w) if w.starts_with("didn't close"))
        );
        assert!(
            matches!(step(&e, 10_000 + STALE_MS + 1, Alive::Yes), Step::Drop(_)),
            "never run late"
        );
        // ended by itself, before or after its deadline
        assert!(
            matches!(step(&e, 6_000, Alive::Gone), Step::Drop(w) if w.contains("ended by itself"))
        );
        // under way: kept (the dashboard says closing…) until it's gone
        e.claimed = Some(10_000);
        assert_eq!(step(&e, 11_000, Alive::Yes), Step::Wait);
        assert_eq!(step(&e, 11_000, Alive::Gone), Step::Forget);
        assert_eq!(
            step(&e, 10_000 + CLAIM_KEEP_MS + 1, Alive::Yes),
            Step::Forget
        );
    }

    #[test]
    fn the_fleet_says_gone_only_after_a_poll_since() {
        let cfg = Config::in_memory(&["hosts=b31", "local=false", "stats=off"]);
        let mut f = Fleet::new(&cfg, None);
        let now = 1_000_000;
        let e = Entry::of(&s("b31", "w1:p1", "ignite"), now + 5_000, now, false, "cli");
        let h = f.host_mut("b31").unwrap();
        h.conn = Conn::Up;
        h.sessions = vec![s("b31", "w1:p1", "ignite")];
        h.fetched = Some(now / 1000 - 1);
        assert_eq!(alive_in(&f, &e), Alive::Yes);
        let h = f.host_mut("b31").unwrap();
        h.sessions.clear();
        assert_eq!(alive_in(&f, &e), Alive::Unknown, "that poll came before");
        f.host_mut("b31").unwrap().fetched = Some(now / 1000 + 1);
        assert_eq!(alive_in(&f, &e), Alive::Gone);
        // another agent in its pane isn't it
        f.host_mut("b31").unwrap().sessions = vec![Session {
            agent: "codex".into(),
            ..s("b31", "w1:p1", "other")
        }];
        assert_eq!(alive_in(&f, &e), Alive::Gone);
        f.host_mut("b31").unwrap().conn = Conn::Down("timeout".into());
        assert_eq!(alive_in(&f, &e), Alive::Unknown);
        assert_eq!(
            alive_in(
                &f,
                &Entry {
                    host: "nosuch".into(),
                    ..e.clone()
                }
            ),
            Alive::Unknown
        );
    }

    #[test]
    fn the_claim_takes_each_close_once() {
        let p = tmp("claim");
        update_at(&p, |v| {
            put(
                v,
                Entry::of(&s("b31", "w1:p1", "ignite"), 10_000, 5_000, false, "cli"),
            );
            put(
                v,
                Entry::of(&s("b31", "w1:p2", "temper"), 20_000, 5_000, false, "cli"),
            );
            put(
                v,
                Entry::of(&s("gpu2", "w1:p4", "weave"), 10_000, 5_000, false, "cli"),
            );
        });
        let alive = |e: &Entry| {
            if e.name == "weave" {
                Alive::Gone
            } else {
                Alive::Yes
            }
        };
        let (close, dropped) = claim_at(&p, 10_000, &alive);
        assert_eq!(
            close.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["ignite"]
        );
        assert_eq!(dropped.len(), 1);
        assert!(dropped[0].1.contains("weave ended by itself"));
        let left = load_at(&p);
        assert_eq!(left.len(), 2, "ignite stays, under way; temper waits");
        assert_eq!(left[0].claimed, Some(10_000));
        // a second executor finds nothing more to do
        let (again, _) = claim_at(&p, 10_500, &alive);
        assert!(again.is_empty());
        // done: gone from the file once the fleet stops listing it
        let gone = |_: &Entry| Alive::Gone;
        let (close, dropped) = claim_at(&p, 11_000, &gone);
        assert!(close.is_empty());
        assert_eq!(dropped.len(), 1, "temper ended by itself");
        assert!(!p.exists(), "nothing left: no file");
    }

    #[test]
    fn the_file_round_trips() {
        let p = tmp("rt");
        let mut e = Entry::of(
            &s("b31", "w1:p1", "ignite"),
            10_000,
            5_000,
            true,
            "api (web)",
        );
        e.claimed = Some(10_001);
        update_at(&p, |v| put(v, e.clone()));
        assert_eq!(load_at(&p), vec![e]);
        let _ = std::fs::remove_file(&p);
        assert!(parse("not json").is_empty());
        assert!(parse(r#"{"v":1,"closing":[{"host":"b31"}]}"#).is_empty());
    }
}
