//! The automation hub: what happens without you, driven by fleet polls.
//!
//! * **Auto-approve** (`auto_approve=on`): when a session starts asking, its
//!   question is matched against your `approve.<rule>=<pattern>` rules (and
//!   the `approve_deny` list, which always wins). A match answers with the
//!   question's yes option (`herdr::approve_keys`).
//! * **Phone/remote notifications** (`notify.rs`) for the kinds in
//!   `notify.remote_on`.
//! * **Task queue** (`queue.rs`): idle sessions get the next queued task.
//! * **Self-heal** (`auto_heal=on`): a machine that was running herdr and now
//!   answers ssh without it gets herdr restarted and its sessions restored.
//!
//! Exactly one process runs it: `aleph watch` when it's running, otherwise the
//! dashboard. Everything it does goes to the inbox.

use crate::cli::Args;
use crate::config::Config;
use crate::events::{self, Event, Kind};
use crate::fleet::{Conn, Fleet, Transition};
use crate::herdr::{self, Herdr};
use crate::out::{err, fail, hint, json, ok, warn};
use crate::{queue, sshcfg};
use serde_json::json;
use std::collections::HashSet;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Never auto-approve questions matching these (comma-separated patterns).
pub const DEFAULT_DENY: &str =
    "*rm -rf*,*sudo *,*git push*,*--force*,*drop table*,*mkfs*,*dd if=*,*shutdown*,*reboot*";

// ------------------------------------------------------------------ rules

/// Case-insensitive pattern match with `*` wildcards, anywhere in `text`
/// (patterns are implicitly `*…*`). Whitespace runs count as one space.
pub fn pmatch(pattern: &str, text: &str) -> bool {
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let (p, t) = (norm(pattern), norm(text));
    let mut at = 0;
    for piece in p.split('*').filter(|x| !x.is_empty()) {
        match t[at..].find(piece) {
            Some(i) => at += i + piece.len(),
            None => return false,
        }
    }
    true
}

pub fn rules(cfg: &Config) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = cfg
        .with_prefix("approve.")
        .into_iter()
        .filter(|(_, v)| !v.trim().is_empty())
        .collect();
    // showcase's demo world: its story's own rule, after yours
    v.extend(crate::showcase::world_rules());
    v
}

pub fn deny_list(cfg: &Config) -> Vec<String> {
    cfg.get("approve_deny")
        .unwrap_or_else(|| DEFAULT_DENY.to_string())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Which rule approves this question? Err says why not (no rule / denied /
/// no clear yes option).
pub fn decide(cfg: &Config, question: &str) -> Result<String, String> {
    if let Some(d) = deny_list(cfg).into_iter().find(|d| pmatch(d, question)) {
        return Err(format!("denied by approve_deny ({d})"));
    }
    let Some((name, _)) = rules(cfg).into_iter().find(|(_, p)| pmatch(p, question)) else {
        return Err("no rule matches".into());
    };
    if herdr::approve_keys(question).is_none() {
        return Err(format!(
            "rule '{name}' matches, but there's no clear yes option"
        ));
    }
    Ok(name)
}

/// First meaningful line of a question, for the inbox.
fn gist(q: &str) -> String {
    let lines: Vec<&str> = q.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    // the command being asked about is usually the line after the question
    let pick = lines
        .iter()
        .position(|l| l.ends_with('?'))
        .and_then(|i| {
            lines
                .get(i + 1)
                .filter(|l| !l.starts_with('❯') && !l.starts_with(char::is_numeric))
        })
        .or(lines.first())
        .copied()
        .unwrap_or("");
    crate::ui::trunc(pick, 140)
}

// ------------------------------------------------------------------ one hub

/// This computer's short name, lowercase, for `hub.machine`: on macOS its
/// LocalHostName (`scutil`), which stays put — `hostname` there follows
/// the network (an ISP's name for the line on one Wi-Fi, something else on the next);
/// elsewhere `hostname -s`.
pub fn this_machine() -> String {
    static N: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    N.get_or_init(|| {
        let stable = if cfg!(target_os = "macos") {
            crate::proc::cap(
                "scutil",
                &["--get", "LocalHostName"],
                Duration::from_secs(3),
            )
            .text()
            .trim()
            .to_lowercase()
        } else {
            String::new()
        };
        if stable.is_empty() {
            short_hostname()
        } else {
            stable
        }
    })
    .clone()
}

fn short_hostname() -> String {
    static N: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    N.get_or_init(|| {
        crate::proc::cap("hostname", &["-s"], Duration::from_secs(3))
            .text()
            .trim()
            .to_lowercase()
    })
    .clone()
}

/// Is this computer the one that runs the hub? (`hub.machine` empty = yes;
/// either of its names counts.)
pub fn hub_here(cfg: &Config) -> bool {
    match cfg.get("hub.machine") {
        None => true,
        Some(m) => {
            let m = m.trim().to_lowercase();
            m.is_empty() || m == "local" || m == this_machine() || m == short_hostname()
        }
    }
}

/// The hub lock (`state/hub.lock`, an OS file lock): the process holding it
/// runs the hub. The OS drops it when the process exits, crashes or execs.
pub struct HubLock(#[allow(dead_code)] std::fs::File);

impl HubLock {
    pub fn path() -> std::path::PathBuf {
        crate::config::state_dir().join("hub.lock")
    }

    /// Take the lock if nobody holds it.
    pub fn try_take() -> Option<HubLock> {
        Self::try_take_at(&Self::path())
    }

    pub fn try_take_at(p: &std::path::Path) -> Option<HubLock> {
        use std::io::Write;
        let _ = std::fs::create_dir_all(p.parent()?);
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(p)
            .ok()?;
        f.try_lock().ok()?;
        let _ = f.set_len(0);
        let _ = writeln!(f, "{}", std::process::id());
        Some(HubLock(f))
    }

    /// Does some process (this one too) hold it right now?
    pub fn held() -> bool {
        Self::held_at(&Self::path())
    }

    pub fn held_at(p: &std::path::Path) -> bool {
        let Ok(f) = std::fs::OpenOptions::new().write(true).open(p) else {
            return false;
        };
        f.try_lock().is_err()
    }

    /// Take it for a moment, without writing our pid (`holder` keeps saying
    /// who ran the hub last): a process that doesn't run the hub claims the
    /// closes that are due while nobody does (`closing.rs`).
    pub fn try_borrow() -> Option<HubLock> {
        Self::try_borrow_at(&Self::path())
    }

    pub fn try_borrow_at(p: &std::path::Path) -> Option<HubLock> {
        let _ = std::fs::create_dir_all(p.parent()?);
        let f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(p)
            .ok()?;
        f.try_lock().ok()?;
        Some(HubLock(f))
    }

    /// Who holds it (the pid written there), for doctor and status.
    pub fn holder() -> Option<u32> {
        std::fs::read_to_string(Self::path())
            .ok()
            .and_then(|s| s.trim().parse().ok())
    }
}

// ------------------------------------------------------------------ the hub

pub struct Hub {
    /// a queue dispatch is in flight
    busy: Arc<AtomicBool>,
    /// sessions just given a task: (host|pane, when)
    given: Arc<Mutex<Vec<(String, Instant)>>>,
    /// machines seen running herdr this run (self-heal only restarts those)
    was_up: HashSet<String>,
    healing: Arc<Mutex<HashSet<String>>>,
    /// sessions being auto-approved right now
    approving: Arc<Mutex<HashSet<String>>>,
    last_queue: Option<Instant>,
    /// sessions reported as stopped at their limit (once each)
    limited: HashSet<String>,
    /// Moshi: inbox cards, phone approvals, live job rows
    moshi: crate::moshibridge::Bridge,
    /// events out to their sinks (routes, quiet hours, escalation)
    pub router: crate::router::Router,
    /// what the agents' own hooks said (each machine's spool)
    spool: crate::hooks::Spool,
    /// phone buttons tapped (ntfy's reply topic)
    replies: crate::actions::Replies,
    /// the last look for needs-you alerts gone stale (`purge`)
    last_purge: Option<Instant>,
    /// each session's states over time, for the timeline charts (the hub
    /// is the log's one writer)
    states: crate::statelog::Writer,
    /// closes waiting out their grace (`closing.json`), carried out here
    pub closer: crate::closing::Closer,
    /// the session journal: today's file now and then, yesterday's finished
    journal: crate::journal::Writer,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        Hub {
            busy: Arc::new(AtomicBool::new(false)),
            given: Arc::new(Mutex::new(Vec::new())),
            was_up: HashSet::new(),
            healing: Arc::new(Mutex::new(HashSet::new())),
            approving: Arc::new(Mutex::new(HashSet::new())),
            last_queue: None,
            limited: HashSet::new(),
            moshi: crate::moshibridge::Bridge::new(),
            router: crate::router::Router::new(false),
            spool: crate::hooks::Spool::default(),
            replies: crate::actions::Replies::default(),
            last_purge: None,
            states: crate::statelog::Writer::new(),
            closer: Default::default(),
            journal: Default::default(),
        }
    }

    /// Should this process try to run the hub? The daemon always does; the
    /// dashboard only while no daemon is running. Whoever then holds the hub
    /// lock (`HubLock`) runs it, so it never runs twice on this computer, and
    /// `hub.machine` picks the one computer in a fleet with several daemons.
    pub fn owner(cfg: &Config, in_watch: bool) -> bool {
        hub_here(cfg) && (in_watch || !queue::watch_running(cfg))
    }

    /// Call after every `fleet.tick()`. `fresh`: hosts whose first poll this
    /// was (their state isn't news).
    pub fn tick(&mut self, cfg: &Config, fleet: &Fleet, trans: &[Transition], fresh: &[String]) {
        for t in trans {
            if fresh.contains(&t.session.host) {
                continue;
            }
            let s = &t.session;
            let kind = match (t.from.as_str(), s.status.as_str()) {
                (_, "blocked") => Some(Kind::Blocked),
                ("working", "done" | "idle") => Some(Kind::Done),
                ("", _) => Some(Kind::New),
                _ => None,
            };
            // (the router sends it on: the poll put it in the event log)
            let _ = kind;
            if s.blocked() && cfg.flag("auto_approve") {
                self.approve(cfg, fleet, s);
            }
        }
        self.states.observe(fleet);
        self.journal.tick(cfg, fleet);
        self.limits(cfg, fleet);
        // a question just answered: its Inbox item goes now, not in 10 s
        if trans.iter().any(|t| t.from == "blocked") {
            self.last_purge = None;
        }
        self.purge(cfg, fleet);
        // showcase's demo world: no phone (Moshi), and self-heal only for its
        // story's drop (what it restarts is the world's own herdr)
        let real = !crate::showcase::demo();
        if real || crate::showcase::world_heals() {
            self.heal(cfg, fleet);
        }
        self.dispatch_queue(fleet);
        // closes whose grace has passed (or whose session ended by itself)
        self.closer.tick(cfg, fleet, true);
        if real {
            self.moshi.tick(cfg, fleet, trans, fresh);
        }
        // the agents' own words, then every new event out through the routes
        self.spool.tick(cfg, fleet);
        self.router.tick(cfg, fleet);
        // (phone replies come over the network: not in the demo world)
        if real {
            self.replies.tick(cfg, fleet);
        }
    }

    /// `purge`, from a process that doesn't run the hub, while nobody here does
    /// (the hub on another machine, say): the lock borrowed, so it never runs
    /// beside a daemon's.
    pub fn purge_borrowed(&mut self, cfg: &Config, fleet: &Fleet) {
        let due = self
            .last_purge
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(10));
        if due {
            if let Some(_lock) = HubLock::try_borrow() {
                self.purge(cfg, fleet);
            }
        }
    }

    /// Needs-you alerts whose session was answered, ended or moved on, and any
    /// older than `inbox.needs_ttl`, go to the archive — for the dashboard,
    /// Raycast, the menu bar and the web console at once — and their desktop
    /// banners with them. Every 10 s at most.
    fn purge(&mut self, cfg: &Config, fleet: &Fleet) {
        if self
            .last_purge
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.last_purge = Some(Instant::now());
        let ttl = match cfg.get_or("inbox.needs_ttl").trim() {
            "off" | "never" | "" | "0" => None,
            v => crate::log::parse_duration(v).map(|d| d.as_secs()),
        };
        // the whole log the Inbox can show, not just its newest: an old
        // question (a hook's, without herdr's numbers) never went otherwise
        let evs = crate::events::load(crate::events::CAP);
        let mut dismissed = crate::events::dismissed();
        let sessions = fleet.sessions();
        let polled = |h: &str| {
            fleet
                .host(h)
                // (a paused machine's sessions are as last seen: no answer is known)
                .is_some_and(|x| matches!(x.conn, crate::fleet::Conn::Up) && !x.stale && !x.paused)
        };
        let stale = crate::events::stale_questions(
            &evs,
            &dismissed,
            &sessions,
            &polled,
            crate::cache::now(),
            ttl,
        );
        if stale.is_empty() {
            return;
        }
        for e in evs
            .iter()
            .filter(|e| stale.contains(&crate::events::key(e)))
        {
            // the banner too, unless something newer from that session replaced it
            if !evs
                .iter()
                .any(|o| o.host == e.host && o.pane == e.pane && o.t > e.t)
            {
                crate::watch::withdraw(&e.host, &e.pane);
            }
        }
        crate::log::info(
            "inbox",
            &format!(
                "{} needs-you alert(s) archived (answered, ended or old)",
                stale.len()
            ),
        );
        dismissed.extend(stale);
        crate::events::save_dismissed(&dismissed, &crate::events::load(crate::events::CAP));
    }

    /// A session newly stopped at its usage limit: into the inbox, and out to
    /// the phone (`notify.remote_on` has `limit` by default).
    fn limits(&mut self, cfg: &Config, fleet: &Fleet) {
        let now: HashSet<String> = fleet
            .sessions()
            .iter()
            .filter(|s| s.limit.is_some())
            .map(|s| s.key())
            .collect();
        for s in fleet.sessions() {
            let Some(r) = &s.limit else { continue };
            if !self.limited.insert(s.key()) {
                continue;
            }
            // already told (by this or an earlier run) within the last 6 h
            // (showcase's demo world: since its story's last hit)
            let told = crate::showcase::limit_told_for(6 * 3600);
            let t0 = crate::cache::now().saturating_sub(told);
            let told = crate::events::load(400).iter().any(|e| {
                e.kind == Kind::Limit && e.host == s.host && e.name == s.name && e.t >= t0
            });
            if told {
                continue;
            }
            let e = Event {
                t: crate::cache::now(),
                host: s.host.clone(),
                name: s.name.clone(),
                agent: s.agent.clone(),
                kind: Kind::Limit,
                title: if r.is_empty() {
                    String::new()
                } else {
                    format!("resets {r}")
                },
                ..Default::default()
            };
            // the router sends it on (it reads the log)
            crate::events::append(e.clone());
            self.moshi.limit(cfg, s, r);
            // opt-in: once it resets, carry on where it stopped
            if crate::permit::matches(
                &crate::permit::globs(cfg, "limits.auto_resume"),
                &s.host,
                &s.agent,
            ) {
                match parse_reset(r, crate::cache::now(), crate::ui::utc_offset()) {
                    Some(at) => {
                        // the agent's own message (resume.<agent>), else everyone's
                        let prompt = cfg
                            .get(&format!("resume.{}", s.agent))
                            .filter(|p| !p.trim().is_empty())
                            .unwrap_or_else(|| cfg.get_or("limits.resume_prompt"));
                        crate::queue::add(crate::queue::Task {
                            host: s.host.clone(),
                            prompt,
                            to: Some(s.name.clone()),
                            after: Some(at + 60),
                            ..Default::default()
                        });
                        crate::log::info(
                            "limits",
                            &format!(
                                "{}: queued to carry on at {}",
                                s.key(),
                                crate::ui::clock(at + 60)
                            ),
                        );
                    }
                    None => crate::log::warn(
                        "limits",
                        &format!(
                            "{}: couldn't read when it resets ({r:?}) — not queued",
                            s.key()
                        ),
                    ),
                }
            }
        }
        self.limited.retain(|k| now.contains(k));
    }

    fn approve(&self, cfg: &Config, fleet: &Fleet, s: &herdr::Session) {
        let key = s.key();
        if !self.approving.lock().unwrap().insert(key.clone()) {
            return;
        }
        let t = fleet
            .host(&s.host)
            .map(|h| h.target.clone())
            .unwrap_or_else(|| cfg.target_for(&s.host, herdr::machine_for));
        let (cfg, s, approving) = (cfg.clone(), s.clone(), self.approving.clone());
        thread::spawn(move || {
            let h = Herdr::new(&t);
            if let Ok(q) = h.read(&s.pane_id, "detection", 0, false) {
                if let Ok(rule) = decide(&cfg, &q) {
                    if let Some(keys) = herdr::approve_keys(&q) {
                        let k: Vec<&str> = keys.iter().map(String::as_str).collect();
                        let sent = h.send_keys(&s.pane_id, &k);
                        crate::log::info(
                            "auto",
                            &format!(
                                "rule {rule} answered {} with {}: {:?}",
                                s.key(),
                                keys.join(" "),
                                sent
                            ),
                        );
                        let e = Event {
                            t: crate::cache::now(),
                            host: s.host.clone(),
                            name: s.name.clone(),
                            agent: s.agent.clone(),
                            kind: Kind::Auto,
                            title: match &sent {
                                Ok(()) => format!("rule {rule}: {}", gist(&q)),
                                Err(x) => format!("rule {rule} matched, but sending failed: {x}"),
                            },
                            ..Default::default()
                        };
                        events::append(e.clone());
                    }
                }
            }
            approving.lock().unwrap().remove(&s.key());
        });
    }

    fn heal(&mut self, cfg: &Config, fleet: &Fleet) {
        for h in &fleet.hosts {
            match &h.conn {
                Conn::Up => {
                    self.was_up.insert(h.alias.clone());
                }
                Conn::Down(e)
                    if crate::fleet::not_running(e)
                        && self.was_up.contains(&h.alias)
                        && cfg.flag("auto_heal")
                        && !sshcfg::is_local(&h.alias) =>
                {
                    if !self.healing.lock().unwrap().insert(h.alias.clone()) {
                        continue;
                    }
                    self.was_up.remove(&h.alias);
                    let (alias, healing) = (h.alias.clone(), self.healing.clone());
                    thread::spawn(move || {
                        let r = heal_now(&alias);
                        let e = Event {
                            t: crate::cache::now(),
                            host: alias.clone(),
                            name: String::new(),
                            agent: String::new(),
                            kind: Kind::Heal,
                            title: match r {
                                Ok(m) => m,
                                Err(x) => format!("couldn't restart herdr: {x}"),
                            },
                            ..Default::default()
                        };
                        events::append(e.clone());
                        healing.lock().unwrap().remove(&alias);
                    });
                }
                _ => {}
            }
        }
    }

    fn dispatch_queue(&mut self, fleet: &Fleet) {
        if self.busy.load(Ordering::SeqCst)
            || self
                .last_queue
                .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        self.last_queue = Some(Instant::now());
        // a session given a task stays "idle" in our view until the next poll
        let busy: Vec<String> = {
            let mut g = self.given.lock().unwrap();
            // until it has picked the task up (working, or asking), or 90 s
            let picked = |k: &str| {
                fleet
                    .sessions()
                    .iter()
                    .any(|s| s.key() == k && (s.status == "working" || s.blocked()))
            };
            g.retain(|(k, t)| t.elapsed() < Duration::from_secs(90) && !picked(k));
            g.iter().map(|(k, _)| k.clone()).collect()
        };
        let plans = queue::plans_for(fleet, &busy);
        if plans.is_empty() {
            return;
        }
        {
            let mut g = self.given.lock().unwrap();
            for p in &plans {
                if let queue::Plan::Send(_, k) = p {
                    g.push((k.clone(), Instant::now()));
                }
            }
        }
        self.busy.store(true, Ordering::SeqCst);
        let sessions: Vec<herdr::Session> = fleet.sessions().into_iter().cloned().collect();
        let targets: Vec<(String, crate::config::Target)> = fleet
            .hosts
            .iter()
            .map(|h| (h.alias.clone(), h.target.clone()))
            .collect();
        let busy = self.busy.clone();
        thread::spawn(move || {
            for p in &plans {
                queue::run(p, &sessions, |host| {
                    targets
                        .iter()
                        .find(|(a, _)| a == host)
                        .map(|(_, t)| t.clone())
                        .unwrap_or_else(|| Config::load().target_for(host, herdr::machine_for))
                });
            }
            busy.store(false, Ordering::SeqCst);
        });
    }
}

/// Start herdr on `alias` and restore its sessions (`aleph restore --yes`,
/// which starts herdr first). Blocking.
/// When a usage limit lifts, from what the agent said (`3pm`, `3:30 pm`,
/// `15:00`, `in 2h 13m`, `2h`, with a timezone in brackets ignored): the
/// unix time, or None.
pub fn parse_reset(r: &str, now: u64, off: i64) -> Option<u64> {
    let mut t = r.to_lowercase();
    if let Some(i) = t.find('(') {
        t.truncate(i);
    }
    let t = t
        .replace("resets", "")
        .replace("at ", "")
        .replace(" at", "")
        .trim()
        .to_string();
    // a duration: "in 2h 13m", "2h13m", "45m"
    let d = t.trim_start_matches("in ").replace(' ', "");
    if !d.is_empty() && d.chars().all(|c| c.is_ascii_digit() || "hms".contains(c)) {
        let mut secs = 0u64;
        let mut n = String::new();
        for c in d.chars() {
            if c.is_ascii_digit() {
                n.push(c);
            } else {
                let v: u64 = n.parse().ok()?;
                n.clear();
                secs += v * match c {
                    'h' => 3600,
                    'm' => 60,
                    _ => 1,
                };
            }
        }
        return (secs > 0).then_some(now + secs);
    }
    // a time of day: "3pm", "3:30 pm", "15:00"
    let pm = t.ends_with("pm");
    let am = t.ends_with("am");
    let clock = t.trim_end_matches("pm").trim_end_matches("am").trim();
    let (h, m) = match clock.split_once(':') {
        Some((h, m)) => (h.trim().parse::<i64>().ok()?, m.trim().parse::<i64>().ok()?),
        None => (clock.parse::<i64>().ok()?, 0),
    };
    let h = match (am, pm, h) {
        (true, _, 12) => 0,
        (_, true, h) if h < 12 => h + 12,
        (_, _, h) => h,
    };
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    crate::queue::parse_after(&format!("{h:02}:{m:02}"), now, off)
        .ok()
        .flatten()
}

pub fn heal_now(alias: &str) -> Result<String, String> {
    // showcase's demo world: its own herdr, restarted where its story stopped it
    if let Some(r) = crate::showcase::world_heal(alias) {
        return r;
    }
    if let Some(e) = crate::showcase::refused() {
        return Err(e);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let o = crate::proc::run(
        std::process::Command::new(exe).args(["--host", alias, "restore", "--yes"]),
        Duration::from_secs(180),
        None,
    )
    .map_err(|e| e.to_string())?;
    if o.ok() {
        Ok("herdr restarted; lost sessions restored".into())
    } else {
        Err(o.reason())
    }
}

// ------------------------------------------------------------------ CLI

/// `aleph auto [status | on | off | add <rule> <pattern> | rm <rule> | test <question…>]`
pub fn cmd(mut a: Args) -> ExitCode {
    let p: Vec<String> = a.positional().iter().map(|s| s.to_string()).collect();
    let sub = p.first().map(String::as_str).unwrap_or("status");
    match sub {
        "on" | "off" => {
            let _ = a
                .cfg
                .set("auto_approve", if sub == "on" { "true" } else { "false" });
            if sub == "on" {
                ok("auto-approve is on");
                if rules(&a.cfg).is_empty() {
                    hint("no rules yet — aleph auto add tests '*pytest*'");
                }
                if !queue::watch_running(&a.cfg) {
                    hint("rules apply while aleph watch (or the dashboard) runs — aleph watch --install");
                }
            } else {
                ok("auto-approve is off");
            }
            ExitCode::SUCCESS
        }
        "add" => {
            let (Some(name), Some(_)) = (p.get(1), p.get(2)) else {
                return fail("usage: aleph auto add <rule> <pattern…>   e.g. aleph auto add tests '*pytest*'");
            };
            if !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return fail("rule names are letters, digits, - and _");
            }
            let pat = p[2..].join(" ");
            let _ = a.cfg.set(&format!("approve.{name}"), &pat);
            ok(&format!("rule {name}: approve questions matching “{pat}”"));
            if !a.cfg.flag("auto_approve") {
                hint("auto-approve is off — aleph auto on");
            }
            ExitCode::SUCCESS
        }
        "rm" | "remove" => {
            let Some(name) = p.get(1) else {
                return fail("usage: aleph auto rm <rule>");
            };
            let _ = a.cfg.unset(&format!("approve.{name}"));
            ok(&format!("rule {name} removed"));
            ExitCode::SUCCESS
        }
        "test" => {
            // `\n` in the argument stands for a line break (questions are menus)
            let q = p[1..].join(" ").replace("\\n", "\n");
            if q.is_empty() {
                return fail("usage: aleph auto test <question text>");
            }
            match decide(&a.cfg, &q) {
                Ok(r) => {
                    ok(&format!("rule {r} would approve it"));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    warn(&format!("not approved: {e}"));
                    ExitCode::from(1)
                }
            }
        }
        "status" | "ls" => {
            let on = a.cfg.flag("auto_approve");
            let r = rules(&a.cfg);
            if a.has("--json") {
                json(&json!({
                    "on": on,
                    "rules": r.iter().map(|(n, p)| json!({"name": n, "pattern": p})).collect::<Vec<_>>(),
                    "deny": deny_list(&a.cfg),
                    "heal": a.cfg.flag("auto_heal"),
                }));
                return ExitCode::SUCCESS;
            }
            if on {
                ok("auto-approve is on");
            } else {
                warn("auto-approve is off (aleph auto on)");
            }
            if r.is_empty() {
                hint("no rules — aleph auto add tests '*pytest*' · aleph auto add lint '*cargo clippy*'");
            }
            for (n, pat) in &r {
                crate::out::say(ratatui::text::Line::raw(format!("  ⚡ {n:<12} {pat}")));
            }
            crate::out::say(ratatui::text::Line::raw(format!(
                "  never:        {}",
                deny_list(&a.cfg).join(" · ")
            )));
            if a.cfg.flag("auto_heal") {
                ok("self-heal is on: herdr is restarted where it stops, and sessions restored");
            }
            ExitCode::SUCCESS
        }
        _ => {
            err(&format!("unknown: aleph auto {sub}"));
            fail("usage: aleph auto [status | on | off | add <rule> <pattern> | rm <rule> | test <question>]")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn when_limits_lift() {
        // 2026-10-22 10:00 UTC; local +05:30 (15:30)
        let (now, off) = (1_792_663_200, 19_800);
        assert_eq!(
            parse_reset("resets 4pm (Asia/Calcutta)", now, off),
            Some(now + 1800)
        );
        assert_eq!(parse_reset("in 2h 13m", now, off), Some(now + 7980));
        assert_eq!(parse_reset("45m", now, off), Some(now + 2700));
        // already past today: tomorrow
        assert_eq!(parse_reset("15:00", now, off), Some(now + 86_400 - 1800));
        assert_eq!(parse_reset("3:30 am", now, off), Some(now + 12 * 3600));
        assert_eq!(parse_reset("12am", now, off), Some(now + 8 * 3600 + 1800));
        assert!(parse_reset("soon", now, off).is_none());
    }

    #[test]
    fn patterns() {
        assert!(pmatch("*pytest*", "Do you want to run   pytest -x tests/?"));
        assert!(pmatch("pytest", "run PYTEST now"));
        assert!(pmatch("cargo*test", "Bash(cargo  nextest run)"));
        assert!(pmatch("bash(ls*)", "⏺ Bash(ls -la src)"));
        assert!(!pmatch("*rm -rf*", "rm -r build"));
        assert!(pmatch("", "anything"));
    }

    #[test]
    fn decide_rules_deny_and_yes() {
        let q = " Do you want to run this command?\n   pytest -x\n\n ❯ 1. Yes\n   2. No\n";
        let cfg = Config::in_memory(&["approve.tests=*pytest*"]);
        assert_eq!(decide(&cfg, q), Ok("tests".into()));
        let cfg = Config::in_memory(&["approve.ls=*ls*"]);
        assert!(decide(&cfg, q).is_err());
        // the deny list always wins
        let cfg = Config::in_memory(&["approve.all=*"]);
        let rm = " Do you want to run this command?\n   rm -rf /tmp/x\n ❯ 1. Yes\n   2. No\n";
        assert!(decide(&cfg, rm).unwrap_err().contains("denied"));
        // a match without a clear yes option isn't approved
        let cfg = Config::in_memory(&["approve.any=*"]);
        assert!(decide(&cfg, "What name should I use?").is_err());
    }

    #[test]
    fn gist_picks_the_command() {
        let q = " Do you want to run this command?\n   cargo bench -p parser\n\n ❯ 1. Yes\n";
        assert_eq!(gist(q), "cargo bench -p parser");
    }

    #[test]
    fn only_one_hub_lock_at_a_time() {
        let p = std::env::temp_dir().join(format!("aleph-hub-{}.lock", std::process::id()));
        let a = HubLock::try_take_at(&p).expect("first");
        assert!(HubLock::try_take_at(&p).is_none(), "second must fail");
        drop(a);
        // a child spawned by another test may hold an inherited copy of the
        // descriptor for a moment (between fork and exec): allow for that
        let t0 = std::time::Instant::now();
        let mut again = HubLock::try_take_at(&p);
        while again.is_none() && t0.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(20));
            again = HubLock::try_take_at(&p);
        }
        assert!(again.is_some(), "free again after drop");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_borrow_is_the_lock_without_the_pid() {
        let p = std::env::temp_dir().join(format!("aleph-hub-b-{}.lock", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert!(!HubLock::held_at(&p), "no file: nobody");
        let a = HubLock::try_take_at(&p).expect("take");
        assert!(HubLock::held_at(&p), "held (by this process too)");
        assert!(
            HubLock::try_borrow_at(&p).is_none(),
            "no borrowing a held lock"
        );
        drop(a);
        let t0 = std::time::Instant::now();
        let mut b = HubLock::try_borrow_at(&p);
        while b.is_none() && t0.elapsed() < std::time::Duration::from_secs(3) {
            std::thread::sleep(std::time::Duration::from_millis(20));
            b = HubLock::try_borrow_at(&p);
        }
        assert!(b.is_some(), "free: borrowed");
        assert!(HubLock::held_at(&p));
        // the last holder's pid stays written
        let pid = std::fs::read_to_string(&p).unwrap();
        assert_eq!(pid.trim(), std::process::id().to_string());
        drop(b);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn hub_machine_picks_one_computer() {
        let c = Config::in_memory(&[]);
        assert!(hub_here(&c));
        let c = Config::in_memory(&["hub.machine=some-other-box-xyz"]);
        assert!(!hub_here(&c));
        let c = Config::in_memory(&["hub.machine=local"]);
        assert!(hub_here(&c));
    }
}
