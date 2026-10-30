//! The session state log: when each session was working, waiting for you,
//! done, idle or at its limit over the last two days, for the timeline
//! charts (Fleet's timeline rows, Monitor › Timeline, the preview's strip).
//!
//! herdr only says what a session is doing now (and the fleet since when),
//! and the event inbox keeps the moments people care about (needs you,
//! finished), not working ↔ idle. So the one process that runs the hub — the
//! daemon, else the dashboard (`auto::HubLock`) — writes every stretch that
//! ended here, one TSV line each: `start end host pane name agent state`.
//! The stretch going on now isn't written: readers take it from the fleet
//! (`Session::since`).
//!
//! * `~/.local/state/aleph/states.tsv` (the world's state folder).
//! * Kept [`KEEP`] (48 h) and at most [`CAP`] lines, rewritten atomically —
//!   only by the writer.
//! * A gap (nothing ran the hub) stays a gap: nothing is made up.

use crate::fleet::{Conn, Fleet};
use crate::herdr::Session;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How long stretches are kept.
pub const KEEP: u64 = 48 * 3600;
/// At most this many lines (the newest).
pub const CAP: usize = 6000;
/// Ticks further apart than this: another process ran the hub meanwhile, so
/// what this one had open is closed where it was last seen.
const RESUME: Duration = Duration::from_secs(60);
/// How often the file is pruned.
const PRUNE: Duration = Duration::from_secs(3600);

/// A session's state, as the charts draw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum St {
    Working,
    Needs,
    Done,
    Idle,
    Limit,
    /// compacting its conversation: at work, and worth seeing apart
    Compacting,
}

impl St {
    /// What a session is doing now (stopped at its plan's limit wins).
    pub fn of(s: &Session) -> St {
        if s.limit.is_some() {
            return St::Limit;
        }
        if s.compacting {
            return St::Compacting;
        }
        match s.status.as_str() {
            "working" => St::Working,
            "blocked" => St::Needs,
            "done" => St::Done,
            _ => St::Idle,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            St::Working => "working",
            St::Needs => "needs",
            St::Done => "done",
            St::Idle => "idle",
            St::Limit => "limit",
            St::Compacting => "compacting",
        }
    }
    /// At work: a compaction counts as much as a turn does.
    pub fn busy(self) -> bool {
        matches!(self, St::Working | St::Compacting)
    }
    pub fn parse(s: &str) -> Option<St> {
        Some(match s {
            "working" => St::Working,
            "needs" => St::Needs,
            "done" => St::Done,
            "idle" => St::Idle,
            "limit" => St::Limit,
            "compacting" => St::Compacting,
            _ => return None,
        })
    }
    /// Which one a chart cell shows when several share it (time being equal).
    pub fn rank(self) -> u8 {
        match self {
            St::Needs => 5,
            St::Limit => 4,
            St::Compacting => 3,
            St::Working => 2,
            St::Done => 1,
            St::Idle => 0,
        }
    }
}

/// One stretch of one session in one state.
#[derive(Clone, Debug, PartialEq)]
pub struct Stretch {
    pub start: u64,
    pub end: u64,
    pub host: String,
    pub pane: String,
    pub name: String,
    pub agent: String,
    pub st: St,
}

impl Stretch {
    /// `host|pane`, like `Session::key`.
    pub fn key(&self) -> String {
        format!("{}|{}", self.host, self.pane)
    }
    fn line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
        format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            self.start,
            self.end,
            clean(&self.host),
            clean(&self.pane),
            clean(&self.name),
            clean(&self.agent),
            self.st.as_str()
        )
    }
    fn parse(l: &str) -> Option<Stretch> {
        let f: Vec<&str> = l.split('\t').collect();
        if f.len() < 7 {
            return None;
        }
        let (start, end) = (f[0].parse().ok()?, f[1].parse().ok()?);
        (end >= start).then_some(())?;
        Some(Stretch {
            start,
            end,
            host: f[2].to_string(),
            pane: f[3].to_string(),
            name: f[4].to_string(),
            agent: f[5].to_string(),
            st: St::parse(f[6].trim())?,
        })
    }
}

pub fn file() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir()
            .join(format!("ch-states-test-{}", std::process::id()))
            .join("states.tsv");
    }
    crate::config::world_state().join("states.tsv")
}

/// Every stretch in a log, as written (oldest first, mostly).
pub fn read_from(p: &std::path::Path) -> Vec<Stretch> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(Stretch::parse)
        .collect()
}

/// One session's stretches put in order (pure — tested): sorted by start,
/// a stretch that runs into the next one cut where that one begins, the same
/// state running on joined. Two hubs' overlapping accounts become one; what
/// a cut stretch said after the later one ended stays unknown (a gap).
pub fn tidy(mut v: Vec<(u64, u64, St)>) -> Vec<(u64, u64, St)> {
    v.retain(|x| x.1 > x.0);
    v.sort_by_key(|x| (x.0, x.1));
    let mut out: Vec<(u64, u64, St)> = Vec::with_capacity(v.len());
    for (s, e, st) in v {
        if let Some(last) = out.last_mut() {
            if s < last.1 && last.2 != st {
                // a later account of another state wins from its start
                last.1 = s;
                if last.1 <= last.0 {
                    out.pop();
                }
            }
        }
        match out.last_mut() {
            Some(last) if last.2 == st && s <= last.1 => last.1 = last.1.max(e),
            _ => out.push((s, e, st)),
        }
    }
    out
}

/// What the writer keeps of a session while its state lasts.
#[derive(Clone, Debug)]
struct Open {
    st: St,
    start: u64,
    /// the last poll that saw it like this
    seen: u64,
    host: String,
    pane: String,
    name: String,
    agent: String,
}

impl Open {
    fn close(&self, end: u64) -> Option<Stretch> {
        (end > self.start).then(|| Stretch {
            start: self.start,
            end,
            host: self.host.clone(),
            pane: self.pane.clone(),
            name: self.name.clone(),
            agent: self.agent.clone(),
            st: self.st,
        })
    }
}

/// A machine as the writer sees it on a tick: its alias, whether it answered
/// (a machine that didn't keeps its sessions' stretches open), its sessions.
pub struct Seen<'a> {
    pub host: &'a str,
    pub up: bool,
    pub sessions: &'a [Session],
}

/// The hub's writer (`auto::Hub` owns one): call `observe` after every fleet
/// tick; it appends the stretches that ended.
#[derive(Default)]
pub struct Writer {
    open: HashMap<String, Open>,
    at: Option<Instant>,
    changed: Option<Instant>,
    pruned: Option<Instant>,
    /// where it writes (tests: a file of their own)
    path: Option<PathBuf>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer::default()
    }

    #[cfg(test)]
    pub fn at_path(p: PathBuf) -> Writer {
        let mut w = Writer::default();
        w.path = Some(p);
        w
    }

    fn path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(file)
    }

    /// After a fleet tick, by the process that runs the hub.
    pub fn observe(&mut self, fleet: &Fleet) {
        let now = crate::cache::now();
        // another process ran the hub since this one last did: what was open
        // here ended (as far as this one knows) where it was last seen
        if self.at.is_some_and(|t| t.elapsed() > RESUME) {
            let v: Vec<Stretch> = self.open.values().filter_map(|o| o.close(o.seen)).collect();
            self.open.clear();
            self.changed = None;
            append(&self.path(), &v);
        }
        self.at = Some(Instant::now());
        if self.pruned.is_none_or(|t| t.elapsed() > PRUNE) {
            self.pruned = Some(Instant::now());
            prune(&self.path(), now);
        }
        if self.changed == Some(fleet.changed_at) {
            return;
        }
        self.changed = Some(fleet.changed_at);
        let seen: Vec<Seen> = fleet
            .hosts
            .iter()
            .map(|h| Seen {
                host: &h.alias,
                // (a paused machine isn't polled: nothing there is seen)
                up: h.conn == Conn::Up && !h.paused,
                sessions: &h.sessions,
            })
            .collect();
        let ended = self.step(now, &seen);
        append(&self.path(), &ended);
    }

    /// The stretches that ended, given the fleet now (pure — tested).
    pub fn step(&mut self, now: u64, hosts: &[Seen]) -> Vec<Stretch> {
        let mut out = Vec::new();
        let mut here: HashSet<String> = HashSet::new();
        let mut waiting: HashSet<&str> = HashSet::new();
        for h in hosts {
            if !h.up {
                waiting.insert(h.host);
                continue;
            }
            for s in h.sessions {
                let k = format!("{}|{}", h.host, s.pane_id);
                let st = St::of(s);
                let since = s.since.unwrap_or(now).min(now);
                here.insert(k.clone());
                let fresh = |start: u64| Open {
                    st,
                    start,
                    seen: now,
                    host: h.host.to_string(),
                    pane: s.pane_id.clone(),
                    name: s.name.clone(),
                    agent: s.agent.clone(),
                };
                match self.open.get_mut(&k) {
                    Some(o) if o.st == st && o.name == s.name => o.seen = now,
                    Some(o) => {
                        // the old stretch ends where the new one begins: the
                        // fleet's `since` when it moved, else now (a limit
                        // found on a screen, a rename)
                        let start = if since > o.start { since } else { now };
                        out.extend(o.close(start));
                        *o = fresh(start);
                    }
                    None => {
                        self.open.insert(k, fresh(since));
                    }
                }
            }
        }
        // gone from a machine that answered (or from the fleet): ended where
        // it was last seen
        let gone: Vec<String> = self
            .open
            .iter()
            .filter(|(k, o)| !here.contains(*k) && !waiting.contains(o.host.as_str()))
            .map(|(k, _)| k.clone())
            .collect();
        for k in gone {
            if let Some(o) = self.open.remove(&k) {
                out.extend(o.close(o.seen));
            }
        }
        out
    }
}

impl Drop for Writer {
    /// What's open ends where it was last seen (the next writer opens it
    /// again from the fleet's `since` if it goes on).
    fn drop(&mut self) {
        if self.open.is_empty() {
            return;
        }
        let v: Vec<Stretch> = self.open.values().filter_map(|o| o.close(o.seen)).collect();
        append(&self.path(), &v);
    }
}

fn append(p: &std::path::Path, v: &[Stretch]) {
    if v.is_empty() {
        return;
    }
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let body: String = v.iter().map(Stretch::line).collect();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(p)
    {
        let _ = f.write_all(body.as_bytes());
    }
}

/// What to keep (pure — tested): stretches that ended within `KEEP` of
/// `now`, the newest `CAP` of them.
pub fn kept(v: Vec<Stretch>, now: u64) -> Vec<Stretch> {
    let mut v: Vec<Stretch> = v.into_iter().filter(|s| s.end + KEEP >= now).collect();
    if v.len() > CAP {
        v.sort_by_key(|s| s.end);
        v.drain(..v.len() - CAP);
    }
    v
}

/// Drop what's past keeping, rewriting the file only when something goes.
fn prune(p: &std::path::Path, now: u64) {
    let Ok(text) = std::fs::read_to_string(p) else {
        return;
    };
    let lines = text.lines().count();
    let all: Vec<Stretch> = text.lines().filter_map(Stretch::parse).collect();
    let keep = kept(all, now);
    if keep.len() == lines {
        return;
    }
    let body: String = keep.iter().map(Stretch::line).collect();
    let _ = crate::config::write_atomic(p, body.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(pane: &str, name: &str, status: &str, since: u64) -> Session {
        Session {
            host: "b31".into(),
            pane_id: pane.into(),
            name: name.into(),
            agent: "claude".into(),
            status: status.into(),
            since: Some(since),
            ..Default::default()
        }
    }

    fn up<'a>(v: &'a [Session]) -> Vec<Seen<'a>> {
        vec![Seen {
            host: "b31",
            up: true,
            sessions: v,
        }]
    }

    #[test]
    fn lines_round_trip() {
        let s = Stretch {
            start: 10,
            end: 20,
            host: "b31".into(),
            pane: "w1:p1".into(),
            name: "ig\tnite".into(),
            agent: "claude".into(),
            st: St::Needs,
        };
        let back = Stretch::parse(s.line().trim_end()).unwrap();
        assert_eq!(back.name, "ig nite");
        assert_eq!((back.start, back.end, back.st), (10, 20, St::Needs));
        assert_eq!(back.key(), "b31|w1:p1");
        assert!(Stretch::parse("20\t10\ta\tb\tc\td\tidle").is_none());
        assert!(Stretch::parse("nonsense").is_none());
        for st in [St::Working, St::Needs, St::Done, St::Idle, St::Limit] {
            assert_eq!(St::parse(st.as_str()), Some(st));
        }
    }

    #[test]
    fn a_change_closes_the_stretch_before_it() {
        let mut w = Writer::default();
        // first look: opened from the fleet's `since`, nothing written
        let v = [sess("p1", "ignite", "working", 100)];
        assert!(w.step(150, &up(&v)).is_empty());
        // still working: nothing
        assert!(w.step(160, &up(&v)).is_empty());
        // it finished at 170
        let v = [sess("p1", "ignite", "done", 170)];
        let out = w.step(171, &up(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(
            (out[0].start, out[0].end, out[0].st),
            (100, 170, St::Working)
        );
        // stopped at its limit (no new `since`): from now
        let mut s = sess("p1", "ignite", "done", 170);
        s.limit = Some("2am".into());
        let out = w.step(200, &up(std::slice::from_ref(&s)));
        assert_eq!((out[0].start, out[0].end, out[0].st), (170, 200, St::Done));
    }

    #[test]
    fn gone_sessions_end_where_last_seen_and_down_machines_wait() {
        let mut w = Writer::default();
        let v = [
            sess("p1", "ignite", "working", 100),
            sess("p2", "temper", "blocked", 120),
        ];
        w.step(130, &up(&v));
        // the machine doesn't answer: nothing closes
        let down = [Seen {
            host: "b31",
            up: false,
            sessions: &v,
        }];
        assert!(w.step(200, &down).is_empty());
        // back, with only ignite: temper ended where it was last seen (130)
        let v2 = [sess("p1", "ignite", "working", 100)];
        let out = w.step(300, &up(&v2));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "temper");
        assert_eq!((out[0].start, out[0].end, out[0].st), (120, 130, St::Needs));
        // the machine left the fleet: ignite ends where it was seen
        let out = w.step(400, &[]);
        assert_eq!((out[0].start, out[0].end), (100, 300));
    }

    #[test]
    fn written_and_read_back_and_pruned() {
        let dir = std::env::temp_dir().join(format!("ch-states-w-{}", std::process::id()));
        let p = dir.join("states.tsv");
        let _ = std::fs::remove_file(&p);
        {
            let mut w = Writer::at_path(p.clone());
            let v = [sess("p1", "ignite", "working", 100)];
            w.step(110, &up(&v));
            let v = [sess("p1", "ignite", "idle", 150)];
            let out = w.step(150, &up(&v));
            append(&p, &out);
            // dropped: the idle stretch ends where it was seen
        }
        let back = read_from(&p);
        assert_eq!(back.len(), 1, "{back:?}");
        assert_eq!(back[0].st, St::Working);
        // (the idle one was opened at 150 and seen at 150: nothing to keep)
        let now = 150 + KEEP + 1;
        assert!(kept(back.clone(), now).is_empty());
        assert_eq!(kept(back, 160).len(), 1);
        let many: Vec<Stretch> = (0..CAP as u64 + 10)
            .map(|i| Stretch {
                start: i,
                end: i + 1,
                host: "h".into(),
                pane: "p".into(),
                name: "n".into(),
                agent: "a".into(),
                st: St::Idle,
            })
            .collect();
        let k = kept(many, 100);
        assert_eq!(k.len(), CAP);
        assert_eq!(k[0].start, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tidy_joins_and_cuts() {
        use St::*;
        // the same state overlapping: joined
        assert_eq!(
            tidy(vec![(10, 20, Working), (15, 30, Working)]),
            vec![(10, 30, Working)]
        );
        // a different state starting inside: the earlier one is cut there
        assert_eq!(
            tidy(vec![(10, 30, Working), (20, 25, Needs), (25, 40, Working)]),
            vec![(10, 20, Working), (20, 25, Needs), (25, 40, Working)]
        );
        // out of order and empty ones
        assert_eq!(
            tidy(vec![(50, 60, Idle), (5, 5, Done), (10, 50, Done)]),
            vec![(10, 50, Done), (50, 60, Idle)]
        );
    }
}
