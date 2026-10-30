//! The notification router. Every new event in the log — questions, finished
//! turns, starts and ends, usage limits, machines going down and coming
//! back, automation and remote actions — goes through the routes, and each
//! route sends it on to its sinks:
//!
//! ```toml
//! [[notify.route]]
//! when   = ["blocked", "limit", "error"]   # event kinds, or "all"
//! hosts  = ["*"]                            # machine globs
//! agents = ["*"]
//! to     = ["ntfy", "desktop"]              # desktop ntfy webpush slack webhook herdr raycast
//! quiet  = "23:00-08:00"                    # held then (notify.quiet_pass kinds still go)
//! escalate_after = "10m"                    # a question still waiting this long…
//! escalate_to = ["ntfy"]                    # …goes here too
//! ```
//!
//! Without routes, today's settings: desktop notifications for questions
//! (and finished turns with `notify_done`) from the daemon, and the
//! `notify.remote_on` kinds to ntfy / Slack / the webhook / Web Push (the
//! browsers that turned notifications on in the console). Do not disturb
//! (`aleph notify dnd 1h`) and per-session mutes (`m` in Fleet) hold back
//! everything but the `notify.quiet_pass` kinds. Each sink retries three
//! times and sends at most 20 a minute.

use crate::config::Config;
use crate::events::{self, Event, Kind};
use crate::fleet::Fleet;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};
use toml_edit::{DocumentMut, Item};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum To {
    Desktop,
    Ntfy,
    Slack,
    Webhook,
    /// a toast in herdr's own window (this computer's herdr)
    Herdr,
    /// refresh the Raycast menu bar
    Raycast,
    /// browsers that subscribed in the web console (Web Push)
    WebPush,
}

impl To {
    pub fn parse(s: &str) -> Option<To> {
        Some(match s.trim() {
            "desktop" => To::Desktop,
            "ntfy" | "phone" => To::Ntfy,
            "slack" => To::Slack,
            "webhook" => To::Webhook,
            "herdr" => To::Herdr,
            "raycast" => To::Raycast,
            "webpush" | "push" | "web" => To::WebPush,
            _ => return None,
        })
    }
    /// Off this computer, to a phone or a team (not the Inbox, Raycast or
    /// the desktop).
    pub fn remote(self) -> bool {
        matches!(self, To::Ntfy | To::Slack | To::Webhook | To::WebPush)
    }

    pub fn name(self) -> &'static str {
        match self {
            To::Desktop => "desktop",
            To::Ntfy => "ntfy",
            To::Slack => "slack",
            To::Webhook => "webhook",
            To::Herdr => "herdr",
            To::Raycast => "raycast",
            To::WebPush => "webpush",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Route {
    /// kinds (`blocked`, `done`, …); empty = all
    pub when: Vec<String>,
    pub hosts: Vec<String>,
    pub agents: Vec<String>,
    pub to: Vec<To>,
    /// minutes after midnight, local: (from, until)
    pub quiet: Option<(u32, u32)>,
    pub escalate_after: Option<Duration>,
    pub escalate_to: Vec<To>,
}

/// `23:00-08:00` → (1380, 480).
pub fn parse_quiet(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('-')?;
    let m = |x: &str| -> Option<u32> {
        let (h, mi) = x.trim().split_once(':')?;
        let (h, mi): (u32, u32) = (h.parse().ok()?, mi.parse().ok()?);
        (h < 24 && mi < 60).then_some(h * 60 + mi)
    };
    Some((m(a)?, m(b)?))
}

/// Is `minute` (of the local day) inside a quiet window (which may wrap midnight)?
pub fn in_quiet(q: (u32, u32), minute: u32) -> bool {
    let (a, b) = q;
    if a <= b {
        minute >= a && minute < b
    } else {
        minute >= a || minute < b
    }
}

fn list(t: &toml_edit::Table, k: &str) -> Vec<String> {
    match t.get(k) {
        Some(Item::Value(toml_edit::Value::Array(a))) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        Some(i) => i
            .as_str()
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        None => vec![],
    }
}

/// `[[notify.route]]` tables from a document.
pub fn routes_in(doc: &DocumentMut) -> Vec<Route> {
    let Some(Item::ArrayOfTables(a)) = doc.get("notify").and_then(|n| n.get("route")) else {
        return vec![];
    };
    a.iter()
        .map(|t| Route {
            when: list(t, "when"),
            hosts: list(t, "hosts"),
            agents: list(t, "agents"),
            to: list(t, "to").iter().filter_map(|x| To::parse(x)).collect(),
            quiet: t.get("quiet").and_then(Item::as_str).and_then(parse_quiet),
            escalate_after: t
                .get("escalate_after")
                .and_then(Item::as_str)
                .and_then(crate::log::parse_duration),
            escalate_to: list(t, "escalate_to")
                .iter()
                .filter_map(|x| To::parse(x))
                .collect(),
        })
        .collect()
}

/// The routes in effect: the settings files' `[[notify.route]]`, else the
/// ones today's settings describe.
pub fn routes(cfg: &Config, in_daemon: bool) -> Vec<Route> {
    let mut v: Vec<Route> = Vec::new();
    if cfg.is_toml() {
        for (_, p) in cfg.files() {
            if let Some(doc) = std::fs::read_to_string(&p)
                .ok()
                .and_then(|t| t.parse::<DocumentMut>().ok())
            {
                v.extend(routes_in(&doc));
            }
        }
    }
    if !v.is_empty() {
        return v;
    }
    // the desktop: questions (and finished turns), from the daemon only —
    // the dashboard is already in front of you
    if in_daemon && cfg.flag("notify") {
        // questions, limits, problems and finished turns (the long ones,
        // unless notify_done: see the sending)
        let when = ["blocked", "match", "limit", "error", "done"]
            .map(String::from)
            .to_vec();
        v.push(Route {
            when,
            to: vec![To::Desktop],
            ..Default::default()
        });
    }
    let mut remote: Vec<To> = [
        ("notify.ntfy", To::Ntfy),
        ("notify.slack", To::Slack),
        ("notify.webhook", To::Webhook),
    ]
    .iter()
    .filter(|(k, _)| cfg.get_raw(k).is_some_and(|x| !x.is_empty()))
    .map(|(_, t)| *t)
    .collect();
    // a browser that turned notifications on in the console asked for them
    if crate::webpush::has_subs() {
        remote.push(To::WebPush);
    }
    if !remote.is_empty() {
        v.push(Route {
            when: cfg
                .get_or("notify.remote_on")
                .split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect(),
            to: remote.clone(),
            quiet: parse_quiet(&cfg.get_or("notify.quiet")),
            escalate_after: crate::log::parse_duration(&cfg.get_or("notify.escalate_after")),
            escalate_to: remote,
            ..Default::default()
        });
    }
    if cfg.flag("notify.herdr") {
        v.push(Route {
            when: vec![
                "blocked".into(),
                "limit".into(),
                "error".into(),
                "match".into(),
            ],
            to: vec![To::Herdr],
            ..Default::default()
        });
    }
    if cfg.flag("notify.raycast") {
        v.push(Route {
            when: vec![],
            to: vec![To::Raycast],
            ..Default::default()
        });
    }
    v
}

fn glob_ok(list: &[String], v: &str) -> bool {
    list.is_empty() || list.iter().any(|g| crate::permit::glob(g, v))
}

fn kind_ok(when: &[String], k: Kind) -> bool {
    when.is_empty()
        || when.iter().any(|w| {
            w == "all"
                || w == k.as_str()
                || (w == "needs_input" && k == Kind::Blocked)
                || (w == "finished" && k == Kind::Done)
        })
}

/// What holds an event back right now (`None`: nothing).
#[derive(Clone, Copy, Debug, Default)]
pub struct Hold {
    pub dnd: bool,
    pub muted: bool,
    /// minute of the local day
    pub minute: u32,
    /// a finished turn shorter than `notify.done_min`: you were likely
    /// there for it, so it stays off the phone (Inbox, Raycast, desktop only)
    pub short: bool,
}

/// Where an event goes (pure — tested): the sinks of every route it matches,
/// minus what's held back (quiet hours, do not disturb, a muted session),
/// except the kinds in `pass`.
pub fn decide(routes: &[Route], e: &Event, hold: Hold, pass: &[String]) -> Vec<To> {
    let passes = kind_ok(pass, e.kind) && !pass.is_empty();
    let mut out: Vec<To> = Vec::new();
    if (hold.dnd || hold.muted) && !passes {
        return out;
    }
    for r in routes {
        if !kind_ok(&r.when, e.kind) || !glob_ok(&r.hosts, &e.host) {
            continue;
        }
        if !e.agent.is_empty() && !glob_ok(&r.agents, &e.agent) {
            continue;
        }
        if r.quiet.is_some_and(|q| in_quiet(q, hold.minute)) && !passes {
            continue;
        }
        for t in &r.to {
            if !out.contains(t) && !(hold.short && t.remote()) {
                out.push(*t);
            }
        }
    }
    out
}

/// How far back each pass looks for events logged late.
const LOOK_BACK: u64 = 30;

/// `notify.question_after` in seconds.
pub fn question_after(cfg: &Config) -> u64 {
    crate::log::parse_duration(&cfg.get_or("notify.question_after")).map_or(0, |d| d.as_secs())
}

/// What becomes of a question in its grace period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Due {
    Send,
    Wait,
    /// answered (or a new question took its place)
    Drop,
}

/// A question for the phone, `now` (pure — tested): `since` = since when its
/// session has been waiting on it, None = it isn't any more.
pub fn question_due(since: Option<u64>, now: u64, grace: u64) -> Due {
    match since {
        None => Due::Drop,
        Some(t) if now.saturating_sub(t) >= grace => Due::Send,
        Some(_) => Due::Wait,
    }
}

/// Since when `e`'s session has been waiting on that question, as this hub
/// sees it (a session it doesn't watch: since the event).
fn waiting_since(fleet: &Fleet, e: &Event) -> Option<u64> {
    match fleet
        .sessions()
        .into_iter()
        .find(|s| s.host == e.host && s.name == e.name)
    {
        Some(s) if s.blocked() && (e.seq == 0 || s.seq == e.seq) => Some(s.since.unwrap_or(e.t)),
        Some(_) => None,
        None => Some(e.t),
    }
}

/// How long a question waits for this hub to see its session waiting.
const QUESTION_WAIT: Duration = Duration::from_secs(10);

/// How long a finished turn waits for its other half: herdr's (how long it
/// worked) and the agent's own (what it did) go out as one alert.
const PAIR_WAIT: Duration = Duration::from_secs(10);

/// Does `cfg` have hooks that report finished turns for this event's agent
/// on its machine (so herdr's version waits for theirs)?
fn hooked(cfg: &Config, e: &Event) -> bool {
    matches!(e.agent.as_str(), "claude" | "codex" | "opencode")
        && cfg.host_get(&e.host, "hooks").as_deref() == Some("on")
}

/// `notify.include_text`: how much of what the agent said leaves the machine.
pub fn trim_text(e: &Event, mode: &str) -> Event {
    let mut e = e.clone();
    match mode {
        "off" => {
            e.title.clear();
            e.body.clear();
        }
        "full" => {}
        _ => {
            e.title = crate::ui::trunc(&e.title, 140);
            e.body = crate::ui::trunc(&e.body, 140);
        }
    }
    e
}

// ------------------------------------------------------------------ state

fn mute_file() -> std::path::PathBuf {
    crate::config::world_state().join("notify_mute.json")
}

/// Mutes: "host|name" → until (unix time).
pub fn mutes() -> Vec<(String, u64)> {
    let now = crate::cache::now();
    std::fs::read_to_string(mute_file())
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v.as_object().cloned())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_u64().map(|u| (k.clone(), u)))
                .filter(|(_, u)| *u > now)
                .collect()
        })
        .unwrap_or_default()
}

/// Mute a session for `secs` (0: unmute). Returns the new state (muted?).
pub fn mute(host: &str, name: &str, secs: u64) -> bool {
    let key = format!("{host}|{name}");
    let mut m: serde_json::Map<String, serde_json::Value> = mutes()
        .into_iter()
        .map(|(k, u)| (k, serde_json::json!(u)))
        .collect();
    let on = secs > 0;
    if on {
        m.insert(key, serde_json::json!(crate::cache::now() + secs));
    } else {
        m.remove(&key);
    }
    let _ = crate::config::write_atomic(
        &mute_file(),
        serde_json::Value::Object(m).to_string().as_bytes(),
    );
    on
}

pub fn muted(host: &str, name: &str) -> bool {
    let key = format!("{host}|{name}");
    mutes().iter().any(|(k, _)| *k == key)
}

/// Do not disturb: `notify.dnd` = `on`, or the unix time it ends.
pub fn dnd(cfg: &Config) -> Option<Option<u64>> {
    match cfg.get_or("notify.dnd").trim() {
        "" | "off" | "0" => None,
        "on" | "true" => Some(None),
        x => x
            .parse::<u64>()
            .ok()
            .filter(|u| *u > crate::cache::now())
            .map(Some),
    }
}

fn local_minute() -> u32 {
    let t = crate::cache::now() as i64 + crate::ui::utc_offset();
    (t.rem_euclid(86_400) / 60) as u32
}

// ------------------------------------------------------------------ sending

/// Send one event to one sink (blocking; on a background thread). A question
/// to ntfy carries Approve / Deny buttons when remote control allows it.
fn send(
    cfg: &Config,
    to: To,
    e: &Event,
    buttons: Option<&serde_json::Value>,
    ctx: &crate::message::Ctx,
) -> Result<(), String> {
    // showcase's demo world: desktop banners and the menu bar's refresh only
    if crate::showcase::quiet_sink(!matches!(to, To::Desktop | To::Raycast)) {
        return Ok(());
    }
    let mut ctx = ctx.clone();
    ctx.buttons = buttons.is_some();
    let m = crate::message::compose(e, &ctx);
    let url = |k: &str| {
        cfg.get(k)
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
            .ok_or_else(|| format!("{k} isn't set"))
    };
    match to {
        To::Ntfy => {
            let b = buttons.map(|b| ntfy_labels(b, &ctx));
            crate::notify::send_msg(
                &crate::notify::Sink::Ntfy(url("notify.ntfy")?),
                e,
                &m,
                b.as_ref(),
            )
        }
        To::Slack => crate::notify::send_msg(
            &crate::notify::Sink::Slack(url("notify.slack")?),
            e,
            &m,
            None,
        ),
        To::Webhook => crate::notify::send_msg(
            &crate::notify::Sink::Webhook(url("notify.webhook")?),
            e,
            &m,
            None,
        ),
        // (a finished turn's numbers after its words)
        To::Desktop => {
            crate::watch::deliver_event(cfg, &crate::turncost::with_line(e, ctx.turn.as_deref()))
                .then_some(())
                .ok_or_else(|| "no terminal-notifier / osascript / notify-send".into())
        }
        To::Herdr => crate::sock::toast(
            crate::sshcfg::LOCAL,
            &crate::notify::title_of(e),
            &e.title,
            e.kind == Kind::Blocked,
        ),
        To::WebPush => crate::webpush::notify(cfg, e, buttons, Some(&m)),
        To::Raycast => {
            if !cfg!(target_os = "macos") {
                return Ok(());
            }
            let link = cfg.get_or("notify.raycast.deeplink");
            let o = crate::proc::cap("open", &["-g", &link], Duration::from_secs(5));
            if o.ok() {
                Ok(())
            } else {
                Err(o.reason())
            }
        }
    }
}

/// Wake Raycast's menu bar (`notify.raycast.deeplink`) for something that
/// isn't an event — the usage numbers moved — at most once a minute per
/// `why`, and only when the Raycast route is on.
pub fn wake_raycast(cfg: &Config, why: &str) {
    use std::sync::{Mutex, OnceLock};
    if !cfg!(target_os = "macos") || cfg!(test) || !cfg.flag("notify.raycast") {
        return;
    }
    static LAST: OnceLock<Mutex<std::collections::HashMap<String, Instant>>> = OnceLock::new();
    let m = LAST.get_or_init(|| Mutex::new(Default::default()));
    {
        let mut m = m.lock().unwrap();
        if m.get(why)
            .is_some_and(|t| t.elapsed() < Duration::from_secs(60))
        {
            return;
        }
        m.insert(why.to_string(), Instant::now());
    }
    let link = cfg.get_or("notify.raycast.deeplink");
    let _ = crate::proc::cap("open", &["-g", &link], Duration::from_secs(5));
}

/// Approve / Deny named after the agent's own options (`✓ 1. Yes`, `✕ 3. No`).
fn ntfy_labels(b: &serde_json::Value, ctx: &crate::message::Ctx) -> serde_json::Value {
    let (yes, no) = crate::message::yes_no(&ctx.options);
    let mut b = b.clone();
    if let Some(a) = b.as_array_mut() {
        for x in a.iter_mut() {
            let label = match x.get("label").and_then(|l| l.as_str()) {
                Some("Approve") => yes.as_ref().map(|y| format!("✓ {y}")),
                Some("Deny") => no.as_ref().map(|n| format!("✕ {n}")),
                _ => None,
            };
            if let Some(l) = label {
                x["label"] = serde_json::json!(crate::ui::trunc(&l, 28));
            }
        }
    }
    b
}

/// Three tries, 2 s and 8 s apart.
fn send_retry(
    cfg: &Config,
    to: To,
    e: &Event,
    b: Option<&serde_json::Value>,
    ctx: &crate::message::Ctx,
) -> Result<(), String> {
    let mut last = String::new();
    for (i, wait) in [0u64, 2, 8].iter().enumerate() {
        if i > 0 {
            std::thread::sleep(Duration::from_secs(*wait));
        }
        match send(cfg, to, e, b, ctx) {
            Ok(()) => return Ok(()),
            Err(x) => last = x,
        }
    }
    Err(last)
}

/// The router's state, kept by the hub.
pub struct Router {
    /// events at or after this time are new (it starts at "now": no replay)
    since: u64,
    /// when it started: nothing older is sent, even in the look-back
    start: u64,
    seen: VecDeque<String>,
    seen_set: HashSet<String>,
    /// questions waiting: "host|name" → (asked at, escalated)
    waiting: HashMap<String, (u64, bool)>,
    /// sends per sink in the last minute
    sent: HashMap<To, VecDeque<Instant>>,
    raycast_at: Option<Instant>,
    in_daemon: bool,
    /// questions / finished turns sent lately: (host, name, kind, when) —
    /// herdr and the agent's own hook say the same thing once
    recent: VecDeque<(String, String, Kind, u64)>,
    /// finished turns of hooked agents waiting for their other half
    held: Vec<(Instant, Event)>,
    /// "host|name" → since when it's been at work (this hub's own polls:
    /// whoever wrote an event, how long the turn took is known here)
    work: HashMap<String, u64>,
    /// questions logged (by another aleph, or a hook) before this hub saw
    /// the session waiting: they go once it does, with the question and
    /// its buttons, or not at all if it was answered meanwhile
    unseen: Vec<(Instant, Event)>,
    /// questions for the phone in their grace period (`notify.question_after`)
    later: Vec<(Event, Vec<To>)>,
}

/// `notify.source`: does this event go out (pure — tested)? `herdr` drops the
/// agents' own, `agents` drops herdr's questions and finished turns, `both`
/// sends whichever says it first.
pub fn source_ok(mode: &str, e: &Event, recent: &VecDeque<(String, String, Kind, u64)>) -> bool {
    let said = matches!(e.kind, Kind::Blocked | Kind::Done);
    match (mode, e.source.as_str()) {
        ("herdr", "agent") => false,
        ("agents", "") if said => false,
        ("herdr" | "agents", _) => true,
        _ => {
            !said
                || !recent.iter().any(|(h, n, k, t)| {
                    *h == e.host && *n == e.name && *k == e.kind && t.abs_diff(e.t) <= 15
                })
        }
    }
}

impl Router {
    pub fn new(in_daemon: bool) -> Router {
        Router {
            since: crate::cache::now(),
            start: crate::cache::now(),
            seen: VecDeque::new(),
            seen_set: HashSet::new(),
            waiting: HashMap::new(),
            sent: HashMap::new(),
            raycast_at: None,
            in_daemon,
            recent: VecDeque::new(),
            held: Vec::new(),
            work: HashMap::new(),
            unseen: Vec::new(),
            later: Vec::new(),
        }
    }

    fn mark(&mut self, id: String) -> bool {
        if !self.seen_set.insert(id.clone()) {
            return false;
        }
        self.seen.push_back(id);
        while self.seen.len() > 2000 {
            if let Some(x) = self.seen.pop_front() {
                self.seen_set.remove(&x);
            }
        }
        true
    }

    /// Room in this sink's budget (20 a minute)?
    fn allow(&mut self, to: To) -> bool {
        let q = self.sent.entry(to).or_default();
        while q
            .front()
            .is_some_and(|t| t.elapsed() > Duration::from_secs(60))
        {
            q.pop_front();
        }
        if q.len() >= 20 {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// New events out to their sinks; questions still waiting escalate.
    pub fn tick(&mut self, cfg: &Config, fleet: &Fleet) {
        // showcase mode: a sound for what just happened (here in the hub,
        // so once per event; nothing without the feature)
        crate::showcase::sounds();
        let routes = routes(cfg, self.in_daemon);
        if routes.is_empty() {
            self.since = crate::cache::now();
            return;
        }
        let pass: Vec<String> = cfg
            .get_or("notify.quiet_pass")
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        let dnd = dnd(cfg).is_some();
        let minute = local_minute();
        let text = cfg.get_or("notify.include_text");
        let source = cfg.get_or("notify.source");
        let done_min =
            crate::log::parse_duration(&cfg.get_or("notify.done_min")).map_or(0, |d| d.as_secs());
        let grace = question_after(cfg);
        // the desktop gets every finished turn, not only the long ones
        let every_done = cfg.flag("notify_done");
        let mut out: Vec<(To, Event)> = Vec::new();
        let mut newest = self.since;
        let since = self.since;
        // a turn runs from its first working poll until it's done (a question
        // in between counts: it's the same turn)
        let now = crate::cache::now();
        for s in fleet.sessions() {
            if matches!(s.status.as_str(), "working" | "blocked") {
                self.work
                    .entry(format!("{}|{}", s.host, s.name))
                    .or_insert(s.since.unwrap_or(now));
            }
        }
        let mut ready: Vec<Event> = Vec::new();
        // a look back: another process may log a moment late (`seen` stops repeats)
        let start = self.start;
        for mut e in events::load(300)
            .into_iter()
            .filter(|e| e.t + LOOK_BACK >= since && e.t >= start)
        {
            newest = newest.max(e.t);
            if !self.mark(e.id()) {
                continue;
            }
            if e.kind == Kind::Done && e.took == 0 {
                if let Some(at) = self.work.get(&format!("{}|{}", e.host, e.name)) {
                    e.took = e.t.saturating_sub(*at);
                }
            }
            // `aleph notify test` already sent its own
            if e.source == "test" {
                continue;
            }
            // a hooked agent's finished turn: herdr's half and the agent's go as one
            if source == "both" && e.kind == Kind::Done && hooked(cfg, &e) {
                match self.held.iter().position(|(_, h)| events::same_news(h, &e)) {
                    Some(i) => ready.push(events::pair(self.held.remove(i).1, e)),
                    None => self.held.push((Instant::now(), e)),
                }
                continue;
            }
            ready.push(e);
        }
        // questions this hub hasn't seen waiting yet: once it does (it reads
        // the question and signs the buttons from its own view)
        let at_rest = |e: &Event| {
            fleet
                .sessions()
                .iter()
                .any(|s| s.host == e.host && s.name == e.name && !s.blocked())
        };
        let (mut q, waiting): (Vec<_>, Vec<_>) = ready
            .into_iter()
            .partition(|e| e.kind == Kind::Blocked && at_rest(e));
        ready = waiting;
        for e in q.drain(..) {
            self.unseen.push((Instant::now(), e));
        }
        for (at, e) in std::mem::take(&mut self.unseen) {
            if !at_rest(&e) {
                ready.push(e);
            } else if at.elapsed() < QUESTION_WAIT {
                self.unseen.push((at, e));
            }
            // still at rest after that: answered meanwhile, not news
        }
        // a half that waited long enough goes alone
        let (due, wait): (Vec<_>, Vec<_>) = std::mem::take(&mut self.held)
            .into_iter()
            .partition(|(at, _)| at.elapsed() >= PAIR_WAIT);
        self.held = wait;
        ready.extend(due.into_iter().map(|(_, e)| e));
        // a session at rest: its turn is over (the next one starts afresh)
        let live: Vec<&crate::herdr::Session> = fleet.sessions();
        self.work.retain(|k, _| {
            live.iter().any(|s| {
                format!("{}|{}", s.host, s.name) == *k
                    && matches!(s.status.as_str(), "working" | "blocked")
            })
        });
        for e in ready {
            if !source_ok(&source, &e, &self.recent) {
                continue;
            }
            if matches!(e.kind, Kind::Blocked | Kind::Done) {
                self.recent
                    .push_back((e.host.clone(), e.name.clone(), e.kind, e.t));
                while self.recent.len() > 200 {
                    self.recent.pop_front();
                }
            }
            let hold = Hold {
                dnd,
                muted: !e.name.is_empty() && muted(&e.host, &e.name),
                minute,
                // a session we watch that we never saw at work: shorter than a poll
                short: e.kind == Kind::Done
                    && e.took < done_min
                    && (e.took > 0 || live.iter().any(|s| s.host == e.host && s.name == e.name)),
            };
            if e.kind == Kind::Blocked {
                self.waiting
                    .insert(format!("{}|{}", e.host, e.name), (e.t, false));
            }
            // a question reaches the phone only if it's still waiting a little
            // later: answered at the terminal, it never does
            let (wait, go): (Vec<To>, Vec<To>) = decide(&routes, &e, hold, &pass)
                .into_iter()
                .partition(|t| e.kind == Kind::Blocked && grace > 0 && t.remote());
            for to in go {
                // the desktop: never about a session on a dashboard's screen
                // in front of you; a finished turn only a long one (notify.
                // done_min) and one you didn't see, unless notify_done
                if to == To::Desktop
                    && (crate::onscreen::showing(&e.host, &e.name)
                        || (e.kind == Kind::Done && (e.seen || (hold.short && !every_done))))
                {
                    continue;
                }
                out.push((to, trim_text(&e, &text)));
            }
            if !wait.is_empty() {
                self.later.push((e, wait));
            }
        }
        for (e, sinks) in std::mem::take(&mut self.later) {
            let since = waiting_since(fleet, &e);
            match question_due(since, now, grace) {
                Due::Send => out.extend(sinks.into_iter().map(|to| (to, trim_text(&e, &text)))),
                Due::Wait => self.later.push((e, sinks)),
                Due::Drop => {}
            }
        }
        self.since = newest;
        // questions still waiting long enough: their route's escalation sinks
        let now = crate::cache::now();
        let live: Vec<crate::herdr::Session> = fleet.sessions().into_iter().cloned().collect();
        self.waiting.retain(|k, _| {
            live.iter()
                .any(|s| format!("{}|{}", s.host, s.name) == *k && s.blocked())
        });
        for (k, (t, done)) in self.waiting.iter_mut() {
            if *done {
                continue;
            }
            let Some(s) = live.iter().find(|s| format!("{}|{}", s.host, s.name) == *k) else {
                continue;
            };
            let e = Event {
                t: *t,
                host: s.host.clone(),
                name: s.name.clone(),
                kind: Kind::Blocked,
                title: format!("still waiting · {}", s.title),
                agent: s.agent.clone(),
                ..Default::default()
            };
            for r in routes.iter().filter(|r| kind_ok(&r.when, Kind::Blocked)) {
                if let Some(after) = r.escalate_after {
                    if now.saturating_sub(*t) >= after.as_secs() && !dnd {
                        *done = true;
                        for to in &r.escalate_to {
                            out.push((*to, trim_text(&e, &text)));
                        }
                    }
                }
            }
        }
        // raycast: one refresh per 2 s at most
        out.retain(|(to, _)| {
            if *to != To::Raycast {
                return true;
            }
            let ok = self
                .raycast_at
                .is_none_or(|t| t.elapsed() > Duration::from_secs(2));
            if ok {
                self.raycast_at = Some(Instant::now());
            }
            ok
        });
        out.retain(|(to, _)| self.allow(*to));
        if out.is_empty() {
            return;
        }
        // questions to ntfy and Web Push: their buttons (remote control permitting)
        let sessions: Vec<crate::herdr::Session> = fleet.sessions().into_iter().cloned().collect();
        let out: Vec<(To, Event, Option<serde_json::Value>)> = out
            .into_iter()
            .map(|(to, e)| {
                let b = (matches!(to, To::Ntfy | To::WebPush) && e.kind == Kind::Blocked)
                    .then(|| {
                        sessions
                            .iter()
                            .find(|s| s.host == e.host && s.name == e.name)
                            .and_then(|s| match to {
                                To::WebPush => crate::webpush::buttons(cfg, s),
                                _ => crate::actions::ntfy_actions(cfg, s),
                            })
                    })
                    .flatten();
                (to, e, b)
            })
            .collect();
        // what each alert needs to say: its session, and for a question, what
        // it asks (read once, off this thread)
        let looks: HashMap<
            String,
            (
                Option<crate::herdr::Session>,
                Option<usize>,
                crate::config::Target,
            ),
        > = out
            .iter()
            .map(|(_, e, _)| {
                let s = sessions
                    .iter()
                    .find(|s| s.host == e.host && s.name == e.name)
                    .cloned();
                let n = fleet.host(&e.host).map(|h| h.sessions.len());
                let t = fleet
                    .host(&e.host)
                    .map(|h| h.target.clone())
                    .unwrap_or_else(|| cfg.target_for(&e.host, crate::herdr::machine_for));
                (e.id(), (s, n, t))
            })
            .collect();
        let cfg = cfg.clone();
        // finished turns last: each may wait a few seconds for its machine's
        // usage to be read (`turncost::for_alert`), and nothing else should
        let mut out = out;
        out.sort_by_key(|(_, e, _)| e.kind == Kind::Done);
        std::thread::spawn(move || {
            let mut ctxs: HashMap<String, crate::message::Ctx> = HashMap::new();
            // each machine asked once, just before its questions leave: still waiting?
            let mut fresh: HashMap<String, Vec<crate::herdr::Session>> = HashMap::new();
            // finished turns that are only a subagent reporting back
            let mut quiet: HashMap<String, bool> = HashMap::new();
            for (to, e, b) in out {
                if e.kind == Kind::Blocked && to.remote() {
                    if let Some((_, _, t)) = looks.get(&e.id()) {
                        let now = fresh.entry(e.host.clone()).or_insert_with(|| {
                            crate::herdr::Herdr::new(t).sessions().unwrap_or_default()
                        });
                        let gone = now.iter().any(|s| {
                            s.name == e.name && (!s.blocked() || (e.seq > 0 && s.seq != e.seq))
                        });
                        if gone {
                            crate::log::debug(
                                "notify",
                                &format!(
                                    "{}:{}: answered before it left — not sent",
                                    e.host, e.name
                                ),
                            );
                            continue;
                        }
                    }
                }
                let ctx = ctxs.entry(e.id()).or_insert_with(|| {
                    let (s, n, t) = looks.get(&e.id()).cloned().unwrap_or((
                        None,
                        None,
                        cfg.target_for(&e.host, crate::herdr::machine_for),
                    ));
                    // the question on screen, only while herdr says one is waiting
                    let det = match (&s, e.kind) {
                        (Some(s), Kind::Blocked) if s.blocked() => crate::herdr::Herdr::new(&t)
                            .read(&s.pane_id, "detection", 0, false)
                            .unwrap_or_default(),
                        _ => String::new(),
                    };
                    let mut c = crate::message::Ctx::of(&cfg, &e, s.as_ref(), &det);
                    if e.name.is_empty() {
                        c.sessions_there = n;
                    }
                    // a finished turn: what it took, once its usage is read
                    if e.kind == Kind::Done {
                        c.turn = crate::turncost::for_alert(&cfg, &e, s.as_ref());
                    }
                    c
                });
                if e.kind == Kind::Done {
                    let live = looks.get(&e.id()).and_then(|(s, _, _)| s.as_ref());
                    let q = *quiet
                        .entry(e.id())
                        .or_insert_with(|| crate::turncost::subagent_turn(&cfg, &e, live));
                    if q {
                        crate::log::debug(
                            "notify",
                            &format!("{}:{}: a subagent reported back — not sent", e.host, e.name),
                        );
                        continue;
                    }
                }
                if let Err(x) = send_retry(&cfg, to, &e, b.as_ref(), ctx) {
                    crate::log::warn("notify", &format!("{} failed: {x}", to.name()));
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind, host: &str, agent: &str) -> Event {
        Event {
            t: 1,
            host: host.into(),
            name: "temper".into(),
            kind,
            agent: agent.into(),
            title: "Run the tests?".into(),
            ..Default::default()
        }
    }

    #[test]
    fn routes_from_toml() {
        let doc: DocumentMut = r#"
[[notify.route]]
when = ["blocked", "limit"]
hosts = ["b31"]
to = ["ntfy", "desktop"]
quiet = "23:00-08:00"
escalate_after = "10m"
escalate_to = ["ntfy"]

[[notify.route]]
when = "all"
to = ["raycast"]
"#
        .parse()
        .unwrap();
        let r = routes_in(&doc);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].to, vec![To::Ntfy, To::Desktop]);
        assert_eq!(r[0].quiet, Some((1380, 480)));
        assert_eq!(r[0].escalate_after, Some(Duration::from_secs(600)));
        assert_eq!(r[1].when, vec!["all"]);
    }

    #[test]
    fn deciding() {
        let doc: DocumentMut = r#"
[[notify.route]]
when = ["blocked", "error"]
hosts = ["b31"]
agents = ["claude"]
to = ["ntfy"]
quiet = "23:00-08:00"

[[notify.route]]
when = ["all"]
to = ["raycast"]
"#
        .parse()
        .unwrap();
        let r = routes_in(&doc);
        let noon = Hold {
            minute: 12 * 60,
            ..Default::default()
        };
        let night = Hold {
            minute: 23 * 60 + 30,
            ..Default::default()
        };
        let pass = vec!["error".to_string()];
        // matched by both routes
        assert_eq!(
            decide(&r, &ev(Kind::Blocked, "b31", "claude"), noon, &pass),
            vec![To::Ntfy, To::Raycast]
        );
        // another machine, another agent: only the catch-all
        assert_eq!(
            decide(&r, &ev(Kind::Blocked, "gpu2", "claude"), noon, &pass),
            vec![To::Raycast]
        );
        assert_eq!(
            decide(&r, &ev(Kind::Blocked, "b31", "codex"), noon, &pass),
            vec![To::Raycast]
        );
        // quiet hours hold the question, not the error
        assert_eq!(
            decide(&r, &ev(Kind::Blocked, "b31", "claude"), night, &pass),
            vec![To::Raycast]
        );
        assert!(decide(&r, &ev(Kind::Error, "b31", "claude"), night, &pass).contains(&To::Ntfy));
        // do not disturb holds everything but what passes
        let dnd = Hold { dnd: true, ..noon };
        assert!(decide(&r, &ev(Kind::Blocked, "b31", "claude"), dnd, &pass).is_empty());
        assert!(!decide(&r, &ev(Kind::Error, "b31", "claude"), dnd, &pass).is_empty());
        // quiet windows wrap midnight
        assert!(in_quiet((1380, 480), 60) && !in_quiet((1380, 480), 600));
        assert!(in_quiet((540, 1020), 600) && !in_quiet((540, 1020), 1100));
    }

    #[test]
    fn herdr_agents_or_both() {
        let herdr = ev(Kind::Blocked, "b31", "claude");
        let mut agent = herdr.clone();
        agent.source = "agent".into();
        agent.t = 5;
        let none = VecDeque::new();
        assert!(!source_ok("herdr", &agent, &none) && source_ok("herdr", &herdr, &none));
        assert!(source_ok("agents", &agent, &none) && !source_ok("agents", &herdr, &none));
        // both: whichever came first; the other is the same news
        let mut seen = VecDeque::new();
        seen.push_back(("b31".to_string(), "temper".to_string(), Kind::Blocked, 1));
        assert!(!source_ok("both", &agent, &seen));
        let mut later = agent.clone();
        later.t = 100;
        assert!(source_ok("both", &later, &seen));
        // a machine down isn't something an agent says: always
        assert!(source_ok("agents", &ev(Kind::Error, "b31", ""), &none));
    }

    #[test]
    fn a_finished_turn_in_one_alert() {
        let mut herdr = ev(Kind::Done, "b31", "claude");
        herdr.pane = "w1:p2".into();
        herdr.seq = 7;
        herdr.took = 340;
        let mut agent = ev(Kind::Done, "b31", "claude");
        agent.source = "agent".into();
        agent.name.clear(); // its pane wasn't known yet
        agent.pane = "w1:p2".into();
        agent.t = 4;
        agent.title = "Fixed the flaky test.".into();
        assert!(
            crate::events::same_news(&herdr, &agent) && crate::events::same_news(&agent, &herdr)
        );
        let one = crate::events::pair(herdr.clone(), agent.clone());
        assert_eq!(
            (one.title.as_str(), one.name.as_str(), one.took, one.seq),
            ("Fixed the flaky test.", "temper", 340, 7)
        );
        assert_eq!(crate::events::pair(agent.clone(), herdr.clone()), one);
        // two of herdr's, another machine, or long apart: not a pair
        assert!(!crate::events::same_news(&herdr, &herdr));
        let mut far = agent.clone();
        far.t = 100;
        assert!(!crate::events::same_news(&herdr, &far));
        // hooked only where hooks are on, for agents that report a finished turn
        let cfg = Config::in_memory(&["host.b31.hooks=on"]);
        assert!(hooked(&cfg, &herdr) && !hooked(&cfg, &ev(Kind::Done, "b31", "cursor")));
        assert!(!hooked(&cfg, &ev(Kind::Done, "gpu2", "claude")));
    }

    #[test]
    fn a_question_answered_at_the_terminal_never_pages() {
        // waiting since 100, a 15 s grace
        assert_eq!(question_due(Some(100), 105, 15), Due::Wait);
        assert_eq!(question_due(Some(100), 115, 15), Due::Send);
        // answered (or another question took its place): gone
        assert_eq!(question_due(None, 200, 15), Due::Drop);
        // no grace: at once
        assert_eq!(question_due(Some(100), 100, 0), Due::Send);
    }

    #[test]
    fn short_turns_stay_off_the_phone() {
        let r = vec![Route {
            to: vec![To::Ntfy, To::Raycast, To::Desktop],
            ..Default::default()
        }];
        let e = ev(Kind::Done, "b31", "claude");
        let short = Hold {
            short: true,
            ..Default::default()
        };
        assert_eq!(decide(&r, &e, short, &[]), vec![To::Raycast, To::Desktop]);
        assert_eq!(decide(&r, &e, Hold::default(), &[]).len(), 3);
    }

    #[test]
    fn text_policy() {
        let mut e = ev(Kind::Blocked, "b31", "claude");
        e.body = "x".repeat(400);
        assert!(trim_text(&e, "off").title.is_empty());
        assert!(trim_text(&e, "summary").body.chars().count() <= 141);
        assert_eq!(trim_text(&e, "full").body.len(), 400);
    }
}
