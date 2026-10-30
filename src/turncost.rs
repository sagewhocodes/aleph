//! Cost per turn: what one finished turn took, as the finished alert and the
//! Inbox say it — `this turn: 412k tok · $1.20 · 6 min`.
//!
//! * **The turn** runs from when the agent started working to when it
//!   finished. The agent's own turn record wins (`Record::turn_ms`: Claude
//!   Code's `turn_duration`, Codex's `task_complete`, written as the turn
//!   ends, in the machine's own clock like its requests); else herdr's
//!   working time the Done event carries (`took`); else the state log's
//!   working stretch that ended there (`statelog.rs`).
//! * **Its tokens and $** are the usage records of that agent session
//!   (`Record::session` = the event's `native`, herdr's `agent_session`)
//!   inside the window, de-duplicated and priced like everything else
//!   (`store::build`: the agent's own reported cost wins, the rest is an
//!   estimate from the price table, marked `*`).
//! * **Which session:** the event carries the agent's session id (`native`),
//!   set when it was logged — the session may have moved on to another
//!   conversation, or ended, by the time anyone looks. Older events without
//!   it fall back to the live session's id.
//! * **Freshness:** the usage store is read about once a minute
//!   (`usage::Background`), so the turn's last requests may not be read yet
//!   when it ends. A turn is `complete` once a read of that machine *started*
//!   after it ended. The Inbox computes it when shown and again while it isn't
//!   complete ([`Cache`]); the alert asks the reader for that one machine
//!   right away and waits up to [`ALERT_WAIT`] for it — on the router's
//!   sending thread, never the daemon's loop — and goes without the line when
//!   it still isn't known ([`for_alert`]).

use crate::config::Config;
use crate::events::Event;
use crate::herdr::Session;
use crate::prices;
use crate::usage::{self, dash, parse::Record, store};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a finished alert waits for its machine's usage to be read.
pub const ALERT_WAIT: Duration = Duration::from_secs(8);
/// An agent's turn record this close to the event (either way, seconds) is
/// that turn's: herdr sees it done a poll later, and clocks differ a little.
const MATCH: u64 = 120;
/// The Inbox computes a turn that isn't complete again this often.
const AGAIN: Duration = Duration::from_secs(20);

/// One finished turn's numbers.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Turn {
    /// unix seconds (the agent's clock when `agent_timed`, else this one's)
    pub start: u64,
    pub end: u64,
    /// how long it worked, seconds
    pub secs: u64,
    pub tokens: u64,
    pub requests: u64,
    pub cost: f64,
    /// some of `cost` is an estimate from the price table
    pub estimated: bool,
    /// the window is the agent's own turn record
    pub agent_timed: bool,
    /// the machine's usage was read after the turn ended (more can't come)
    pub complete: bool,
}

/// Where the turn ran (pure — tested): `(start, end, secs, agent_timed)`.
/// `t` is when the event says it finished, `took` how long herdr saw it at
/// work, `turns` the session's turn records, `stretch` the state log's
/// working stretch that ended there.
pub fn window(
    t: u64,
    took: u64,
    native: &str,
    turns: &[Record],
    stretch: Option<(u64, u64)>,
) -> Option<(u64, u64, u64, bool)> {
    let own = turns
        .iter()
        .filter(|r| r.is_turn() && r.session == native && r.ts.abs_diff(t) <= MATCH)
        .min_by_key(|r| r.ts.abs_diff(t));
    if let Some(r) = own {
        let secs = r.turn_ms.div_ceil(1000);
        return Some((r.ts.saturating_sub(secs), r.ts, secs, true));
    }
    if took > 0 {
        return Some((t.saturating_sub(took), t, took, false));
    }
    let (a, _) = stretch?;
    (a < t).then(|| (a, t, t - a, false))
}

/// Which turn: the machine and agent session it ran in, when it finished,
/// how long herdr saw it at work, the state log's stretch that ended there.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ask<'a> {
    pub host: &'a str,
    pub native: &'a str,
    pub t: u64,
    pub took: u64,
    pub stretch: Option<(u64, u64)>,
}

impl<'a> Ask<'a> {
    pub fn of(e: &'a Event, native: &'a str) -> Ask<'a> {
        Ask {
            host: &e.host,
            native,
            t: e.t,
            took: e.took,
            stretch: None,
        }
    }
}

/// Records of `host` (two aliases of one machine are one, `store::same_machine`):
/// a home shared between machines (NFS on a cluster) shows the same agent
/// session to each, and only the one it ran on counts.
pub fn on_host<'r>(recs: &'r [Record], host: &str) -> Vec<&'r Record> {
    let canon = store::same_machine();
    let one = |h: &str| canon.get(h).cloned().unwrap_or_else(|| h.to_string());
    let me = one(host);
    recs.iter().filter(|r| one(&r.host) == me).collect()
}

/// The turn `a` asks about (pure — tested): its window, then the session's
/// requests on its machine in it, priced. None when it can't be told (no
/// window, no requests in it).
pub fn of(a: &Ask, recs: &[Record], table: &prices::Table, tz: i64) -> Option<Turn> {
    if a.native.is_empty() {
        return None;
    }
    let mine: Vec<Record> = on_host(recs, a.host)
        .into_iter()
        .filter(|r| r.session == a.native)
        .cloned()
        .collect();
    // streamed and re-read lines collapse first (the last one of each wins)
    let mine = store::dedup(mine);
    let (start, end, secs, agent_timed) = window(a.t, a.took, a.native, &mine, a.stretch)?;
    // the agent's own window is in its clock, like its requests; ours may be
    // a poll late and a few seconds off theirs (a next turn can start at once)
    let (lo, hi) = if agent_timed {
        (start.saturating_sub(2), end + 2)
    } else {
        (start.saturating_sub(5), end + 3)
    };
    let inside: Vec<Record> = mine
        .into_iter()
        .filter(|r| !r.is_turn() && r.ts >= lo && r.ts <= hi)
        .collect();
    if inside.is_empty() {
        return None;
    }
    let mut out = Turn {
        start,
        end,
        secs,
        agent_timed,
        ..Default::default()
    };
    for r in store::build(inside, table, tz) {
        out.tokens += r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
        out.requests += r.requests;
        out.cost += r.cost_reported + r.cost_estimated;
        out.estimated |= r.cost_estimated > 0.0;
    }
    Some(out)
}

/// `412k tok`, `$1.20*`, `6 min` — what's known, in that order.
pub fn parts(t: &Turn) -> Vec<String> {
    let mut v = vec![format!("{} tok", dash::tokens(t.tokens))];
    if let Some(m) = money(t.cost, t.estimated) {
        v.push(m);
    }
    if t.secs > 0 {
        v.push(minutes(t.secs));
    }
    v
}

/// `this turn: 412k tok · $1.20 · 6 min`.
pub fn line(t: &Turn) -> String {
    format!("this turn: {}", parts(t).join(" · "))
}

/// `$1.20`, `$1.20*` (an estimate), `<$0.01`; None with $ hidden
/// (`analytics.cost = off`) or nothing spent; showcase's money lens applies.
fn money(v: f64, estimated: bool) -> Option<String> {
    if v <= 0.0 || !dash::cost_shown() {
        return None;
    }
    let Some(v) = crate::showcase::money(v) else {
        return Some(crate::showcase::MONEY_HIDDEN.into());
    };
    let s = if v >= 100.0 {
        format!("${v:.0}")
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else {
        "<$0.01".into()
    };
    Some(if estimated { s + "*" } else { s })
}

/// `40 s`, `6 min`, `1 h 12 min`.
pub fn minutes(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs} s"),
        60..=3599 => format!("{} min", (secs + 30) / 60),
        _ => {
            let m = (secs + 30) / 60;
            match m % 60 {
                0 => format!("{} h", m / 60),
                r => format!("{} h {r} min", m / 60),
            }
        }
    }
}

// ------------------------------------------------------------------ reading

/// The agent session a finished turn belongs to: the event's own id, else
/// the live session's (older events, a hook that didn't say).
pub fn native_of(e: &Event, live: Option<&Session>) -> String {
    if !e.native.is_empty() {
        return e.native.clone();
    }
    live.and_then(|s| s.native_id.clone()).unwrap_or_default()
}

/// When the last good read of `host`'s usage started (its finish less its run).
pub fn read_from(host: &str) -> Option<u64> {
    let st = store::Offsets::load();
    let h = st.hosts.get(host)?;
    if h.ok_at == 0 {
        return None;
    }
    let took = match &h.run {
        Some(r) if r.error.is_none() && r.at == h.ok_at => r.took_ms / 1000 + 1,
        // the last run failed: when the good one began isn't kept, so be wary
        _ => 120,
    };
    Some(h.ok_at.saturating_sub(took))
}

/// Has `host`'s usage been read since `t`?
pub fn fresh(host: &str, t: u64) -> bool {
    read_from(host).is_some_and(|r| r >= t)
}

/// The state log's working stretch of `e`'s session that ended as it
/// finished (when the event doesn't say how long it worked).
fn stretch_of(e: &Event) -> Option<(u64, u64)> {
    crate::statelog::read_from(&crate::statelog::file())
        .into_iter()
        .filter(|s| {
            s.host == e.host
                && s.st.busy()
                && (s.name == e.name || (!e.pane.is_empty() && s.pane == e.pane))
                && s.end <= e.t + 5
                && s.end + MATCH >= e.t
        })
        .max_by_key(|s| s.end)
        .map(|s| (s.start, s.end))
}

/// Records of these agent sessions in the months around `t` (two at most).
pub fn records(natives: &[&str], from: u64, to: u64) -> Vec<Record> {
    let mut ms = vec![store::month_of(from)];
    let last = store::month_of(to);
    if !ms.contains(&last) {
        ms.push(last);
    }
    ms.iter()
        .flat_map(|m| store::read_month_matching(m, natives))
        .collect()
}

/// A finished turn's numbers from the store as it stands (reads files; off
/// the UI thread). None: analytics off, no session id, nothing in its window.
pub fn lookup(cfg: &Config, e: &Event, native: &str) -> Option<Turn> {
    if native.is_empty() || !usage::enabled(cfg) {
        return None;
    }
    let stretch = if e.took == 0 { stretch_of(e) } else { None };
    let back = e.took.max(stretch.map_or(0, |s| e.t.saturating_sub(s.0)));
    // a day back reaches any turn record of it
    let recs = records(&[native], e.t.saturating_sub(back + 86_400), e.t + MATCH);
    let ask = Ask {
        stretch,
        ..Ask::of(e, native)
    };
    let mut t = of(
        &ask,
        &recs,
        &prices::Table::from_cfg(cfg),
        crate::ui::utc_offset(),
    )?;
    t.complete = fresh(&e.host, e.t);
    Some(t)
}

/// The alert's line for a finished turn, once its numbers are all in: asks
/// the usage reader for that machine now when it hasn't read it since the
/// turn ended, waiting [`ALERT_WAIT`] at most (blocking: the router's
/// sending thread calls it). None: not known in time, or nothing to say.
pub fn for_alert(cfg: &Config, e: &Event, live: Option<&Session>) -> Option<String> {
    let native = native_of(e, live);
    if native.is_empty() || !usage::enabled(cfg) || cfg!(test) {
        return None;
    }
    let read_after = read_after_turn(cfg, e);
    let t = lookup(cfg, e, &native)?;
    if !t.complete && !read_after {
        crate::log::debug(
            "notify",
            &format!(
                "{}:{}: its usage wasn't read in time — sent without this turn's numbers",
                e.host, e.name
            ),
        );
        return None;
    }
    Some(line(&t))
}

/// Has `e`'s machine been read since the turn ended? Asks for it now when not,
/// waiting [`ALERT_WAIT`] at most. True only for a read asked for after the
/// turn, done in time and whole: what it found is that turn's, even inside the
/// same second (`fresh` can't tell).
fn read_after_turn(cfg: &Config, e: &Event) -> bool {
    if fresh(&e.host, e.t) {
        return false;
    }
    let (tx, rx) = mpsc::channel();
    let (c, host) = (cfg.clone(), e.host.clone());
    // (it finishes even if we stop waiting: the Inbox gets it then)
    std::thread::spawn(move || {
        let r = usage::sync(&c, std::slice::from_ref(&host), false, ALERT_WAIT, |_| {});
        let _ = tx.send(r);
    });
    rx.recv_timeout(ALERT_WAIT).is_ok_and(|r| {
        r.iter()
            .any(|x| x.host == e.host && x.error.is_none() && x.budget.is_none())
    })
}

/// Should this finished turn stay quiet? A compaction ending isn't a turn, and
/// nor is an agent waking because a subagent reported back with others still
/// out (`notify.subagents` turns that one off). Blocking, like [`for_alert`].
/// Nothing read in time: it isn't held back.
pub fn quiet_turn(cfg: &Config, e: &Event, live: Option<&Session>) -> bool {
    let native = native_of(e, live);
    if native.is_empty() || !usage::enabled(cfg) || cfg!(test) {
        return false;
    }
    read_after_turn(cfg, e);
    let steps = usage::steps::of_session(&e.host, &native, 2);
    let at = (e.t + 1) * 1000;
    usage::steps::ended_in_compaction(&steps, at)
        || (!cfg.flag("notify.subagents") && usage::steps::woken_by_subagent(&steps, at))
}

/// A finished turn for the desktop banner: its numbers after its words.
pub fn with_line(e: &Event, line: Option<&str>) -> Event {
    let mut e = e.clone();
    if let Some(l) = line {
        let words = if e.body.is_empty() { &e.title } else { &e.body };
        e.body = if words.trim().is_empty() {
            l.to_string()
        } else {
            format!("{}\n{l}", words.trim_end())
        };
    }
    e
}

// ------------------------------------------------------------------ the Inbox's cache

struct Slot {
    turn: Option<Turn>,
    at: Instant,
    /// nothing more will come (complete, or read since and still nothing)
    settled: bool,
}

/// Turns as the Inbox shows them: computed in the background the first time
/// an event is shown, and again every [`AGAIN`] until its machine's usage
/// was read after it ended. One computation at a time.
#[derive(Clone, Default)]
pub struct Cache {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    busy: Arc<AtomicBool>,
}

impl Cache {
    /// What's known of `e`'s turn now (asks for it when it isn't settled).
    pub fn get(&self, cfg: &Config, e: &Event, native: &str) -> Option<Turn> {
        if native.is_empty() {
            return None;
        }
        let id = e.id();
        let (have, due) = match self.slots.lock() {
            Ok(m) => match m.get(&id) {
                Some(s) => (s.turn.clone(), !s.settled && s.at.elapsed() >= AGAIN),
                None => (None, true),
            },
            Err(_) => return None,
        };
        if due && !cfg!(test) && !self.busy.swap(true, Ordering::SeqCst) {
            let (slots, busy, cfg, e, native) = (
                self.slots.clone(),
                self.busy.clone(),
                cfg.clone(),
                e.clone(),
                native.to_string(),
            );
            std::thread::spawn(move || {
                let turn = lookup(&cfg, &e, &native);
                let settled = turn
                    .as_ref()
                    .map_or(fresh(&e.host, e.t + 60), |t| t.complete);
                if let Ok(mut m) = slots.lock() {
                    m.insert(
                        e.id(),
                        Slot {
                            turn,
                            at: Instant::now(),
                            settled,
                        },
                    );
                    // a long session of reading the Inbox: keep the newest few hundred
                    if m.len() > 400 {
                        let mut v: Vec<(String, Instant)> =
                            m.iter().map(|(k, s)| (k.clone(), s.at)).collect();
                        v.sort_by_key(|x| x.1);
                        for (k, _) in v.into_iter().take(100) {
                            m.remove(&k);
                        }
                    }
                }
                busy.store(false, Ordering::SeqCst);
            });
        }
        have
    }

    /// Known numbers for an event (render tests).
    #[cfg(test)]
    pub fn put(&self, e: &Event, turn: Turn) {
        self.slots.lock().unwrap().insert(
            e.id(),
            Slot {
                settled: turn.complete,
                turn: Some(turn),
                at: Instant::now(),
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(ts: u64, session: &str, key: &str, input: u64, out: u64, cost: Option<f64>) -> Record {
        Record {
            ts,
            host: "b31".into(),
            agent: "claude".into(),
            session: session.into(),
            key: key.into(),
            model: "claude-sonnet-4-5".into(),
            input,
            output: out,
            cost,
            ..Default::default()
        }
    }

    fn turn(ts: u64, session: &str, ms: u64) -> Record {
        Record {
            ts,
            host: "b31".into(),
            agent: "claude".into(),
            session: session.into(),
            key: format!("turn-{ts}"),
            turn_ms: ms,
            ..Default::default()
        }
    }

    fn table() -> prices::Table {
        prices::Table::from_cfg(&Config::in_memory(&[]))
    }

    #[test]
    fn the_agents_own_turn_record_wins() {
        // herdr saw it done at 1000 after 300 s; Claude says its turn took 6 min, ending at 990
        let turns = vec![turn(990, "s1", 360_000), turn(400, "s1", 10_000)];
        assert_eq!(
            window(1000, 300, "s1", &turns, None),
            Some((630, 990, 360, true))
        );
        // another session's record, or one far off, doesn't count
        assert_eq!(
            window(1000, 300, "s2", &turns, None),
            Some((700, 1000, 300, false))
        );
        assert_eq!(
            window(5000, 0, "s1", &turns, Some((4000, 4990))),
            Some((4000, 5000, 1000, false))
        );
        // nothing known: no window
        assert_eq!(window(5000, 0, "s1", &turns, None), None);
    }

    #[test]
    fn a_turns_tokens_and_cost_in_its_window() {
        let recs = vec![
            // before the turn: the previous one's
            req(500, "s1", "k0", 9_000, 900, Some(5.0)),
            req(700, "s1", "k1", 100_000, 2_000, Some(0.40)),
            req(800, "s1", "k2", 200_000, 10_000, Some(0.80)),
            // streamed twice: counted once
            req(800, "s1", "k2", 200_000, 10_000, Some(0.80)),
            // another session at the same time
            req(750, "s2", "k3", 1_000_000, 0, Some(9.0)),
            // the next turn, after it ended
            req(1100, "s1", "k4", 50_000, 0, Some(1.0)),
            // the same conversation read on another machine (a shared home):
            // not this turn's
            Record {
                host: "gpu2".into(),
                ..req(800, "s1", "k2", 200_000, 10_000, Some(0.80))
            },
        ];
        let ask = |t: u64, took: u64, native: &'static str| Ask {
            host: "b31",
            native,
            t,
            took,
            stretch: None,
        };
        let t = of(&ask(1000, 300, "s1"), &recs, &table(), 0).unwrap();
        assert_eq!((t.tokens, t.requests, t.secs), (312_000, 2, 300));
        assert!((t.cost - 1.20).abs() < 1e-9 && !t.estimated, "{t:?}");
        assert_eq!(line(&t), "this turn: 312k tok · $1.20 · 5 min");
        // no reported cost: priced from the table, marked as an estimate
        let est: Vec<Record> = recs
            .iter()
            .map(|r| Record {
                cost: None,
                ..r.clone()
            })
            .collect();
        let t = of(&ask(1000, 300, "s1"), &est, &table(), 0).unwrap();
        assert!(t.estimated && t.cost > 0.0);
        assert!(parts(&t)[1].ends_with('*'), "{:?}", parts(&t));
    }

    #[test]
    fn an_event_without_data_says_nothing() {
        let recs = vec![req(700, "s1", "k1", 100, 10, None)];
        let ask = |t: u64, took: u64, native: &'static str| Ask {
            host: "b31",
            native,
            t,
            took,
            stretch: None,
        };
        // no session id, no window, nothing in the window, another machine
        assert!(of(&ask(1000, 300, ""), &recs, &table(), 0).is_none());
        assert!(of(&ask(1000, 0, "s1"), &recs, &table(), 0).is_none());
        assert!(of(&ask(5000, 60, "s1"), &recs, &table(), 0).is_none());
        let elsewhere = Ask {
            host: "gpu2",
            ..ask(1000, 300, "s1")
        };
        assert!(of(&elsewhere, &recs, &table(), 0).is_none());
        let e = Event {
            native: String::new(),
            ..Default::default()
        };
        let s = Session {
            native_id: Some("live-id".into()),
            ..Default::default()
        };
        // an older event: the live session's id stands in
        assert_eq!(native_of(&e, Some(&s)), "live-id");
        assert_eq!(native_of(&e, None), "");
        // the Inbox's cache asks for nothing without an id
        assert!(Cache::default()
            .get(&Config::in_memory(&[]), &e, "")
            .is_none());
    }

    #[test]
    fn looked_up_from_the_store_and_marked_until_read() {
        let now = crate::cache::now();
        let id = format!("turncost-test-{}", std::process::id());
        let r = |dt: u64, key: &str| Record {
            host: "tc-test-host".into(),
            ..req(now - dt, &id, key, 40_000, 1_000, Some(0.25))
        };
        let recs = vec![r(200, "a"), r(100, "b"), r(5000, "old")];
        store::append(&recs).unwrap();
        let e = Event {
            t: now - 30,
            host: "tc-test-host".into(),
            name: "weave".into(),
            kind: crate::events::Kind::Done,
            took: 300,
            native: id.clone(),
            ..Default::default()
        };
        let on = Config::in_memory(&["analytics.enabled=on"]);
        let t = lookup(&on, &e, &id).unwrap();
        assert_eq!((t.requests, t.tokens), (2, 82_000));
        assert!((t.cost - 0.5).abs() < 1e-9);
        // that machine's usage was never read after it ended: more may come
        assert!(!t.complete);
        // analytics off: nothing at all
        let off = Config::in_memory(&["analytics.enabled=off"]);
        assert!(lookup(&off, &e, &id).is_none());
    }

    #[test]
    fn words_for_time_and_the_banner() {
        assert_eq!(minutes(40), "40 s");
        assert_eq!(minutes(359), "6 min");
        assert_eq!(minutes(3600), "1 h");
        assert_eq!(minutes(4320), "1 h 12 min");
        let e = Event {
            title: "fix the flaky test".into(),
            ..Default::default()
        };
        assert_eq!(
            with_line(&e, Some("this turn: 4k tok")).body,
            "fix the flaky test\nthis turn: 4k tok"
        );
        assert_eq!(with_line(&e, None).body, "");
    }
}
