//! The fleet: every machine in `hosts=` polled in parallel, each with its own
//! herdr state and agent sessions (every agent, or the ones `agents=` names)
//! and colour. Shared by the dashboard, `aleph ls`,
//! `aleph status`, `aleph watch` and the Raycast/JSON outputs.
//!
//! * Polls run on background threads (`herdr --machine <m> agent list`), one per
//!   host, every `refresh` seconds; a slow or dead host never delays the others.
//! * The last good snapshot is kept in `~/.cache/aleph/fleet.json`, so an
//!   unreachable machine still shows its sessions (dimmed, "cached 5m ago") and
//!   every view starts instantly.
//! * Status changes are timestamped (`since`) and persisted, so "needs you · 4m"
//!   survives restarts; `tick` returns the transitions (for toasts/notifications).

use crate::agents::Filter;
use crate::cache::now;
use crate::config::{Config, Target};
use crate::herdr::{self, Herdr, Session};
use crate::{sshcfg, theme};
use ratatui::style::Color;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub enum Conn {
    /// never polled yet this run
    Unknown,
    Up,
    Down(String),
}

#[derive(Clone, Debug)]
pub struct Host {
    pub alias: String,
    pub target: Target,
    pub color: Color,
    pub user: String,
    /// `user@hostname[:port]`
    pub addr: String,
    pub conn: Conn,
    pub sessions: Vec<Session>,
    /// unix time of the last successful poll (this run or the snapshot)
    pub fetched: Option<u64>,
    /// sessions come from the on-disk snapshot, not a live poll
    pub stale: bool,
    pub latency: Option<Duration>,
    pub polling: Option<Instant>,
    pub last_poll: Option<Instant>,
    pub collapsed: bool,
    /// paused (`host.<a>.paused`): stays in the fleet, isn't polled
    pub paused: bool,
    /// failed polls in a row (unreachable machines back off); forgiven only
    /// after a minute of answering again, so a flapping link stays slow
    pub fails: u32,
    /// round trip of a poll, smoothed (ms)
    pub rtt_ewma: Option<f64>,
    /// share of polls that went unanswered, smoothed (0–1)
    pub loss: f64,
    /// answering again since
    pub healthy_since: Option<Instant>,
    /// the machine's herdr version (cached 10 min; None = unknown)
    pub herdr_version: Option<String>,
    pub herdr_info: Option<HerdrInfo>,
    /// polls failing now: since when, and what was at work there
    pub outage: Option<Outage>,
}

/// A machine not answering (`Fleet`'s outage rule): since when, the
/// sessions working or waiting on you at its last good poll (pane, name,
/// status, in that state since), and what was decided about it.
#[derive(Clone, Debug, Default)]
pub struct Outage {
    pub since: u64,
    pub at_risk: Vec<(String, String, String, Option<u64>)>,
    /// its Inbox item, once filed (the event's key)
    pub filed: Option<String>,
    /// the grace is over: filed, quiet (nothing at work), or ours
    pub decided: bool,
    /// this computer was the one offline: nothing filed; what's gone is
    /// told when it's back
    pub ours: bool,
}

/// What the Inbox hears of machines going down and coming back
/// (`inbox.down_grace`, `inbox.machine_back`).
#[derive(Clone, Debug)]
pub struct Outages {
    pub grace: u64,
    pub back: String,
}

impl Outages {
    pub fn from_cfg(cfg: &Config) -> Outages {
        Outages {
            grace: crate::log::parse_duration(&cfg.get_or("inbox.down_grace"))
                .map_or(30, |d| d.as_secs()),
            back: cfg.get_or("inbox.machine_back"),
        }
    }
}

/// The sessions an outage puts at risk: working, or waiting on you.
fn at_risk(sessions: &[Session]) -> Vec<(String, String, String, Option<u64>)> {
    sessions
        .iter()
        .filter(|s| s.status == "working" || s.blocked())
        .map(|s| {
            (
                s.pane_id.clone(),
                if s.display().is_empty() {
                    s.name.clone()
                } else {
                    s.display().to_string()
                },
                s.status.clone(),
                s.since,
            )
        })
        .collect()
}

/// The grace over: an Inbox item when an agent was at work there (one row
/// a machine a day), nothing when it was quiet, nothing while this computer
/// is the one offline (what's gone is told when it's back).
fn outage_due(alias: &str, h: &mut Host, err: &str, t: u64, rules: &Outages) {
    let Some(o) = h.outage.as_mut() else {
        return;
    };
    if o.decided || t.saturating_sub(o.since) < rules.grace {
        return;
    }
    o.decided = true;
    if crate::net::offline_now() {
        o.ours = true;
        return;
    }
    if o.at_risk.is_empty() {
        return;
    }
    let who: Vec<String> = o
        .at_risk
        .iter()
        .map(|(_, n, st, since)| match (st.as_str(), since) {
            ("blocked", _) => format!("{n} was waiting on you"),
            (_, Some(s)) => format!(
                "{n} was {} into a turn",
                crate::ui::dur(o.since.saturating_sub(*s))
            ),
            _ => format!("{n} was working"),
        })
        .collect();
    let earlier = crate::events::fold_outages(alias, t);
    let mut body = who.join("\n");
    if !earlier.is_empty() {
        let times: Vec<String> = earlier.iter().map(|x| crate::ui::clock(*x)).collect();
        body.push_str(&format!(
            "\ndown {}× today: {}, {}",
            earlier.len() + 1,
            times.join(", "),
            crate::ui::clock(o.since)
        ));
    }
    let ev = crate::events::Event {
        t: o.since,
        host: alias.to_string(),
        kind: crate::events::Kind::Error,
        title: err.to_string(),
        body,
        ..Default::default()
    };
    o.filed = Some(crate::events::key(&ev));
    crate::events::append(ev);
}

/// It answers again (`now`: its sessions now; none when herdr stopped there):
/// `close` ends its outage row with what became of the sessions at work
/// (archived when none is gone), `lost` says only what's gone, `off` nothing.
/// An outage that filed nothing (a blip, a quiet machine, this computer
/// offline) speaks only of what's gone.
fn outage_over(alias: &str, o: Outage, now: &[Session], t: u64, rules: &Outages) {
    if rules.back == "off" {
        return;
    }
    let gone: Vec<&str> = o
        .at_risk
        .iter()
        .filter(|(pane, ..)| !now.iter().any(|s| s.pane_id == *pane))
        .map(|x| x.1.as_str())
        .collect();
    let span = format!(
        "{}–{} ({})",
        crate::ui::clock(o.since),
        crate::ui::clock(t),
        crate::ui::dur(t.saturating_sub(o.since))
    );
    if rules.back == "lost" || o.filed.is_none() {
        if let Some(k) = &o.filed {
            crate::events::dismiss_keys(std::slice::from_ref(k));
        }
        if !gone.is_empty() {
            crate::events::append(crate::events::Event {
                t,
                host: alias.to_string(),
                kind: crate::events::Kind::Error,
                title: format!("{} gone — it didn't answer {span}", gone.join(", ")),
                body: "r on it in Fleet's outside rows, or aleph restore, brings a session back"
                    .into(),
                ..Default::default()
            });
        }
        return;
    }
    let what: Vec<String> = o
        .at_risk
        .iter()
        .map(
            |(pane, n, ..)| match now.iter().find(|s| s.pane_id == *pane) {
                Some(s) if s.status == "working" => format!("{n} still working ✓"),
                Some(s) => format!("{n} {} ✓", s.status),
                None => format!("{n} gone — aleph restore brings it back"),
            },
        )
        .collect();
    let ev = crate::events::Event {
        t,
        host: alias.to_string(),
        kind: crate::events::Kind::HostUp,
        title: format!("back after {}", crate::ui::dur(t.saturating_sub(o.since))),
        body: format!("didn't answer {span}\n{}", what.join("\n")),
        ..Default::default()
    };
    let k = crate::events::key(&ev);
    crate::events::append(ev);
    let mut done: Vec<String> = o.filed.into_iter().collect();
    if gone.is_empty() {
        done.push(k);
    }
    crate::events::dismiss_keys(&done);
}

impl Host {
    /// `Some((theirs, ours))` when this machine's herdr differs from this
    /// computer's — `herdr --machine`, the hub and `machine add` need them equal.
    /// `Some((theirs, ours))` when this machine's herdr needs attention:
    /// the running server says it isn't compatible with our client, or (when
    /// it isn't running / can't tell) its version differs from ours. herdr's
    /// docs: compatible versions don't have to match.
    pub fn drift(&self) -> Option<(String, String)> {
        let theirs = self.herdr_version.clone()?;
        let ours = local_herdr_version()?;
        match self.herdr_info.as_ref().and_then(|i| i.compatible) {
            Some(true) => None,
            Some(false) => Some((theirs, ours)),
            None => (theirs != ours).then_some((theirs, ours)),
        }
    }
    /// The machine's OS line (`Linux · Ubuntu 24.04.3 LTS`), when known.
    pub fn os(&self) -> &str {
        self.herdr_info.as_ref().map_or("", |i| i.os.as_str())
    }
    /// herdr there asks for a restart (or runs an older binary than installed).
    pub fn herdr_restart(&self) -> bool {
        self.herdr_info.as_ref().is_some_and(|i| i.restart)
    }
    pub fn count(&self, status: &str) -> usize {
        self.sessions.iter().filter(|s| s.status == status).count()
    }
    pub fn short_path(&self, p: &str) -> String {
        herdr::short_path(p, &self.user)
    }
}

/// herdr's "no server is running" (the machine itself answered).
pub fn not_running(e: &str) -> bool {
    let l = e.to_lowercase();
    l.contains("server_not_running")
        || l.contains("no herdr server")
        || l.contains("not running")
        // older herdr (0.7): the raw socket error, e.g. `Os { code: 111, kind: ConnectionRefused … }`
        || (l.contains("os { code:") && (l.contains("connectionrefused") || l.contains("notfound")))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Transition {
    pub session: Session,
    pub from: String,
    /// seconds it spent in `from` (0: unknown)
    pub took: u64,
}

enum Msg {
    Polled {
        alias: String,
        gen: u64,
        res: Result<Vec<Session>, String>,
        took: Duration,
    },
    Resolved {
        alias: String,
        user: String,
        addr: String,
    },
    Version {
        alias: String,
        info: HerdrInfo,
    },
    /// a limit scan: (pane, status when read, reset if stopped at a limit)
    Limits {
        alias: String,
        found: Vec<(String, String, Option<String>)>,
    },
    /// a compaction scan: the panes read, and those compacting
    Compacting {
        alias: String,
        read: Vec<String>,
        found: Vec<String>,
    },
    /// a command scan: each shell's foreground (a command, or its prompt)
    Commands {
        alias: String,
        seen: HashMap<String, Option<String>>,
    },
}

/// How often idle sessions' screens are read for usage limits.
const LIMIT_SCAN: Duration = Duration::from_secs(30);
/// How often working sessions' screens are read for a compaction (they last
/// from seconds to a few minutes; a short one missed only goes unlabelled).
/// How often shells are asked what's in their foreground.
const COMMAND_SCAN: Duration = Duration::from_secs(3);
const COMPACT_SCAN: Duration = Duration::from_secs(8);

/// This computer's herdr version, asked once per run.
pub fn local_herdr_version() -> Option<String> {
    use std::sync::OnceLock;
    static V: OnceLock<Option<String>> = OnceLock::new();
    V.get_or_init(|| {
        let o = crate::proc::cap(&herdr::bin(), &["--version"], Duration::from_secs(5));
        o.ok()
            .then(|| herdr::version_of(&o.text()))
            .filter(|v| !v.is_empty())
    })
    .clone()
}

const VERSION_TTL: u64 = 600;

fn versions_file() -> std::path::PathBuf {
    crate::config::world_cache().join("herdr-versions.json")
}

/// Forget the cached herdr version of `alias` (it was just installed or
/// updated there): the next poll asks again.
pub fn forget_version(alias: &str) {
    let f = versions_file();
    let Some(mut root) = std::fs::read_to_string(&f)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return;
    };
    if let Some(o) = root.as_object_mut() {
        if o.remove(alias).is_some() {
            let _ = crate::config::write_atomic(&f, root.to_string().as_bytes());
        }
    }
}

/// A machine's herdr version: from the cache when it's younger than 6 h,
/// else asked over the (shared) ssh connection and remembered.
/// Re-report `mark`/`hue` tokens for agent sessions that have none (a herdr
/// restart drops them), at most once per pane per run.
fn retoken(h: &Herdr, alias: &str, v: &mut [Session]) {
    use std::sync::OnceLock;
    static DONE: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    let done = DONE.get_or_init(|| Mutex::new(Default::default()));
    let in_use: Vec<String> = v
        .iter()
        .map(|s| s.hue.clone())
        .filter(|x| !x.is_empty())
        .collect();
    let mut used = in_use.clone();
    for s in v
        .iter_mut()
        .filter(|s| s.hue.is_empty() && s.mark.is_empty())
    {
        let key = format!("{alias}|{}", s.pane_id);
        if !done.lock().unwrap().insert(key) {
            continue;
        }
        let (hue, mark) = theme::pick_hue(&used, theme::hash(&s.pane_id));
        used.push(hue.to_string());
        if h.report_tokens(&s.pane_id, &[("mark", mark), ("hue", hue)])
            .is_ok()
        {
            s.hue = hue.to_string();
            s.mark = mark.to_string();
        }
    }
}

/// A machine's herdr: the running server's status (`status server --json`:
/// version, compatible, restart needed) when it answers, else the installed
/// binary's version. Cached 10 minutes (one cheap call over the shared ssh).
fn herdr_version_of(h: &Herdr, alias: &str) -> Option<HerdrInfo> {
    if let Some((version, os)) = crate::showcase::herdr_info(alias) {
        return Some(HerdrInfo {
            version,
            os,
            compatible: Some(true),
            restart: false,
        });
    }
    let local = crate::sshcfg::is_local(alias);
    let mut root: Value = std::fs::read_to_string(versions_file())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let Some(c) = root.get(alias) {
        let at = c.get("at").and_then(Value::as_u64).unwrap_or(0);
        if now().saturating_sub(at) < VERSION_TTL {
            if let Some(i) = HerdrInfo::from_json(c) {
                return Some(i);
            }
        }
    }
    let st = if local { None } else { h.server_status() };
    let os = os_of(alias);
    let info = match st {
        Some(s) if s.running => HerdrInfo {
            version: s.version.or_else(|| h.version()).unwrap_or_default(),
            os,
            compatible: s.compatible,
            restart: s.restart_needed || s.binary_stale,
        },
        _ => HerdrInfo {
            version: if local {
                local_herdr_version()?
            } else {
                h.version()?
            },
            os,
            compatible: None,
            restart: false,
        },
    };
    let mut c = info.to_json();
    c["at"] = json!(now());
    root[alias] = c;
    let _ = crate::config::write_atomic(&versions_file(), root.to_string().as_bytes());
    Some(info)
}

/// `Linux · Ubuntu 24.04.3 LTS` / `Darwin · macOS 27.0` (one quick call).
fn os_of(alias: &str) -> String {
    let line = "uname -s; (. /etc/os-release 2>/dev/null && printf '%s\\n' \"$PRETTY_NAME\") || printf 'macOS %s\\n' \"$(sw_vers -productVersion 2>/dev/null)\"";
    let o = crate::proc::run(
        &mut crate::sshcfg::shell(alias, line),
        Duration::from_secs(10),
        None,
    );
    let text = o.map(|o| o.text()).unwrap_or_default();
    let v: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match v.as_slice() {
        [k, d, ..] => format!("{k} · {d}"),
        [k] => k.to_string(),
        _ => String::new(),
    }
}

/// What we know about a machine's herdr.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HerdrInfo {
    pub version: String,
    /// `uname -s` + distribution / product name, e.g. `Linux · Ubuntu 24.04.3 LTS`
    pub os: String,
    /// from the running server; None = not running / older herdr
    pub compatible: Option<bool>,
    /// restart needed, or the installed binary is newer than the server
    pub restart: bool,
}

impl HerdrInfo {
    fn to_json(&self) -> Value {
        json!({"version": self.version, "os": self.os, "compatible": self.compatible, "restart": self.restart})
    }
    fn from_json(v: &Value) -> Option<HerdrInfo> {
        Some(HerdrInfo {
            version: v.get("version")?.as_str()?.to_string(),
            os: v
                .get("os")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            compatible: v.get("compatible").and_then(Value::as_bool),
            restart: v.get("restart").and_then(Value::as_bool).unwrap_or(false),
        })
    }
}

pub struct Fleet {
    pub hosts: Vec<Host>,
    /// list every agent's sessions; false = only the ones `agents` names
    /// (kept in step with the `agents` setting: `set_agents`)
    pub all_agents: bool,
    /// the agents listed when `all_agents` is off
    pub agents: Filter,
    /// list every pane with no agent, or only the shells Aleph started (`shell.list`)
    pub plain_shells: bool,
    pub interval: Duration,
    /// what the Inbox hears of outages (`inbox.down_grace`, `inbox.machine_back`)
    pub outages: Outages,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    gen: HashMap<String, u64>,
    /// "host|pane" → (status, since)
    since: HashMap<String, (String, u64)>,
    machines: Arc<Mutex<HashMap<String, String>>>,
    pub changed_at: Instant,
    /// hosts to poll right away (an event said something changed)
    nudged: std::collections::HashSet<String>,
    snap_at: Instant,
    /// live herdr event subscriptions (`listen`), when started
    listener: Option<crate::listen::Listener>,
    /// watch rules (re-read now and then) and what they matched lately
    watch_rules: Vec<crate::watchrules::Rule>,
    rules_at: Option<Instant>,
    matched: HashMap<String, Instant>,
    events: Option<Receiver<crate::listen::Ev>>,
    /// read idle sessions' screens for usage limits (dashboard, watch)
    pub scan_limits: bool,
    /// "host|pane" → (status when seen, reset): cleared once the status moves
    limits: HashMap<String, (String, String)>,
    /// alias → when its last limit scan started (and whether it runs)
    limit_scan: HashMap<String, (Instant, bool)>,
    /// sessions (`Session::key`) whose screens last said they're compacting
    compacting: std::collections::HashSet<String>,
    /// sessions at rest after a compaction, with the `done_seq` that ended it
    compacted: HashMap<String, u64>,
    /// per machine: the last compaction scan, and whether one is running
    compact_scan: HashMap<String, (Instant, bool)>,
    /// the commands shells are running now (`Session::key`), what ended and
    /// hasn't been polled yet, and what each ran while this was watching: held
    /// here only, never written
    commands: HashMap<String, Ran>,
    ended: HashMap<String, Ran>,
    ran: HashMap<String, std::collections::VecDeque<Ran>>,
    command_scan: HashMap<String, (Instant, bool)>,
}

/// How long to wait before polling `h` again: the refresh interval while it
/// answers; 1 s while herdr isn't running there; doubling up to 2 min (with
/// ±20 % jitter, so machines don't retry in lockstep) while it doesn't
/// answer at all.
pub fn backoff(interval: Duration, h: &Host) -> Duration {
    match &h.conn {
        Conn::Down(e) if not_running(e) => interval.max(Duration::from_secs(1)),
        Conn::Down(_) => {
            let base = interval.max(Duration::from_secs(1));
            let d = (base * 2u32.saturating_pow(h.fails.min(7))).min(Duration::from_secs(120));
            jitter(d, &h.alias, h.fails)
        }
        _ => interval,
    }
}

/// `d` ± 20 %, the same for the same machine and attempt (no clock, no rng).
fn jitter(d: Duration, alias: &str, n: u32) -> Duration {
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    for b in alias.bytes().chain(n.to_le_bytes()) {
        x = (x ^ b as u64).wrapping_mul(0x100_0000_01b3);
    }
    let f = 0.8 + (x % 1000) as f64 / 1000.0 * 0.4;
    d.mul_f64(f)
}

/// How a machine's link is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Unknown,
    Up,
    /// answering, but slowly or not every time
    Degraded,
    Down,
}

/// How often a machine with a live event listener is polled anyway (events
/// poll it at once; this catches what isn't an event, like titles).
pub const LIVE_RECONCILE: Duration = Duration::from_secs(3);

/// A poll slower than this (smoothed) is a degraded link.
pub const SLOW_MS: f64 = 800.0;

impl Host {
    pub fn link(&self) -> Link {
        match &self.conn {
            Conn::Unknown => Link::Unknown,
            Conn::Down(e) if !not_running(e) => Link::Down,
            _ if self.rtt_ewma.is_some_and(|r| r > SLOW_MS) || self.loss >= 0.2 => Link::Degraded,
            _ => Link::Up,
        }
    }

    /// A poll answered after `took`.
    fn answered(&mut self, took: Duration) {
        let ms = took.as_secs_f64() * 1000.0;
        // (this computer has no link: a slow answer here is a busy herdr)
        if !sshcfg::is_local(&self.alias) {
            self.rtt_ewma = Some(match self.rtt_ewma {
                Some(p) => 0.3 * ms + 0.7 * p,
                None => ms,
            });
        }
        self.loss *= 0.7;
        let since = *self.healthy_since.get_or_insert_with(Instant::now);
        // a minute of answering earns fast retries again
        if since.elapsed() >= Duration::from_secs(60) {
            self.fails = 0;
        }
    }

    /// A poll went unanswered.
    fn unanswered(&mut self) {
        self.loss = 0.3 + 0.7 * self.loss;
        self.healthy_since = None;
        self.fails = self.fails.saturating_add(1);
    }
}

/// One polled session and what's known of its compaction. Working: labelled.
/// At rest with a compaction just seen: nothing finished, so it reads idle
/// (herdr calls it done, and counts a completion) until it moves again.
fn note_compaction(
    s: &mut Session,
    compacting: &mut std::collections::HashSet<String>,
    compacted: &mut HashMap<String, u64>,
) {
    let k = s.key();
    let at_rest = matches!(s.status.as_str(), "done" | "idle");
    if compacting.contains(&k) {
        if s.status == "working" {
            s.compacting = true;
        } else {
            compacting.remove(&k);
            if at_rest {
                compacted.insert(k.clone(), s.done_seq);
            }
        }
    }
    if let Some(seq) = compacted.get(&k).copied() {
        // (herdr's completion_seq goes once it's been seen: at rest still counts)
        if at_rest && (s.done_seq == seq || s.done_seq == 0) {
            s.compacted = true;
            s.status = "idle".into();
        } else {
            compacted.remove(&k);
        }
    }
}

/// A command a shell ran.
#[derive(Clone, Debug, PartialEq)]
pub struct Ran {
    pub cmd: String,
    pub from: u64,
    /// when it ended; None while it's running
    pub to: Option<u64>,
}

/// How many of a shell's past commands are held.
const RAN_KEPT: usize = 30;

/// Things you leave running and end yourself: their end isn't news.
const INTERACTIVE: &[&str] = &[
    "vim", "nvim", "vi", "nano", "emacs", "less", "more", "man", "top", "htop", "btop", "atop",
    "nvtop", "tmux", "screen", "ssh", "mosh", "sftp", "lazygit", "tig", "fzf", "ranger", "yazi",
    "watch", "claude", "codex", "opencode",
];

/// A shell's foreground as a scan saw it: a command started, went on or ended.
fn note_foreground(
    key: &str,
    fg: Option<String>,
    t: u64,
    commands: &mut HashMap<String, Ran>,
    ended: &mut HashMap<String, Ran>,
    ran: &mut HashMap<String, std::collections::VecDeque<Ran>>,
) {
    if commands.get(key).map(|r| &r.cmd) == fg.as_ref() {
        return;
    }
    if let Some(mut r) = commands.remove(key) {
        r.to = Some(t);
        let past = ran.entry(key.to_string()).or_default();
        past.push_back(r.clone());
        if past.len() > RAN_KEPT {
            past.pop_front();
        }
        ended.insert(key.to_string(), r);
    }
    if let Some(cmd) = fg {
        commands.insert(
            key.to_string(),
            Ran {
                cmd,
                from: t,
                to: None,
            },
        );
    }
}

/// Is a command's end worth telling you about (`notify.commands`: how long it has
/// to have run)?
fn worth_telling(r: &Ran, min_secs: u64) -> bool {
    let word = r.cmd.split_whitespace().next().unwrap_or("");
    let word = word.rsplit('/').next().unwrap_or(word);
    let took = r.to.unwrap_or(r.from).saturating_sub(r.from);
    min_secs > 0 && took >= min_secs && !INTERACTIVE.contains(&word)
}

/// A shell in a poll: working while a command runs, named for it. Its end goes the
/// way an agent's turn does, to the Inbox and every notification, unless it isn't
/// worth telling. `was_working`: the last poll said so, maybe before a restart. Until
/// a scan has answered that's still the best word, and a command that ended unseen
/// meanwhile is no news.
fn note_command(
    s: &mut Session,
    commands: &HashMap<String, Ran>,
    ended: &mut HashMap<String, Ran>,
    (was_working, scanned): (bool, bool),
    min_secs: impl Fn() -> u64,
) {
    if s.agent != crate::agents::SHELL {
        return;
    }
    let k = s.key();
    if let Some(r) = commands.get(&k) {
        s.status = "working".into();
        s.title = r.cmd.clone();
        return;
    }
    if let Some(r) = ended.remove(&k) {
        // (a quiet end is told to nobody, as a compaction's isn't)
        s.compacted = !worth_telling(&r, min_secs());
        s.title = r.cmd;
    } else if was_working && scanned {
        s.compacted = true;
    } else if was_working {
        s.status = "working".into();
    }
}

impl Fleet {
    /// The machines this computer can't reach. A machine that answers with no
    /// herdr running isn't one of them: its agents' logs can still be read.
    pub fn unreachable(&self) -> Vec<String> {
        self.hosts
            .iter()
            .filter(|h| matches!(&h.conn, Conn::Down(e) if !not_running(e)))
            .map(|h| h.alias.clone())
            .collect()
    }

    /// All fleet hosts, or just `only` (a `--host` override).
    pub fn new(cfg: &Config, only: Option<&str>) -> Fleet {
        // whether this computer is the one offline (the outage rule)
        crate::net::start_global(&crate::net::target(cfg));
        let aliases: Vec<String> = match only {
            Some(h) => vec![h.to_string()],
            None => cfg.fleet(),
        };
        let (tx, rx) = channel();
        let agents = Filter::from_cfg(cfg);
        let mut f = Fleet {
            hosts: Vec::new(),
            all_agents: agents.is_all(),
            agents,
            plain_shells: cfg.get_or("shell.list") != "aleph",
            interval: cfg.refresh_dur(),
            outages: Outages::from_cfg(cfg),
            tx,
            rx,
            gen: HashMap::new(),
            since: HashMap::new(),
            machines: Arc::new(Mutex::new(HashMap::new())),
            changed_at: Instant::now(),
            nudged: Default::default(),
            snap_at: Instant::now(),
            listener: None,
            watch_rules: Vec::new(),
            rules_at: None,
            matched: HashMap::new(),
            events: None,
            scan_limits: false,
            limits: HashMap::new(),
            limit_scan: HashMap::new(),
            compacting: Default::default(),
            compacted: HashMap::new(),
            compact_scan: HashMap::new(),
            commands: HashMap::new(),
            ended: HashMap::new(),
            ran: HashMap::new(),
            command_scan: HashMap::new(),
        };
        for a in aliases {
            f.add_host(cfg, &a);
        }
        f.load_snapshot();
        f
    }

    /// An empty fleet that does nothing (stands in while the real one is
    /// lent to a background thread).
    pub fn placeholder() -> Fleet {
        let (tx, rx) = channel();
        let agents = Filter::parse("all").unwrap_or_else(|_| Filter::from_cfg(&Config::load()));
        Fleet {
            hosts: Vec::new(),
            all_agents: true,
            agents,
            plain_shells: true,
            interval: Duration::from_secs(1),
            outages: Outages {
                grace: 30,
                back: "close".into(),
            },
            tx,
            rx,
            gen: HashMap::new(),
            since: HashMap::new(),
            machines: Arc::new(Mutex::new(HashMap::new())),
            changed_at: Instant::now(),
            nudged: Default::default(),
            snap_at: Instant::now(),
            listener: None,
            watch_rules: Vec::new(),
            rules_at: None,
            matched: HashMap::new(),
            events: None,
            scan_limits: false,
            limits: HashMap::new(),
            limit_scan: HashMap::new(),
            compacting: Default::default(),
            compacted: HashMap::new(),
            compact_scan: HashMap::new(),
            commands: HashMap::new(),
            ended: HashMap::new(),
            ran: HashMap::new(),
            command_scan: HashMap::new(),
        }
    }

    pub fn add_host(&mut self, cfg: &Config, alias: &str) {
        if self.hosts.iter().any(|h| h.alias == alias) {
            return;
        }
        if let Some(l) = &self.listener {
            l.add(alias);
        }
        let machine = cfg.host_get(alias, "machine").or_else(|| {
            (cfg.get("ssh_host").as_deref() == Some(alias))
                .then(|| cfg.get("machine"))
                .flatten()
        });
        if let Some(m) = &machine {
            self.machines
                .lock()
                .unwrap()
                .insert(alias.to_string(), m.clone());
        }
        let target = Target {
            ssh: alias.to_string(),
            machine: machine.unwrap_or_else(|| alias.to_string()),
            source: "fleet",
        };
        self.hosts.push(Host {
            alias: alias.to_string(),
            target,
            color: theme::machine(cfg, alias),
            user: String::new(),
            addr: String::new(),
            conn: Conn::Unknown,
            sessions: Vec::new(),
            fetched: None,
            stale: false,
            latency: None,
            polling: None,
            last_poll: None,
            collapsed: false,
            paused: cfg.host_get(alias, "paused").is_some_and(|v| v == "true"),
            fails: 0,
            rtt_ewma: None,
            loss: 0.0,
            healthy_since: None,
            herdr_version: None,
            herdr_info: None,
            outage: None,
        });
        let (tx, a) = (self.tx.clone(), alias.to_string());
        thread::spawn(move || {
            if let Some(r) = sshcfg::resolve(&a) {
                let _ = tx.send(Msg::Resolved {
                    alias: a,
                    user: r.user.clone(),
                    addr: r.short(),
                });
            }
        });
    }

    /// Follow the settings' `host.<a>.paused` (a dashboard paused a machine,
    /// or `aleph settings` resumed it): a machine resumed is polled now.
    /// Whether anything changed.
    pub fn follow_paused(&mut self, cfg: &Config) -> bool {
        let mut resumed = Vec::new();
        let mut moved = false;
        for h in self.hosts.iter_mut() {
            let p = cfg
                .host_get(&h.alias, "paused")
                .is_some_and(|v| v == "true");
            if p != h.paused {
                h.paused = p;
                moved = true;
                if !p {
                    resumed.push(h.alias.clone());
                }
            }
        }
        for a in resumed {
            crate::log::info("fleet", &format!("{a} resumed (the settings say so)"));
            self.poll(&a);
        }
        if moved {
            self.changed_at = Instant::now();
        }
        moved
    }

    /// The machines that are paused (not polled: what they show is as last seen).
    pub fn paused(&self) -> Vec<&Host> {
        self.hosts.iter().filter(|h| h.paused).collect()
    }

    pub fn remove_host(&mut self, alias: &str) {
        self.hosts.retain(|h| h.alias != alias);
    }

    pub fn host(&self, alias: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    pub fn host_mut(&mut self, alias: &str) -> Option<&mut Host> {
        self.hosts.iter_mut().find(|h| h.alias == alias)
    }

    /// Every session, grouped by host in fleet order.
    pub fn sessions(&self) -> Vec<&Session> {
        self.hosts.iter().flat_map(|h| h.sessions.iter()).collect()
    }

    pub fn find(&self, key: &str) -> Option<&Session> {
        self.sessions().into_iter().find(|s| s.key() == key)
    }

    /// Live agent names across the fleet (for picking fresh names).
    pub fn live_names(&self) -> Vec<String> {
        self.sessions().iter().map(|s| s.name.clone()).collect()
    }

    pub fn polling(&self) -> bool {
        self.hosts.iter().any(|h| h.polling.is_some())
    }

    /// Subscribe to every machine's herdr events (dashboard, watch): changes
    /// arrive as they happen, and each one triggers an immediate re-read.
    pub fn listen(&mut self, cfg: &Config) {
        // (the demo world has no herdr to listen to)
        if self.listener.is_some() || !cfg.flag("listen") || crate::showcase::demo() {
            return;
        }
        let (tx, rx) = channel();
        let hosts: Vec<String> = self.hosts.iter().map(|h| h.alias.clone()).collect();
        self.listener = Some(crate::listen::Listener::start(&hosts, tx));
        self.events = Some(rx);
    }

    /// A watch rule matched: an Inbox event (the same line again within ten
    /// minutes is the same news).
    fn on_match(&mut self, e: &crate::listen::Ev) {
        let rule = e
            .data
            .get("rule")
            .and_then(Value::as_str)
            .unwrap_or("watch");
        let pane = e.data.get("pane_id").and_then(Value::as_str).unwrap_or("");
        let line = crate::watchrules::matched_line(&e.data);
        let key = format!("{}|{pane}|{rule}|{line}", e.host);
        self.matched
            .retain(|_, t| t.elapsed() < Duration::from_secs(600));
        if self.matched.contains_key(&key) {
            return;
        }
        self.matched.insert(key, Instant::now());
        let s = self
            .host(&e.host)
            .and_then(|h| h.sessions.iter().find(|s| s.pane_id == pane))
            .cloned();
        crate::events::append(crate::events::Event {
            t: now(),
            host: e.host.clone(),
            name: s.as_ref().map(|s| s.name.clone()).unwrap_or_default(),
            kind: crate::events::Kind::Match,
            title: format!("{rule}: {}", crate::ui::trunc(&line, 160)),
            agent: s.map(|s| s.agent).unwrap_or_default(),
            source: "aleph".into(),
            body: line,
            ..Default::default()
        });
        self.changed_at = Instant::now();
    }

    /// Stop the live-event subscriptions (and their ssh forwards).
    pub fn stop_listening(&mut self) {
        self.listener = None;
        self.events = None;
    }

    /// The live-event state of a machine (None when not listening).
    pub fn live(&self, alias: &str) -> Option<crate::listen::LState> {
        self.listener.as_ref().map(|l| l.state(alias))
    }

    /// Poll `alias` as soon as its current poll (if any) is done — an event
    /// listener saw something change there.
    pub fn nudge(&mut self, alias: &str) {
        self.nudged.insert(alias.to_string());
    }

    /// Start a poll of `alias` (or every host) now.
    pub fn refresh(&mut self, alias: Option<&str>) {
        let list: Vec<String> = self
            .hosts
            .iter()
            .filter(|h| alias.is_none_or(|a| a == h.alias))
            .map(|h| h.alias.clone())
            .collect();
        for a in list {
            self.poll(&a);
        }
    }

    /// Follow a new value of the `shell.list` setting.
    pub fn set_shells(&mut self, setting: &str) {
        self.plain_shells = setting != "aleph";
    }

    /// Follow a new value of the `agents` setting (`all`, or agent ids).
    pub fn set_agents(&mut self, setting: &str) {
        self.agents = Filter::parse(setting).unwrap_or_default();
        self.all_agents = self.agents.is_all();
    }

    /// Whose sessions polls list.
    fn filter(&mut self) -> Filter {
        if self.all_agents {
            return Filter::All;
        }
        if self.agents.is_all() {
            // `all_agents` was turned off without saying which agents (the
            // dashboard's Settings view): the setting says
            self.agents = Filter::from_cfg(&Config::load());
            self.all_agents = self.agents.is_all();
        }
        self.agents.clone()
    }

    fn poll(&mut self, alias: &str) {
        let filter = self.filter();
        let plain_shells = self.plain_shells;
        let g = {
            let e = self.gen.entry(alias.to_string()).or_insert(0);
            *e += 1;
            *e
        };
        let Some(h) = self.host_mut(alias) else {
            return;
        };
        h.polling = Some(Instant::now());
        h.last_poll = Some(Instant::now());
        let (ssh, tx, machines) = (h.alias.clone(), self.tx.clone(), self.machines.clone());
        thread::spawn(move || {
            let t0 = Instant::now();
            let machine = {
                let known = machines.lock().unwrap().get(&ssh).cloned();
                match known {
                    Some(m) => m,
                    None => {
                        let m = herdr::machine_for(&ssh);
                        machines.lock().unwrap().insert(ssh.clone(), m.clone());
                        m
                    }
                }
            };
            let t = Target {
                ssh: ssh.clone(),
                machine,
                source: "fleet",
            };
            let h = Herdr::new(&t);
            let mut res = h.sessions_matching(&filter);
            // `shell.list = aleph`: only the shells Aleph started
            if let (false, Ok(v)) = (plain_shells, res.as_mut()) {
                v.retain(|s| !s.plain);
            }
            // herdr doesn't restore pane tokens after a server restart: give
            // sessions that lost their hue/mark one again (once per run)
            if let Ok(v) = res.as_mut() {
                retoken(&h, &ssh, v);
                // a conversation herdr didn't name (Codex's hooks run in its
                // daemon, away from the pane): the store's newest there
                crate::usage::recent::fill(&ssh, v);
            }
            let took = t0.elapsed();
            // the machine answered (even if its herdr server is down): check the version
            if res.as_ref().map_or_else(|e| not_running(e), |_| true) {
                if let Some(info) = herdr_version_of(&h, &ssh) {
                    let _ = tx.send(Msg::Version {
                        alias: ssh.clone(),
                        info,
                    });
                }
            }
            let _ = tx.send(Msg::Polled {
                alias: ssh,
                gen: g,
                res,
                took,
            });
        });
    }

    /// Drain results and start polls that are due. Returns status transitions.
    /// Every machine's colour made again (the theme changed).
    pub fn recolor(&mut self, cfg: &Config) {
        for h in self.hosts.iter_mut() {
            h.color = theme::machine(cfg, &h.alias);
        }
    }

    pub fn tick(&mut self) -> Vec<Transition> {
        let mut out = Vec::new();
        // the watch rules, re-read every half minute (a change resubscribes)
        if self.listener.is_some()
            && self
                .rules_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(30))
        {
            self.rules_at = Some(Instant::now());
            self.watch_rules = crate::watchrules::rules(&Config::load());
        }
        let mut matches = Vec::new();
        if let Some(rx) = &self.events {
            while let Ok(e) = rx.try_recv() {
                if e.kind == "pane.output_matched" {
                    matches.push(e);
                } else {
                    self.nudged.insert(e.host);
                }
            }
        }
        for e in matches {
            self.on_match(&e);
        }
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Version { alias, info } => {
                    if let Some(h) = self.host_mut(&alias) {
                        h.herdr_version = Some(info.version.clone());
                        h.herdr_info = Some(info);
                    }
                }
                Msg::Commands { alias, seen } => {
                    self.command_scan
                        .insert(alias.clone(), (Instant::now(), false));
                    let t = now();
                    for (pane, fg) in seen {
                        note_foreground(
                            &format!("{alias}|{pane}"),
                            fg,
                            t,
                            &mut self.commands,
                            &mut self.ended,
                            &mut self.ran,
                        );
                    }
                }
                Msg::Compacting { alias, read, found } => {
                    self.compact_scan
                        .insert(alias.clone(), (Instant::now(), false));
                    for pane in read {
                        let k = format!("{alias}|{pane}");
                        if found.contains(&pane) {
                            self.compacting.insert(k);
                        } else {
                            self.compacting.remove(&k);
                        }
                    }
                    let compacting = &self.compacting;
                    if let Some(h) = self.hosts.iter_mut().find(|h| h.alias == alias) {
                        for s in h.sessions.iter_mut() {
                            s.compacting = s.status == "working" && compacting.contains(&s.key());
                        }
                    }
                }
                Msg::Limits { alias, found } => {
                    self.limit_scan
                        .insert(alias.clone(), (Instant::now(), false));
                    for (pane, status, reset) in found {
                        let k = format!("{alias}|{pane}");
                        match reset {
                            Some(r) => {
                                self.limits.insert(k, (status, r));
                            }
                            None => {
                                self.limits.remove(&k);
                            }
                        }
                    }
                    let limits = &self.limits;
                    if let Some(h) = self.hosts.iter_mut().find(|h| h.alias == alias) {
                        for s in h.sessions.iter_mut() {
                            s.limit = limits
                                .get(&s.key())
                                .filter(|(st, _)| *st == s.status)
                                .map(|(_, r)| r.clone());
                        }
                    }
                    self.changed_at = Instant::now();
                }
                Msg::Resolved { alias, user, addr } => {
                    if let Some(h) = self.host_mut(&alias) {
                        h.user = user;
                        h.addr = addr;
                    }
                }
                Msg::Polled {
                    alias,
                    gen,
                    res,
                    took,
                } => {
                    if self.gen.get(&alias) != Some(&gen) {
                        continue;
                    }
                    let known = self.machines.lock().unwrap().get(&alias).cloned();
                    if let Some(m) = known {
                        if let Some(h) = self.host_mut(&alias) {
                            h.target.machine = m;
                        }
                    }
                    out.extend(self.apply(&alias, res, took));
                    self.changed_at = Instant::now();
                }
            }
        }
        let due: Vec<String> = self
            .hosts
            .iter()
            .filter(|h| !h.paused)
            .filter(|h| {
                // events from a live listener poll at once (nudged); the timer
                // is then only a reconcile, every few seconds
                let iv = if self.live(&h.alias) == Some(crate::listen::LState::Live) {
                    self.interval.max(LIVE_RECONCILE)
                } else {
                    self.interval
                };
                let wait = backoff(iv, h);
                (h.polling.is_none() && h.last_poll.is_none_or(|t| t.elapsed() >= wait))
                    || (h.polling.is_none() && self.nudged.contains(&h.alias))
            })
            .map(|h| h.alias.clone())
            .collect();
        for a in &due {
            self.nudged.remove(a);
        }
        for a in due {
            self.poll(&a);
        }
        if self.scan_limits {
            self.scan_for_limits();
            self.scan_for_compaction();
            self.scan_for_commands();
        }
        out
    }

    /// Every `LIMIT_SCAN`, read the tail of each machine's idle sessions (one
    /// ssh call per machine) and look for "limit reached … resets …".
    fn scan_for_limits(&mut self) {
        for h in &self.hosts {
            if h.conn != Conn::Up {
                continue;
            }
            let due = self
                .limit_scan
                .get(&h.alias)
                .is_none_or(|(t, running)| !running && t.elapsed() >= LIMIT_SCAN);
            if !due {
                continue;
            }
            let cands: Vec<(String, String)> = h
                .sessions
                .iter()
                .filter(|s| {
                    matches!(s.status.as_str(), "idle" | "done") && s.agent != crate::agents::SHELL
                })
                .map(|s| (s.pane_id.clone(), s.status.clone()))
                .collect();
            self.limit_scan
                .insert(h.alias.clone(), (Instant::now(), !cands.is_empty()));
            if cands.is_empty() {
                // nothing idle: forget limits seen there
                let pre = format!("{}|", h.alias);
                self.limits.retain(|k, _| !k.starts_with(&pre));
                continue;
            }
            let (t, tx, alias) = (h.target.clone(), self.tx.clone(), h.alias.clone());
            thread::spawn(move || {
                let panes: Vec<String> = cands.iter().map(|(p, _)| p.clone()).collect();
                let text = Herdr::new(&t).read_many(&panes, crate::limits::LINES);
                let found = cands
                    .into_iter()
                    .map(|(p, st)| {
                        let r = text.get(&p).and_then(|x| crate::limits::detect(x));
                        (p, st, r)
                    })
                    .collect();
                let _ = tx.send(Msg::Limits { alias, found });
            });
        }
    }

    /// Every `COMPACT_SCAN`, read the end of each machine's working agents'
    /// screens (one ssh call per machine): which are compacting?
    /// Every `COMMAND_SCAN`, what each machine's shells have in the foreground.
    fn scan_for_commands(&mut self) {
        for h in &self.hosts {
            if h.conn != Conn::Up || h.stale || h.paused {
                continue;
            }
            let due = self
                .command_scan
                .get(&h.alias)
                .is_none_or(|(t, running)| !running && t.elapsed() >= COMMAND_SCAN);
            let panes: Vec<String> = h
                .sessions
                .iter()
                .filter(|s| s.agent == crate::agents::SHELL)
                .map(|s| s.pane_id.clone())
                .collect();
            if !due || panes.is_empty() {
                continue;
            }
            self.command_scan
                .insert(h.alias.clone(), (Instant::now(), true));
            let (t, tx, alias) = (h.target.clone(), self.tx.clone(), h.alias.clone());
            thread::spawn(move || {
                let seen = Herdr::new(&t).foreground_many(&panes);
                let _ = tx.send(Msg::Commands { alias, seen });
            });
        }
    }

    /// What a shell has run while this was watching, newest first; the first one
    /// with no end is still running.
    pub fn commands_of(&self, key: &str) -> Vec<Ran> {
        let mut v: Vec<Ran> = self.commands.get(key).cloned().into_iter().collect();
        if let Some(r) = self.ran.get(key) {
            v.extend(r.iter().rev().cloned());
        }
        v
    }

    fn scan_for_compaction(&mut self) {
        for h in &self.hosts {
            if h.conn != Conn::Up {
                continue;
            }
            let due = self
                .compact_scan
                .get(&h.alias)
                .is_none_or(|(t, running)| !running && t.elapsed() >= COMPACT_SCAN);
            let panes: Vec<String> = h
                .sessions
                .iter()
                .filter(|s| s.status == "working" && s.agent != crate::agents::SHELL)
                .map(|s| s.pane_id.clone())
                .collect();
            if !due || panes.is_empty() {
                continue;
            }
            self.compact_scan
                .insert(h.alias.clone(), (Instant::now(), true));
            let (t, tx, alias) = (h.target.clone(), self.tx.clone(), h.alias.clone());
            thread::spawn(move || {
                let text = Herdr::new(&t).read_many(&panes, crate::limits::LINES);
                let found = panes
                    .iter()
                    .filter(|p| text.get(*p).is_some_and(|x| crate::limits::compacting(x)))
                    .cloned()
                    .collect();
                let _ = tx.send(Msg::Compacting {
                    alias,
                    read: panes,
                    found,
                });
            });
        }
    }

    fn apply(
        &mut self,
        alias: &str,
        res: Result<Vec<Session>, String>,
        took: Duration,
    ) -> Vec<Transition> {
        let t = now();
        let mut trans = Vec::new();
        let mut dirty = false;
        let filter = self.filter();
        let rules = self.outages.clone();
        let since = &mut self.since;
        let Some(h) = self.hosts.iter_mut().find(|h| h.alias == alias) else {
            return trans;
        };
        h.polling = None;
        match res {
            Ok(mut v) => {
                for s in v.iter_mut() {
                    s.host = alias.to_string();
                    let k = s.key();
                    note_compaction(s, &mut self.compacting, &mut self.compacted);
                    let was_working = since
                        .get(&k)
                        .is_some_and(|(st, _)| st.split('#').next() == Some("working"));
                    // (a fleet that doesn't scan, as a one-off `aleph ls`, won't hear more)
                    let scanned = !self.scan_limits
                        || self.command_scan.get(alias).is_some_and(|(_, busy)| !busy);
                    note_command(
                        s,
                        &self.commands,
                        &mut self.ended,
                        (was_working, scanned),
                        || Config::load().num("notify.commands"),
                    );
                    match self.limits.get(&k) {
                        Some((st, r)) if *st == s.status => s.limit = Some(r.clone()),
                        Some(_) => {
                            self.limits.remove(&k);
                        }
                        None => {}
                    }
                    let state = s.state_key();
                    match since.get(&k) {
                        Some((st, at)) if *st == state || (*st == s.status && s.seq == 0) => {
                            s.since = Some(*at)
                        }
                        prev => {
                            if let Some((st, at)) = prev {
                                trans.push(Transition {
                                    session: s.clone(),
                                    from: st.split('#').next().unwrap_or("").to_string(),
                                    took: t.saturating_sub(*at),
                                });
                            } else if h.fetched.is_some() && !h.stale {
                                // brand-new session since the last live poll
                                trans.push(Transition {
                                    session: s.clone(),
                                    from: String::new(),
                                    took: 0,
                                });
                            }
                            since.insert(k, (state, t));
                            s.since = Some(t);
                        }
                    }
                }
                // inbox events + session memory (events.rs, restore.rs); at
                // sub-second polling, session memory is only touched on change
                crate::events::on_poll(alias, t, h.fetched, &h.sessions, &v, &trans);
                let changed = h.sessions.len() != v.len()
                    || h.sessions.iter().zip(&v).any(|(a, b)| {
                        a.pane_id != b.pane_id || a.status != b.status || a.name != b.name
                    });
                let was_down = matches!(h.conn, Conn::Down(_));
                if changed || was_down || h.fetched.is_none_or(|f| t.saturating_sub(f) > 60) {
                    crate::restore::seen(alias, &v, &filter);
                }
                if matches!(&h.conn, Conn::Down(e) if !not_running(e)) {
                    crate::log::info("fleet", &format!("{alias} is back ({} sessions)", v.len()));
                }
                if let Some(o) = h.outage.take() {
                    outage_over(alias, o, &v, t, &rules);
                }
                h.answered(took);
                dirty |= changed;
                if let Some(l) = &self.listener {
                    l.set_panes(alias, v.iter().map(|s| s.pane_id.clone()).collect());
                    let ss: Vec<(String, String)> = v
                        .iter()
                        .map(|s| (s.pane_id.clone(), s.agent.clone()))
                        .collect();
                    l.set_watch(
                        alias,
                        crate::watchrules::plans(&self.watch_rules, alias, &ss),
                    );
                }
                h.sessions = v;
                h.conn = Conn::Up;
                h.fetched = Some(t);
                h.stale = false;
                h.latency = (!sshcfg::is_local(alias)).then_some(took);
            }
            // the machine answered, but no herdr server runs there: nothing is
            // running, so there's nothing to show from the cache either
            Err(e) if not_running(&e) => {
                // it answers: an outage is over, its sessions with herdr gone
                if let Some(o) = h.outage.take() {
                    outage_over(alias, o, &[], t, &rules);
                }
                dirty |= !h.sessions.is_empty();
                h.fails = h.fails.saturating_add(1);
                h.sessions.clear();
                h.conn = Conn::Down(e);
                h.fetched = Some(t);
                h.stale = false;
                h.latency = (!sshcfg::is_local(alias)).then_some(took);
            }
            Err(e) => {
                // the shared connection died under us: the next poll opens a new one
                if sshcfg::master_broken(&e) {
                    let a = alias.to_string();
                    thread::spawn(move || sshcfg::drop_master(&a));
                }
                if h.conn == Conn::Up {
                    crate::log::warn("fleet", &format!("{alias} is unreachable: {e}"));
                }
                // an outage: filed (or not) once it's lasted the grace
                if h.outage.is_none() && h.conn == Conn::Up {
                    h.outage = Some(Outage {
                        since: t,
                        at_risk: at_risk(&h.sessions),
                        ..Default::default()
                    });
                }
                outage_due(alias, h, &e, t, &rules);
                h.conn = Conn::Down(e);
                h.stale = !h.sessions.is_empty();
                h.latency = None;
                h.unanswered();
            }
        }
        // forget timestamps of sessions that are gone from every host
        let live: Vec<String> = self
            .hosts
            .iter()
            .flat_map(|h| h.sessions.iter().map(|s| s.key()))
            .collect();
        self.since
            .retain(|k, _| live.contains(k) || !k.starts_with(&format!("{alias}|")));
        // at sub-second polling, write the snapshot on change (or every 5 s)
        if dirty || self.snap_at.elapsed() > Duration::from_secs(5) {
            self.save_snapshot();
            self.snap_at = Instant::now();
        }
        trans
    }

    /// Poll every host once and wait (bounded) — for one-shot CLI commands.
    pub fn poll_blocking(&mut self, timeout: Duration) -> Vec<Transition> {
        self.refresh(None);
        let end = Instant::now() + timeout;
        let mut out = Vec::new();
        while self.polling() && Instant::now() < end {
            thread::sleep(Duration::from_millis(40));
            out.extend(self.tick_no_schedule());
        }
        out
    }

    fn tick_no_schedule(&mut self) -> Vec<Transition> {
        let saved = self.interval;
        self.interval = Duration::from_secs(u64::MAX / 4);
        let t = self.tick();
        self.interval = saved;
        t
    }

    // ------------------------------------------------------------ snapshot

    fn snap_file() -> std::path::PathBuf {
        crate::config::world_cache().join("fleet.json")
    }

    fn load_snapshot(&mut self) {
        let Ok(s) = std::fs::read_to_string(Self::snap_file()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&s) else {
            return;
        };
        if let Some(m) = v.get("since").and_then(Value::as_object) {
            for (k, x) in m {
                if let (Some(st), Some(at)) = (
                    x.get(0).and_then(Value::as_str),
                    x.get(1).and_then(Value::as_u64),
                ) {
                    self.since.insert(k.clone(), (st.to_string(), at));
                }
            }
        }
        for h in self.hosts.iter_mut() {
            let Some(hv) = v.pointer(&format!("/hosts/{}", json_ptr(&h.alias))) else {
                continue;
            };
            h.fetched = hv.get("fetched").and_then(Value::as_u64);
            h.sessions = hv
                .get("sessions")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(session_from_json).collect())
                .unwrap_or_default();
            for s in h.sessions.iter_mut() {
                s.host = h.alias.clone();
                s.since = self
                    .since
                    .get(&s.key())
                    .filter(|(st, _)| *st == s.status)
                    .map(|x| x.1);
                // a limit a dashboard / watch saw holds while the status stays put
                if let Some(r) = &s.limit {
                    self.limits.insert(s.key(), (s.status.clone(), r.clone()));
                }
            }
            h.stale = !h.sessions.is_empty();
        }
    }

    pub fn save_snapshot(&self) {
        // merge with what's on disk so hosts outside this fleet (e.g. --host runs) survive
        let mut root: Value = std::fs::read_to_string(Self::snap_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        if !root.get("hosts").is_some_and(Value::is_object) {
            root["hosts"] = json!({});
        }
        for h in &self.hosts {
            if h.stale || h.fetched.is_none() {
                continue;
            }
            root["hosts"][&h.alias] = json!({
                "fetched": h.fetched,
                "sessions": h.sessions.iter().map(session_to_json).collect::<Vec<_>>(),
            });
        }
        let mut since = serde_json::Map::new();
        if let Some(old) = root.get("since").and_then(Value::as_object) {
            for (k, v) in old {
                if !self
                    .hosts
                    .iter()
                    .any(|h| k.starts_with(&format!("{}|", h.alias)))
                {
                    since.insert(k.clone(), v.clone());
                }
            }
        }
        for (k, (st, at)) in &self.since {
            since.insert(k.clone(), json!([st, at]));
        }
        root["since"] = Value::Object(since);
        root["schema"] = json!(2);
        let _ = crate::config::write_atomic(&Self::snap_file(), root.to_string().as_bytes());
    }

    // ------------------------------------------------------------ JSON (ls --json, Raycast)

    pub fn to_json(&self, cfg: &Config) -> Value {
        json!({
            "version": 1,
            "generated_at": now(),
            "hosts": self.hosts.iter().map(|h| json!({
                "alias": h.alias,
                "machine": h.target.machine,
                "color": theme::machine_hex(cfg, &h.alias),
                "address": h.addr,
                // not polled yet this run: what the last run saw stands (marked stale)
                "reachable": h.conn == Conn::Up || (h.conn == Conn::Unknown && h.stale),
                "error": match &h.conn { Conn::Down(e) => Some(e.clone()), _ => None },
                // a paused machine isn't polled: what it shows is as last seen
                "stale": h.stale || h.paused,
                "paused": h.paused,
                "fetched_at": h.fetched,
                "latency_ms": h.latency.map(|d| d.as_millis() as u64),
                "herdr_version": h.herdr_version,
                "herdr_drift": h.drift().is_some(),
                "herdr_compatible": h.herdr_info.as_ref().and_then(|i| i.compatible),
                "herdr_restart_needed": h.herdr_restart(),
                // `Linux · Ubuntu 24.04.3 LTS` (from the herdr check): what a machine is, before anything else asks
                "os": Some(h.os()).filter(|o| !o.is_empty()),
                "sessions": h.sessions.iter().map(|s| {
                    let mut v = session_to_json(s);
                    v["display"] = json!(s.display());
                    v["color"] = json!(crate::ui::hex(s.color()));
                    v["glyph"] = json!(s.glyph());
                    // phones and the web may act on it (remote.allow)
                    v["remote"] = json!(crate::permit::allowed(cfg, &s.host, &s.agent));
                    v
                }).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        })
    }
}

fn json_ptr(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

pub fn session_to_json(s: &Session) -> Value {
    json!({
        "host": s.host, "name": s.name, "label": s.label, "pane_id": s.pane_id, "tab_id": s.tab_id,
        "workspace_id": s.workspace_id, "terminal_id": s.terminal_id, "agent": s.agent,
        "status": s.status, "status_since": s.since, "cwd": s.cwd, "title": s.title,
        "mark": s.mark, "hue": s.hue, "focused": s.focused, "limit": s.limit,
        "seq": s.seq, "done_seq": s.done_seq, "ready": s.ready, "native_id": s.native_id,
        "compacting": s.compacting, "compacted": s.compacted,
        "plain": s.plain, "restore_error": s.restore_error,
        "launch_pending": s.launch_pending,
    })
}

pub(crate) fn session_from_json(v: &Value) -> Option<Session> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let pane = s("pane_id");
    if pane.is_empty() {
        return None;
    }
    Some(Session {
        host: s("host"),
        pane_id: pane,
        tab_id: s("tab_id"),
        workspace_id: s("workspace_id"),
        terminal_id: s("terminal_id"),
        label: s("label"),
        name: s("name"),
        agent: s("agent"),
        status: s("status"),
        cwd: s("cwd"),
        title: s("title"),
        mark: s("mark"),
        hue: s("hue"),
        focused: v.get("focused").and_then(Value::as_bool).unwrap_or(false),
        since: v.get("status_since").and_then(Value::as_u64),
        limit: v.get("limit").and_then(Value::as_str).map(str::to_string),
        seq: v.get("seq").and_then(Value::as_u64).unwrap_or(0),
        done_seq: v.get("done_seq").and_then(Value::as_u64).unwrap_or(0),
        ready: v.get("ready").and_then(Value::as_bool),
        compacting: v
            .get("compacting")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        compacted: v.get("compacted").and_then(Value::as_bool).unwrap_or(false),
        plain: v.get("plain").and_then(Value::as_bool).unwrap_or(false),
        launch_pending: v
            .get("launch_pending")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        restore_error: v
            .get("restore_error")
            .and_then(Value::as_str)
            .map(str::to_string),
        native_id: v
            .get("native_id")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shells_command_shows_as_working_and_a_long_one_tells_you_it_ended() {
        let (mut on, mut ended, mut ran) = (HashMap::new(), HashMap::new(), HashMap::new());
        let k = "b31|w1:p7";
        let poll = |on: &HashMap<String, Ran>, ended: &mut HashMap<String, Ran>, min: u64| {
            let mut s = Session {
                host: "b31".into(),
                pane_id: "w1:p7".into(),
                agent: crate::agents::SHELL.into(),
                status: "idle".into(),
                ..Default::default()
            };
            note_command(&mut s, on, ended, (false, true), || min);
            s
        };
        // at its prompt: nothing
        let s = poll(&on, &mut ended, 30);
        assert!(s.status == "idle" && s.title.is_empty() && !s.compacted);
        // a command: working, named for it, and it keeps its start while it runs
        note_foreground(
            k,
            Some("cargo build".into()),
            100,
            &mut on,
            &mut ended,
            &mut ran,
        );
        note_foreground(
            k,
            Some("cargo build".into()),
            103,
            &mut on,
            &mut ended,
            &mut ran,
        );
        assert_eq!(on[k].from, 100);
        let s = poll(&on, &mut ended, 30);
        assert_eq!(
            (s.status.as_str(), s.title.as_str()),
            ("working", "cargo build")
        );
        // it ends: the poll that says so names it, and a turn's Done goes out
        note_foreground(k, None, 200, &mut on, &mut ended, &mut ran);
        let s = poll(&on, &mut ended, 30);
        assert_eq!(
            (s.status.as_str(), s.title.as_str()),
            ("idle", "cargo build")
        );
        assert!(!s.compacted, "worth telling");
        let tr = Transition {
            session: s,
            from: "working".into(),
            took: 100,
        };
        let e = crate::events::from_poll("b31", 200, &[tr], &[], &[]);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, crate::events::Kind::Done);
        assert_eq!(e[0].title, "cargo build");
        // the next poll is an ordinary one
        assert!(poll(&on, &mut ended, 30).title.is_empty());
        // too short, or something you left running: no news
        for (cmd, secs, min) in [
            ("make", 10, 30),
            ("vim notes.md", 900, 30),
            ("/usr/bin/ssh gpu2", 900, 30),
            ("make", 900, 0),
        ] {
            note_foreground(k, Some(cmd.into()), 1000, &mut on, &mut ended, &mut ran);
            note_foreground(k, None, 1000 + secs, &mut on, &mut ended, &mut ran);
            let s = poll(&on, &mut ended, min);
            assert!(s.compacted, "{cmd}");
            let tr = Transition {
                session: s,
                from: "working".into(),
                took: secs,
            };
            assert!(crate::events::from_poll("b31", 1, &[tr], &[], &[]).is_empty());
        }
        // one after another with no prompt seen between: the first has ended
        note_foreground(k, Some("a".into()), 1, &mut on, &mut ended, &mut ran);
        note_foreground(k, Some("b".into()), 2, &mut on, &mut ended, &mut ran);
        assert_eq!(on[k].cmd, "b");
        assert_eq!(ended[k].cmd, "a");
        // held in memory only, and only so many
        for i in 0..100 {
            note_foreground(k, Some(i.to_string()), i, &mut on, &mut ended, &mut ran);
        }
        assert_eq!(ran[k].len(), RAN_KEPT);
        // an agent is never touched
        let mut a = Session {
            host: "b31".into(),
            pane_id: "w1:p7".into(),
            agent: "claude".into(),
            status: "idle".into(),
            ..Default::default()
        };
        note_command(&mut a, &on, &mut ended, (true, false), || 30);
        assert_eq!(a.status, "idle");
    }

    /// A dashboard restarted while a command runs: no false "finished" in the gap
    /// before its first scan, and a command that ended unseen says nothing.
    #[test]
    fn a_restart_while_a_command_runs_is_no_news() {
        let shell = || Session {
            host: "b31".into(),
            pane_id: "w1:p7".into(),
            agent: crate::agents::SHELL.into(),
            status: "idle".into(),
            ..Default::default()
        };
        let (on, mut ended) = (HashMap::new(), HashMap::new());
        // the last poll said working, no scan has answered yet: it still is
        let mut s = shell();
        note_command(&mut s, &on, &mut ended, (true, false), || 30);
        assert_eq!(s.status, "working");
        assert!(!s.compacted);
        // the scan says its prompt: at rest, and nobody's told
        let mut s = shell();
        note_command(&mut s, &on, &mut ended, (true, true), || 30);
        assert_eq!(s.status, "idle");
        assert!(s.compacted);
        let tr = Transition {
            session: s,
            from: "working".into(),
            took: 12,
        };
        assert!(crate::events::from_poll("b31", 1, &[tr], &[], &[]).is_empty());
        // never working: nothing at all
        let mut s = shell();
        note_command(&mut s, &on, &mut ended, (false, false), || 30);
        assert!(s.status == "idle" && !s.compacted);
    }

    #[test]
    fn watch_matches_land_in_the_inbox_once() {
        let mut f = Fleet::placeholder();
        let e = crate::listen::Ev {
            host: "b31".into(),
            kind: "pane.output_matched".into(),
            data: json!({"pane_id": "w1:p2", "rule": "tests", "matched_line": "  3 FAILED  "}),
            at: std::time::SystemTime::now(),
        };
        f.on_match(&e);
        f.on_match(&e);
        let n = crate::events::load(50)
            .iter()
            .filter(|x| x.kind == crate::events::Kind::Match && x.title == "tests: 3 FAILED")
            .count();
        assert_eq!(n, 1);
    }

    #[test]
    fn links_and_backoff() {
        let mut f = Fleet::placeholder();
        f.add_host(&Config::in_memory(&["hosts=x", "local=false"]), "x");
        let mut h = f.hosts.remove(0);
        h.conn = Conn::Down("ssh: connect to host x port 22: Operation timed out".into());
        h.unanswered();
        let w1 = backoff(Duration::from_millis(100), &h);
        for _ in 0..10 {
            h.unanswered();
        }
        let w2 = backoff(Duration::from_millis(100), &h);
        assert!(w1 < w2, "{w1:?} {w2:?}");
        // capped at 2 min (+20 % jitter at most)
        assert!(w2 <= Duration::from_secs(144), "{w2:?}");
        assert_eq!(h.link(), Link::Down);
        // back, but slow and lossy: degraded; fails are kept for a minute
        h.conn = Conn::Up;
        h.answered(Duration::from_millis(1500));
        assert_eq!(h.link(), Link::Degraded);
        assert!(h.fails > 0);
        // a minute of answering forgives them
        h.healthy_since = Some(Instant::now() - Duration::from_secs(61));
        for _ in 0..12 {
            h.answered(Duration::from_millis(40));
        }
        assert_eq!(h.fails, 0);
        assert_eq!(h.link(), Link::Up);
        // jitter differs per machine, and stays the same for one
        assert_eq!(
            jitter(Duration::from_secs(10), "a", 3),
            jitter(Duration::from_secs(10), "a", 3)
        );
        assert_ne!(
            jitter(Duration::from_secs(10), "a", 3),
            jitter(Duration::from_secs(10), "b", 3)
        );
    }

    #[test]
    fn a_compactions_end_is_not_a_finished_turn() {
        let (mut on, mut after) = (std::collections::HashSet::new(), HashMap::new());
        let polled = |status: &str, done_seq: u64, on: &mut _, after: &mut _| {
            let mut s = Session {
                host: "b31".into(),
                pane_id: "w1:p1".into(),
                status: status.into(),
                done_seq,
                ..Default::default()
            };
            note_compaction(&mut s, on, after);
            s
        };
        // nothing known: as herdr says
        let s = polled("done", 44, &mut on, &mut after);
        assert!(s.status == "done" && !s.compacted);
        // its screen says compacting (herdr: working, then done with its seq up)
        on.insert("b31|w1:p1".to_string());
        let s = polled("working", 0, &mut on, &mut after);
        assert!(s.compacting && s.status == "working");
        let s = polled("done", 46, &mut on, &mut after);
        assert!(s.compacted && !s.compacting && s.status == "idle");
        // seen: herdr drops the seq, and it's still that rest
        let s = polled("idle", 0, &mut on, &mut after);
        assert!(s.compacted && s.status == "idle");
        // it works again, and finishes: a real end
        let s = polled("working", 0, &mut on, &mut after);
        assert!(!s.compacted && !s.compacting);
        let s = polled("done", 48, &mut on, &mut after);
        assert!(!s.compacted && s.status == "done");
    }

    #[test]
    fn this_computer_has_no_link_to_grade() {
        // a slow answer from the local herdr is a busy machine, not a bad link
        let host = |alias: &str| {
            let mut f = Fleet::placeholder();
            f.add_host(&Config::in_memory(&["local=false"]), alias);
            f.hosts.remove(0)
        };
        let mut h = host(crate::sshcfg::LOCAL);
        h.conn = Conn::Up;
        h.answered(Duration::from_millis(5000));
        assert_eq!(h.rtt_ewma, None);
        assert_eq!(h.link(), Link::Up);
        let mut h = host("b31");
        h.conn = Conn::Up;
        h.answered(Duration::from_millis(5000));
        assert_eq!(h.link(), Link::Degraded);
    }

    #[test]
    fn session_json_roundtrip() {
        let s = Session {
            host: "b31".into(),
            pane_id: "w1:p3".into(),
            label: "claude_ignite:px".into(),
            name: "ignite".into(),
            agent: "claude".into(),
            status: "blocked".into(),
            hue: "amber".into(),
            since: Some(42),
            ..Default::default()
        };
        assert_eq!(session_from_json(&session_to_json(&s)).unwrap(), s);
    }

    /// A fleet of one machine, `alias`, for the poll tests.
    fn one_host(alias: &str) -> Fleet {
        let mut f = Fleet {
            hosts: vec![],
            all_agents: false,
            agents: Filter::Only(vec!["claude"]),
            plain_shells: true,
            interval: Duration::from_secs(10),
            outages: Outages {
                grace: 30,
                back: "close".into(),
            },
            tx: channel().0,
            rx: channel().1,
            gen: HashMap::new(),
            since: HashMap::new(),
            machines: Arc::new(Mutex::new(HashMap::new())),
            changed_at: Instant::now(),
            nudged: Default::default(),
            snap_at: Instant::now(),
            listener: None,
            watch_rules: Vec::new(),
            rules_at: None,
            matched: HashMap::new(),
            events: None,
            scan_limits: false,
            limits: HashMap::new(),
            limit_scan: HashMap::new(),
            compacting: Default::default(),
            compacted: HashMap::new(),
            compact_scan: HashMap::new(),
            commands: HashMap::new(),
            ended: HashMap::new(),
            ran: HashMap::new(),
            command_scan: HashMap::new(),
        };
        f.hosts.push(Host {
            alias: alias.into(),
            target: Target {
                ssh: alias.into(),
                machine: "zz".into(),
                source: "fleet",
            },
            color: Color::Reset,
            user: String::new(),
            addr: String::new(),
            conn: Conn::Unknown,
            sessions: vec![],
            fetched: None,
            stale: false,
            latency: None,
            polling: None,
            last_poll: None,
            collapsed: false,
            paused: false,
            fails: 0,
            rtt_ewma: None,
            loss: 0.0,
            healthy_since: None,
            herdr_version: None,
            herdr_info: None,
            outage: None,
        });
        f
    }

    #[test]
    fn an_outage_reaches_the_inbox_only_when_an_agent_was_at_work() {
        std::env::set_var(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("chf-{}", std::process::id())),
        );
        let mk = |pane: &str, st: &str| Session {
            pane_id: pane.into(),
            name: format!("s{pane}"),
            status: st.into(),
            since: Some(now() - 600),
            ..Default::default()
        };
        let mine = |host: &str| -> Vec<crate::events::Event> {
            crate::events::load(400)
                .into_iter()
                .filter(|e| e.host == host)
                .collect()
        };
        let archived =
            |e: &crate::events::Event| crate::events::dismissed().contains(&crate::events::key(e));
        let timeout = "ssh: connect to host x port 22: Operation timed out";
        // a quiet machine drops: nothing, during the grace or after it
        let mut f = one_host("zz-quiet");
        f.apply("zz-quiet", Ok(vec![mk("w1:p1", "idle")]), Duration::ZERO);
        f.apply("zz-quiet", Err(timeout.into()), Duration::ZERO);
        f.hosts[0].outage.as_mut().unwrap().since -= 31;
        f.apply("zz-quiet", Err(timeout.into()), Duration::ZERO);
        f.apply("zz-quiet", Ok(vec![mk("w1:p1", "idle")]), Duration::ZERO);
        assert!(mine("zz-quiet").is_empty(), "{:?}", mine("zz-quiet"));
        // an agent at work: nothing in the grace, an item after it
        let mut f = one_host("zz-busy");
        f.apply("zz-busy", Ok(vec![mk("w1:p1", "working")]), Duration::ZERO);
        f.apply("zz-busy", Err(timeout.into()), Duration::ZERO);
        assert!(mine("zz-busy").is_empty());
        f.hosts[0].outage.as_mut().unwrap().since -= 31;
        f.apply("zz-busy", Err(timeout.into()), Duration::ZERO);
        let ev = mine("zz-busy");
        assert_eq!(ev.len(), 1, "{ev:?}");
        assert!(
            ev[0].body.starts_with("sw1:p1 was ") && ev[0].body.contains("into a turn"),
            "{ev:?}"
        );
        // back, the session still there: the row closes and both are archived
        f.apply("zz-busy", Ok(vec![mk("w1:p1", "working")]), Duration::ZERO);
        let ev = mine("zz-busy");
        let of = |ev: &[crate::events::Event], k: crate::events::Kind| {
            ev.iter().find(|e| e.kind == k).cloned().unwrap()
        };
        assert_eq!(ev.len(), 2, "{ev:?}");
        assert!(
            of(&ev, crate::events::Kind::HostUp)
                .body
                .contains("still working ✓"),
            "{ev:?}"
        );
        assert!(ev.iter().all(archived), "{ev:?}");
        // lost: a session gone when it's back stays in the Inbox
        let mut f = one_host("zz-lost");
        f.apply("zz-lost", Ok(vec![mk("w1:p1", "blocked")]), Duration::ZERO);
        f.apply("zz-lost", Err(timeout.into()), Duration::ZERO);
        f.hosts[0].outage.as_mut().unwrap().since -= 31;
        f.apply("zz-lost", Err(timeout.into()), Duration::ZERO);
        f.apply("zz-lost", Ok(vec![]), Duration::ZERO);
        let ev = mine("zz-lost");
        let down = ev.iter().find(|e| e.body.contains("was waiting on you"));
        let back = of(&ev, crate::events::Kind::HostUp);
        assert!(down.is_some(), "{ev:?}");
        assert!(back.body.contains("gone") && !archived(&back), "{ev:?}");
        // off: back says nothing
        let mut f = one_host("zz-off");
        f.outages.back = "off".into();
        f.apply("zz-off", Ok(vec![mk("w1:p1", "working")]), Duration::ZERO);
        f.apply("zz-off", Err(timeout.into()), Duration::ZERO);
        f.hosts[0].outage.as_mut().unwrap().since -= 31;
        f.apply("zz-off", Err(timeout.into()), Duration::ZERO);
        f.apply("zz-off", Ok(vec![]), Duration::ZERO);
        let ours = mine("zz-off")
            .into_iter()
            .filter(|e| {
                matches!(
                    e.kind,
                    crate::events::Kind::Error | crate::events::Kind::HostUp
                )
            })
            .count();
        assert_eq!(ours, 1);
    }

    #[test]
    fn transitions_and_since() {
        let mut f = one_host("zz-test-host");
        let mk = |st: &str| Session {
            pane_id: "w1:p1".into(),
            status: st.into(),
            ..Default::default()
        };
        std::env::set_var(
            "XDG_CACHE_HOME",
            std::env::temp_dir().join(format!("chf-{}", std::process::id())),
        );
        assert!(f
            .apply("zz-test-host", Ok(vec![mk("working")]), Duration::ZERO)
            .is_empty());
        let since = f.hosts[0].sessions[0].since;
        assert!(f
            .apply("zz-test-host", Ok(vec![mk("working")]), Duration::ZERO)
            .is_empty());
        assert_eq!(f.hosts[0].sessions[0].since, since);
        let t = f.apply("zz-test-host", Ok(vec![mk("blocked")]), Duration::ZERO);
        assert_eq!(t.len(), 1);
        assert_eq!(
            (t[0].from.as_str(), t[0].session.status.as_str()),
            ("working", "blocked")
        );
        f.apply("zz-test-host", Err("boom".into()), Duration::ZERO);
        assert!(f.hosts[0].stale && matches!(f.hosts[0].conn, Conn::Down(_)));
        assert_eq!(f.hosts[0].sessions.len(), 1);
    }
}
