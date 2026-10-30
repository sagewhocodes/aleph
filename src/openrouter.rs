//! OpenRouter (openrouter.ai): one API key for hundreds of models.
//!
//! * **The key**: `openrouter.key`, by default `secret:openrouter` — the
//!   keystore (`aleph secret set openrouter`, or setup's Agents step, which
//!   stores it there at once). [`key`] reads it (cached a minute; the
//!   Keychain is a subprocess, so never on the UI thread). Every value read
//!   is masked in whatever this process writes down (`log::mask`).
//! * **Agents**: sessions Aleph starts get it as [`ENV`] when their agent is
//!   in `openrouter.agents` ([`wants`]) — only in the new pane's own
//!   environment, through herdr's socket (`layout.apply`'s pane `env`, see
//!   `herdr.rs`), so it is never on a command line, never on disk (herdr
//!   doesn't keep a pane's launch environment in its session file), never
//!   in a log. Where only herdr's CLI could start the pane, the key is left
//!   out and the start says so ([`not_passed`]).
//! * **Spend**: `GET /api/v1/key` (usage, limit, today / this week / this
//!   month where OpenRouter has them) and `/api/v1/credits` (credits bought
//!   and used), the key as a bearer header in curl's config on its stdin
//!   (`-K -`), never in argv. Numbers only are kept, in
//!   `cache_dir()/openrouter/spend.json`; read again every [`FRESH`] s, stale
//!   after [`STALE`] s. [`Watch`] does it off the UI thread for the
//!   dashboard (the header, Monitor › Usage); `aleph usage` asks [`spend`].
//! * **Models**: `GET /api/v1/models` (public, no key), kept a day in
//!   `cache_dir()/openrouter/models.json`; [`MODEL_ARGS`] says how each agent
//!   takes one at launch (`opencode --model openrouter/<id>`, …) — the
//!   new-session form's picker and `aleph new --openrouter <id>`.
//!
//! JSON is read defensively: objects walked, fields picked by name, numbers
//! that come as strings read too.

use crate::config::Config;
use serde_json::{json, Map, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The keystore name `openrouter.key` points at by default.
pub const SECRET: &str = "openrouter";
/// The variable agents read the key from.
pub const ENV: &str = "OPENROUTER_API_KEY";
const BASE: &str = "https://openrouter.ai/api/v1";
/// Spend is read again after this long (seconds).
pub const FRESH: u64 = 5 * 60;
/// …and called stale after this long.
pub const STALE: u64 = 15 * 60;
/// The model list is read again after a day.
pub const MODELS_FRESH: u64 = 86_400;
/// One request's deadline (curl's own `-m`, and ours a little after).
const T_REQ: u64 = 10;

/// How each agent that reads [`ENV`] by itself takes one of OpenRouter's
/// models at launch; `{id}` is OpenRouter's model id (`qwen/qwen3-coder`).
/// From each one's docs (2026-10): OpenCode and Kilo (its fork) take
/// `provider/model` (models.dev's OpenRouter entry reads the variable); Pi
/// `--provider openrouter --model <id>`; OMP `openrouter/<id>`. Crush,
/// Hermes and Maki read the key too, but their interactive start takes no
/// model (Crush's TUI has no `--model`; Hermes's is `hermes chat -m`):
/// the key, no picker. Cline only takes a key on its command line, Qwen
/// Code wants it as OPENAI_API_KEY, Codex and Claude Code need config of
/// their own: none of them get it.
pub const MODEL_ARGS: &[(&str, &[&str])] = &[
    ("opencode", &["--model", "openrouter/{id}"]),
    ("kilo", &["--model", "openrouter/{id}"]),
    ("pi", &["--provider", "openrouter", "--model", "{id}"]),
    ("omp", &["--model", "openrouter/{id}"]),
];

/// `openrouter.agents`' default: the agents that read [`ENV`] by themselves.
pub const DEFAULT_AGENTS: &str = "opencode,kilo,pi,omp,crush,hermes,maki";

// ------------------------------------------------------------------ the key

/// `openrouter.key` as written (`secret:openrouter` unless set otherwise;
/// empty or `off`: none).
fn raw(cfg: &Config) -> String {
    cfg.get_raw("openrouter.key")
        .unwrap_or_else(|| format!("secret:{SECRET}"))
        .trim()
        .to_string()
}

/// Is a key set up at all (without reading it)? `off` / empty: no.
pub fn configured(cfg: &Config) -> bool {
    let r = raw(cfg);
    !(r.is_empty() || r == "off")
}

/// A key that's safe to hand on: one line, no quotes, nothing odd.
fn usable(k: &str) -> bool {
    !k.is_empty()
        && k.len() < 512
        && k.chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\' && c != '\'')
}

/// The key, when `openrouter.key` resolves to one (None: none stored, or
/// `off`). Cached a minute in this process; never call it on the UI thread
/// (the Keychain is a subprocess).
pub fn key(cfg: &Config) -> Option<String> {
    // the demo world has no account
    if crate::showcase::demo() {
        return None;
    }
    let r = raw(cfg);
    if let Ok(g) = KEY.lock() {
        if let Some((at, was, v)) = g.as_ref() {
            if *was == r && at.elapsed() < Duration::from_secs(60) {
                return v.clone();
            }
        }
    }
    let v = if r.is_empty() || r == "off" {
        None
    } else if let Some(name) = r.strip_prefix("secret:") {
        // no warning when it isn't stored: most people have no OpenRouter key
        crate::secrets::get(name)
    } else {
        crate::log::mask(&r);
        Some(r.clone())
    };
    let v = v.map(|k| k.trim().to_string()).filter(|k| usable(k));
    if let Ok(mut g) = KEY.lock() {
        *g = Some((Instant::now(), r, v.clone()));
    }
    v
}

/// [`key`]'s cache: (when, `openrouter.key` as written then, the key).
static KEY: Mutex<Option<(Instant, String, Option<String>)>> = Mutex::new(None);

/// Read the key again next time (setup just stored one).
pub fn forget() {
    if let Ok(mut g) = KEY.lock() {
        *g = None;
    }
}

/// Does `agent` get the key (`openrouter.agents`: agent ids, `all`, or
/// `off`)? Settings only — the key itself isn't read.
pub fn wants(cfg: &Config, agent: &str) -> bool {
    let list = cfg
        .get("openrouter.agents")
        .unwrap_or_else(|| DEFAULT_AGENTS.to_string());
    wants_in(&list, agent)
}

/// [`wants`] for a list as written (pure — tested).
pub fn wants_in(list: &str, agent: &str) -> bool {
    if agent.is_empty() || agent == crate::agents::SHELL {
        return false;
    }
    let mut on = false;
    for w in list.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        match w {
            "off" | "none" => return false,
            "all" => on = true,
            w => {
                let (neg, w) = match w.strip_prefix('-') {
                    Some(x) => (true, x),
                    None => (false, w),
                };
                let id = crate::agents::find(w).map_or(w, |a| a.id);
                if id == agent {
                    on = !neg;
                }
            }
        }
    }
    on
}

/// The key for a session of `agent`, when it should get one and there is one
/// (`herdr.rs` asks per tab: [`wants`] for each pane, then [`key`] once).
#[cfg(test)]
pub fn for_agent(cfg: &Config, agent: &str) -> Option<String> {
    if !wants(cfg, agent) || !configured(cfg) {
        return None;
    }
    key(cfg)
}

/// What a start says when the key couldn't go along (herdr's CLI would
/// have put it on a command line).
pub fn not_passed(host: &str, why: &str) -> String {
    format!(
        "{ENV} isn't in this session: herdr's socket on {host} {why}, and its command line would show the key"
    )
}

// ------------------------------------------------------------------ requests

/// curl's config for one GET (pure — tested): the URL, and the bearer header
/// when there's a key. It goes to curl on its stdin (`-K -`), never argv.
pub(crate) fn curl_config(url: &str, key: Option<&str>) -> Option<String> {
    if url.contains(['"', '\n', '\\']) {
        return None;
    }
    let mut s = format!("url = \"{url}\"\n");
    if let Some(k) = key {
        if !usable(k) {
            return None;
        }
        s.push_str(&format!("header = \"Authorization: Bearer {k}\"\n"));
    }
    Some(s)
}

/// curl's arguments: quiet but with errors, a deadline, the status on a line
/// of its own after the body, the rest from stdin. Nothing secret.
pub(crate) fn curl_args() -> Vec<String> {
    [
        "-sS",
        "-m",
        &T_REQ.to_string(),
        "-H",
        "Accept: application/json",
        "-w",
        "\\n%{http_code}",
        "-K",
        "-",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// The body and status curl printed (`-w '\n%{http_code}'`).
pub(crate) fn split_status(out: &str) -> (&str, u16) {
    match out.rsplit_once('\n') {
        Some((body, code)) => (body, code.trim().parse().unwrap_or(0)),
        None => ("", out.trim().parse().unwrap_or(0)),
    }
}

/// What went wrong with an answer, in words (the key never in them).
fn refusal(code: u16, body: &str) -> String {
    let said = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(String::from)
        })
        .map(|m| crate::sentlog::scrub(&m));
    let what = match code {
        401 => "OpenRouter refused the key (401) — aleph secret set openrouter".to_string(),
        403 => "OpenRouter says this key can't read that (403)".to_string(),
        429 => "OpenRouter is rate-limiting (429)".to_string(),
        0 => "no answer from OpenRouter".to_string(),
        c => format!("OpenRouter answered {c}"),
    };
    match said {
        Some(m) if !m.is_empty() && code != 401 => format!("{what}: {}", crate::ui::trunc(&m, 120)),
        _ => what,
    }
}

/// One GET to OpenRouter's API (`path` after `/api/v1`), its JSON.
fn get(path: &str, key: Option<&str>) -> Result<Value, String> {
    // showcase's demo world reaches no network; nor do unit tests (the fake
    // world's curl answers for the e2e checks)
    if let Some(e) = crate::showcase::refused() {
        return Err(e);
    }
    if cfg!(test) {
        return Err("no network in unit tests".into());
    }
    let cfg = curl_config(&format!("{BASE}{path}"), key)
        .ok_or("the OpenRouter key has characters it can't have")?;
    let o = crate::proc::run(
        Command::new("curl").args(curl_args()),
        Duration::from_secs(T_REQ + 3),
        Some(cfg.as_bytes()),
    )
    .map_err(|e| format!("curl: {e}"))?;
    if o.timed_out {
        return Err("OpenRouter didn't answer in time".into());
    }
    let text = o.text();
    let (body, code) = split_status(&text);
    if !o.ok() && code == 0 {
        return Err(crate::sentlog::scrub(&format!(
            "can't reach OpenRouter: {}",
            o.reason()
        )));
    }
    if code != 200 {
        return Err(refusal(code, body));
    }
    serde_json::from_str(body).map_err(|_| "OpenRouter's answer isn't JSON".to_string())
}

// ------------------------------------------------------------------ JSON

/// Every object in `v`, depth first.
fn objects<'a>(v: &'a Value, out: &mut Vec<&'a Map<String, Value>>) {
    match v {
        Value::Object(m) => {
            out.push(m);
            for x in m.values() {
                objects(x, out);
            }
        }
        Value::Array(a) => a.iter().for_each(|x| objects(x, out)),
        _ => {}
    }
}

/// A number by any of `keys` (also one sent as a string).
fn num(m: &Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| match m.get(*k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    })
}

fn text(m: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| m.get(*k).and_then(Value::as_str).map(String::from))
}

// ------------------------------------------------------------------ spend

/// The account's spend, as OpenRouter reports it ($).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Spend {
    /// spent with this key, ever
    pub usage: Option<f64>,
    pub today: Option<f64>,
    pub week: Option<f64>,
    pub month: Option<f64>,
    /// the key's own spending limit, and what's left of it
    pub limit: Option<f64>,
    pub limit_left: Option<f64>,
    /// the limit's period (`daily`, `weekly`, `monthly`), when it resets
    pub limit_reset: Option<String>,
    pub free_tier: Option<bool>,
    /// credits bought, and used, on the account
    pub credits: Option<f64>,
    pub credits_used: Option<f64>,
    /// the key's label at OpenRouter (left out when it's a piece of the key)
    pub label: Option<String>,
    /// when these numbers were read (unix seconds)
    pub at: u64,
    /// the last read that failed (the numbers are the last good ones)
    pub error: Option<String>,
}

impl Spend {
    /// Credits left on the account.
    pub fn left(&self) -> Option<f64> {
        Some((self.credits? - self.credits_used.unwrap_or(0.0)).max(0.0))
    }
    /// Is it older than [`STALE`]?
    pub fn stale(&self, now: u64) -> bool {
        now.saturating_sub(self.at) > STALE
    }
    /// Anything to show?
    pub fn any(&self) -> bool {
        self.usage.is_some() || self.credits.is_some() || self.today.is_some()
    }
    /// How low the money left is: (left, of what) — the key's limit when it
    /// has one, else the account's credits.
    pub fn headroom(&self) -> Option<(f64, f64)> {
        match (self.limit, self.limit_left) {
            (Some(l), Some(left)) if l > 0.0 => Some((left.max(0.0), l)),
            _ => Some((self.left()?, self.credits?)),
        }
    }

    pub fn to_json(&self, now: u64) -> Value {
        let r = |v: Option<f64>| v.map(|x| (x * 10_000.0).round() / 10_000.0);
        json!({
            "spent": {"today": r(self.today), "week": r(self.week), "month": r(self.month), "total": r(self.usage)},
            "limit": r(self.limit),
            "limit_remaining": r(self.limit_left),
            "limit_reset": self.limit_reset,
            "free_tier": self.free_tier,
            "credits": {"total": r(self.credits), "used": r(self.credits_used), "left": r(self.left())},
            "label": self.label,
            "at": self.at,
            "age": now.saturating_sub(self.at),
            "stale": self.stale(now),
            "error": self.error,
        })
    }

    /// [`to_json`](Self::to_json) for output (`aleph usage --json`): showcase
    /// mode's money rules (scaled, or 0 when hidden) apply; the kept file
    /// never has them.
    pub fn shown_json(&self, now: u64) -> Value {
        let m = |v: Option<f64>| v.map(|x| crate::showcase::money(x).unwrap_or(0.0));
        let s = Spend {
            usage: m(self.usage),
            today: m(self.today),
            week: m(self.week),
            month: m(self.month),
            limit: m(self.limit),
            limit_left: m(self.limit_left),
            credits: m(self.credits),
            credits_used: m(self.credits_used),
            ..self.clone()
        };
        s.to_json(now)
    }

    fn from_json(v: &Value) -> Option<Spend> {
        let f = |p: &str| v.pointer(p).and_then(Value::as_f64);
        Some(Spend {
            usage: f("/spent/total"),
            today: f("/spent/today"),
            week: f("/spent/week"),
            month: f("/spent/month"),
            limit: f("/limit"),
            limit_left: f("/limit_remaining"),
            limit_reset: v
                .get("limit_reset")
                .and_then(Value::as_str)
                .map(String::from),
            free_tier: v.get("free_tier").and_then(Value::as_bool),
            credits: f("/credits/total"),
            credits_used: f("/credits/used"),
            label: v.get("label").and_then(Value::as_str).map(String::from),
            at: v.get("at").and_then(Value::as_u64)?,
            error: v.get("error").and_then(Value::as_str).map(String::from),
        })
    }
}

/// `GET /key`'s answer into `s` (pure — tested): the object that has `usage`
/// or `limit` in it, wherever it's nested (`data`).
pub fn parse_key(v: &Value, s: &mut Spend) {
    let mut objs = Vec::new();
    objects(v, &mut objs);
    let Some(m) = objs
        .iter()
        .find(|m| m.contains_key("usage") || m.contains_key("limit"))
    else {
        return;
    };
    s.usage = num(m, &["usage"]);
    s.today = num(m, &["usage_daily", "usage_today"]);
    s.week = num(m, &["usage_weekly", "usage_week"]);
    s.month = num(m, &["usage_monthly", "usage_month"]);
    s.limit = num(m, &["limit"]);
    s.limit_left = num(m, &["limit_remaining"]);
    s.limit_reset = text(m, &["limit_reset"]);
    s.free_tier = m.get("is_free_tier").and_then(Value::as_bool);
    // OpenRouter's default label is a piece of the key itself: not kept
    s.label = text(m, &["label", "name"]).filter(|l| !l.contains("sk-") && !l.is_empty());
}

/// `GET /credits`' answer into `s` (pure — tested).
pub fn parse_credits(v: &Value, s: &mut Spend) {
    let mut objs = Vec::new();
    objects(v, &mut objs);
    if let Some(m) = objs.iter().find(|m| m.contains_key("total_credits")) {
        s.credits = num(m, &["total_credits"]);
        s.credits_used = num(m, &["total_usage"]);
    }
}

fn dir() -> PathBuf {
    crate::config::cache_dir().join("openrouter")
}

fn spend_file() -> PathBuf {
    dir().join("spend.json")
}

/// The spend as last read (None: never).
pub fn cached() -> Option<Spend> {
    let t = std::fs::read_to_string(spend_file()).ok()?;
    Spend::from_json(&serde_json::from_str(&t).ok()?)
}

fn keep(s: &Spend) {
    let _ = crate::config::write_atomic(&spend_file(), s.to_json(s.at).to_string().as_bytes());
}

/// Read the spend now (two requests, each ≤ 10 s). `/credits` may be refused
/// for a key without that right: the key's own numbers still count.
pub fn fetch_with(key: &str) -> Result<Spend, String> {
    let mut s = Spend {
        at: crate::cache::now(),
        ..Default::default()
    };
    let k = get("/key", Some(key))?;
    parse_key(&k, &mut s);
    match get("/credits", Some(key)) {
        Ok(c) => parse_credits(&c, &mut s),
        Err(e) => crate::log::debug("openrouter", &format!("credits: {e}")),
    }
    Ok(s)
}

/// Read it now and keep it; a failure keeps the last good numbers with the
/// error beside them. None: there's no key.
pub fn refresh(cfg: &Config) -> Option<Spend> {
    let k = key(cfg)?;
    Some(match fetch_with(&k) {
        Ok(s) => {
            keep(&s);
            s
        }
        Err(e) => {
            crate::log::info("openrouter", &format!("spend: {e}"));
            let mut s = cached().unwrap_or_default();
            s.error = Some(e);
            s
        }
    })
}

/// The spend for `aleph usage`: what's kept when it's under `fresh` seconds
/// old, else read now. None: no key (or `openrouter.key = off`).
pub fn spend(cfg: &Config, fresh: u64) -> Option<Spend> {
    if !configured(cfg) || crate::showcase::demo() {
        return None;
    }
    if let Some(s) = cached() {
        if crate::cache::now().saturating_sub(s.at) < fresh && key(cfg).is_some() {
            return Some(s);
        }
    }
    refresh(cfg)
}

/// The dashboard's spend, read off the UI thread every [`FRESH`] s
/// (generation-counted; the kept numbers show at once, marked stale by
/// their age).
pub struct Watch {
    pub spend: Option<Spend>,
    /// None: not known yet
    pub has_key: Option<bool>,
    gen: u64,
    at: Option<Instant>,
    rx: Option<Receiver<(u64, Option<Spend>)>>,
    pub busy: bool,
    /// the next read asks OpenRouter even when what's kept is fresh
    force: bool,
}

impl Default for Watch {
    fn default() -> Self {
        Watch {
            spend: cached(),
            has_key: None,
            gen: 0,
            at: None,
            rx: None,
            busy: false,
            force: false,
        }
    }
}

impl Watch {
    /// Call every frame: take a finished read; start one when it's due.
    pub fn pump(&mut self, cfg: &Config) {
        if let Some(rx) = &self.rx {
            if let Ok((g, r)) = rx.try_recv() {
                if g == self.gen {
                    self.busy = false;
                    self.has_key = Some(r.is_some());
                    // (a failed first read: no numbers, the error to show)
                    self.spend = r;
                }
            }
        }
        if !configured(cfg) || crate::showcase::demo() {
            self.has_key = Some(false);
            self.spend = None;
            return;
        }
        let due = self
            .at
            .is_none_or(|t| t.elapsed() > Duration::from_secs(FRESH));
        if !due || self.busy {
            return;
        }
        self.gen += 1;
        self.busy = true;
        self.at = Some(Instant::now());
        let (tx, rx) = channel();
        self.rx = Some(rx);
        let (cfg, g, force) = (cfg.clone(), self.gen, std::mem::take(&mut self.force));
        std::thread::spawn(move || {
            // what's kept is fresh enough: no request (another dashboard, `aleph usage`)
            let r = match (cached(), key(&cfg)) {
                (_, None) => None,
                (Some(s), Some(_))
                    if !force && crate::cache::now().saturating_sub(s.at) < FRESH =>
                {
                    Some(s)
                }
                _ => refresh(&cfg),
            };
            let _ = tx.send((g, r));
        });
    }
    /// Read again now, from OpenRouter itself (the key read again too).
    pub fn again(&mut self) {
        forget();
        self.at = None;
        self.busy = false;
        self.force = true;
        self.gen += 1;
    }
    /// The spend to show (there's a key, as far as we know).
    pub fn shown(&self) -> Option<&Spend> {
        if self.has_key == Some(false) {
            return None;
        }
        self.spend.as_ref().filter(|s| s.any())
    }
}

// ------------------------------------------------------------------ models

/// One of OpenRouter's models.
#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    /// OpenRouter's id: `qwen/qwen3-coder`
    pub id: String,
    pub name: String,
    /// context length (tokens)
    pub context: u64,
    /// $ per million tokens in and out (None: variable, or not said)
    pub input: Option<f64>,
    pub output: Option<f64>,
    /// $ per million cached input tokens read (None: not said)
    pub cache_read: Option<f64>,
}

impl Model {
    /// `256k ctx · $0.22/$0.95` (what the picker shows beside it).
    pub fn facts(&self) -> String {
        let ctx = match self.context {
            0 => String::new(),
            n if n >= 1_000_000 => format!("{}M ctx", trim(n as f64 / 1e6)),
            n if n >= 1000 => format!("{}k ctx", n / 1000),
            n => format!("{n} ctx"),
        };
        let price = match (self.input, self.output) {
            (Some(i), Some(o)) if i == 0.0 && o == 0.0 => "free".to_string(),
            (Some(i), Some(o)) => format!("${}/${} per M", per(i), per(o)),
            _ => String::new(),
        };
        match (ctx.is_empty(), price.is_empty()) {
            (false, false) => format!("{ctx} · {price}"),
            (false, true) => ctx,
            _ => price,
        }
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.1}");
    s.trim_end_matches(".0").to_string()
}

fn per(v: f64) -> String {
    if v >= 10.0 {
        format!("{v:.0}")
    } else if v >= 0.1 {
        format!("{v:.2}")
    } else {
        format!("{v:.3}")
    }
}

/// $/token as OpenRouter writes it (`"0.0000003"`) → $/MTok; a negative
/// one means "variable" (its router models).
fn per_mtok(v: Option<&Value>) -> Option<f64> {
    let x = match v? {
        Value::Number(n) => n.as_f64()?,
        Value::String(s) => s.trim().parse().ok()?,
        _ => return None,
    };
    (x >= 0.0).then(|| (x * 1e6 * 10_000.0).round() / 10_000.0)
}

/// `GET /models`' answer (pure — tested): every entry with an id, in
/// OpenRouter's order.
pub fn parse_models(v: &Value) -> Vec<Model> {
    let list = v
        .get("data")
        .or_else(|| v.get("models"))
        .unwrap_or(v)
        .as_array()
        .cloned()
        .unwrap_or_default();
    list.iter()
        .filter_map(|m| {
            let m = m.as_object()?;
            let id = text(m, &["id", "slug"])?;
            if id.is_empty() || id.contains(char::is_whitespace) {
                return None;
            }
            let pricing = m.get("pricing").and_then(Value::as_object);
            Some(Model {
                name: text(m, &["name"]).unwrap_or_else(|| id.clone()),
                context: num(m, &["context_length"])
                    .or_else(|| {
                        m.get("top_provider")
                            .and_then(Value::as_object)
                            .and_then(|t| num(t, &["context_length"]))
                    })
                    .unwrap_or(0.0) as u64,
                input: pricing.and_then(|p| per_mtok(p.get("prompt"))),
                output: pricing.and_then(|p| per_mtok(p.get("completion"))),
                cache_read: pricing.and_then(|p| per_mtok(p.get("input_cache_read"))),
                id,
            })
        })
        .collect()
}

fn models_file() -> PathBuf {
    dir().join("models.json")
}

fn models_json(at: u64, v: &[Model]) -> Value {
    json!({
        "at": at,
        "data": v.iter().map(|m| json!({
            "id": m.id, "name": m.name, "context_length": m.context,
            // kept as OpenRouter writes them ($ per token)
            "pricing": {
                "prompt": m.input.map(|x| x / 1e6),
                "completion": m.output.map(|x| x / 1e6),
                "input_cache_read": m.cache_read.map(|x| x / 1e6),
            },
        })).collect::<Vec<_>>(),
    })
}

/// The kept model list and when it was read.
pub fn models_cached() -> Option<(u64, Vec<Model>)> {
    let t = std::fs::read_to_string(models_file()).ok()?;
    let v: Value = serde_json::from_str(&t).ok()?;
    let at = v.get("at").and_then(Value::as_u64)?;
    Some((at, parse_models(&v)))
}

/// OpenRouter's models: the kept list when it's under a day old, else read
/// now (no key needed) and kept; a failed read falls back to an older list.
pub fn models() -> Result<Vec<Model>, String> {
    let kept = models_cached();
    if let Some((at, v)) = &kept {
        if crate::cache::now().saturating_sub(*at) < MODELS_FRESH && !v.is_empty() {
            return Ok(v.clone());
        }
    }
    match get("/models", None).map(|v| parse_models(&v)) {
        Ok(v) if !v.is_empty() => {
            let _ = crate::config::write_atomic(
                &models_file(),
                models_json(crate::cache::now(), &v).to_string().as_bytes(),
            );
            Ok(v)
        }
        Ok(_) => kept
            .map(|k| k.1)
            .ok_or_else(|| "OpenRouter listed no models".into()),
        Err(e) => match kept {
            Some((_, v)) if !v.is_empty() => Ok(v),
            _ => Err(e),
        },
    }
}

/// OpenRouter's paid models as a price list, from the kept model list (no
/// request: it's read daily only with a key, `refresh_catalog`): (id, $/MTok
/// in, out, cached read) and when it was read. Free ones are left out (their
/// requests are marked free where they're read); kept in memory until the
/// file changes.
pub fn price_catalog() -> (u64, Arc<Vec<Priced>>) {
    type Cat = (std::time::SystemTime, u64, Arc<Vec<Priced>>);
    static KEPT: std::sync::Mutex<Option<Cat>> = std::sync::Mutex::new(None);
    let Ok(mt) = std::fs::metadata(models_file()).and_then(|m| m.modified()) else {
        return (0, Arc::new(Vec::new()));
    };
    let mut k = KEPT.lock().unwrap();
    if let Some((t, at, v)) = k.as_ref() {
        if *t == mt {
            return (*at, v.clone());
        }
    }
    let (at, list) = models_cached().unwrap_or_default();
    let v = Arc::new(paid(list));
    *k = Some((mt, at, v.clone()));
    (at, v)
}

/// A paid model's price: its id, $/MTok in and out, and for cached input read.
pub type Priced = (String, f64, f64, Option<f64>);

/// The models with a price (free and variable ones left out) — pure.
pub fn paid(list: Vec<Model>) -> Vec<Priced> {
    list.into_iter()
        .filter_map(|m| match (m.input, m.output) {
            (Some(i), Some(o)) if i > 0.0 || o > 0.0 => Some((m.id, i, o, m.cache_read)),
            _ => None,
        })
        .collect()
}

/// Free: OpenRouter charges nothing for it (a `:free` variant, or $0 both ways).
pub fn is_free(m: &Model) -> bool {
    m.id.ends_with(":free") || (m.input == Some(0.0) && m.output == Some(0.0))
}

/// OpenCode's own config for `openrouter.models` (pure): only the free ones
/// (a whitelist), or none of them (a blacklist); None for all, or with no
/// list to name them from.
pub fn opencode_config_for(choice: &str, models: &[Model]) -> Option<String> {
    let free: Vec<&str> = models
        .iter()
        .filter(|m| is_free(m))
        .map(|m| m.id.as_str())
        .collect();
    let key = match choice {
        "free" => "whitelist",
        "paid" => "blacklist",
        _ => return None,
    };
    if free.is_empty() {
        return None;
    }
    Some(json!({"provider": {"openrouter": {key: free}}}).to_string())
}

/// [`opencode_config_for`] with the setting and the kept list (read now when
/// there's none: a new session's thread, never the UI's).
pub fn opencode_config(cfg: &Config) -> Option<String> {
    let choice = cfg.get_or("openrouter.models");
    if choice == "all" {
        return None;
    }
    let list = match models_cached() {
        Some((_, v)) if !v.is_empty() => v,
        _ => models().ok()?,
    };
    opencode_config_for(&choice, &list)
}

/// The daemon's daily read of OpenRouter's model list (prices for models the
/// agents don't price themselves) — only with a key set: without one, Aleph
/// asks OpenRouter nothing.
pub fn refresh_catalog(cfg: &Config) {
    // a key stored, not just the setting's default pointing at the keystore
    // (off the UI thread: the daemon's usage round)
    if !configured(cfg) || key(cfg).is_none() {
        return;
    }
    let fresh = models_cached().is_some_and(|(at, v)| {
        !v.is_empty() && crate::cache::now().saturating_sub(at) < MODELS_FRESH
    });
    if !fresh {
        let _ = models();
    }
}

/// The models matching `q`, best first (fuzzy, on the id and the name);
/// all of them in OpenRouter's order for an empty `q`.
pub fn search<'a>(models: &'a [Model], q: &str) -> Vec<&'a Model> {
    let q = q.trim();
    if q.is_empty() {
        return models.iter().collect();
    }
    let mut v: Vec<(i32, usize, &Model)> = models
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let a = crate::fuzzy::score(q, &m.id).map(|x| x.0);
            let b = crate::fuzzy::score(q, &m.name).map(|x| x.0 - 5);
            a.max(b).map(|s| (s, i, m))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    v.into_iter().map(|x| x.2).collect()
}

/// The model list for a form, read once off the UI thread.
#[derive(Clone, Default)]
pub struct ModelList {
    inner: Arc<Mutex<ListState>>,
}

#[derive(Default)]
struct ListState {
    started: bool,
    models: Option<Result<Arc<Vec<Model>>, String>>,
}

impl ModelList {
    /// A list already in hand (tests).
    #[cfg(test)]
    pub fn with(models: Vec<Model>) -> ModelList {
        let l = ModelList::default();
        if let Ok(mut g) = l.inner.lock() {
            g.started = true;
            g.models = Some(Ok(Arc::new(models)));
        }
        l
    }
    /// Start reading it (once, off the calling thread): what's kept shows
    /// as soon as it's read, even when it's old; then OpenRouter's own when
    /// that's due.
    pub fn ensure(&self) {
        let Ok(mut g) = self.inner.lock() else { return };
        if g.started {
            return;
        }
        g.started = true;
        drop(g);
        let inner = self.inner.clone();
        std::thread::spawn(move || {
            if let Some((_, v)) = models_cached().filter(|x| !x.1.is_empty()) {
                if let Ok(mut g) = inner.lock() {
                    g.models = Some(Ok(Arc::new(v)));
                }
            }
            let r = models().map(Arc::new);
            if let Ok(mut g) = inner.lock() {
                let had = matches!(g.models, Some(Ok(_)));
                if r.is_ok() || !had {
                    g.models = Some(r);
                }
            }
        });
    }
    /// None: still reading.
    pub fn get(&self) -> Option<Result<Arc<Vec<Model>>, String>> {
        self.inner.lock().ok()?.models.clone()
    }
}

// ------------------------------------------------------------------ models → agents

/// Can `agent` take an OpenRouter model at launch?
pub fn takes_model(agent: &str) -> bool {
    MODEL_ARGS
        .iter()
        .any(|(a, args)| *a == agent && !args.is_empty())
}

/// The arguments that start `agent` on OpenRouter's model `id` (pure —
/// tested); None when it can't take one that way.
pub fn model_args(agent: &str, id: &str) -> Option<Vec<String>> {
    let id = id.trim().trim_start_matches("openrouter/");
    if id.is_empty() || id.contains(char::is_whitespace) {
        return None;
    }
    let (_, args) = MODEL_ARGS
        .iter()
        .find(|(a, args)| *a == agent && !args.is_empty())?;
    Some(args.iter().map(|a| a.replace("{id}", id)).collect())
}

/// The agents that can, by name (for messages).
/// The agents that can, by their short names (`OpenCode`, `Kilo`, `OMP`).
pub fn model_agents() -> Vec<String> {
    MODEL_ARGS
        .iter()
        .filter(|(_, a)| !a.is_empty())
        .map(|(a, _)| {
            let l = crate::agents::label(a);
            l.split([' ', '(']).next().unwrap_or(&l).to_string()
        })
        .collect()
}

#[cfg(test)]
mod tests;
