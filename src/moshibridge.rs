//! The Moshi bridge: driven by the hub after every fleet poll (in the daemon,
//! or the dashboard when no daemon runs). It turns what happens across the
//! fleet into Moshi inbox cards, phone approvals and live job rows (see
//! `moshi.rs`).
//!
//! Settings:
//! * `moshi` = auto | on | off. Auto turns it on when moshi-hook is paired here.
//! * `moshi.events`: what becomes a card. Default:
//!   needs_you, limit, error, down, up.
//! * `moshi.approvals` = on | off. Questions go to the phone as buttons, for
//!   sessions `remote.allow` covers.
//! * `moshi.jobs` = on | off: working sessions as live rows.
//! * `moshi.session` = on | off: keep the `aleph` tmux session for Moshi's picker.
//! * `host.<a>.moshi` = auto | full | machine-only | off (see `moshi::mode`).

use crate::config::Config;
use crate::events::{self, Event, Kind};
use crate::fleet::{Conn, Fleet, Transition};
use crate::herdr::{self, Herdr, Session};
use crate::moshi::{self, Answer, Ask, Mode, Note};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const JOB_EVERY: Duration = Duration::from_secs(20);

/// Work for the single moshi-hook worker thread (calls run one at a time,
/// never on the caller's thread).
enum Op {
    Note(Note),
    /// nothing to do: starts the worker, so others can hand it work
    Wake,
    JobStart {
        key: String,
        label: String,
        title: String,
    },
    JobUpdate {
        key: String,
        message: String,
    },
    JobEnd {
        key: String,
        error: bool,
        message: String,
    },
}

pub struct Bridge {
    tx: Option<Sender<Op>>,
    worker: Option<thread::JoinHandle<()>>,
    /// sessions with a question on the phone: key → cancel flag
    asks: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    /// sessions with a live job row: key → (last update, last message)
    jobs: HashMap<String, (Instant, String)>,
    /// last known reachability per machine
    up: HashMap<String, bool>,
    /// sessions seen blocked, so a question is asked once per block
    blocked: HashSet<String>,
    tmux_checked: Option<Instant>,
    /// finished turns waiting a moment for the agent's own words (its hook):
    /// (seen at, session, seconds it worked)
    finished: Vec<(Instant, Session, u64)>,
    /// how long a question waits before it's asked (None: notify.question_after)
    pub grace: Option<u64>,
}

impl Default for Bridge {
    fn default() -> Self {
        Self::new()
    }
}

/// `moshi.events` as a set (`needs_you,limit,…`).
pub fn events_wanted(cfg: &Config) -> HashSet<String> {
    cfg.get_or("moshi.events")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn label(host: &str) -> String {
    format!("{} · {host}", crate::brand::NAME)
}

fn session_label(s: &Session) -> String {
    format!("{} · {} · {}", crate::brand::NAME, s.host, s.display())
}

/// What the dashboard should show when the phone opens the `aleph` tmux session.
pub fn hint_focus(host: &str, pane: &str) {
    let p = crate::config::state_dir().join("focus-hint");
    let _ = crate::config::write_atomic(&p, format!("{host}|{pane}\n").as_bytes());
}

impl Bridge {
    pub fn new() -> Bridge {
        Bridge {
            tx: None,
            worker: None,
            asks: Arc::new(Mutex::new(HashMap::new())),
            jobs: HashMap::new(),
            up: HashMap::new(),
            blocked: HashSet::new(),
            finished: Vec::new(),
            grace: None,
            tmux_checked: None,
        }
    }

    fn send(&mut self, op: Op) {
        if self.tx.is_none() {
            let (tx, rx) = channel::<Op>();
            self.worker = Some(thread::spawn(move || {
                let mut ids: HashMap<String, String> = HashMap::new();
                while let Ok(op) = rx.recv() {
                    match op {
                        Op::Wake => {}
                        Op::Note(n) => {
                            if let Err(e) = moshi::notify(&n) {
                                crate::log::warn(
                                    "moshi",
                                    &format!("card '{}' not sent: {e}", n.title),
                                );
                            }
                        }
                        Op::JobStart { key, label, title } => {
                            if let std::collections::hash_map::Entry::Vacant(e) = ids.entry(key) {
                                if let Some(id) = moshi::job_start(&label, &title) {
                                    e.insert(id);
                                }
                            }
                        }
                        Op::JobUpdate { key, message } => {
                            if let Some(id) = ids.get(&key) {
                                moshi::job_update(id, None, &message);
                            }
                        }
                        Op::JobEnd {
                            key,
                            error,
                            message,
                        } => {
                            if let Some(id) = ids.remove(&key) {
                                moshi::job_end(&id, error, &message);
                            }
                        }
                    }
                }
            }));
            self.tx = Some(tx);
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(op);
        }
    }

    /// Wait for questions in flight (up to `timeout`) and for every queued
    /// moshi-hook call to finish (one-shot runs: `aleph moshi sync`).
    pub fn drain(&mut self, timeout: Duration) {
        let t0 = Instant::now();
        while !self.asks.lock().unwrap().is_empty() && t0.elapsed() < timeout {
            thread::sleep(Duration::from_millis(100));
        }
        self.tx = None;
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }

    pub fn note(&mut self, n: Note) {
        self.send(Op::Note(n));
    }

    /// Finished turns out as cards: with the agent's own words once its hook
    /// said something (that takes a spool read), or alone after a short wait.
    fn send_finished(&mut self, cfg: &Config) {
        if self.finished.is_empty() {
            return;
        }
        let said: Vec<Event> = events::load(60)
            .into_iter()
            .filter(|e| e.source == "agent" && e.kind == Kind::Done)
            .collect();
        let now = crate::cache::now();
        for (at, s, took) in std::mem::take(&mut self.finished) {
            let ended = now.saturating_sub(at.elapsed().as_secs());
            let words = said.iter().rev().find(|e| {
                e.host == s.host
                    && (e.name == s.name || (!e.pane.is_empty() && e.pane == s.pane_id))
                    && e.t + 20 >= ended
            });
            if words.is_none() && at.elapsed() < Duration::from_secs(10) {
                self.finished.push((at, s, took));
                continue;
            }
            hint_focus(&s.host, &s.pane_id);
            let said = words.map_or("", |e| e.body.as_str()).to_string();
            // what the turn took (`turncost.rs`): its machine's usage is read
            // first, so the card waits on a thread of its own, never here
            let turn = Event {
                t: ended,
                host: s.host.clone(),
                name: s.name.clone(),
                kind: Kind::Done,
                agent: s.agent.clone(),
                pane: s.pane_id.clone(),
                took,
                native: words.map(|e| e.native.clone()).unwrap_or_default(),
                ..Default::default()
            };
            self.send(Op::Wake);
            let (tx, cfg) = (self.tx.clone(), cfg.clone());
            thread::spawn(move || {
                let line = crate::turncost::for_alert(&cfg, &turn, Some(&s));
                // only a subagent reporting back, or a compaction ending
                if crate::turncost::quiet_turn(&cfg, &turn, Some(&s)) {
                    return;
                }
                let m = crate::message::finished_turn(&cfg, &s, took, &said, line);
                if let Some(tx) = tx {
                    let _ = tx.send(Op::Note(Note {
                        label: session_label(&s),
                        title: m.title,
                        message: m.body,
                        ..Default::default()
                    }));
                }
            });
        }
    }

    /// Call after every fleet poll (with the hub's transitions).
    pub fn tick(&mut self, cfg: &Config, fleet: &Fleet, trans: &[Transition], fresh: &[String]) {
        if !moshi::enabled(cfg) {
            self.cancel_all();
            return;
        }
        // a question goes to the phone once it has waited this long
        let grace = self
            .grace
            .unwrap_or_else(|| crate::router::question_after(cfg));
        let now = crate::cache::now();
        let native = moshi::status().map(|s| s.native()).unwrap_or_default();
        let want = events_wanted(cfg);
        self.machines(cfg, fleet, &want);
        for t in trans {
            if fresh.contains(&t.session.host) {
                continue;
            }
            let s = &t.session;
            let m = moshi::mode(cfg, &s.host, &s.agent, &native);
            if m != Mode::Full {
                continue;
            }
            match (t.from.as_str(), s.status.as_str()) {
                (_, "working") if cfg.flag("moshi.jobs") => {
                    self.job_start(s);
                }
                ("working", "done" | "idle") => {
                    self.job_end(&s.key(), false, "finished");
                    // a short turn: you were likely there for it
                    let min = crate::log::parse_duration(&cfg.get_or("notify.done_min"))
                        .map_or(0, |d| d.as_secs());
                    if want.contains("done") && !s.compacted && (t.took == 0 || t.took >= min) {
                        self.finished.push((Instant::now(), s.clone(), t.took));
                    }
                }
                _ => {}
            }
        }
        self.send_finished(cfg);
        // every question (each blocked state; a new one in the same session
        // counts again — herdr's state_change_seq) gets asked once
        let now_blocked: HashSet<String> = fleet
            .sessions()
            .iter()
            .filter(|s| s.blocked())
            .map(|s| format!("{}#{}", s.key(), s.seq))
            .collect();
        let blocked_keys: HashSet<String> = fleet
            .sessions()
            .iter()
            .filter(|s| s.blocked())
            .map(|s| s.key())
            .collect();
        for s in fleet.sessions() {
            let bkey = format!("{}#{}", s.key(), s.seq);
            if !s.blocked() || self.blocked.contains(&bkey) || fresh.contains(&s.host) {
                continue;
            }
            // answered at the terminal within the grace: never asked
            if now.saturating_sub(s.since.unwrap_or(now)) < grace {
                continue;
            }
            let m = moshi::mode(cfg, &s.host, &s.agent, &native);
            if m != Mode::Full {
                continue;
            }
            // an older question of this session still on the phone: withdraw it
            // first; the new one goes out on the next pass
            if let Some(c) = self.asks.lock().unwrap().get(&s.key()) {
                c.store(true, Ordering::Relaxed);
                continue;
            }
            self.blocked.insert(bkey);
            self.job_update(&s.key(), "needs you");
            self.question(cfg, fleet, s, &want);
        }
        // answered at a terminal (or gone): withdraw the phone's question
        self.blocked.retain(|k| now_blocked.contains(k));
        {
            let asks = self.asks.lock().unwrap();
            for (k, cancel) in asks.iter() {
                if !blocked_keys.contains(k) {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
        }
        // sessions that ended: close their job rows
        let live: HashSet<String> = fleet.sessions().iter().map(|s| s.key()).collect();
        let gone: Vec<String> = self
            .jobs
            .keys()
            .filter(|k| !live.contains(*k))
            .cloned()
            .collect();
        for k in gone {
            self.job_end(&k, true, "session ended");
        }
        // working sessions: keep their rows fresh
        if cfg.flag("moshi.jobs") {
            for s in fleet.sessions() {
                if s.status == "working"
                    && moshi::mode(cfg, &s.host, &s.agent, &native) == Mode::Full
                {
                    self.job_start(s);
                    let msg = if s.title.is_empty() {
                        "working".to_string()
                    } else {
                        s.title.clone()
                    };
                    self.job_update(&s.key(), &msg);
                }
            }
        }
        if cfg.flag("moshi.session") {
            self.keep_tmux_session();
        }
    }

    fn machines(&mut self, cfg: &Config, fleet: &Fleet, want: &HashSet<String>) {
        for h in &fleet.hosts {
            let up = match &h.conn {
                Conn::Up => Some(true),
                Conn::Down(e) if !crate::fleet::not_running(e) => Some(false),
                _ => None,
            };
            let Some(up) = up else { continue };
            if cfg.host_get(&h.alias, "moshi").as_deref() == Some("off") {
                continue;
            }
            match self.up.insert(h.alias.clone(), up) {
                Some(true) if !up && want.contains("down") => {
                    let why = match &h.conn {
                        Conn::Down(e) => e.lines().next().unwrap_or("").to_string(),
                        _ => String::new(),
                    };
                    let m = machine_msg(&h.alias, Kind::Error, &why, h.sessions.len());
                    self.note(Note {
                        label: label(&h.alias),
                        title: m.title,
                        message: m.body,
                        error: true,
                        ..Default::default()
                    });
                }
                Some(false) if up && want.contains("up") => {
                    let m = machine_msg(&h.alias, Kind::HostUp, "", h.sessions.len());
                    self.note(Note {
                        label: label(&h.alias),
                        title: m.title,
                        message: m.body,
                        silent: true,
                        ..Default::default()
                    })
                }
                _ => {}
            }
        }
    }

    /// A session newly stopped at its usage limit (from the hub).
    pub fn limit(&mut self, cfg: &Config, s: &Session, resets: &str) {
        if !moshi::enabled(cfg) || !events_wanted(cfg).contains("limit") {
            return;
        }
        if cfg.host_get(&s.host, "moshi").as_deref() == Some("off") {
            return;
        }
        hint_focus(&s.host, &s.pane_id);
        let mut s2 = s.clone();
        if !resets.is_empty() {
            s2.limit = Some(resets.to_string());
        }
        let m = crate::message::about(cfg, &s2, Kind::Limit, "");
        self.note(Note {
            label: session_label(s),
            title: m.title,
            message: m.body,
            ..Default::default()
        });
    }

    fn job_start(&mut self, s: &Session) {
        let key = s.key();
        if self.jobs.contains_key(&key) {
            return;
        }
        self.jobs
            .insert(key.clone(), (Instant::now(), String::new()));
        self.send(Op::JobStart {
            key,
            label: label(&s.host),
            title: format!("{} · {}", crate::agents::label(&s.agent), s.display()),
        });
    }

    fn job_update(&mut self, key: &str, msg: &str) {
        let Some((last, prev)) = self.jobs.get_mut(key) else {
            return;
        };
        if prev == msg && last.elapsed() < Duration::from_secs(300) {
            return;
        }
        if last.elapsed() < JOB_EVERY && !prev.is_empty() {
            return;
        }
        *last = Instant::now();
        *prev = msg.to_string();
        let (key, message) = (key.to_string(), msg.to_string());
        self.send(Op::JobUpdate { key, message });
    }

    fn job_end(&mut self, key: &str, error: bool, msg: &str) {
        if self.jobs.remove(key).is_some() {
            self.send(Op::JobEnd {
                key: key.to_string(),
                error,
                message: msg.to_string(),
            });
        }
    }

    fn cancel_all(&mut self) {
        for c in self.asks.lock().unwrap().values() {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// A blocked session: a phone question when remote control allows it,
    /// otherwise (or for terminal-only questions) a "needs you" card.
    fn question(&mut self, cfg: &Config, fleet: &Fleet, s: &Session, want: &HashSet<String>) {
        let key = s.key();
        hint_focus(&s.host, &s.pane_id);
        let ask = cfg.flag("moshi.approvals") && crate::permit::allowed(cfg, &s.host, &s.agent);
        if !ask {
            if want.contains("needs_you") {
                let m = crate::message::about(cfg, s, Kind::Blocked, "");
                self.note(Note {
                    label: session_label(s),
                    title: m.title,
                    message: m.body,
                    ..Default::default()
                });
            }
            return;
        }
        if self.asks.lock().unwrap().contains_key(&key) {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.asks
            .lock()
            .unwrap()
            .insert(key.clone(), cancel.clone());
        let t = fleet
            .host(&s.host)
            .map(|h| h.target.clone())
            .unwrap_or_else(|| cfg.target_for(&s.host, herdr::machine_for));
        let (cfg, s, asks, tx) = (cfg.clone(), s.clone(), self.asks.clone(), self.tx.clone());
        let notify_needs_you = want.contains("needs_you");
        thread::spawn(move || {
            let r = ask_once(&cfg, &t, &s, &cancel, notify_needs_you);
            if let Err(e) = r {
                crate::log::warn("moshi", &format!("couldn't ask about {}: {e}", s.key()));
                if let Some(tx) = tx {
                    let _ = tx.send(Op::Note(Note {
                        label: session_label(&s),
                        title: format!("{} needs you", s.display()),
                        message: e,
                        ..Default::default()
                    }));
                }
            }
            asks.lock().unwrap().remove(&s.key());
        });
    }

    /// The persistent `aleph` tmux session Moshi's picker lists (checked
    /// every minute; recreated if someone closed it).
    fn keep_tmux_session(&mut self) {
        if self
            .tmux_checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        self.tmux_checked = Some(Instant::now());
        thread::spawn(|| {
            let _ = ensure_tmux_session();
        });
    }
}

/// A machine went down or came back, in words (with its sessions).
fn machine_msg(host: &str, kind: Kind, why: &str, sessions: usize) -> crate::message::Msg {
    let e = Event {
        t: crate::cache::now(),
        host: host.to_string(),
        kind,
        title: why.to_string(),
        ..Default::default()
    };
    let c = crate::message::Ctx {
        sessions_there: Some(sessions),
        ..Default::default()
    };
    crate::message::compose(&e, &c)
}

/// Start the `aleph` tmux session if it isn't running. Ok(true) = started.
pub fn ensure_tmux_session() -> Result<bool, String> {
    let has = crate::proc::cap(
        "tmux",
        &["has-session", "-t", moshi::TMUX_SESSION],
        Duration::from_secs(3),
    );
    if has.ok() {
        return Ok(false);
    }
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| crate::brand::BIN.into());
    let q = crate::proc::sh_quote(&exe);
    let script = format!(
        "while :; do {q}; printf '\\n\\033[2m{} exited — starting again in 2s (ctrl+b d leaves it running)\\033[0m\\n'; sleep 2; done",
        crate::brand::NAME
    );
    let o = crate::proc::cap(
        "tmux",
        &[
            "new-session",
            "-d",
            "-s",
            moshi::TMUX_SESSION,
            "-x",
            "120",
            "-y",
            "40",
            "sh",
            "-c",
            &script,
        ],
        Duration::from_secs(5),
    );
    if o.ok() {
        Ok(true)
    } else {
        Err(o.reason())
    }
}

/// Ask the phone about one blocked session and apply the answer. Err = why
/// it couldn't ask (the caller falls back to a "needs you" card).
fn ask_once(
    cfg: &Config,
    t: &crate::config::Target,
    s: &Session,
    cancel: &AtomicBool,
    notify_needs_you: bool,
) -> Result<(), String> {
    let h = Herdr::new(t);
    let det = h.read(&s.pane_id, "detection", 0, false)?;
    if det.trim().is_empty() {
        return Ok(()); // nothing readable to ask about
    }
    if let Some(rule) = crate::permit::not_from_phone(cfg, &det) {
        if notify_needs_you {
            let m = crate::message::about(cfg, s, Kind::Blocked, &det);
            let _ = moshi::notify(&Note {
                label: session_label(s),
                title: format!("{} needs you at a terminal", s.display()),
                message: if m.body.is_empty() {
                    format!("not answerable from the phone ({rule})")
                } else {
                    m.body
                },
                ..Default::default()
            });
        }
        return Ok(());
    }
    let fp = moshi::fingerprint(&det);
    let opts = moshi::options(&det);
    // Moshi's app answers only Approve / Deny so far: unless `moshi.choices`
    // says it shows option buttons, those two pick the agent's yes and no
    let choices = cfg.flag("moshi.choices") && !opts.is_empty();
    let (yes, no) = if choices {
        (vec![], vec![])
    } else {
        let yes = herdr::approve_keys(&det)
            .ok_or("no clear yes option for Approve / Deny: answer it in Aleph")?;
        (yes, crate::actions::deny_keys(&det))
    };
    let buttons: Vec<String> = if choices {
        opts.iter().map(|(n, l)| moshi::button(*n, l)).collect()
    } else {
        vec![]
    };
    let multi = !buttons.is_empty();
    let question = moshi::question_text(&det);
    // the context first: which agent, where, how long it has waited
    let place = crate::message::where_of(cfg, s);
    let mut text = if question.is_empty() {
        format!("{place}\n{} is waiting for an answer", s.display())
    } else {
        format!("{place}\n\n{question}")
    };
    if !choices {
        text.push_str(&format!("\n\n{}", approve_deny_line(&opts, &yes, &no)));
    }
    let q = Ask {
        label: session_label(s),
        question: text,
        options: buttons,
        timeout: ASK_TIMEOUT,
    };
    let child = moshi::ask_spawn(&q)?;
    let answer = moshi::ask_wait(child, multi, cancel, ASK_TIMEOUT + Duration::from_secs(60));
    let keys: Vec<String> = match &answer {
        Answer::Chosen(b) => match moshi::button_number(b) {
            Some(n) => vec![n.to_string()],
            None => return Err(format!("couldn't map the answer '{b}'")),
        },
        Answer::Approved if !choices => yes,
        Answer::Approved => herdr::approve_keys(&det).ok_or("no clear yes option")?,
        // Deny means the agent's own "no" when it was offered as such
        Answer::Denied if !choices => no,
        Answer::Denied | Answer::TimedOut => return Ok(()),
        Answer::Unreachable(e) => return Err(format!("Moshi unreachable: {e}")),
        Answer::Failed(e) if e == "cancelled" => return Ok(()),
        Answer::Failed(e) => return Err(e.clone()),
    };
    // only while the very same question is still waiting
    let now = h.read(&s.pane_id, "detection", 0, false)?;
    if moshi::fingerprint(&now) != fp {
        let _ = moshi::notify(&Note {
            label: session_label(s),
            title: format!("{}: already answered", s.display()),
            message: "the question changed before your answer arrived — nothing was sent".into(),
            silent: true,
            ..Default::default()
        });
        return Ok(());
    }
    let k: Vec<&str> = keys.iter().map(String::as_str).collect();
    let sent = h.send_keys(&s.pane_id, &k);
    crate::log::info(
        "moshi",
        &format!(
            "answer from the phone for {}: {} → {:?}",
            s.key(),
            keys.join(" "),
            sent.as_ref().map(|_| "sent")
        ),
    );
    let choice = match &answer {
        Answer::Chosen(b) => b.clone(),
        Answer::Denied => "deny".into(),
        _ => "approve".into(),
    };
    events::append(Event {
        t: crate::cache::now(),
        host: s.host.clone(),
        name: s.name.clone(),
        agent: s.agent.clone(),
        kind: Kind::Auto,
        title: match &sent {
            Ok(()) => format!("answered from Moshi: {choice}"),
            Err(e) => format!("answer from Moshi ({choice}) couldn't be sent: {e}"),
        },
        ..Default::default()
    });
    sent
}

/// What Approve and Deny will answer, in the agent's own words (pure).
fn approve_deny_line(opts: &[(u32, String)], yes: &[String], no: &[String]) -> String {
    let say = |keys: &[String]| match keys {
        [k] => match opts.iter().find(|(n, _)| n.to_string() == *k) {
            Some((n, l)) => format!("“{n}. {l}”"),
            None if k == "esc" => "esc (cancel)".to_string(),
            None => format!("“{k}”"),
        },
        ks => ks.join(" "),
    };
    format!("Approve answers {} · Deny answers {}", say(yes), say(no))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approve_and_deny_say_what_they_answer() {
        let opts = vec![
            (1, "Yes".to_string()),
            (2, "Yes, and don't ask again".to_string()),
            (3, "No".to_string()),
        ];
        assert_eq!(
            approve_deny_line(&opts, &["1".into()], &["3".into()]),
            "Approve answers “1. Yes” · Deny answers “3. No”"
        );
        assert_eq!(
            approve_deny_line(&[], &["y".into()], &["esc".into()]),
            "Approve answers “y” · Deny answers esc (cancel)"
        );
    }

    #[test]
    fn default_events() {
        let c = Config::in_memory(&[]);
        let w = events_wanted(&c);
        for k in ["needs_you", "limit", "error", "down", "up"] {
            assert!(w.contains(k), "{k}");
        }
        assert!(w.contains("done"));
        let c = Config::in_memory(&["moshi.events=done"]);
        assert_eq!(events_wanted(&c).len(), 1);
    }
}
