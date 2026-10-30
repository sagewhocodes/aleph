//! Token, time and model analytics from the agents' own local logs.
//!
//! Nothing sits between you and the agents (no proxies) and nothing in them
//! changes: once in a while a plain `sh` + awk script ([`extract`]) runs on
//! each machine, reads only what the logs gained since last time and prints
//! ids, timestamps, model/effort, token counts, cost and the folder — never
//! conversation text. Records land in a local store ([`store`]); [`query`]
//! sums them by machine, agent, model, effort, folder, day or session, with a
//! cost that is either what the agent reported or an estimate from
//! [`crate::prices`].
//!
//! Never on the poll path: [`sync_all`] is called by `aleph usage sync`, by
//! `aleph usage` when the data is stale, and by the daemon ([`Background`]).
//!
//! Settings: `analytics.enabled`, `analytics.include` / `analytics.exclude`
//! (host:agent globs), `analytics.backfill` (how far back the first run
//! reads, by file mtime), `analytics.keep` (retention), `analytics.interval`,
//! `analytics.max_seconds` / `analytics.max_mb` (the per-run budget) and
//! `analytics.prices.<model>`.

pub mod accounts;
pub mod cmd;
pub mod dash;
pub mod explore;
pub mod extract;
pub mod live;
pub mod now;
pub mod parse;
pub mod projects;
pub mod recent;
pub mod steps;
pub mod store;

use crate::config::Config;
use crate::{prices, proc, sshcfg, ui};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// Agents in the registry with no reader, and why (`aleph usage sources`).
pub fn no_reader_reason(agent: &str) -> &'static str {
    match agent {
        "cursor" => "Cursor CLI's local chat store has no token counts",
        "amp" => "Amp keeps its threads and usage on ampcode.com",
        "copilot" => "GitHub Copilot CLI logs no per-request token counts locally",
        "crush" => "Crush keeps per-session totals in a project .crush/crush.db (not read yet)",
        "shell" => "a plain shell",
        _ => "no local usage log we know of",
    }
}

// ------------------------------------------------------------------ settings

/// `90d`, `12w`, `1y`, `30` (days); `all` / `0` = no limit → days (0 = no limit).
pub fn parse_days(v: &str) -> Result<u64, String> {
    let v = v.trim().to_lowercase();
    if v.is_empty() || v == "all" || v == "0" || v == "forever" {
        return Ok(0);
    }
    let (num, unit) = match v.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => (&v[..i], &v[i..]),
        None => (v.as_str(), "d"),
    };
    let n: u64 = num
        .parse()
        .map_err(|_| format!("not a duration: {v} (use 90d, 12w, 1y or all)"))?;
    let k = match unit.trim() {
        "d" | "day" | "days" => 1,
        "w" | "week" | "weeks" => 7,
        "y" | "year" | "years" => 365,
        _ => return Err(format!("not a duration: {v} (use 90d, 12w, 1y or all)")),
    };
    Ok(n * k)
}

pub fn enabled(cfg: &Config) -> bool {
    cfg.flag("analytics.enabled")
}

fn globs(cfg: &Config, key: &str) -> Vec<String> {
    cfg.get(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Does analytics cover `agent` on `host` (`analytics.include` / `exclude`)?
pub fn allowed(cfg: &Config, host: &str, agent: &str) -> bool {
    let inc = globs(cfg, "analytics.include");
    (inc.is_empty() || crate::permit::matches(&inc, host, agent))
        && !crate::permit::matches(&globs(cfg, "analytics.exclude"), host, agent)
}

/// The extractor's knobs for one machine.
pub fn opts_for(cfg: &Config, host: &str) -> extract::Opts {
    extract::Opts {
        agents: extract::READERS
            .iter()
            .filter(|a| allowed(cfg, host, a))
            .map(|a| a.to_string())
            .collect(),
        days: parse_days(&cfg.get_or("analytics.backfill")).unwrap_or(90),
        max_seconds: cfg.num("analytics.max_seconds").max(1),
        max_bytes: cfg.num("analytics.max_mb").max(1) << 20,
        claude_roots: list(cfg, host, "claude_roots"),
        codex_homes: list(cfg, host, "codex_homes"),
    }
}

/// A folder list for a machine: its own (`host.<a>.<field>`), else everyone's (`analytics.<field>`).
fn list(cfg: &Config, host: &str, field: &str) -> Vec<String> {
    cfg.host_get(host, field)
        .or_else(|| cfg.get(&format!("analytics.{field}")))
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Seconds between background syncs.
pub fn interval(cfg: &Config) -> u64 {
    cfg.num("analytics.interval").max(60)
}

// ------------------------------------------------------------------ queries

/// Local days, inclusive; a rolling range (`24h`) also starts at an hour
/// (`since`), and the one before it ends at one (`until`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub from: i64,
    pub to: i64,
    /// unix seconds: only the hours from this one on
    pub since: Option<u64>,
    /// unix seconds: only the hours before this one
    pub until: Option<u64>,
}

/// The periods offered (the Usage view, Raycast, the palette): rolling,
/// then calendar ones. `(word, label)`, the word as `Range::parse` takes it.
pub const PERIODS: &[(&str, &str)] = &[
    ("24h", "24 hours"),
    ("3d", "3 days"),
    ("7d", "7 days"),
    ("30d", "30 days"),
    ("6mo", "6 months"),
    ("all", "all time"),
    ("today", "today"),
    ("this-week", "this week"),
    ("this-month", "this month"),
];

impl Range {
    fn days_from_to(from: i64, to: i64) -> Range {
        Range {
            from,
            to,
            since: None,
            until: None,
        }
    }
    pub fn today() -> Range {
        let d = ui::local_day(crate::cache::now());
        Range::days_from_to(d, d)
    }
    /// The last `n` days, today included.
    pub fn days(n: u64) -> Range {
        let d = ui::local_day(crate::cache::now());
        Range::days_from_to(d - n.max(1) as i64 + 1, d)
    }
    /// The last `n` hours, to the hour (24 h: yesterday's evening and today).
    pub fn hours(n: u64) -> Range {
        let now = crate::cache::now();
        let since = now.saturating_sub(n.max(1) * 3600);
        Range {
            from: ui::local_day(since),
            to: ui::local_day(now),
            since: Some(since),
            until: None,
        }
    }
    pub fn all() -> Range {
        Range::days_from_to(0, ui::local_day(crate::cache::now()))
    }
    /// From a local day to today.
    pub fn since(day: i64) -> Range {
        Range::days_from_to(day, ui::local_day(crate::cache::now()).max(day))
    }
    /// From one local day to another (either way round).
    pub fn between(a: i64, b: i64) -> Range {
        Range::days_from_to(a.min(b), a.max(b))
    }
    /// `today`, `yesterday`, `24h`, `7d`, `4w`, `6mo`, `1y`, `all`, `this-week`
    /// (from Monday), `this-month`, `last-month`, `2026-10-01` (from then on),
    /// `2026-09-01..2026-10-15`, `2026-09-01..`, `..2026-10-15`.
    pub fn parse(w: &str) -> Option<Range> {
        let w = w.trim().to_lowercase();
        let today = ui::local_day(crate::cache::now());
        match w.as_str() {
            "today" => return Some(Range::today()),
            "yesterday" => return Some(Range::days_from_to(today - 1, today - 1)),
            "all" => return Some(Range::all()),
            "week" => return Some(Range::days(7)),
            "month" => return Some(Range::days(30)),
            // 1970-01-01 was a Thursday: Monday is 3 days on
            "this-week" | "thisweek" => {
                return Some(Range::days_from_to(
                    today - (today + 3).rem_euclid(7),
                    today,
                ))
            }
            "this-month" | "thismonth" => {
                let (y, m, _) = ui::civil(today);
                return Some(Range::days_from_to(parse::days_from_civil(y, m, 1), today));
            }
            "last-month" | "lastmonth" => {
                let (y, m, _) = ui::civil(today);
                let first = parse::days_from_civil(y, m, 1);
                let (py, pm, _) = ui::civil(first - 1);
                return Some(Range::days_from_to(
                    parse::days_from_civil(py, pm, 1),
                    first - 1,
                ));
            }
            _ => {}
        }
        if let Some((a, b)) = w.split_once("..") {
            let a = if a.trim().is_empty() {
                Some(0)
            } else {
                parse_date(a)
            };
            let b = if b.trim().is_empty() {
                Some(today)
            } else {
                parse_date(b)
            };
            return Some(Range::between(a?, b?));
        }
        if let Some(d) = parse_date(&w) {
            return Some(Range::since(d));
        }
        if let Some(h) = w.strip_suffix('h') {
            let h: u64 = h.parse().ok().filter(|h| *h > 0)?;
            return Some(Range::hours(h));
        }
        for (suffix, days) in [("months", 30), ("month", 30), ("mo", 30), ("m", 30)] {
            if let Some(n) = w.strip_suffix(suffix).and_then(|n| n.parse::<u64>().ok()) {
                // 6 months: 182 days (a month is about 30.4)
                return (n > 0).then(|| Range::days((n * days * 1014).div_ceil(1000)));
            }
        }
        match parse_days(&w) {
            Ok(0) | Err(_) => None,
            Ok(n) => Some(Range::days(n)),
        }
    }
    pub fn contains(&self, day: i64) -> bool {
        (self.from..=self.to).contains(&day)
    }
    /// Is this local hour (of `day`, at UTC offset `tz`) in it?
    pub fn keeps(&self, day: i64, hour: u8, tz: i64) -> bool {
        if !self.contains(day) {
            return false;
        }
        let start = (day * 86_400 + hour as i64 * 3600 - tz).max(0) as u64;
        self.since.is_none_or(|s| start + 3600 > s) && self.until.is_none_or(|u| start < u)
    }
    /// How many days it spans (a rolling range: its hours, rounded up).
    pub fn len_days(&self) -> i64 {
        match (self.since, self.until) {
            (Some(s), u) => {
                let end = u.unwrap_or_else(crate::cache::now);
                (end.saturating_sub(s).div_ceil(86_400)).max(1) as i64
            }
            _ => self.to - self.from + 1,
        }
    }
    /// The same length just before it: what "▲18 %" compares against.
    pub fn prev(&self) -> Range {
        match self.since {
            Some(s) => {
                let end = self.until.unwrap_or_else(crate::cache::now);
                let len = end.saturating_sub(s);
                let ps = s.saturating_sub(len);
                Range {
                    from: ui::local_day(ps),
                    to: ui::local_day(s),
                    since: Some(ps),
                    until: Some(s),
                }
            }
            None => {
                let n = self.to - self.from + 1;
                Range::days_from_to(self.from - n, self.from - 1)
            }
        }
    }
    pub fn label(&self) -> String {
        let today = ui::local_day(crate::cache::now());
        if let Some(s) = self.since {
            let end = self.until.unwrap_or_else(crate::cache::now);
            let h = end.saturating_sub(s).div_ceil(3600);
            return if self.until.is_some() {
                format!("the {h} hours before")
            } else {
                format!("last {h} hours")
            };
        }
        if self.from == self.to {
            return match today - self.from {
                0 => "today".into(),
                1 => "yesterday".into(),
                _ => day_name(self.from),
            };
        }
        if self.to == today {
            if self.from <= 0 {
                return "all time".into();
            }
            if self.from == today - (today + 3).rem_euclid(7) {
                return "this week".into();
            }
            let (y, m, _) = ui::civil(today);
            if self.from == parse::days_from_civil(y, m, 1) {
                return "this month".into();
            }
            return format!("last {} days", today - self.from + 1);
        }
        format!("{} – {}", day_name(self.from), day_name(self.to))
    }
    /// UTC months that may hold records for these local days.
    pub(crate) fn months(&self) -> Vec<String> {
        let all = store::months();
        let lo = store::month_of(((self.from - 1).max(0) * 86_400) as u64);
        let hi = store::month_of(((self.to + 1).max(0) * 86_400) as u64);
        all.into_iter().filter(|m| *m >= lo && *m <= hi).collect()
    }
}

/// `2026-10-01` → a local day number.
pub fn parse_date(s: &str) -> Option<i64> {
    let p: Vec<&str> = s.trim().split('-').collect();
    if p.len() != 3 || p[0].len() != 4 {
        return None;
    }
    let (y, m, d) = (p[0].parse().ok()?, p[1].parse().ok()?, p[2].parse().ok()?);
    ((1..=12).contains(&m) && (1..=31).contains(&d)).then(|| parse::days_from_civil(y, m, d))
}

/// `2026-10-22`.
pub fn day_name(day: i64) -> String {
    let (y, m, d) = ui::civil(day);
    format!("{y:04}-{m:02}-{d:02}")
}

/// How rows are grouped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Total,
    Host,
    Agent,
    Model,
    Effort,
    Project,
    Day,
    Session,
    Account,
    Hour,
    Folder,
}

impl By {
    pub const ALL: &'static [(&'static str, By)] = &[
        ("host", By::Host),
        ("agent", By::Agent),
        ("account", By::Account),
        ("model", By::Model),
        ("effort", By::Effort),
        ("project", By::Project),
        ("folder", By::Folder),
        ("day", By::Day),
        ("hour", By::Hour),
        ("session", By::Session),
        ("total", By::Total),
    ];
    pub fn parse(s: &str) -> Option<By> {
        let s = match s {
            "machine" | "machines" | "hosts" => "host",
            "agents" => "agent",
            "models" => "model",
            "dir" | "dirs" | "folders" => "folder",
            "projects" | "repo" | "repos" => "project",
            "days" | "daily" => "day",
            "hours" | "hourly" => "hour",
            "sessions" => "session",
            "accounts" | "acct" | "login" => "account",
            other => other,
        };
        By::ALL.iter().find(|(n, _)| *n == s).map(|(_, b)| *b)
    }
    pub fn name(self) -> &'static str {
        By::ALL
            .iter()
            .find(|(_, b)| *b == self)
            .map(|(n, _)| *n)
            .unwrap_or("total")
    }
}

/// Narrow a query: each list keeps what matches any of it (empty: everything).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filter {
    /// machine aliases (`*` globs allowed)
    pub hosts: Vec<String>,
    pub agents: Vec<String>,
    /// accounts' ids (16 hex digits: `accounts::Book::find` gives one; "" the unknown)
    pub accounts: Vec<String>,
    pub models: Vec<String>,
    /// folders (a request in one or under it)
    pub dirs: Vec<String>,
    /// projects: ids, names or remotes (`projects::Index::find`)
    pub projects: Vec<String>,
}

impl Filter {
    /// One machine (`--host`), or everything.
    pub fn host(h: Option<&str>) -> Filter {
        Filter {
            hosts: h.map(|h| vec![h.to_string()]).unwrap_or_default(),
            ..Default::default()
        }
    }
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.agents.is_empty()
            && self.accounts.is_empty()
            && self.models.is_empty()
            && self.dirs.is_empty()
            && self.projects.is_empty()
    }
    /// Does it keep a request of this machine, agent, account, model and folder?
    pub fn keeps_parts(&self, host: &str, agent: &str, acct: &str, model: &str, dir: &str) -> bool {
        let any = |l: &[String], f: &dyn Fn(&str) -> bool| l.is_empty() || l.iter().any(|x| f(x));
        any(&self.hosts, &|p| {
            crate::permit::matches(&[format!("{p}:*")], host, "x")
        }) && any(&self.agents, &|a| a == agent)
            && any(&self.accounts, &|a| a == acct)
            && any(&self.models, &|m| {
                prices::normalize(m) == prices::normalize(model)
            })
            && any(&self.dirs, &|p| {
                dir == p || dir.starts_with(&format!("{p}/"))
            })
    }
    fn keeps(&self, r: &store::Row, pids: &BTreeSet<String>) -> bool {
        self.keeps_parts(&r.host, &r.agent, &r.acct, &r.model, &r.project)
            && (self.projects.is_empty() || pids.contains(&r.pid))
    }
    fn keeps_prompt(&self, p: &store::Prompt, pids: &BTreeSet<String>) -> bool {
        self.keeps_parts(&p.host, &p.agent, &p.acct, &p.model, &p.project)
            && (self.projects.is_empty() || pids.contains(&p.pid))
    }
}

/// What a user typed → a filter: agents by any name (`cc` → `claude`),
/// accounts by id, email or nickname (`unknown`: the unknown one), the rest
/// as they are. An agent or account not known is an error that says why.
pub fn filter_of(
    cfg: &Config,
    hosts: &[String],
    agents: &[String],
    accounts: &[String],
    models: &[String],
    projects: &[String],
) -> Result<Filter, String> {
    // a path is a folder; anything else a project (its name, id or remote)
    let (dirs, projects): (Vec<String>, Vec<String>) =
        projects.iter().cloned().partition(|p| projects::is_path(p));
    let agents = agents
        .iter()
        .map(|a| crate::agents::resolve(a).map(str::to_string))
        .collect::<Result<Vec<_>, _>>()?;
    let book = accounts::Book::load();
    let accounts = accounts
        .iter()
        .map(|q| {
            if q.eq_ignore_ascii_case("unknown") {
                return Ok(String::new());
            }
            book.find(cfg, q).ok_or_else(|| {
                format!(
                    "no account {q} (an id, email or nickname: aleph usage accounts lists them)"
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Filter {
        hosts: hosts.to_vec(),
        agents,
        accounts,
        models: models.to_vec(),
        dirs,
        projects,
    })
}

/// A comma-separated list (`claude,codex`), each trimmed, empties dropped.
pub fn list_of(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// One line of a breakdown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// what the row groups by (a host, model, `2026-10-22`, a session id, …)
    pub key: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub reasoning: u64,
    pub requests: u64,
    pub sessions: u64,
    /// prompts the agents finished (their turn records)
    pub prompts: u64,
    /// time between requests less than 5 minutes apart
    pub active_ms: u64,
    /// what the agents logged as turn time
    pub turn_ms: u64,
    pub cost_reported: f64,
    pub cost_estimated: f64,
    pub unpriced: u64,
    /// requests at no charge (free models): in the tokens, a known $0
    pub free: u64,
    pub web: u64,
    pub first: u64,
    pub last: u64,
    pub hosts: Vec<String>,
    pub agents: Vec<String>,
    pub models: Vec<String>,
    /// the folders
    pub projects: Vec<String>,
    pub accounts: Vec<String>,
    /// what the key is called (a project's name)
    pub name: String,
    /// some of it is on an account guessed (≈: its records didn't say)
    pub guessed: bool,
}

impl Row {
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write()
    }
    /// Reported where the agent said, estimated elsewhere.
    pub fn cost(&self) -> f64 {
        self.cost_reported + self.cost_estimated
    }
    pub fn estimated(&self) -> bool {
        self.cost_estimated > 0.0
    }
    /// Every request in it was free (a free model): its cost says "free", not $0.
    pub fn all_free(&self) -> bool {
        self.free > 0 && self.cost() <= 0.0 && self.unpriced == 0
    }
    /// Active time: the larger of the request-gap sum and logged turn time.
    pub fn active_secs(&self) -> u64 {
        self.active_ms.max(self.turn_ms) / 1000
    }
    /// $ per prompt finished (None: no turn records — an agent that doesn't write them).
    pub fn per_prompt(&self) -> Option<f64> {
        (self.prompts > 0).then(|| self.cost() / self.prompts as f64)
    }
    /// $ per session.
    pub fn per_session(&self) -> Option<f64> {
        (self.sessions > 0).then(|| self.cost() / self.sessions as f64)
    }
    fn add(&mut self, r: &store::Row) {
        self.input += r.input;
        self.output += r.output;
        self.cache_read += r.cache_read;
        self.cache_write_5m += r.cache_write_5m;
        self.cache_write_1h += r.cache_write_1h;
        self.reasoning += r.reasoning;
        self.requests += r.requests;
        self.prompts += r.prompts;
        self.active_ms += r.active_ms;
        self.turn_ms += r.turn_ms;
        self.cost_reported += r.cost_reported;
        self.cost_estimated += r.cost_estimated;
        self.unpriced += r.unpriced;
        self.free += r.free;
        self.web += r.web;
        if r.first > 0 && (self.first == 0 || r.first < self.first) {
            self.first = r.first;
        }
        self.last = self.last.max(r.last);
    }
}

fn key_of(by: By, r: &store::Row) -> String {
    match by {
        By::Total => "total".into(),
        By::Host => r.host.clone(),
        By::Agent => r.agent.clone(),
        By::Model => r.model.clone(),
        By::Effort => r.effort.clone(),
        By::Project => {
            if r.pid.is_empty() {
                r.project.clone()
            } else {
                r.pid.clone()
            }
        }
        By::Folder => r.project.clone(),
        By::Day => day_name(r.day),
        By::Session => format!("{}:{}:{}", r.host, r.agent, r.session),
        By::Account => r.acct.clone(),
        By::Hour => format!("{} {:02}", day_name(r.day), r.hour),
    }
}

/// The store's rows for these days that the filter and the include/exclude
/// settings keep.
pub fn rows(cfg: &Config, range: Range, filter: &Filter) -> Vec<store::Row> {
    rows_ix(cfg, range, filter).0
}

/// [`rows`], and the projects index they were matched with (names, folders).
pub fn rows_ix(cfg: &Config, range: Range, filter: &Filter) -> (Vec<store::Row>, projects::Index) {
    let table = prices::Table::from_cfg(cfg);
    let tz = ui::utc_offset();
    let mut rs: Vec<store::Row> = store::rollup(&range.months(), &table)
        .into_iter()
        .filter(|r| range.keeps(r.day, r.hour, tz) && allowed(cfg, &r.host, &r.agent))
        .collect();
    let ix = index_of(
        cfg,
        rs.iter().map(|r| (r.host.as_str(), r.project.as_str())),
    );
    for r in &mut rs {
        r.pid = ix.id_of(&r.host, &r.project).to_string();
    }
    let pids = wanted(&ix, filter);
    rs.retain(|r| filter.keeps(r, &pids));
    projects::remember(&ix);
    (rs, ix)
}

/// The projects index for these (machine, folder) pairs.
fn index_of<'a>(cfg: &Config, pairs: impl Iterator<Item = (&'a str, &'a str)>) -> projects::Index {
    let set: BTreeSet<(String, String)> =
        pairs.map(|(h, f)| (h.to_string(), f.to_string())).collect();
    projects::Index::build(cfg, &projects::Book::load(), &set)
}

/// The project ids a filter asks for.
fn wanted(ix: &projects::Index, filter: &Filter) -> BTreeSet<String> {
    filter.projects.iter().filter_map(|q| ix.find(q)).collect()
}

/// The prompts finished in these hours that the filter keeps.
pub fn prompts(cfg: &Config, range: Range, filter: &Filter) -> Vec<store::Prompt> {
    let table = prices::Table::from_cfg(cfg);
    let tz = ui::utc_offset();
    let mut ps: Vec<store::Prompt> = store::prompts(&range.months(), &table)
        .into_iter()
        .filter(|p| range.keeps(p.day, p.hour, tz) && allowed(cfg, &p.host, &p.agent))
        .collect();
    let ix = index_of(
        cfg,
        ps.iter().map(|p| (p.host.as_str(), p.project.as_str())),
    );
    for p in &mut ps {
        p.pid = ix.id_of(&p.host, &p.project).to_string();
    }
    let pids = wanted(&ix, filter);
    ps.retain(|p| filter.keeps_prompt(p, &pids));
    ps
}

/// Sum rows by `by`: biggest cost first (days in order).
pub fn group(rows: &[store::Row], by: By) -> Vec<Row> {
    type Sessions = BTreeSet<(String, String, String)>;
    let mut m: BTreeMap<String, (Row, Sessions)> = BTreeMap::new();
    for r in rows {
        let k = key_of(by, r);
        let e = m.entry(k.clone()).or_insert_with(|| {
            (
                Row {
                    key: k,
                    ..Default::default()
                },
                BTreeSet::new(),
            )
        });
        e.0.add(r);
        e.0.guessed |= r.guess;
        if r.requests > 0 || r.turn_ms > 0 {
            e.1.insert((r.host.clone(), r.agent.clone(), r.session.clone()));
        }
        for (list, v) in [
            (&mut e.0.hosts, &r.host),
            (&mut e.0.agents, &r.agent),
            (&mut e.0.models, &r.model),
            (&mut e.0.projects, &r.project),
            (&mut e.0.accounts, &r.acct),
        ] {
            if !v.is_empty() && !list.contains(v) {
                list.push(v.clone());
            }
        }
    }
    let mut v: Vec<Row> = m
        .into_values()
        .map(|(mut r, s)| {
            r.sessions = s.len() as u64;
            r
        })
        .collect();
    match by {
        By::Day | By::Hour => v.sort_by(|a, b| a.key.cmp(&b.key)),
        By::Session => v.sort_by_key(|r| std::cmp::Reverse(r.last)),
        _ => v.sort_by(|a, b| {
            b.cost()
                .total_cmp(&a.cost())
                .then(b.tokens().cmp(&a.tokens()))
                .then(a.key.cmp(&b.key))
        }),
    }
    v
}

/// Two levels: rows by `by`, each with its own split by `then` (agent →
/// accounts, project → machines, …), in [`group`]'s order.
pub fn split(rows: &[store::Row], by: By, then: By) -> Vec<(Row, Vec<Row>)> {
    let mut parts: BTreeMap<String, Vec<store::Row>> = BTreeMap::new();
    for r in rows {
        parts.entry(key_of(by, r)).or_default().push(r.clone());
    }
    group(rows, by)
        .into_iter()
        .map(|g| {
            let sub = parts
                .get(&g.key)
                .map(|v| group(v, then))
                .unwrap_or_default();
            (g, sub)
        })
        .collect()
}

/// Usage over `range`, one row per `by` key (the dashboard's Monitor › Usage
/// calls this from a background thread).
pub fn query(cfg: &Config, range: Range, by: By, filter: &Filter) -> Vec<Row> {
    group(&rows(cfg, range, filter), by)
}

/// A distribution: how many, their sum and mean, percentiles, the biggest,
/// and log-spaced buckets (1–3–10) for a histogram: `(from, to, how many)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dist {
    pub n: u64,
    pub sum: f64,
    pub mean: f64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub max: f64,
    pub buckets: Vec<(f64, f64, u64)>,
}

impl Dist {
    pub fn of(mut v: Vec<f64>) -> Dist {
        v.retain(|x| x.is_finite() && *x >= 0.0);
        if v.is_empty() {
            return Dist::default();
        }
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let at = |q: f64| v[((q * n as f64).ceil() as usize).clamp(1, n) - 1];
        let sum: f64 = v.iter().sum();
        let max = v[n - 1];
        // 1–3–10 edges from the smallest positive value's power of ten
        let mut buckets = Vec::new();
        if let Some(lo) = v.iter().copied().find(|x| *x > 0.0) {
            let mut e = 10f64.powf(lo.log10().floor());
            let mut edges = vec![0.0, e];
            while e <= max {
                e *= if (e / 10f64.powf(e.log10().floor()) - 1.0).abs() < 1e-9 {
                    3.0
                } else {
                    10.0 / 3.0
                };
                edges.push(e);
            }
            for w in edges.windows(2) {
                let c = v.iter().filter(|x| **x >= w[0] && **x < w[1]).count() as u64;
                buckets.push((w[0], w[1], c));
            }
            // the first bucket holds the zeros too; trailing empties go
            while buckets.last().is_some_and(|b| b.2 == 0) {
                buckets.pop();
            }
        }
        Dist {
            n: n as u64,
            sum,
            mean: sum / n as f64,
            p50: at(0.5),
            p90: at(0.9),
            p99: at(0.99),
            max,
            buckets,
        }
    }
    pub fn to_json(&self) -> serde_json::Value {
        let r = |x: f64| (x * 1e4).round() / 1e4;
        serde_json::json!({
            "n": self.n, "sum": r(self.sum), "mean": r(self.mean),
            "p50": r(self.p50), "p90": r(self.p90), "p99": r(self.p99), "max": r(self.max),
            "buckets": self.buckets.iter().map(|(a, b, c)| serde_json::json!([r(*a), r(*b), c])).collect::<Vec<_>>(),
        })
    }
}

/// What the prompts and sessions of a range took: cost, time, tokens and
/// requests per prompt (each turn record and the requests in it), cost and
/// time per session, the costliest of each, and what no prompt holds
/// (agents that don't write turn records, requests between prompts).
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub prompt_cost: Dist,
    pub prompt_secs: Dist,
    pub prompt_tokens: Dist,
    pub prompt_requests: Dist,
    pub session_cost: Dist,
    pub session_secs: Dist,
    pub top_prompts: Vec<store::Prompt>,
    pub top_sessions: Vec<Row>,
    /// (requests, $) in no prompt
    pub outside: (u64, f64),
}

/// [`Stats`] over a range and filter (`top`: how many of the costliest).
pub fn stats(cfg: &Config, range: Range, filter: &Filter, top: usize) -> Stats {
    let ps = prompts(cfg, range, filter);
    let rs = rows(cfg, range, filter);
    let sessions = group(&rs, By::Session);
    let total = group(&rs, By::Total).pop().unwrap_or_default();
    let mut tops = ps.clone();
    tops.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    tops.truncate(top);
    let in_prompts: u64 = ps.iter().map(|p| p.requests).sum();
    let in_cost: f64 = ps.iter().map(|p| p.cost).sum();
    Stats {
        prompt_cost: Dist::of(ps.iter().map(|p| p.cost).collect()),
        prompt_secs: Dist::of(ps.iter().map(|p| p.secs as f64).collect()),
        prompt_tokens: Dist::of(ps.iter().map(|p| p.tokens as f64).collect()),
        prompt_requests: Dist::of(ps.iter().map(|p| p.requests as f64).collect()),
        session_cost: Dist::of(sessions.iter().map(Row::cost).collect()),
        session_secs: Dist::of(sessions.iter().map(|r| r.active_secs() as f64).collect()),
        top_sessions: {
            let mut v = sessions.clone();
            v.sort_by(|a, b| b.cost().total_cmp(&a.cost()));
            v.truncate(top);
            v
        },
        top_prompts: tops,
        outside: (
            total.requests.saturating_sub(in_prompts),
            (total.cost() - in_cost).max(0.0),
        ),
    }
}

/// Everything over `range` in one row.
pub fn totals(cfg: &Config, range: Range) -> Row {
    group(&rows(cfg, range, &Filter::default()), By::Total)
        .pop()
        .unwrap_or_else(|| Row {
            key: "total".into(),
            ..Default::default()
        })
}

// ------------------------------------------------------------------ sync

/// What one machine's sync did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub host: String,
    pub error: Option<String>,
    /// analytics.include / exclude leave nothing to read there
    pub excluded: bool,
    pub files_read: u64,
    pub bytes: u64,
    pub records: u64,
    pub took_ms: u64,
    /// (why, files left, bytes left)
    pub budget: Option<(String, u64, u64)>,
    pub skipped: Vec<(String, String, String)>,
}

impl Report {
    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "host": self.host, "ok": self.ok(), "error": self.error, "excluded": self.excluded,
            "files_read": self.files_read, "bytes": self.bytes, "records": self.records,
            "took_ms": self.took_ms,
            "budget": self.budget.as_ref().map(|(w, f, b)| serde_json::json!({"reason": w, "files": f, "bytes": b})),
            "skipped": self.skipped.iter().map(|(a, p, w)| serde_json::json!({"agent": a, "path": p, "why": w})).collect::<Vec<_>>(),
        })
    }
}

/// The script `sync` would send to `host` now (`aleph usage script`).
pub fn script_for(cfg: &Config, host: &str) -> String {
    let st = store::Offsets::load();
    extract::script(&opts_for(cfg, host), &known(&st, host))
}

fn known(st: &store::Offsets, host: &str) -> Vec<extract::Known> {
    st.hosts
        .get(host)
        .map(|h| {
            h.files
                .iter()
                .map(|(p, f)| extract::Known {
                    path: p.clone(),
                    dev: f.dev,
                    ino: f.ino,
                    size: f.size,
                    mtime: f.mtime,
                    off: f.off,
                    ctx: f.ctx.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run the extractor on one machine; its parsed output, or why it failed.
fn run_one(host: &str, script: &str, timeout: Duration) -> Result<extract::Output, String> {
    // showcase's demo world: what its agents' logs would hold
    if let Some(r) = crate::showcase::usage(host) {
        return r;
    }
    let o = proc::run(
        &mut sshcfg::shell(host, "sh -s"),
        timeout,
        Some(script.as_bytes()),
    )
    .map_err(|e| format!("couldn't start: {e}"))?;
    let text = o.text();
    let mut out = extract::parse_output(&text, host);
    if out.hostname.is_empty() && out.files.is_empty() {
        return Err(if o.timed_out {
            "timed out".into()
        } else {
            o.reason()
        });
    }
    out.problem = read_problem(&out, &o);
    Ok(out)
}

/// What went wrong in a run that still printed something: a script that never
/// reached its end, or files whose reader never finished (its awk refused to
/// load, say, when the bytes were still counted and nothing came of them).
/// None: a clean read.
fn read_problem(out: &extract::Output, o: &proc::Out) -> Option<String> {
    let done = out.files.iter().filter(|f| f.next.is_some()).count();
    if !out.ended {
        return Some(if o.timed_out {
            format!("timed out after {done} files; the rest wait for the next read")
        } else {
            format!("stopped before the end ({done} files read): {}", o.reason())
        });
    }
    let failed = out.files.len() - done;
    if failed == 0 {
        return None;
    }
    let why = o
        .stderr
        .lines()
        .map(str::trim)
        .find(|l| l.contains("awk"))
        .map(str::to_string)
        .unwrap_or_else(|| o.reason());
    let why: String = why.chars().take(160).collect();
    Some(format!(
        "the log reader failed on {failed} of {} files: {why}",
        out.files.len()
    ))
}

/// Fold one machine's run into the offsets; the records to append.
fn apply(
    st: &mut store::HostState,
    out: &extract::Output,
    agents_on: &[String],
) -> (Vec<parse::Record>, Report) {
    let mut rep = Report::default();
    for (old, new) in &out.moved {
        if let Some(f) = st.files.remove(old) {
            st.files.insert(new.clone(), f);
        }
    }
    if out.listing_done {
        for p in &out.gone {
            st.files.remove(p);
        }
        // agents excluded since last time: forget their files
        st.files.retain(|_, f| agents_on.contains(&f.agent));
    }
    let mut recs = Vec::new();
    for f in &out.files {
        let Some(next) = f.next else { continue };
        rep.files_read += 1;
        recs.extend(f.records.iter().cloned());
        st.files.insert(
            f.path.clone(),
            store::FileState {
                agent: f.agent.clone(),
                reader: f.reader.clone(),
                dev: f.dev,
                ino: f.ino,
                size: f.size,
                mtime: f.mtime,
                off: next,
                ctx: f.ctx.clone(),
            },
        );
    }
    rep.records = recs.len() as u64;
    rep.bytes = out.bytes_read;
    rep.budget = out.budget.clone();
    rep.skipped = out.skipped.clone();
    (recs, rep)
}

/// Sync these machines in parallel (one thread each), applying results as
/// they arrive; `progress` sees each report. Safe from a background thread;
/// another process's sync in progress makes this one wait (`wait`) or skip.
pub fn sync(
    cfg: &Config,
    hosts: &[String],
    full: bool,
    wait: Duration,
    progress: impl FnMut(&Report),
) -> Vec<Report> {
    sync_with(cfg, hosts, full, wait, &|_| true, progress)
}

/// Codex's plan is asked live of one machine per account each round, the
/// machines taking turns (pure — tested): every machine with Codex whose
/// account isn't known yet, and for each known account the machine whose
/// turn it is (`round` % how many share it).
pub fn codex_turn(hosts: &[String], raw: &serde_json::Value, round: u64) -> BTreeSet<String> {
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = BTreeSet::new();
    for h in hosts {
        let acct = raw
            .get(h)
            .and_then(|a| a.get("codex"))
            .and_then(serde_json::Value::as_object)
            .and_then(|ids| {
                ids.values()
                    .filter_map(|s| s.get("account").and_then(serde_json::Value::as_str))
                    .find(|a| !a.is_empty())
            });
        match acct {
            Some(a) => by.entry(a.to_string()).or_default().push(h.clone()),
            None => {
                out.insert(h.clone());
            }
        }
    }
    for hs in by.values() {
        out.insert(hs[(round as usize) % hs.len()].clone());
    }
    out
}

/// The Codex homes its last read found on a machine (their `sessions` folders'
/// parents), None for the default one; just the default before any read.
fn codex_homes(st: Option<&store::HostState>) -> Vec<Option<String>> {
    let Some(run) = st.and_then(|s| s.run.as_ref()) else {
        return vec![None];
    };
    let default = format!("{}/.codex", run.home.trim_end_matches('/'));
    let mut out: Vec<Option<String>> = Vec::new();
    for a in run.agents.iter().filter(|a| a.agent == "codex") {
        for r in &a.roots {
            let Some(home) = r
                .strip_suffix("/sessions")
                .or_else(|| r.strip_suffix("/archived_sessions"))
            else {
                continue;
            };
            let h = (home != default && !run.home.is_empty()).then(|| home.to_string());
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    if out.is_empty() {
        out.push(None);
    }
    out
}

/// [`sync`], asking Codex's plan live only where `ask_codex` says.
pub fn sync_with(
    cfg: &Config,
    hosts: &[String],
    full: bool,
    wait: Duration,
    ask_codex: &dyn Fn(&str) -> bool,
    mut progress: impl FnMut(&Report),
) -> Vec<Report> {
    let Some(_lock) = store::lock(wait) else {
        return hosts
            .iter()
            .map(|h| Report {
                host: h.clone(),
                error: Some("another sync is running".into()),
                ..Default::default()
            })
            .collect();
    };
    let mut offsets = store::Offsets::load();
    let mut book = accounts::Book::load();
    // readers that read more now (the timeline's steps): every log once more, from the start
    let mut gens = store::gens();
    for h in hosts {
        let (g, _) = gens.get(h).copied().unwrap_or((1, false));
        if g < store::GEN {
            let had = offsets.hosts.get(h).is_some_and(|s| !s.files.is_empty());
            if let Some(st) = offsets.hosts.get_mut(h) {
                store::regen(g, &mut st.files, crate::cache::now());
            }
            gens.insert(h.clone(), (store::GEN, had));
        }
    }
    store::save_gens(&gens);
    let (tx, rx) = mpsc::channel();
    let mut pending = 0;
    let mut reports = Vec::new();
    for h in hosts {
        let mut o = opts_for(cfg, h);
        if o.agents.is_empty() {
            let r = Report {
                host: h.clone(),
                excluded: true,
                ..Default::default()
            };
            progress(&r);
            reports.push(r);
            continue;
        }
        // the last read ran out of time (a machine busy with other work): ask
        // for less, so at least one file finishes and the read moves on
        let timed_out = offsets
            .hosts
            .get(h)
            .and_then(|s| s.run.as_ref())
            .and_then(|r| r.error.as_deref())
            .is_some_and(|e| e.starts_with("timed out"));
        if timed_out {
            o.max_bytes = (o.max_bytes / 4).max(1 << 20);
        }
        if full {
            offsets.hosts.remove(h);
        }
        let script = extract::script(&o, &known(&offsets, h));
        let timeout = Duration::from_secs(o.max_seconds + 45);
        let homes = codex_homes(offsets.hosts.get(h));
        let (tx, h) = (tx.clone(), h.clone());
        let ask = ask_codex(&h);
        pending += 1;
        std::thread::spawn(move || {
            let t0 = Instant::now();
            // Codex's plan and account, asked live beside the log read (a
            // log's snapshot goes stale), once per Codex home it has
            let codex = o.agents.iter().any(|a| a == "codex") && ask;
            let ask = std::thread::spawn({
                let h = h.clone();
                move || {
                    if !codex {
                        return Vec::new();
                    }
                    homes
                        .iter()
                        .filter_map(|home| {
                            live::codex(&h, home.as_deref(), Duration::from_secs(15))
                        })
                        .collect::<Vec<_>>()
                }
            });
            let r = run_one(&h, &script, timeout);
            let asked = ask.join().unwrap_or_default();
            let _ = tx.send((h, o.agents, r, t0.elapsed(), asked));
        });
    }
    drop(tx);
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for _ in 0..pending {
        let Ok((h, agents_on, r, took, asked)) = rx.recv() else {
            break;
        };
        let now = crate::cache::now();
        let st = offsets.hosts.entry(h.clone()).or_default();
        let mut rep = match r {
            Ok(mut out) => {
                // whose each request was (exact when its time is in a sighting)
                let at = if out.now > 0 { out.now } else { now };
                accounts::stamp(&mut book, &h, &mut out, at);
                let before = st.clone();
                let (recs, mut rep) = apply(st, &out, &agents_on);
                rep.error = out.problem.clone();
                match store::append(&recs) {
                    Ok(ms) => touched.extend(ms),
                    Err(e) => {
                        // nothing was kept: the same bytes get read again next time
                        *st = before;
                        rep.error = Some(format!("couldn't save the records: {e}"));
                    }
                }
                // what the agents did (the timeline): from the files read to their end
                let done: Vec<steps::Step> = out
                    .files
                    .iter()
                    .filter(|f| f.next.is_some())
                    .flat_map(|f| f.steps.iter().cloned())
                    .collect();
                if let Err(e) = steps::append(&done) {
                    crate::log::warn("usage", &format!("couldn't save the steps: {e}"));
                }
                for f in &out.files {
                    store::save_limits(&h, &f.agent, &f.limits);
                }
                for (agent, l) in &out.plans {
                    store::save_limits(&h, agent, std::slice::from_ref(l));
                }
                for agent in &out.no_plan {
                    store::drop_cached_limits(&h, agent);
                }
                for a in &asked {
                    if let Some(l) = &a.limit {
                        store::save_limits(&h, "codex", std::slice::from_ref(l));
                        accounts::note_live(&mut book, &h, &l.account, &a.plan, &a.email, now);
                    }
                }
                st.run = Some(store::RunInfo {
                    at: now,
                    took_ms: took.as_millis() as u64,
                    error: rep.error.clone(),
                    hostname: out.hostname.clone(),
                    home: out.home.clone(),
                    files_read: rep.files_read,
                    bytes: rep.bytes,
                    records: rep.records,
                    budget: rep.budget.clone(),
                    agents: out.agents.clone(),
                    skipped: out.skipped.clone(),
                    has_zstd: out.has_zstd,
                    has_sqlite: out.has_sqlite,
                });
                if rep.error.is_none() {
                    st.ok_at = now;
                }
                rep
            }
            Err(e) => {
                let mut run = st.run.clone().unwrap_or_default();
                run.at = now;
                run.took_ms = took.as_millis() as u64;
                run.error = Some(e.clone());
                st.run = Some(run);
                Report {
                    error: Some(e),
                    ..Default::default()
                }
            }
        };
        rep.host = h;
        rep.took_ms = took.as_millis() as u64;
        crate::log::info(
            "usage",
            &match &rep.error {
                None => format!(
                    "{}: {} files, {} new records in {:.1}s",
                    rep.host,
                    rep.files_read,
                    rep.records,
                    rep.took_ms as f64 / 1000.0
                ),
                Some(e) => format!("{}: {e}", rep.host),
            },
        );
        progress(&rep);
        reports.push(rep);
    }
    if let Err(e) = offsets.save() {
        crate::log::warn("usage", &format!("couldn't save offsets.json: {e}"));
    }
    book.save();
    // a re-read that has caught up: its duplicate lines go
    let caught_up: Vec<String> = reports
        .iter()
        .filter(|r| r.error.is_none() && r.budget.is_none() && !r.excluded)
        .filter(|r| gens.get(&r.host).is_some_and(|g| g.1))
        .map(|r| r.host.clone())
        .collect();
    if !caught_up.is_empty() {
        let mut dropped = 0;
        for m in store::months() {
            dropped += store::compact(&m).unwrap_or(0);
            touched.insert(m);
        }
        for m in steps::months() {
            let _ = steps::compact(&m);
        }
        for h in &caught_up {
            if let Some(g) = gens.get_mut(h) {
                g.1 = false;
            }
        }
        store::save_gens(&gens);
        crate::log::info(
            "usage",
            &format!(
                "{}: read again for the timeline; {dropped} repeated lines dropped",
                caught_up.join(", ")
            ),
        );
    }
    // the timeline's steps: kept timeline.keep days (whole months)
    if let Ok(keep) = parse_days(&cfg.get_or("timeline.keep")) {
        if keep > 0 {
            steps::prune(crate::cache::now().saturating_sub(keep * 86_400));
        }
    }
    // whole months past the retention go; the rest is summed for queries
    if let Ok(keep) = parse_days(&cfg.get_or("analytics.keep")) {
        if keep > 0 {
            let before = crate::cache::now().saturating_sub(keep * 86_400);
            let old: Vec<String> = store::months()
                .into_iter()
                .filter(|m| store::month_bounds(m).is_some_and(|(_, end)| end < before))
                .collect();
            if !old.is_empty() {
                let _ = store::prune(Some(before), None, false);
            }
        }
    }
    if !touched.is_empty() {
        let ms: Vec<String> = touched.into_iter().collect();
        store::rollup(&ms, &prices::Table::from_cfg(cfg));
    }
    // which repo each machine's new folders are in (one look per machine with any)
    let ok: Vec<(String, String)> = reports
        .iter()
        .filter(|r| r.error.is_none() && !r.excluded)
        .filter_map(|r| {
            let home = offsets.hosts.get(&r.host)?.run.as_ref()?.home.clone();
            Some((r.host.clone(), home))
        })
        .collect();
    resolve_projects(cfg, &ok);
    reports
}

/// Ask machines which repo their folders are in: the ones the last six
/// months' requests ran in that aren't known yet (or not for a week), a
/// look per machine, in parallel (blocking: off the UI thread).
pub fn resolve_projects(cfg: &Config, hosts: &[(String, String)]) {
    if hosts.is_empty() {
        return;
    }
    let book = projects::Book::load();
    let now = crate::cache::now();
    let rs = store::rollup(&Range::days(183).months(), &prices::Table::from_cfg(cfg));
    let mut by_host: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for r in &rs {
        by_host
            .entry(r.host.clone())
            .or_default()
            .insert(r.project.clone());
    }
    let mut jobs = Vec::new();
    for (h, home) in hosts {
        let due = book.due(h, by_host.get(h).unwrap_or(&BTreeSet::new()), now);
        if due.is_empty() {
            continue;
        }
        let (h, home, b) = (h.clone(), home.clone(), book.clone());
        jobs.push(std::thread::spawn(move || {
            projects::resolve(&h, &due, &b, &home).map(|a| (h, a))
        }));
    }
    if jobs.is_empty() {
        return;
    }
    let mut book = projects::Book::load();
    for j in jobs {
        if let Ok(Some((h, a))) = j.join() {
            crate::log::info(
                "usage",
                &format!("{h}: {} folders looked up for their projects", a.len()),
            );
            book.apply(&h, a);
        }
    }
    book.save();
}

/// The daemon hook: sync every machine in the fleet (when analytics is on).
/// Blocking; call it from a background thread.
pub fn sync_all(cfg: &Config) -> Vec<Report> {
    if !enabled(cfg) {
        return Vec::new();
    }
    sync(cfg, &cfg.fleet(), false, LOCK_WAIT, |_| {})
}

/// How long a background read waits its turn when another is running (off
/// the UI thread; a round that gives up simply waits for the next one).
const LOCK_WAIT: Duration = Duration::from_secs(30);

/// How a machine's logs were last read (the strip on the Usage screen).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadState {
    pub host: String,
    /// when it was last tried (0: never)
    pub at: u64,
    pub error: Option<String>,
    /// files the last read's budget left for the next one
    pub waiting: u64,
}

/// These machines' last reads, from the store.
pub fn reads(hosts: &[String]) -> Vec<ReadState> {
    let st = store::Offsets::load();
    hosts
        .iter()
        .map(|h| {
            let run = st.hosts.get(h).and_then(|s| s.run.as_ref());
            ReadState {
                host: h.clone(),
                at: run.map_or(0, |r| r.at),
                error: run.and_then(|r| r.error.clone()),
                waiting: run
                    .and_then(|r| r.budget.as_ref())
                    .map_or(0, |(_, files, _)| *files),
            }
        })
        .collect()
}

/// When the last sync that worked finished, per machine.
pub fn last_ok() -> BTreeMap<String, u64> {
    store::Offsets::load()
        .hosts
        .into_iter()
        .map(|(h, s)| (h, s.ok_at))
        .collect()
}

/// The daemon's analytics, every `analytics.interval` seconds (a minute):
/// every machine the fleet reaches is read (one that's down is tried again
/// every 10 minutes, its history counting meanwhile), Codex's plan asked of
/// one machine per account in turn, then the summary (`now`) worked out and
/// kept — and the menu bar woken when its numbers changed. `tick` is called
/// from the daemon's loop and never blocks.
pub struct Background {
    busy: Arc<AtomicBool>,
    next: Instant,
    round: u64,
    /// when each machine that's down was last tried
    tried_down: Arc<Mutex<BTreeMap<String, Instant>>>,
    last: Arc<Mutex<Option<serde_json::Value>>>,
}

impl Default for Background {
    fn default() -> Self {
        Self::new()
    }
}

const DOWN_RETRY: Duration = Duration::from_secs(600);

impl Background {
    pub fn new() -> Background {
        Background {
            busy: Arc::new(AtomicBool::new(false)),
            // let the daemon settle first
            next: Instant::now() + Duration::from_secs(20),
            round: 0,
            tried_down: Arc::new(Mutex::new(BTreeMap::new())),
            last: Arc::new(Mutex::new(now::load())),
        }
    }

    /// `down`: the machines the fleet can't reach right now.
    pub fn tick(&mut self, cfg: &Config, down: &[String]) {
        if Instant::now() < self.next || self.busy.load(Ordering::Relaxed) || !enabled(cfg) {
            return;
        }
        self.next = Instant::now() + Duration::from_secs(interval(cfg));
        self.round += 1;
        self.busy.store(true, Ordering::Relaxed);
        let (cfg, busy, round) = (cfg.clone(), self.busy.clone(), self.round);
        let (tried, last) = (self.tried_down.clone(), self.last.clone());
        let down = down.to_vec();
        std::thread::spawn(move || {
            let hosts: Vec<String> = {
                let mut t = tried.lock().unwrap();
                t.retain(|h, _| down.contains(h));
                cfg.fleet()
                    .into_iter()
                    .filter(|h| {
                        if !down.contains(h) {
                            return true;
                        }
                        let due = t.get(h).is_none_or(|at| at.elapsed() >= DOWN_RETRY);
                        if due {
                            t.insert(h.clone(), Instant::now());
                        }
                        due
                    })
                    .collect()
            };
            let turn = codex_turn(&hosts, &store::raw_limits(), round);
            sync_with(
                &cfg,
                &hosts,
                false,
                LOCK_WAIT,
                &|h| turn.contains(h),
                |_| {},
            );
            // prices for models the agents don't price (OpenRouter's list: with a key only)
            crate::openrouter::refresh_catalog(&cfg);
            let v = now::refresh(&cfg, &down);
            let mut l = last.lock().unwrap();
            if !l.as_ref().is_some_and(|o| now::same_numbers(o, &v)) {
                crate::router::wake_raycast(&cfg, "usage");
            }
            *l = Some(v);
            busy.store(false, Ordering::Relaxed);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "H\tbox\t/home/u\t100\tgnu\t\t\n";
    const FILE: &str = "F\tclaude\tclaude\t/home/u/.claude/projects/p/a.jsonl\t1\t2\t500\t90\t0\n";

    fn stderr(s: &str) -> proc::Out {
        proc::Out {
            code: Some(0),
            stderr: s.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_reader_that_never_loads_is_an_error_not_a_clean_read() {
        // what mawk did to every Linux machine: each file's bytes counted, its
        // reader refused, the script ending normally with nothing from it
        let text = format!("{HEAD}{FILE}E\t100\t1\t500\n");
        let out = extract::parse_output(&text, "box");
        assert!(out.ended);
        let e = read_problem(
            &out,
            &stderr("Warning: x\nawk: line 473: syntax error at or near val\n"),
        )
        .unwrap();
        assert!(
            e.contains("failed on 1 of 1 files") && e.contains("syntax error"),
            "{e}"
        );
        // its offsets don't move, so the next read after a fix gets those bytes
        let mut st = store::HostState::default();
        let (recs, rep) = apply(&mut st, &out, &["claude".to_string()]);
        assert!(recs.is_empty() && st.files.is_empty() && rep.files_read == 0);
    }

    #[test]
    fn a_read_that_stops_early_says_so() {
        let unfinished = extract::parse_output(&format!("{HEAD}{FILE}"), "box");
        let timed = proc::Out {
            timed_out: true,
            ..Default::default()
        };
        let e = read_problem(&unfinished, &timed).unwrap();
        assert!(e.starts_with("timed out"), "{e}");
        let e = read_problem(&unfinished, &stderr("sh: broken pipe")).unwrap();
        assert!(
            e.contains("stopped before the end") && e.contains("broken pipe"),
            "{e}"
        );
    }

    #[test]
    fn a_clean_read_has_no_problem() {
        let f = "f\t/home/u/.claude/projects/p/a.jsonl\t500\t-\t4\t500\n";
        let out = extract::parse_output(&format!("{HEAD}{FILE}{f}E\t100\t1\t500\n"), "box");
        assert_eq!(out.files[0].next, Some(500));
        assert_eq!(read_problem(&out, &stderr("")), None);
        // nothing to read at all: fine too
        let none = extract::parse_output(&format!("{HEAD}E\t100\t0\t0\n"), "box");
        assert_eq!(read_problem(&none, &stderr("")), None);
    }

    #[test]
    fn codex_is_asked_once_per_account_in_turn() {
        let raw = serde_json::json!({
            "local": {"codex": {"codex": {"account": "c1"}}},
            "b31": {"codex": {"codex": {"account": "c1"}}},
            "q7x": {"codex": {"codex": {"account": "c2"}}},
        });
        let hosts: Vec<String> = ["local", "b31", "q7x", "new"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let r0 = codex_turn(&hosts, &raw, 0);
        let r1 = codex_turn(&hosts, &raw, 1);
        // c1 is shared: one of its machines each round, taking turns
        assert_eq!(
            r0.iter().filter(|h| *h == "local" || *h == "b31").count(),
            1
        );
        assert_ne!(r0.contains("local"), r1.contains("local"));
        // c2 has one machine; an unknown account is always asked
        assert!(r0.contains("q7x") && r1.contains("q7x") && r0.contains("new"));
    }
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (p, q) = (e.path(), to.join(e.file_name()));
            if p.is_dir() {
                copy_dir(&p, &q);
            } else {
                std::fs::copy(&p, &q).unwrap();
            }
        }
    }

    fn have(tool: &str) -> bool {
        proc::which(tool).is_some()
    }

    /// A home with every agent's logs (the fixtures), as a machine would have.
    fn fake_home() -> PathBuf {
        let fx = extract::tests::fixtures();
        let h = std::env::temp_dir().join(format!("aleph-usage-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        copy_dir(&fx.join("claude/projects"), &h.join(".claude/projects"));
        // Claude Code installed (its plan is read only where it is)
        std::fs::create_dir_all(h.join(".local/bin")).unwrap();
        std::fs::write(h.join(".local/bin/claude"), "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                h.join(".local/bin/claude"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        std::fs::copy(fx.join("claude/claude.json"), h.join(".claude.json")).unwrap();
        // a second Claude login, started as CLAUDE_CONFIG_DIR=~/.claude-work from an alias
        copy_dir(
            &fx.join("claude-work/projects"),
            &h.join(".claude-work/projects"),
        );
        std::fs::copy(
            fx.join("claude-work/claude.json"),
            h.join(".claude-work/.claude.json"),
        )
        .unwrap();
        // credentials beside them: never read (each holds SECRET, which must not come out)
        let canary = |from: &str, to: &str| {
            std::fs::create_dir_all(h.join(to).parent().unwrap()).unwrap();
            std::fs::copy(fx.join("canary").join(from), h.join(to)).unwrap();
        };
        canary("claude-credentials.json", ".claude/.credentials.json");
        canary("claude-credentials.json", ".claude-work/.credentials.json");
        canary("codex-auth.json", ".codex/auth.json");
        canary("opencode-auth.json", ".local/share/opencode/auth.json");
        canary("gemini-oauth.json", ".gemini/oauth_creds.json");
        canary("google_accounts.json", ".gemini/google_accounts.json");
        copy_dir(&fx.join("codex/sessions"), &h.join(".codex/sessions"));
        copy_dir(&fx.join("pi/sessions"), &h.join(".pi/agent/sessions"));
        copy_dir(&fx.join("gemini/tmp"), &h.join(".gemini/tmp"));
        copy_dir(&fx.join("qwen/tmp"), &h.join(".qwen/tmp"));
        copy_dir(
            &fx.join("opencode/storage"),
            &h.join(".local/share/opencode/storage"),
        );
        if have("sqlite3") {
            let sql = std::fs::read(fx.join("opencode/opencode.sql")).unwrap();
            let db = h.join(".local/share/opencode/opencode.db");
            let o = proc::run(
                std::process::Command::new("sqlite3").arg(&db),
                Duration::from_secs(10),
                Some(&sql),
            )
            .unwrap();
            assert!(o.ok(), "{}", o.stderr);
        }
        if have("zstd") {
            // an archived rollout, compressed: the same records as one above
            let name = "rollout-2026-10-21T18-30-00-019a0002-aaaa-7bbb-8ccc-000000000002.jsonl";
            let src = fx.join("codex/sessions/2026/10/21").join(name);
            let dst = h
                .join(".codex/archived_sessions")
                .join(format!("{name}.zst"));
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            let o = proc::cap(
                "zstd",
                &[
                    "-q",
                    "-f",
                    "-o",
                    dst.to_str().unwrap(),
                    src.to_str().unwrap(),
                ],
                Duration::from_secs(10),
            );
            assert!(o.ok(), "{}", o.stderr);
        }
        h
    }

    /// The whole script, run here with `home` as $HOME.
    fn run_script(home: &Path, o: &extract::Opts, known: &[extract::Known]) -> extract::Output {
        let script = extract::script(o, known);
        let mut c = std::process::Command::new("sh");
        c.arg("-s")
            .env("HOME", home)
            .env("SHELL", "/bin/sh")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("XDG_DATA_HOME");
        let out = proc::run(&mut c, Duration::from_secs(60), Some(script.as_bytes())).unwrap();
        let text = out.text();
        assert!(
            !text.contains("SECRET"),
            "conversation text leaked:\n{text}"
        );
        assert!(out.ok(), "{}\n{text}", out.stderr);
        extract::parse_output(&text, "local")
    }

    fn known_of(st: &store::HostState) -> Vec<extract::Known> {
        st.files
            .iter()
            .map(|(p, f)| extract::Known {
                path: p.clone(),
                dev: f.dev,
                ino: f.ino,
                size: f.size,
                mtime: f.mtime,
                off: f.off,
                ctx: f.ctx.clone(),
            })
            .collect()
    }

    #[test]
    fn antigravity_conversations_with_each_requests_model() {
        if !have("sqlite3") {
            return;
        }
        let fx = extract::tests::fixtures();
        let h = std::env::temp_dir().join(format!("aleph-usage-agy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        let a = h.join(".gemini/antigravity-cli");
        copy_dir(&fx.join("agy/brain"), &a.join("brain"));
        let id = "7e1d2c3b-aaaa-4bbb-8ccc-0d1e2f3a4b5c";
        std::fs::create_dir_all(a.join("conversations")).unwrap();
        let sql = std::fs::read(fx.join("agy/conversation.sql")).unwrap();
        let o = proc::run(
            std::process::Command::new("sqlite3").arg(a.join(format!("conversations/{id}.db"))),
            Duration::from_secs(10),
            Some(&sql),
        )
        .unwrap();
        assert!(o.ok(), "{}", o.stderr);
        let opts = extract::Opts::default();
        let out = run_script(&h, &opts, &[]);
        let recs: Vec<&parse::Record> = out
            .files
            .iter()
            .flat_map(|f| f.records.iter())
            .filter(|r| r.agent == "agy")
            .collect();
        let reqs: Vec<_> = recs.iter().filter(|r| !r.is_turn()).collect();
        // each request its own model (it changed between them), the folder from the
        // conversation, thinking inside the output
        let m: Vec<(&str, u64, u64, u64, u64)> = reqs
            .iter()
            .map(|r| {
                (
                    r.model.as_str(),
                    r.input,
                    r.output,
                    r.reasoning,
                    r.cache_read,
                )
            })
            .collect();
        assert_eq!(
            m,
            [
                ("gemini-3.8-flash-n", 1000, 150, 40, 0),
                ("gemini-3-pro", 300, 90, 30, 1200)
            ]
        );
        assert!(reqs
            .iter()
            .all(|r| r.session == id && r.project == "/home/u/app"));
        let turns: Vec<u64> = recs
            .iter()
            .filter(|r| r.is_turn())
            .map(|r| r.turn_ms)
            .collect();
        assert_eq!(turns, [5000], "the prompt ran until the answer");
        use steps::Kind;
        let st: Vec<&steps::Step> = out.files.iter().flat_map(|f| f.steps.iter()).collect();
        assert!(st.iter().any(|s| s.kind == Kind::Prompt && s.name == "u"));
        assert!(st
            .iter()
            .any(|s| s.kind == Kind::ToolStart && s.name == "read_file"));
        assert!(st.iter().any(|s| s.kind == Kind::ToolEnd));
        // read again: nothing new, and the state carried says where it was
        let mut st2 = store::HostState::default();
        apply(&mut st2, &out, &opts.agents);
        let again = run_script(&h, &opts, &known_of(&st2));
        let n: usize = again
            .files
            .iter()
            .filter(|f| f.agent == "agy")
            .map(|f| f.records.len())
            .sum();
        assert_eq!(n, 0);
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn gemini_on_an_api_key_is_the_keys_account_not_the_google_one() {
        let fx = extract::tests::fixtures();
        let h = std::env::temp_dir().join(format!("aleph-usage-gem-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        copy_dir(&fx.join("gemini/tmp"), &h.join(".gemini/tmp"));
        std::fs::copy(
            fx.join("canary/google_accounts.json"),
            h.join(".gemini/google_accounts.json"),
        )
        .unwrap();
        let account = |settings: &str| {
            std::fs::write(h.join(".gemini/settings.json"), settings).unwrap();
            let mut out = run_script(&h, &extract::Opts::default(), &[]);
            let mut book = accounts::Book::default();
            let at = out.now;
            accounts::stamp(&mut book, "local", &mut out, at);
            let a: Vec<accounts::Account> = book
                .accounts
                .values()
                .filter(|a| a.agent == "gemini")
                .cloned()
                .collect();
            assert_eq!(a.len(), 1, "{a:#?}");
            a[0].clone()
        };
        let key = account(r#"{"security":{"auth":{"selectedType":"gemini-api-key"}}}"#);
        assert_eq!((key.kind.as_str(), key.email.as_str()), ("api", ""));
        let google = account(r#"{"security":{"auth":{"selectedType":"oauth-personal"}}}"#);
        assert_eq!(google.email, "u@gmail.example");
        let vertex = account(r#"{"selectedAuthType":"vertex-ai"}"#);
        assert_eq!(
            (vertex.kind.as_str(), vertex.plan.as_str()),
            ("api", "Vertex AI")
        );
        let _ = std::fs::remove_dir_all(&h);
    }

    #[test]
    fn opencode_2_database_is_read_from_its_own_tables() {
        if !have("sqlite3") {
            return;
        }
        let fx = extract::tests::fixtures();
        let h = std::env::temp_dir().join(format!("aleph-usage-oc2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&h);
        let db = h.join(".local/share/opencode/opencode.db");
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        let sql = std::fs::read(fx.join("opencode/opencode2.sql")).unwrap();
        let o = proc::run(
            std::process::Command::new("sqlite3").arg(&db),
            Duration::from_secs(10),
            Some(&sql),
        )
        .unwrap();
        assert!(o.ok(), "{}", o.stderr);
        let out = run_script(&h, &extract::Opts::default(), &[]);
        let _ = std::fs::remove_dir_all(&h);
        let recs: Vec<&parse::Record> = out
            .files
            .iter()
            .flat_map(|f| f.records.iter())
            .filter(|r| r.agent == "opencode")
            .collect();
        // three requests (the copy in the old table isn't read again), then three
        // more through OpenRouter and OpenCode's own models
        let reqs: Vec<_> = recs.iter().filter(|r| !r.is_turn()).collect();
        assert_eq!(reqs.len(), 6, "{recs:#?}");
        let by_model = |m: &str| reqs.iter().find(|r| r.model == m).unwrap();
        let or_paid = by_model("anthropic/claude-sonnet-4.5");
        assert_eq!((or_paid.cost, or_paid.free()), (Some(0.0123), false));
        assert!(by_model("qwen/qwen3-coder:free").free());
        assert!(by_model("big-pickle").free(), "OpenCode's own model, $0");
        assert!(
            !by_model("gpt-5.1-codex").free(),
            "a subscription's $0 is priced"
        );
        // priced: what OpenCode reported, free ones a known zero, the rest estimated
        let owned: Vec<parse::Record> = reqs.iter().map(|r| (**r).clone()).collect();
        let rows = store::build(owned, &prices::Table::default(), 0);
        let total = |f: fn(&store::Row) -> f64| rows.iter().map(f).sum::<f64>();
        assert!((total(|r| r.cost_reported) - 0.0323).abs() < 1e-9);
        assert!(total(|r| r.cost_estimated) > 0.0);
        assert_eq!(rows.iter().map(|r| r.unpriced).sum::<u64>(), 0);
        // a free model's row: its tokens and requests counted, its cost "free"
        let by_m = group(&rows, By::Model);
        let free = by_m
            .iter()
            .find(|r| r.key == "qwen/qwen3-coder:free")
            .unwrap();
        assert_eq!((free.requests, free.tokens(), free.free), (1, 2200, 1));
        assert!(free.all_free());
        assert_eq!(cmd::money(free), "free");
        let all = group(&rows, By::Agent);
        assert_eq!(all[0].free, 2);
        assert!(!all[0].all_free() && cmd::money(&all[0]).starts_with('$'));
        let first = reqs.iter().find(|r| r.ts == 1_792_661_001).unwrap();
        assert_eq!(
            (
                first.session.as_str(),
                first.model.as_str(),
                first.effort.as_str()
            ),
            ("ses_v2a", "gpt-5.1-codex", "high")
        );
        assert_eq!(first.project, "/home/u/app");
        assert_eq!(
            (first.input, first.output, first.reasoning, first.cache_read),
            (900, 160, 40, 6000),
            "output carries reasoning, as with OpenCode 1"
        );
        assert!(reqs.iter().any(|r| r.session == "ses_v2b"));
        let turns: Vec<u64> = recs
            .iter()
            .filter(|r| r.is_turn())
            .map(|r| r.turn_ms)
            .collect();
        assert_eq!(turns, [30_000, 100_000]);
        // the timeline: prompts by whom, tools, the subagent's lane and the call that
        // started it, the compaction, the interruption
        use steps::Kind;
        let all: Vec<&steps::Step> = out.files.iter().flat_map(|f| f.steps.iter()).collect();
        let has = |kind: Kind, name: &str| {
            all.iter()
                .any(|s| s.agent == "opencode" && s.kind == kind && s.name == name)
        };
        assert!(has(Kind::Prompt, "u") && has(Kind::Prompt, "a") && has(Kind::Prompt, "s"));
        assert!(has(Kind::ToolStart, "read") && has(Kind::ToolStart, "task"));
        assert!(has(Kind::ToolStart, "grep") && has(Kind::ToolEnd, "err"));
        assert!(has(Kind::Compact, "") || all.iter().any(|s| s.kind == Kind::Compact));
        assert!(all
            .iter()
            .any(|s| s.kind == Kind::Error && s.name == "interrupted"));
        let lane = all.iter().find(|s| s.kind == Kind::Lane).unwrap();
        assert_eq!(
            (
                lane.session.as_str(),
                lane.lane.as_str(),
                lane.name.as_str()
            ),
            ("ses_v2a", "ses_v2b", "explore")
        );
        let task = all
            .iter()
            .find(|s| s.kind == Kind::ToolStart && s.name == "task")
            .unwrap();
        assert_eq!(lane.class, task.key, "the lane hangs off the task call");
    }

    #[test]
    fn script_reads_every_agent_then_only_what_is_new() {
        let home = fake_home();
        let o = extract::Opts::default();
        let first = run_script(&home, &o, &[]);
        assert!(first.listing_done && first.ended, "{first:#?}");
        // Claude Code's plan windows, from its own settings
        let (agent, plan) = &first.plans[0];
        assert_eq!(
            (agent.as_str(), plan.plan.as_str(), plan.ts),
            ("claude", "Max 5x", 1_792_737_920)
        );
        assert_eq!(plan.account.len(), 16, "the account as a hash");
        assert!(!plan.account.contains("72f198b7"));
        let w: Vec<_> = plan
            .windows
            .iter()
            .map(|w| (w.used_percent, w.minutes, w.resets_at))
            .collect();
        assert_eq!(
            w,
            [
                (13.0, 300, Some(1_792_753_199)),
                (44.5, 10080, Some(1_793_185_200))
            ]
        );
        assert!(first.budget.is_none());
        let sqlite = have("sqlite3");
        for a in ["claude", "codex", "pi", "gemini", "qwen", "opencode"] {
            let seen = first
                .agents
                .iter()
                .find(|x| x.agent == a)
                .unwrap_or_else(|| panic!("{a}: {first:#?}"));
            assert!(!seen.roots.is_empty(), "{a} found no logs");
        }
        let omp = first.agents.iter().find(|x| x.agent == "omp").unwrap();
        assert!(omp.roots.is_empty(), "no OMP logs here");
        if !sqlite {
            assert!(
                first
                    .skipped
                    .iter()
                    .any(|(a, _, w)| a == "opencode" && w == "needs sqlite3"),
                "{:?}",
                first.skipped
            );
        }
        // whose: each Claude config folder's own account (the second login found
        // beside the first), Gemini's sign-in; never a credential (SECRET above)
        let n = |root_end: &str| {
            first
                .accounts
                .iter()
                .find(|a| a.agent == "claude" && a.root.ends_with(root_end))
                .unwrap_or_else(|| panic!("no account for {root_end}: {:#?}", first.accounts))
        };
        let home_acct = n("/.claude/projects").clone();
        assert_eq!(
            (home_acct.id.as_str(), home_acct.email.as_str()),
            ("72f198b7-0000-4c4a-91f0-b3f43cd7df5d", "u@home.example")
        );
        let work = n("/.claude-work/projects").clone();
        assert_eq!(
            (
                work.id.as_str(),
                work.email.as_str(),
                work.org.as_str(),
                work.tier.as_str()
            ),
            (
                "9c1d0000-1111-4222-8333-0000000000a1",
                "u@work.example",
                "Work Co",
                "default_claude_max_20x"
            )
        );
        assert!(first
            .accounts
            .iter()
            .any(|a| a.agent == "gemini" && a.email == "u@gmail.example"));
        // what each agent did, for the timeline (no text: the SECRET check above covers it)
        {
            use steps::Kind;
            let all: Vec<&steps::Step> = first.files.iter().flat_map(|f| f.steps.iter()).collect();
            let has = |agent: &str, kind: Kind, name: &str| {
                all.iter().any(|s| {
                    s.agent == agent && s.kind == kind && (name.is_empty() || s.name == name)
                })
            };
            assert!(has("claude", Kind::ToolStart, "Bash") && has("claude", Kind::Lane, "Explore"));
            assert!(has("codex", Kind::ToolStart, "shell") && has("codex", Kind::Prompt, "u"));
            assert!(
                has("pi", Kind::ToolStart, "bash")
                    && has("gemini", Kind::ToolStart, "run_shell_command")
            );
            if sqlite {
                assert!(has("opencode", Kind::ToolStart, "bash"), "{all:#?}");
                assert!(
                    has("opencode", Kind::ToolStart, "task")
                        && has("opencode", Kind::ToolEnd, "err")
                );
                assert!(has("opencode", Kind::Lane, "") && has("opencode", Kind::Prompt, "u"));
            }
        }
        let mut first = first;
        let mut book = accounts::Book::default();
        let at = first.now;
        accounts::stamp(&mut book, "local", &mut first, at);
        let work_id = parse::hash(&["claude", &work.id]);
        assert_eq!(book.accounts[&work_id].plan, "Max 20x");
        let codex_id = parse::hash(&["codex", "0e1d2c3b-4a59-4687-9a1b-2c3d4e5f6a7b"]);
        let all: Vec<&parse::Record> = first.files.iter().flat_map(|f| f.records.iter()).collect();
        assert!(
            all.iter().any(|r| r.agent == "codex" && r.acct == codex_id),
            "the session named its account"
        );
        assert_eq!(book.accounts[&codex_id].kind, "plan");
        // the second login's requests: logged before this read, so its folder's account is the guess
        let w = all
            .iter()
            .find(|r| r.project.ends_with("/work") && r.agent == "claude")
            .unwrap();
        assert_eq!(w.acct, "", "older than the sighting: not stamped");
        assert_eq!(book.guess(w).as_deref(), Some(work_id.as_str()));
        let first = first;
        let mut st = store::HostState::default();
        let (recs, rep) = apply(&mut st, &first, &o.agents);
        assert_eq!(rep.files_read as usize, first.files.len());
        assert!(
            first.files.iter().all(|f| f.next.is_some()),
            "every file committed"
        );
        let count = |agent: &str| {
            store::dedup(recs.clone())
                .iter()
                .filter(|r| r.agent == agent && !r.is_turn())
                .count()
        };
        assert_eq!(
            count("claude"),
            7,
            "three in the session, one in its subagent, one in a session with a tool call, two from the second login"
        );
        assert_eq!(
            count("codex"),
            4,
            "the archived copy of a rollout counts once"
        );
        assert_eq!(count("pi"), 2);
        assert_eq!(count("gemini"), 3, "a JSONL chat and an older JSON one");
        assert_eq!(count("qwen"), 1);
        assert_eq!(count("opencode"), if sqlite { 3 } else { 1 });
        // into rows: costs, sessions, turn time
        let rows = store::build(recs.clone(), &prices::Table::default(), 0);
        let by_agent = group(&rows, By::Agent);
        let claude = by_agent.iter().find(|r| r.key == "claude").unwrap();
        assert_eq!((claude.requests, claude.sessions), (7, 3));
        assert_eq!(claude.turn_ms, 64_000);
        assert!(claude.cost_reported > 0.0 && claude.cost_estimated > 0.0);
        let pi = by_agent.iter().find(|r| r.key == "pi").unwrap();
        assert!((pi.cost_reported - 0.0213).abs() < 1e-9);

        // again: nothing new is read (the database is always asked)
        let second = run_script(&home, &o, &known_of(&st));
        let reread: Vec<&str> = second
            .files
            .iter()
            .filter(|f| f.reader != "ocdb")
            .map(|f| f.path.as_str())
            .collect();
        assert!(reread.is_empty(), "{reread:?}");
        let cl = second.agents.iter().find(|x| x.agent == "claude").unwrap();
        assert_eq!((cl.listed, cl.unchanged), (4, 4));
        apply(&mut st, &second, &o.agents);

        // a new line in the session: only that is read
        let log = std::fs::read_dir(home.join(".claude/projects/-home-u-shop"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
            .unwrap();
        let line = r#"{"type":"assistant","message":{"model":"claude-opus-5-5","id":"msg_D","usage":{"input_tokens":1,"output_tokens":2,"cache_read_input_tokens":3}},"requestId":"req_D","timestamp":"2026-10-22T09:30:00.000Z","sessionId":"3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10","cwd":"/home/u/shop"}"#;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        writeln!(f, "{line}").unwrap();
        // and a half-written one after it, which waits
        write!(f, "{{\"type\":\"assistant\",\"message\":{{\"model\"").unwrap();
        drop(f);
        let third = run_script(&home, &o, &known_of(&st));
        let fr: Vec<&extract::FileRun> =
            third.files.iter().filter(|f| f.reader != "ocdb").collect();
        assert_eq!(fr.len(), 1, "{third:#?}");
        assert_eq!(fr[0].records.len(), 1);
        assert_eq!(fr[0].records[0].output, 2);
        let len = std::fs::metadata(&log).unwrap().len();
        assert!(fr[0].next.unwrap() < len, "the partial line isn't consumed");
        assert_eq!(fr[0].start, st.files[fr[0].path.as_str()].off);
        apply(&mut st, &third, &o.agents);

        // the log replaced by a shorter one: read again from the start
        std::fs::write(&log, format!("{line}\n")).unwrap();
        let fourth = run_script(&home, &o, &known_of(&st));
        assert!(
            fourth.reset.iter().any(|p| p.ends_with(".jsonl")),
            "{fourth:#?}"
        );
        let fr: Vec<&extract::FileRun> =
            fourth.files.iter().filter(|f| f.reader != "ocdb").collect();
        assert_eq!((fr.len(), fr[0].start, fr[0].records.len()), (1, 0, 1));

        // a tiny byte budget: the newest file goes first, the rest waits
        let tight = extract::Opts {
            max_bytes: 1500,
            ..extract::Opts::default()
        };
        let fifth = run_script(&home, &tight, &[]);
        let (why, left, _) = fifth.budget.clone().expect("out of budget");
        assert_eq!(why, "bytes");
        assert!(left > 0 && fifth.files.len() < first.files.len());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn durations() {
        assert_eq!(parse_days("90d"), Ok(90));
        assert_eq!(parse_days("12w"), Ok(84));
        assert_eq!(parse_days("1y"), Ok(365));
        assert_eq!(parse_days("30"), Ok(30));
        assert_eq!(parse_days("all"), Ok(0));
        assert!(parse_days("soon").is_err());
        assert!(parse_days("3h").is_err());
    }

    #[test]
    fn ranges() {
        let t = Range::today();
        assert_eq!(t.from, t.to);
        let w = Range::parse("7d").unwrap();
        assert_eq!(w.to - w.from, 6);
        let h48 = Range::parse("48h").unwrap();
        assert!(h48.to - h48.from <= 2 && h48.since.is_some());
        assert_eq!(Range::parse("yesterday").unwrap().from, t.from - 1);
        assert_eq!(Range::parse("2026-10-01").unwrap().from, 20_727);
        assert!(Range::parse("nonsense").is_none());
        assert_eq!(Range::days(7).label(), "last 7 days");
        assert_eq!(Range::today().label(), "today");
        assert_eq!(day_name(20_748), "2026-10-22");
    }

    #[test]
    fn include_and_exclude() {
        let c = Config::in_memory(&[]);
        assert!(allowed(&c, "b31", "claude"));
        let c = Config::in_memory(&["analytics.exclude=b31:codex,*:gemini"]);
        assert!(allowed(&c, "b31", "claude"));
        assert!(!allowed(&c, "b31", "codex"));
        assert!(!allowed(&c, "gpu2", "gemini"));
        let c = Config::in_memory(&["analytics.include=b31:*", "analytics.exclude=b31:pi"]);
        assert!(allowed(&c, "b31", "codex"));
        assert!(!allowed(&c, "gpu2", "codex"));
        assert!(!allowed(&c, "b31", "pi"));
        let o = opts_for(&c, "b31");
        assert!(!o.agents.contains(&"pi".to_string()) && o.agents.contains(&"claude".to_string()));
        assert!(opts_for(&c, "gpu2").agents.is_empty());
    }

    fn row(host: &str, agent: &str, session: &str, model: &str, day: i64, cost: f64) -> store::Row {
        store::Row {
            day,
            host: host.into(),
            agent: agent.into(),
            session: session.into(),
            model: model.into(),
            project: "~/p".into(),
            input: 10,
            output: 5,
            requests: 2,
            cost_estimated: cost,
            first: 100,
            last: 200,
            ..Default::default()
        }
    }

    #[test]
    fn grouping() {
        let rows = vec![
            row("b31", "claude", "s1", "claude-opus-5-5", 20_748, 2.0),
            row("b31", "claude", "s1", "claude-opus-5-5", 20_747, 1.0),
            row("gpu2", "codex", "s2", "gpt-5", 20_748, 0.5),
        ];
        let by_host = group(&rows, By::Host);
        assert_eq!(by_host[0].key, "b31");
        assert_eq!((by_host[0].requests, by_host[0].sessions), (4, 1));
        assert!((by_host[0].cost() - 3.0).abs() < 1e-9 && by_host[0].estimated());
        let by_day = group(&rows, By::Day);
        assert_eq!(by_day[0].key, "2026-10-21");
        assert_eq!(by_day[1].sessions, 2);
        let total = group(&rows, By::Total);
        assert_eq!(total.len(), 1);
        assert_eq!(total[0].hosts, vec!["b31", "gpu2"]);
        let none = BTreeSet::new();
        let f = Filter {
            hosts: vec!["gpu*".into()],
            ..Default::default()
        };
        assert!(f.keeps(&rows[2], &none) && !f.keeps(&rows[0], &none));
        let f = Filter {
            models: vec!["claude-opus-5-5[1m]".into()],
            ..Default::default()
        };
        assert!(f.keeps(&rows[0], &none) && !f.keeps(&rows[2], &none));
        // a list keeps any of it; lists together keep what all of them keep
        let f = Filter {
            hosts: vec!["b31".into(), "gpu2".into()],
            models: vec!["claude-opus-5-5".into()],
            ..Default::default()
        };
        assert!(f.keeps(&rows[0], &none) && !f.keeps(&rows[2], &none));
        // two levels: each machine's models
        let sp = split(&rows, By::Host, By::Model);
        assert_eq!(sp[0].0.key, "b31");
        assert!(!sp[0].1.is_empty() && sp[0].1.iter().all(|r| r.hosts == vec!["b31"]));
    }

    #[test]
    fn periods_rolling_calendar_and_custom() {
        let today = ui::local_day(crate::cache::now());
        let h = Range::parse("24h").unwrap();
        assert!(h.since.is_some() && h.to == today && h.to - h.from <= 1);
        assert_eq!(h.label(), "last 24 hours");
        let tz = ui::utc_offset();
        let now = crate::cache::now();
        let (d, hr) = (
            ui::local_day(now),
            ((now as i64 + tz).rem_euclid(86_400) / 3600) as u8,
        );
        assert!(h.keeps(d, hr, tz), "this hour is in the last 24");
        let old = ui::local_day(now - 30 * 3600);
        assert!(!h.keeps(
            old,
            ((now as i64 - 30 * 3600 + tz).rem_euclid(86_400) / 3600) as u8,
            tz
        ));
        // the 24 hours before: right up to where this one starts
        let p = h.prev();
        assert_eq!(p.until, h.since);
        assert!(!p.keeps(d, hr, tz));
        let w = Range::parse("this-week").unwrap();
        assert_eq!((w.from + 3).rem_euclid(7), 0, "from a Monday");
        assert!(w.to == today && w.to - w.from < 7);
        let m = Range::parse("this-month").unwrap();
        assert_eq!(ui::civil(m.from).2, 1);
        let lm = Range::parse("last-month").unwrap();
        assert_eq!((ui::civil(lm.from).2, lm.to), (1, m.from - 1));
        assert_eq!(Range::parse("6mo").unwrap().len_days(), 183);
        assert_eq!(Range::parse("3d").unwrap().len_days(), 3);
        let c = Range::parse("2026-09-01..2026-10-15").unwrap();
        assert_eq!(
            (day_name(c.from), day_name(c.to)),
            ("2026-09-01".into(), "2026-10-15".into())
        );
        assert_eq!(c.label(), "2026-09-01 – 2026-10-15");
        let pc = c.prev();
        assert_eq!((pc.to, pc.to - pc.from), (c.from - 1, c.to - c.from));
        assert_eq!(Range::parse("..2026-10-15").unwrap().from, 0);
        assert_eq!(Range::parse("2026-10-01..").unwrap().to, today);
        assert!(Range::parse("2026-13-01..").is_none());
        for (w, _) in PERIODS {
            assert!(Range::parse(w).is_some(), "{w}");
        }
    }

    #[test]
    fn distributions() {
        let d = Dist::of(vec![0.02, 0.05, 0.1, 0.4, 1.2, 0.0, 9.0]);
        assert_eq!((d.n, d.p50, d.max), (7, 0.1, 9.0));
        assert_eq!(d.p90, 9.0);
        // 1–3–10 buckets from 0.01, every value in one
        assert_eq!(d.buckets.iter().map(|b| b.2).sum::<u64>(), 7);
        assert_eq!(d.buckets[0].0, 0.0);
        assert!(d.buckets.windows(2).all(|w| w[0].1 == w[1].0));
        assert!(d.buckets.last().unwrap().1 > 9.0);
        assert_eq!(Dist::of(vec![]), Dist::default());
        assert_eq!(Dist::of(vec![f64::NAN, -1.0]).n, 0);
    }

    #[test]
    fn by_names() {
        assert_eq!(By::parse("hourly"), Some(By::Hour));
        assert_eq!(By::parse("accounts"), Some(By::Account));
        assert_eq!(By::parse("machine"), Some(By::Host));
        assert_eq!(By::parse("folder"), Some(By::Folder));
        assert_eq!(By::parse("repo"), Some(By::Project));
        assert_eq!(By::parse("nope"), None);
        assert_eq!(By::Session.name(), "session");
    }
}
