//! `aleph usage` — tokens, requests, sessions, active time and cost from the
//! agents' own logs, across the fleet:
//!
//! ```text
//! aleph usage [24h|3d|7d|30d|6mo|all|today|this-week|this-month|A..B|--since D]
//!             [--by host|agent|account|model|effort|project|day|hour|session]
//!             [--host H] [--agent A] [--account X] [--model M]   (each a comma list)
//!             [--stats] [--compare] [--json|--csv] [--no-sync]
//! aleph usage accounts [--json]      every account the agents were signed in to, and what each used
//! aleph usage projects [--json]      every project (a repo, wherever it was worked on), and what each used
//! aleph usage sources [--json]       what each machine's logs gave, and what was skipped
//! aleph usage prices [MODEL] [--all] [--json]   each model's rate and where it came from
//! aleph usage prices sync | accept MODEL        read the lists now; take a held change
//! aleph usage script [--host H]      the exact script a sync sends there
//! aleph usage sync [--host H] [--full] [--until-done] [--json]
//! aleph usage on|off [--host H] [--agent A]
//! aleph usage prune [--keep 400d] [--host H] [--dry-run]
//! ```
//!
//! With an OpenRouter key (`openrouter.key`), the account's own numbers
//! from its API go under the table and into `--json` (`openrouter`: spent
//! today / week / month / total, credits, the key's limit, how old).

use super::{store, By, Filter, Range, Report, Row};
use crate::cli::{self, Args};
use crate::config::Config;
use crate::out::{err, fail, head, hint, json, ok, out, say, warn, Spinner};
use crate::ui::{self, c};
use crate::{agents, theme};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

const USAGE: &str = "usage: aleph usage [24h|3d|7d|30d|6mo|all|today|this-week|this-month|A..B|--since D] [--by host|agent|account|model|effort|project|day|hour|session] [--host H] [--agent A] [--account X] [--stats] [--compare] [--json|--csv] · accounts · sources · script · sync · on|off · prune";

pub fn cmd(a: Args) -> ExitCode {
    match a.rest.first().map(String::as_str) {
        Some("sources") => sources(&a),
        Some("prices") => super::pricescmd::cmd(&a),
        Some("accounts") => accounts_cmd(&a),
        Some("projects") => projects_cmd(&a),
        Some("script") => script(&a),
        Some("sync") => sync_cmd(&a),
        Some("on") => toggle(a, true),
        Some("off") => toggle(a, false),
        Some("prune") => prune(&a),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            println!("  aleph help usage for the details");
            ExitCode::SUCCESS
        }
        _ => report(&a),
    }
}

/// `--host H` / `--host=H` / the global `--host`, and the other flags with values.
struct Flags {
    host: Option<String>,
    agent: Option<String>,
    model: Option<String>,
    project: Option<String>,
    account: Option<String>,
    by: Option<String>,
    since: Option<String>,
    keep: Option<String>,
    words: Vec<String>,
    switches: Vec<String>,
}

fn flags(a: &Args, skip: usize) -> Result<Flags, String> {
    let mut f = Flags {
        host: a.flag.clone(),
        agent: None,
        model: None,
        project: None,
        account: None,
        by: None,
        since: None,
        keep: None,
        words: Vec::new(),
        switches: Vec::new(),
    };
    let rest = &a.rest[skip.min(a.rest.len())..];
    let mut i = 0;
    while i < rest.len() {
        let w = rest[i].as_str();
        let (name, inline) = match w.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (w, None),
        };
        let slot = match name {
            "--host" | "-H" => Some(&mut f.host),
            "--agent" | "-a" => Some(&mut f.agent),
            "--model" | "-m" => Some(&mut f.model),
            "--project" | "-p" | "--dir" | "--folder" => Some(&mut f.project),
            "--account" | "-A" => Some(&mut f.account),
            "--by" | "-b" => Some(&mut f.by),
            "--since" => Some(&mut f.since),
            "--keep" => Some(&mut f.keep),
            _ => None,
        };
        match slot {
            Some(s) => {
                let v = match inline {
                    Some(v) => v,
                    None => {
                        i += 1;
                        rest.get(i)
                            .cloned()
                            .ok_or_else(|| format!("{name} needs a value"))?
                    }
                };
                *s = Some(v);
            }
            None if w.starts_with('-') => f.switches.push(w.to_string()),
            None => f.words.push(w.to_string()),
        }
        i += 1;
    }
    // one agent by any of its names (a list is resolved by `filter_of`)
    if let Some(ag) = f.agent.as_deref().filter(|a| !a.contains(',')) {
        let id = agents::resolve(ag)?;
        f.agent = Some(id.to_string());
    }
    Ok(f)
}

impl Flags {
    fn has(&self, s: &str) -> bool {
        self.switches.iter().any(|x| x == s)
    }
    fn hosts(&self, cfg: &Config) -> Vec<String> {
        match &self.host {
            // a glob reads every machine (the filter picks)
            Some(h) if !h.contains('*') => super::list_of(h),
            _ => cfg.fleet(),
        }
    }
}

// ------------------------------------------------------------------ numbers

/// `999`, `12.4k`, `3.1M`, `1.2B`.
pub fn compact(n: u64) -> String {
    let f = n as f64;
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => trim(f / 1e3, "k"),
        1_000_000..=999_999_999 => trim(f / 1e6, "M"),
        _ => trim(f / 1e9, "B"),
    }
}

fn trim(v: f64, unit: &str) -> String {
    if v >= 100.0 {
        format!("{v:.0}{unit}")
    } else {
        format!("{v:.1}{unit}")
    }
}

/// `$142.10`, `$0.03`, `<$0.01`, `free` (free models only), `–`.
pub fn money(r: &Row) -> String {
    let c = r.cost();
    if r.all_free() {
        return "free".into();
    }
    if c <= 0.0 {
        return if r.unpriced > 0 {
            "?".into()
        } else {
            "–".into()
        };
    }
    // showcase: scaled, or hidden (`showcase.money`)
    let Some(c) = crate::showcase::money(c) else {
        return crate::showcase::MONEY_HIDDEN.into();
    };
    if c < 0.01 {
        "<$0.01".to_string()
    } else if c < 1000.0 {
        format!("${c:.2}")
    } else {
        format!("${c:.0}")
    }
}

fn cost_text(r: &Row) -> String {
    let mut s = money(r);
    if r.unpriced > 0 && r.cost() > 0.0 {
        s.push('+');
    }
    if r.estimated() {
        s.push_str(" est.");
    }
    // some of it on free models: said, beside what the rest cost
    if r.free > 0 && !r.all_free() {
        s.push_str(&format!(" · {} free", r.free));
    }
    s
}

fn active_text(secs: u64) -> String {
    if secs == 0 {
        "–".into()
    } else {
        ui::dur(secs)
    }
}

/// A row's first column for a breakdown.
fn label(by: By, r: &Row) -> String {
    match by {
        By::Session => {
            let mut p = r.key.splitn(3, ':');
            let (h, a, s) = (
                p.next().unwrap_or(""),
                p.next().unwrap_or(""),
                p.next().unwrap_or(""),
            );
            let short: String = s.chars().take(8).collect();
            let proj = r.projects.first().cloned().unwrap_or_default();
            format!("{h} {a} {short} {proj}").trim_end().to_string()
        }
        By::Day => {
            let d = super::parse_date(&r.key).unwrap_or(0);
            const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
            format!("{} {}", r.key, WD[d.rem_euclid(7) as usize])
        }
        By::Account => {
            let n = super::accounts::name(&r.key);
            if r.guessed {
                format!("{n} ≈")
            } else {
                n
            }
        }
        By::Effort if r.key.is_empty() => "(not logged)".into(),
        By::Project => super::projects::name_of(&r.key),
        By::Model | By::Folder if r.key.is_empty() => "(unknown)".into(),
        _ => r.key.clone(),
    }
}

pub(crate) fn row_json(by: By, r: &Row) -> Value {
    json!({
        "key": r.key,
        "label": label(by, r),
        "input": r.input,
        "output": r.output,
        "cache_read": r.cache_read,
        "cache_write": r.cache_write(),
        "cache_write_5m": r.cache_write_5m,
        "cache_write_1h": r.cache_write_1h,
        "reasoning": r.reasoning,
        "tokens": r.tokens(),
        "requests": r.requests,
        "sessions": r.sessions,
        "prompts": r.prompts,
        "cost_per_prompt": r.per_prompt().map(|c| (c * 1e6).round() / 1e6),
        "cost_per_session": r.per_session().map(|c| (c * 1e6).round() / 1e6),
        "active_seconds": r.active_secs(),
        "turn_seconds": r.turn_ms / 1000,
        "cost": (r.cost() * 1e6).round() / 1e6,
        "cost_reported": (r.cost_reported * 1e6).round() / 1e6,
        "cost_estimated": (r.cost_estimated * 1e6).round() / 1e6,
        "estimated": r.estimated(),
        "unpriced_requests": r.unpriced,
        "free_requests": r.free,
        "web_searches": r.web,
        "first": r.first,
        "last": r.last,
        "hosts": r.hosts,
        "agents": r.agents,
        "models": r.models,
        "projects": r.projects,
        "accounts": r.accounts,
        "account_guessed": r.guessed,
    })
}

const COLS: &[&str] = &[
    "key",
    "input",
    "output",
    "cache_read",
    "cache_write",
    "reasoning",
    "requests",
    "sessions",
    "active_seconds",
    "cost",
    "cost_reported",
    "cost_estimated",
    "unpriced_requests",
    "prompts",
    "free_requests",
];

fn cells(by: By, r: &Row) -> Vec<String> {
    // showcase: dollars scaled, or left empty when hidden (`showcase.money`)
    let usd = |v: f64| crate::showcase::money(v).map_or_else(String::new, |v| format!("{v:.4}"));
    vec![
        if by == By::Session {
            r.key.clone()
        } else {
            label(by, r)
        },
        r.input.to_string(),
        r.output.to_string(),
        r.cache_read.to_string(),
        r.cache_write().to_string(),
        r.reasoning.to_string(),
        r.requests.to_string(),
        r.sessions.to_string(),
        r.active_secs().to_string(),
        usd(r.cost()),
        usd(r.cost_reported),
        usd(r.cost_estimated),
        r.unpriced.to_string(),
        r.prompts.to_string(),
        r.free.to_string(),
    ]
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ------------------------------------------------------------------ the report

/// Machines whose last read (worked or not) is older than `analytics.interval`:
/// a machine that's down is tried once per interval, not on every call.
fn stale(cfg: &Config, hosts: &[String]) -> Vec<String> {
    let st = store::Offsets::load();
    let iv = super::interval(cfg);
    let now = crate::cache::now();
    hosts
        .iter()
        .filter(|h| {
            let tried = st
                .hosts
                .get(*h)
                .and_then(|s| s.run.as_ref())
                .map(|r| r.at)
                .unwrap_or(0);
            now.saturating_sub(tried) >= iv
        })
        .cloned()
        .collect()
}

/// Sync with a spinner; failures as warnings.
fn sync_now(cfg: &Config, hosts: &[String], full: bool, wait: Duration) -> Vec<Report> {
    let sp = Spinner::start(format!(
        "Reading usage logs on {}",
        if hosts.len() == 1 {
            hosts[0].clone()
        } else {
            format!("{} machines", hosts.len())
        }
    ));
    let total = hosts.len();
    let mut done = 0;
    let reps = super::sync(cfg, hosts, full, wait, |r| {
        done += 1;
        if done < total {
            sp.set(format!(
                "Reading usage logs ({done}/{total}, {} done)",
                r.host
            ));
        }
    });
    sp.stop();
    reps
}

/// Every project and its last 30 days, the most used first (`usage projects
/// --json`, the API's `usage.projects`).
pub fn projects_json(cfg: &Config, range: Range) -> Value {
    let (rs, ix) = super::rows_ix(cfg, range, &Filter::default());
    let rows = super::group(&rs, By::Project);
    let mut list = super::projects::to_json(&ix);
    for v in &mut list {
        let id = v["id"].as_str().unwrap_or("").to_string();
        v["usage"] = rows
            .iter()
            .find(|r| r.key == id)
            .map(|r| row_json(By::Project, r))
            .unwrap_or(Value::Null);
    }
    list.sort_by(|x, y| {
        let c = |v: &Value| v["usage"]["cost"].as_f64().unwrap_or(0.0);
        c(y).total_cmp(&c(x))
    });
    json!({"range": range.label(), "projects": list})
}

/// `aleph usage projects [range]`: every project — one repo, on any machine,
/// its worktrees and subfolders — what it used, and where.
fn projects_cmd(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&format!("{e} (usage: aleph usage projects [30d] [--json])")),
    };
    let range = match f.words.first() {
        Some(w) => match Range::parse(w) {
            Some(r) => r,
            None => return fail(&format!("unknown range '{w}'")),
        },
        None => Range::days(30),
    };
    let cfg = &a.cfg;
    let v = projects_json(cfg, range);
    if f.has("--json") {
        json(&v);
        return ExitCode::SUCCESS;
    }
    let list = v["projects"].as_array().cloned().unwrap_or_default();
    if !std::io::stdout().is_terminal() {
        println!("id\tname\tremote\tmachines\ttokens\tcost");
        for p in &list {
            let hosts: Vec<&str> = p["machines"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m["host"].as_str())
                .collect();
            println!(
                "{}\t{}\t{}\t{}\t{}\t{:.4}",
                p["id"].as_str().unwrap_or(""),
                p["name"].as_str().unwrap_or(""),
                p["remote"].as_str().unwrap_or(""),
                hosts.join(","),
                p["usage"]["tokens"].as_u64().unwrap_or(0),
                crate::showcase::money(p["usage"]["cost"].as_f64().unwrap_or(0.0)).unwrap_or(0.0)
            );
        }
        return ExitCode::SUCCESS;
    }
    head(&format!("Projects · {}", range.label()));
    if list.is_empty() {
        say(Line::raw("  nothing used in this range"));
        return ExitCode::SUCCESS;
    }
    let nw = list
        .iter()
        .map(|p| ui::width(p["name"].as_str().unwrap_or("")))
        .max()
        .unwrap_or(8)
        .clamp(8, 32);
    for p in &list {
        let name = p["name"].as_str().unwrap_or("");
        let u = &p["usage"];
        let machines: Vec<String> = p["machines"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|m| {
                let n = m["folders"].as_array().map(|f| f.len()).unwrap_or(0);
                let h = m["host"].as_str().unwrap_or("");
                if n > 1 {
                    format!("{h} ({n} folders)")
                } else {
                    h.to_string()
                }
            })
            .collect();
        out(Line::from(vec![
            Span::styled(format!("  {}", ui::pad(name, nw)), ui::bold()),
            Span::styled(
                format!("  {:>8}", compact(u["tokens"].as_u64().unwrap_or(0))),
                ui::muted(),
            ),
            Span::styled(
                format!("  {:>10}", usd(u["cost"].as_f64().unwrap_or(0.0))),
                ui::fg(c::ACCENT()),
            ),
            Span::styled(format!("  {}", machines.join(", ")), ui::faint()),
            Span::styled(
                p["remote"]
                    .as_str()
                    .map(|r| format!("  {r}"))
                    .unwrap_or_default(),
                ui::faint(),
            ),
        ]));
    }
    hint("one repo is one project: its remote, else its first commit · merge or rename: aleph settings set analytics.projects.<name> github.com/x/y,b31:~/src/y");
    ExitCode::SUCCESS
}

/// Every account and its last 30 days, the most used first (`usage accounts
/// --json`, the API's `usage.accounts`).
pub fn accounts_json(cfg: &Config) -> Value {
    let b = super::accounts::Book::load();
    let rows = super::group(
        &super::rows(cfg, Range::days(30), &Filter::default()),
        By::Account,
    );
    let used = |id: &str| rows.iter().find(|r| r.key == id);
    let mut list = super::accounts::to_json(cfg, &b);
    for v in &mut list {
        let id = v["account"].as_str().unwrap_or("").to_string();
        v["usage_30d"] = used(&id)
            .map(|r| row_json(By::Account, r))
            .unwrap_or(Value::Null);
    }
    list.sort_by(|x, y| {
        let c = |v: &Value| v["usage_30d"]["cost"].as_f64().unwrap_or(0.0);
        c(y).total_cmp(&c(x))
    });
    json!({
        "accounts": list,
        "unknown_30d": used("").map(|r| row_json(By::Account, r)),
    })
}

/// `aleph usage accounts`: every account the agents were signed in to — its
/// agent, plan, machines — and what it used in the last 30 days (≈: some of
/// it guessed, from before Aleph knew whose).
fn accounts_cmd(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&format!("{e} (usage: aleph usage accounts [--json])")),
    };
    let cfg = &a.cfg;
    let rows = super::group(
        &super::rows(cfg, Range::days(30), &Filter::default()),
        By::Account,
    );
    let used = |id: &str| rows.iter().find(|r| r.key == id);
    let all = accounts_json(cfg);
    if f.has("--json") {
        json(&all);
        return ExitCode::SUCCESS;
    }
    let list: Vec<Value> = all["accounts"].as_array().cloned().unwrap_or_default();
    let unknown = used("");
    if list.is_empty() {
        say(Line::raw(
            "No accounts seen yet: they are read with the usage logs (aleph usage sync).",
        ));
        return ExitCode::SUCCESS;
    }
    if !std::io::stdout().is_terminal() {
        println!("id\tagent\tlabel\tkind\tplan\thosts\ttokens_30d\tcost_30d");
        for v in &list {
            let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
            let hosts: Vec<&str> = v["hosts"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.4}",
                s("id"),
                s("agent"),
                s("label"),
                s("kind"),
                s("plan"),
                hosts.join(","),
                v["usage_30d"]["tokens"].as_u64().unwrap_or(0),
                crate::showcase::money(v["usage_30d"]["cost"].as_f64().unwrap_or(0.0))
                    .unwrap_or(0.0)
            );
        }
        return ExitCode::SUCCESS;
    }
    head("Accounts · last 30 days");
    let lw = list
        .iter()
        .map(|v| ui::width(v["label"].as_str().unwrap_or("")))
        .max()
        .unwrap_or(8)
        .clamp(8, 44);
    for v in &list {
        let s = |k: &str| v[k].as_str().unwrap_or("").to_string();
        let id = s("account");
        let r = used(&id);
        let hosts: Vec<&str> = v["hosts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let line = Line::from(vec![
            Span::styled(format!("  {:<8} ", s("id")), ui::faint()),
            Span::styled(
                format!("{:<12}", agents::label(&s("agent"))),
                ui::fg(c::ACCENT()),
            ),
            Span::raw(ui::pad(&s("label"), lw)),
            Span::styled(
                format!(
                    "  {:>8}",
                    r.map(|r| compact(r.tokens())).unwrap_or_else(|| "–".into())
                ),
                ui::muted(),
            ),
            Span::styled(
                format!("  {:>10}", r.map(cost_text).unwrap_or_else(|| "–".into())),
                ui::bold(),
            ),
            Span::styled(
                if r.is_some_and(|r| r.guessed) {
                    " ≈"
                } else {
                    "  "
                },
                ui::faint(),
            ),
            Span::styled(format!("  {}", hosts.join(", ")), ui::faint()),
        ]);
        out(line);
    }
    if let Some(u) = unknown {
        out(Line::from(vec![
            Span::styled(format!("  {:<8} {:<12}", "", ""), ui::faint()),
            Span::styled(ui::pad("account unknown", lw), ui::muted()),
            Span::styled(format!("  {:>8}", compact(u.tokens())), ui::muted()),
            Span::styled(format!("  {:>10}", cost_text(u)), ui::muted()),
        ]));
    }
    hint("≈ some of it guessed (from before Aleph knew whose) · name one: aleph settings set analytics.accounts.<id> work");
    ExitCode::SUCCESS
}

fn report(a: &Args) -> ExitCode {
    let f = match flags(a, 0) {
        Ok(f) => f,
        Err(e) => return fail(&format!("{e} ({USAGE})")),
    };
    let by = match f.by.as_deref() {
        None => By::Model,
        Some(b) => match By::parse(b) {
            Some(b) => b,
            None => return fail(&format!(
                "--by takes host, agent, account, model, effort, project, day, hour or session (not {b})"
            )),
        },
    };
    let mut range = Range::days(7);
    if let Some(w) = f.words.first() {
        match Range::parse(w) {
            Some(r) => range = r,
            None => return fail(&format!("unknown range '{w}' ({USAGE})")),
        }
    }
    if f.words.len() > 1 {
        return fail(&format!("unexpected '{}' ({USAGE})", f.words[1]));
    }
    if let Some(s) = &f.since {
        match super::parse_date(s)
            .map(Range::since)
            .or_else(|| Range::parse(s))
        {
            Some(r) => range = r,
            None => {
                return fail(&format!(
                    "--since takes a date (2026-10-01) or 7d / 4w, not {s}"
                ))
            }
        }
    }
    let want_json = f.has("--json");
    let want_csv = f.has("--csv");
    for s in &f.switches {
        if ![
            "--json",
            "--csv",
            "--no-sync",
            "--sync",
            "--stats",
            "--compare",
            "--explore",
        ]
        .contains(&s.as_str())
        {
            return fail(&format!("unknown option '{s}' ({USAGE})"));
        }
    }
    let cfg = &a.cfg;
    let hosts = f.hosts(cfg);
    let on = super::enabled(cfg);
    let mut failed: Vec<Report> = Vec::new();
    let mut behind: Vec<Report> = Vec::new();
    if on && !f.has("--no-sync") {
        let due = if f.has("--sync") {
            hosts.clone()
        } else {
            stale(cfg, &hosts)
        };
        if !due.is_empty() {
            // the daemon may be reading right now: show what's stored rather than wait
            let wait = Duration::from_secs(if f.has("--sync") { 90 } else { 3 });
            for r in sync_now(cfg, &due, false, wait) {
                if r.error.is_some() {
                    failed.push(r);
                } else if r.budget.is_some() {
                    behind.push(r);
                }
            }
        }
    }
    let l = |v: &Option<String>| v.as_deref().map(super::list_of).unwrap_or_default();
    let filter = match super::filter_of(
        cfg,
        &l(&f.host),
        &l(&f.agent),
        &l(&f.account),
        &l(&f.model),
        &l(&f.project),
    ) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let srows = super::rows(cfg, range, &filter);
    let rows = super::group(&srows, by);
    let total = super::group(&srows, By::Total).pop().unwrap_or_default();
    let stats = f
        .has("--stats")
        .then(|| super::stats(cfg, range, &filter, 10));
    let previous = f.has("--compare").then(|| {
        super::group(&super::rows(cfg, range.prev(), &filter), By::Total)
            .pop()
            .unwrap_or_default()
    });
    let mut unpriced: Vec<String> = srows
        .iter()
        .filter(|r| r.unpriced > 0)
        .map(|r| r.model.clone())
        .collect();
    unpriced.sort();
    unpriced.dedup();
    let synced = super::last_ok();
    // the OpenRouter account's own numbers (its API), when there's a key:
    // what's kept when it's fresh, else read now; --no-sync: only what's kept
    let openrouter = if f.has("--no-sync") {
        crate::openrouter::configured(cfg)
            .then(crate::openrouter::cached)
            .flatten()
    } else {
        crate::openrouter::spend(cfg, crate::openrouter::FRESH)
    };
    let now = crate::cache::now();
    if want_json {
        json(&json!({
            "openrouter": openrouter.as_ref().map(|s| s.shown_json(now)),
            "range": {"from": super::day_name(range.from.max(0)), "to": super::day_name(range.to), "label": range.label()},
            "by": by.name(),
            "generated_at": crate::cache::now(),
            "enabled": on,
            "rows": rows.iter().map(|r| row_json(by, r)).collect::<Vec<_>>(),
            "total": row_json(By::Total, &total),
            "unpriced_models": unpriced,
            "stats": stats.as_ref().map(stats_json),
            // Usage's screens beyond the totals, split by --by (accounts by default)
            "explore": f.has("--explore").then(|| {
                let split = f.by.as_deref().and_then(By::parse).filter(|b| *b != By::Total).unwrap_or(By::Account);
                explore_json(cfg, range, &filter, split)
            }),
            "previous": previous.as_ref().map(|p| {
                let mut v = row_json(By::Total, p);
                v["range"] = json!(range.prev().label());
                v
            }),
            "synced": synced,
            "errors": failed.iter().map(Report::to_json).collect::<Vec<_>>(),
            // plan windows taken from the logs: host → agent → id → {ts, plan, windows}
            "limits": super::store::limits(),
        }));
        return ExitCode::SUCCESS;
    }
    if want_csv || !std::io::stdout().is_terminal() {
        let sep = if want_csv { "," } else { "\t" };
        println!("{}", COLS.join(sep));
        for r in &rows {
            let c = cells(by, r);
            let c: Vec<String> = if want_csv {
                c.iter().map(|x| csv_cell(x)).collect()
            } else {
                c.iter().map(|x| x.replace('\t', " ")).collect()
            };
            crate::out::raw(&c.join(sep));
        }
        for r in &failed {
            err(&format!(
                "{}: {}",
                r.host,
                r.error.clone().unwrap_or_default()
            ));
        }
        return ExitCode::SUCCESS;
    }
    for l in table(cfg, by, range, &hosts, &rows, &total, ui::print_width()) {
        out(l);
    }
    if let Some(p) = &previous {
        out(Line::from(vec![
            Span::styled(format!("  vs {}: ", range.prev().label()), ui::faint()),
            Span::raw(format!("{} ", money(p))),
            change_span(total.cost(), p.cost()),
        ]));
    }
    if let Some(st) = &stats {
        out(Line::raw(""));
        for l in stats_lines(st) {
            out(l);
        }
    }
    if let Some(s) = &openrouter {
        out(Line::raw(""));
        for l in openrouter_lines(s, now) {
            out(l);
        }
        if let Some(e) = &s.error {
            warn(&format!("OpenRouter: {e} — showing what was read before"));
        }
    }
    for r in &failed {
        warn(&format!(
            "{}: {} — showing what was read before",
            r.host,
            r.error.clone().unwrap_or_default()
        ));
    }
    for r in &behind {
        if let Some((_, n, b)) = &r.budget {
            hint(&format!(
                "{}: {} ({}) of older logs still to read — aleph usage sync --until-done, or let aleph watch catch up",
                r.host,
                plural(*n, "file"),
                ui::bytes(*b as f64)
            ));
        }
    }
    if !unpriced.is_empty() {
        hint(&format!(
            "no price for {} — aleph settings set analytics.prices.{} <in>,<out>[,<cache write 5m>,<1h>,<cache read>] ($ per million tokens)",
            unpriced.join(", "),
            unpriced[0]
        ));
    }
    if !on {
        warn("analytics is off: nothing new is read — aleph usage on");
    } else if rows.is_empty() {
        hint("nothing logged in that range — aleph usage sources shows what each machine has");
    }
    ExitCode::SUCCESS
}

/// The OpenRouter account under the table (pure — tested): what it spent
/// today, this week, this month and in all, then credits and the key's
/// limit left, and how old the numbers are.
pub(crate) fn openrouter_lines(s: &crate::openrouter::Spend, now: u64) -> Vec<Line<'static>> {
    use super::dash::money;
    let lab = |t: &str| Span::styled(format!("{t:<13}"), ui::bold());
    let sep = || Span::styled(" · ", ui::faint());
    let mut v = Vec::new();
    if !s.any() {
        v.push(Line::from(vec![
            lab("OpenRouter"),
            Span::styled("no numbers yet", ui::muted()),
        ]));
        return v;
    }
    let mut l1 = vec![lab("OpenRouter")];
    for (i, (what, x)) in [
        ("today", s.today),
        ("this week", s.week),
        ("this month", s.month),
        ("total", s.usage),
    ]
    .into_iter()
    .filter(|(_, x)| x.is_some())
    .enumerate()
    {
        if i > 0 {
            l1.push(sep());
        }
        l1.push(Span::styled(format!("{what} "), ui::muted()));
        l1.push(Span::styled(
            money(x.unwrap_or(0.0), false),
            ui::fg(c::ACCENT()),
        ));
    }
    v.push(Line::from(l1));
    let mut l2 = vec![Span::raw(" ".repeat(13))];
    let low = |left: f64, of: f64| {
        if of > 0.0 && left / of < 0.1 {
            c::RED()
        } else if of > 0.0 && left / of < 0.25 {
            c::YELLOW()
        } else {
            c::GREEN()
        }
    };
    if let (Some(left), Some(total)) = (s.left(), s.credits) {
        l2.push(Span::styled("credits ", ui::muted()));
        l2.push(Span::styled(money(left, false), ui::fg(low(left, total))));
        l2.push(Span::styled(
            format!(" left of {}", money(total, false)),
            ui::muted(),
        ));
    }
    if let Some(lim) = s.limit.filter(|l| *l > 0.0) {
        if l2.len() > 1 {
            l2.push(sep());
        }
        let left = s.limit_left.unwrap_or(lim);
        l2.push(Span::styled("limit ", ui::muted()));
        l2.push(Span::styled(money(left, false), ui::fg(low(left, lim))));
        l2.push(Span::styled(
            format!(
                " left of {}{}",
                money(lim, false),
                s.limit_reset
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            ),
            ui::muted(),
        ));
    }
    if l2.len() > 1 {
        l2.push(sep());
    }
    let age = now.saturating_sub(s.at);
    l2.push(if s.stale(now) {
        Span::styled(
            format!("read {} — stale", ui::ago(age)),
            ui::fg(c::YELLOW()),
        )
    } else {
        Span::styled(format!("read {}", ui::ago(age)), ui::faint())
    });
    v.push(Line::from(l2));
    v
}

/// The human table.
/// `▲ 18%` (more than before, in the accent), `▼ 7%`, `=`, `new`.
pub(crate) fn change_span(now: f64, before: f64) -> Span<'static> {
    if before <= 0.0 {
        return Span::styled(if now > 0.0 { "new" } else { "=" }, ui::faint());
    }
    let pct = 100.0 * (now - before) / before;
    if pct.abs() < 0.5 {
        Span::styled("=", ui::faint())
    } else if pct > 0.0 {
        Span::styled(format!("▲ {pct:.0}%"), ui::fg(c::PEACH()))
    } else {
        Span::styled(format!("▼ {:.0}%", -pct), ui::fg(c::GREEN()))
    }
}

/// `$0.42`: one prompt's or session's cost (showcase: scaled or hidden).
fn usd(v: f64) -> String {
    match crate::showcase::money(v) {
        None => crate::showcase::MONEY_HIDDEN.into(),
        Some(v) if v < 0.01 && v > 0.0 => "<$0.01".into(),
        Some(v) if v < 1000.0 => format!("${v:.2}"),
        Some(v) => format!("${v:.0}"),
    }
}

/// The stats as JSON (`--stats`, `usage.stats`).
/// What Usage's screens show beyond the totals (`usage.explore`, `aleph usage
/// --explore --json`): the cost broken up by `split`, the days stacked by it,
/// this period against the one before, a day's tokens · requests · prompts ·
/// active hours · cache hits, the model mix, the week's hours, the 12 weeks,
/// the tools, the plans, the stats and the insights.
pub fn explore_json(cfg: &Config, range: Range, filter: &Filter, split: By) -> Value {
    let rs = super::rows(cfg, range, filter);
    let book = super::accounts::Book::load();
    let names: std::collections::HashMap<String, String> = book
        .accounts
        .keys()
        .map(|id| (id.clone(), super::accounts::label(cfg, &book, id)))
        .collect();
    let m = super::explore::more(
        cfg,
        range,
        filter,
        split,
        By::Agent,
        By::Account,
        &rs,
        &names,
    );
    let usd = |v: f64| json!(crate::showcase::money(v));
    // the week's hours, Monday first (day 0, 1970-01-01, was a Thursday)
    let mut heat_cost = vec![vec![0.0f64; 24]; 7];
    let mut heat_tokens = vec![vec![0u64; 24]; 7];
    for r in &rs {
        let wd = (r.day + 3).rem_euclid(7) as usize;
        let h = r.hour as usize % 24;
        heat_cost[wd][h] += r.cost_reported + r.cost_estimated;
        heat_tokens[wd][h] +=
            r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
    }
    json!({
        "range": range.label(),
        "split": split.name(),
        "breakup": m.breakup.iter().map(|(k, l, c, t)| json!({"key": k, "label": l, "cost": usd(*c), "tokens": t})).collect::<Vec<_>>(),
        "series": m.series.iter().map(|(k, l)| json!({"key": k.trim_start_matches('\u{0}'), "label": l})).collect::<Vec<_>>(),
        "daily": m.daily.iter().map(|(d, l, v)| json!({
            "day": super::day_name(*d), "label": l, "cost": v.iter().map(|x| usd(*x)).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "cumulative": {
            "now": m.cum.iter().map(|x| usd(*x)).collect::<Vec<_>>(),
            "before": m.cum_prev.iter().map(|x| usd(*x)).collect::<Vec<_>>(),
        },
        "per_day": m.per_day,
        "models": m.models,
        "model_mix": m.model_mix.iter().map(|(l, v)| json!({"label": l, "shares": v})).collect::<Vec<_>>(),
        "heat": {
            "cost": heat_cost.iter().map(|w| w.iter().map(|x| usd(*x)).collect::<Vec<_>>()).collect::<Vec<_>>(),
            "tokens": heat_tokens,
        },
        "calendar": m.calendar.iter().map(|(d, c)| json!({"day": super::day_name(*d), "cost": usd(*c)})).collect::<Vec<_>>(),
        "tools": m.tools.iter().map(|(n, calls, bad)| json!({"name": n, "calls": calls, "failed": bad})).collect::<Vec<_>>(),
        "parallel": m.parallel,
        "insights": m.insights.iter().map(|(l, v)| json!({"label": l, "value": crate::showcase::text(v)})).collect::<Vec<_>>(),
        "plans": m.plans.iter().map(|(a, p, fee, value)| json!({
            "account": crate::showcase::text(a), "plan": p, "monthly": usd(*fee), "value": usd(*value),
        })).collect::<Vec<_>>(),
        "stats": stats_json(&m.stats),
    })
}

pub fn stats_json(st: &super::Stats) -> Value {
    let money_dist = |d: &super::Dist| {
        let mut v = d.to_json();
        // showcase: scaled, or left out when hidden
        for k in ["sum", "mean", "p50", "p90", "p99", "max"] {
            v[k] = json!(crate::showcase::money(v[k].as_f64().unwrap_or(0.0)));
        }
        v
    };
    json!({
        "prompt_cost": money_dist(&st.prompt_cost),
        "prompt_seconds": st.prompt_secs.to_json(),
        "prompt_tokens": st.prompt_tokens.to_json(),
        "prompt_requests": st.prompt_requests.to_json(),
        "session_cost": money_dist(&st.session_cost),
        "session_seconds": st.session_secs.to_json(),
        "top_prompts": st.top_prompts.iter().map(|p| json!({
            "ts": p.ts, "seconds": p.secs, "host": p.host, "agent": p.agent, "session": p.session,
            "account": p.acct, "account_label": super::accounts::name(&p.acct), "guessed": p.guess,
            "project": p.project, "model": p.model, "tokens": p.tokens, "requests": p.requests,
            "cost": crate::showcase::money(p.cost), "estimated": p.estimated,
        })).collect::<Vec<_>>(),
        "top_sessions": st.top_sessions.iter().map(|r| row_json(By::Session, r)).collect::<Vec<_>>(),
        "outside_prompts": {"requests": st.outside.0, "cost": crate::showcase::money(st.outside.1)},
    })
}

/// The stats as lines under the table.
fn stats_lines(st: &super::Stats) -> Vec<Line<'static>> {
    let mut v = vec![Line::from(Span::styled(
        "Per prompt and per session",
        ui::bold(),
    ))];
    let kv = |k: &str, val: String| {
        Line::from(vec![
            Span::styled(format!("  {:<18}", k), ui::faint()),
            Span::raw(val),
        ])
    };
    let d = &st.prompt_cost;
    if d.n == 0 {
        v.push(kv(
            "prompts",
            "none finished (only Claude Code and Codex write turn records yet)".into(),
        ));
    } else {
        v.push(kv(
            "prompts",
            format!(
                "{} · median {} · p90 {} · the costliest {}",
                d.n,
                usd(d.p50),
                usd(d.p90),
                usd(d.max)
            ),
        ));
        let t = &st.prompt_secs;
        v.push(kv(
            "time per prompt",
            format!(
                "median {} · p90 {} · longest {}",
                ui::dur(t.p50 as u64),
                ui::dur(t.p90 as u64),
                ui::dur(t.max as u64)
            ),
        ));
        let r = &st.prompt_requests;
        v.push(kv(
            "requests / prompt",
            format!("median {:.0} · p90 {:.0} · most {:.0}", r.p50, r.p90, r.max),
        ));
    }
    let s = &st.session_cost;
    if s.n > 0 {
        v.push(kv(
            "sessions",
            format!(
                "{} · median {} · p90 {} · the costliest {}",
                s.n,
                usd(s.p50),
                usd(s.p90),
                usd(s.max)
            ),
        ));
    }
    if st.outside.0 > 0 {
        v.push(kv(
            "in no prompt",
            format!(
                "{} requests · {} (agents that don't mark their turns)",
                st.outside.0,
                usd(st.outside.1)
            ),
        ));
    }
    if let Some(p) = st.top_prompts.first() {
        v.push(kv(
            "costliest prompt",
            format!(
                "{} · {} {} · {} · {} · {} requests",
                usd(p.cost),
                p.host,
                agents::label(&p.agent),
                p.project,
                ui::dur(p.secs),
                p.requests
            ),
        ));
    }
    v
}

fn table(
    cfg: &Config,
    by: By,
    range: Range,
    hosts: &[String],
    rows: &[Row],
    total: &Row,
    width: usize,
) -> Vec<Line<'static>> {
    let mut v: Vec<Line<'static>> = Vec::new();
    let what = if hosts.len() == 1 {
        hosts[0].clone()
    } else {
        format!("{} machines", hosts.len())
    };
    v.push(Line::from(vec![
        Span::styled("⏺ ", ui::fg(c::ACCENT())),
        Span::styled(format!("Usage · {}", range.label()), ui::bold()),
        Span::styled(format!("  ·  {what} · by {}", by.name()), ui::faint()),
    ]));
    if rows.is_empty() {
        v.push(Line::from(Span::styled("  nothing yet", ui::muted())));
        return v;
    }
    // columns: (head, width, text per row), dropped right to left when narrow
    let heads = [
        "input",
        "output",
        "cache read",
        "cache write",
        "reqs",
        "sess",
        "active",
        "cost",
    ];
    let vals = |r: &Row| -> [String; 8] {
        [
            compact(r.input),
            compact(r.output),
            compact(r.cache_read),
            compact(r.cache_write()),
            r.requests.to_string(),
            r.sessions.to_string(),
            active_text(r.active_secs()),
            cost_text(r),
        ]
    };
    let all: Vec<[String; 8]> = rows
        .iter()
        .chain(std::iter::once(total))
        .map(vals)
        .collect();
    let mut w: Vec<usize> = heads
        .iter()
        .enumerate()
        .map(|(i, h)| {
            all.iter()
                .map(|x| ui::width(&x[i]))
                .max()
                .unwrap_or(0)
                .max(ui::width(h))
        })
        .collect();
    let lw = rows
        .iter()
        .map(|r| ui::width(&label(by, r)))
        .max()
        .unwrap_or(5)
        .clamp(5, 36);
    // drop the least important columns until it fits: cache write, sessions,
    // cache read, active
    let mut shown = [true; 8];
    let fit = |shown: &[bool; 8], w: &[usize]| {
        2 + lw
            + (0..8)
                .filter(|&i| shown[i])
                .map(|i| w[i] + 2)
                .sum::<usize>()
    };
    for drop in [3usize, 5, 2, 6] {
        if fit(&shown, &w) <= width {
            break;
        }
        shown[drop] = false;
    }
    let lw = if fit(&shown, &w) > width {
        lw.saturating_sub(fit(&shown, &w) - width).max(8)
    } else {
        lw
    };
    for (i, s) in shown.iter().enumerate() {
        if !s {
            w[i] = 0;
        }
    }
    let mut head = format!("  {}", ui::pad(by.name(), lw));
    for i in 0..8 {
        if shown[i] {
            head.push_str(&format!("  {:>1$}", heads[i], w[i]));
        }
    }
    v.push(Line::from(Span::styled(head, ui::faint())));
    let line_for = |name: Span<'static>, vals: &[String; 8], bold: bool| -> Line<'static> {
        let mut l = vec![Span::raw("  "), name];
        for i in 0..8 {
            if !shown[i] {
                continue;
            }
            let txt = format!("  {:>1$}", vals[i], w[i]);
            let st = match i {
                7 if vals[i].ends_with("est.") => ui::fg(c::YELLOW()),
                7 => ui::fg(c::GREEN()),
                4..=6 => ui::muted(),
                _ => Style::new(),
            };
            l.push(Span::styled(
                txt,
                if bold {
                    st.add_modifier(Modifier::BOLD)
                } else {
                    st
                },
            ));
        }
        Line::from(l)
    };
    for (r, vals) in rows.iter().zip(&all) {
        let name = ui::pad(&label(by, r), lw);
        let st = match by {
            By::Host => Style::new().fg(theme::machine(cfg, &r.key)),
            By::Agent => ui::bold(),
            _ => Style::new(),
        };
        v.push(line_for(Span::styled(name, st), vals, false));
    }
    if rows.len() > 1 {
        let full = fit(&shown, &w).min(width).saturating_sub(2);
        v.push(Line::from(Span::styled(
            format!("  {}", "─".repeat(full.max(10))),
            ui::fg(c::LINE()),
        )));
        let last = all.last().expect("the total");
        v.push(line_for(
            Span::styled(ui::pad("total", lw), ui::bold()),
            last,
            true,
        ));
    }
    // when each machine was read
    let synced = super::last_ok();
    let now = crate::cache::now();
    let mut s = vec![Span::styled("  read ", ui::faint())];
    for (i, h) in hosts.iter().enumerate() {
        if i > 0 {
            s.push(Span::styled(" · ", ui::faint()));
        }
        s.push(Span::styled(
            h.clone(),
            Style::new().fg(theme::machine(cfg, h)),
        ));
        let t = synced.get(h).copied().unwrap_or(0);
        s.push(Span::styled(
            format!(
                " {}",
                if t == 0 {
                    "never".to_string()
                } else {
                    ui::ago(now.saturating_sub(t))
                }
            ),
            ui::faint(),
        ));
    }
    v.push(Line::from(s));
    v
}

// ------------------------------------------------------------------ sources

fn tilde(p: &str, home: &str) -> String {
    if !home.is_empty() && home != "/" {
        if let Some(r) = p.strip_prefix(home) {
            if r.is_empty() || r.starts_with('/') {
                return format!("~{r}");
            }
        }
    }
    p.to_string()
}

/// `aleph usage sources --json` (and the API's `usage.sources`): what the
/// analytics read on each machine.
pub fn sources_json(cfg: &crate::config::Config, hosts: &[String]) -> Value {
    let st = store::Offsets::load();
    let book = super::accounts::Book::load();
    let others: Vec<&str> = agents::ALL
        .iter()
        .map(|g| g.id)
        .filter(|id| !super::extract::READERS.contains(id))
        .collect();
    let hs: Vec<Value> = hosts
        .iter()
        .map(|h| {
            let hs = st.hosts.get(h);
            let files = hs.map(|x| x.files.len()).unwrap_or(0);
            let bytes: u64 = hs
                .map(|x| x.files.values().map(|f| f.size).sum())
                .unwrap_or(0);
            json!({
                "host": h,
                "enabled": super::enabled(cfg),
                "agents": super::opts_for(cfg, h).agents,
                "files_tracked": files,
                "bytes_tracked": bytes,
                "ok_at": hs.map(|x| x.ok_at).unwrap_or(0),
                "last_run": hs.and_then(|x| x.run.as_ref()).map(store::RunInfo::to_json),
                // whose each log folder is (its latest sighting): root → account
                "accounts": hs
                    .and_then(|x| x.run.as_ref())
                    .map(|r| {
                        r.agents
                            .iter()
                            .flat_map(|a| a.roots.iter())
                            .filter_map(|root| {
                                let id = super::parse::root_id(h, root);
                                book.seen
                                    .iter()
                                    .filter(|s| s.host == *h && s.root == id)
                                    .max_by_key(|s| s.to)
                                    .map(|s| json!({"root": root, "account": s.acct, "id": book.short(&s.acct), "label": super::accounts::label(cfg, &book, &s.acct)}))
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            })
        })
        .collect();
    json!({
        "hosts": hs,
        "store": {"dir": store::dir(), "months": store::months()},
        "no_reader": others.iter().map(|id| json!({"agent": id, "why": super::no_reader_reason(id)})).collect::<Vec<_>>(),
    })
}

fn sources(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let cfg = &a.cfg;
    let hosts = f.hosts(cfg);
    let st = store::Offsets::load();
    let book = super::accounts::Book::load();
    let now = crate::cache::now();
    let others: Vec<&str> = agents::ALL
        .iter()
        .map(|g| g.id)
        .filter(|id| !super::extract::READERS.contains(id))
        .collect();
    if f.has("--json") {
        json(&sources_json(cfg, &hosts));
        return ExitCode::SUCCESS;
    }
    head(&format!(
        "Usage sources · {}",
        if hosts.len() == 1 {
            hosts[0].clone()
        } else {
            format!("{} machines", hosts.len())
        }
    ));
    if !super::enabled(cfg) {
        warn("analytics is off — aleph usage on");
    }
    for h in &hosts {
        let col = theme::machine(cfg, h);
        let mut l = vec![
            Span::styled("▌", ui::fg(col)),
            Span::styled(h.clone(), Style::new().fg(col).add_modifier(Modifier::BOLD)),
        ];
        let hs = st.hosts.get(h);
        let run = hs.and_then(|x| x.run.as_ref());
        let on_here = super::opts_for(cfg, h).agents;
        match run {
            None => l.push(Span::styled("  never read — aleph usage sync", ui::muted())),
            Some(r) => match &r.error {
                Some(e) => l.push(Span::styled(
                    format!("  ✕ {e} ({})", ui::ago(now.saturating_sub(r.at))),
                    ui::fg(c::RED()),
                )),
                None => l.push(Span::styled(
                    format!(
                        "  read {} in {:.1}s · {}",
                        ui::ago(now.saturating_sub(r.at)),
                        r.took_ms as f64 / 1000.0,
                        gained(r.files_read, r.bytes, r.records)
                    ),
                    ui::faint(),
                )),
            },
        }
        out(Line::from(l));
        if on_here.is_empty() {
            out(Line::from(Span::styled(
                "  excluded by analytics.include / analytics.exclude",
                ui::muted(),
            )));
            continue;
        }
        let Some(r) = run else { continue };
        let home = r_home(hs);
        // whose each log folder is: its latest sighting
        let whose = |root: &str| -> Option<String> {
            let id = super::parse::root_id(h, root);
            book.seen
                .iter()
                .filter(|s| s.host == *h && s.root == id)
                .max_by_key(|s| s.to)
                .map(|s| super::accounts::label(cfg, &book, &s.acct))
        };
        let tracked = |agent: &str| -> (usize, u64) {
            hs.map(|x| {
                let v: Vec<&store::FileState> =
                    x.files.values().filter(|f| f.agent == agent).collect();
                (v.len(), v.iter().map(|f| f.size).sum())
            })
            .unwrap_or((0, 0))
        };
        for ag in super::extract::READERS {
            let seen = r.agents.iter().find(|x| x.agent == *ag);
            let name = format!("  {}", ui::pad(ag, 9));
            if !on_here.iter().any(|x| x == ag) {
                out(Line::from(vec![
                    Span::raw(name),
                    Span::styled("excluded", ui::muted()),
                ]));
                continue;
            }
            let mut l = vec![Span::raw(name)];
            match seen {
                Some(s) if !s.roots.is_empty() => {
                    // folders inside another listed one say nothing new
                    let roots: Vec<String> = s
                        .roots
                        .iter()
                        .filter(|p| {
                            !s.roots
                                .iter()
                                .any(|q| q != *p && p.starts_with(&format!("{q}/")))
                        })
                        .map(|p| tilde(p, &home))
                        .collect();
                    let (n, b) = tracked(ag);
                    l.push(Span::raw(ui::pad(&roots.join(", "), 30)));
                    let mut facts = vec![format!(
                        "{} ({})",
                        plural(n as u64, "file"),
                        ui::bytes(b as f64)
                    )];
                    let mut names: Vec<String> = s.roots.iter().filter_map(|p| whose(p)).collect();
                    names.dedup();
                    if !names.is_empty() {
                        facts.push(format!("signed in: {}", names.join(", ")));
                    }
                    if s.to_read > 0 {
                        facts.push(format!("{} new or changed", s.to_read));
                    }
                    if s.zst_skipped > 0 {
                        facts.push(format!(
                            "{} skipped: needs zstd",
                            plural(s.zst_skipped, ".zst file")
                        ));
                    }
                    l.push(Span::styled(
                        format!("  {}", facts.join(" · ")),
                        ui::faint(),
                    ));
                }
                _ => l.push(Span::styled("no logs here", ui::muted())),
            }
            out(Line::from(l));
        }
        for (ag, p, why) in &r.skipped {
            out(Line::from(vec![
                Span::raw(format!("  {}", ui::pad(ag, 9))),
                Span::styled(
                    format!("skipped {}: {why}", tilde(p, &home)),
                    ui::fg(c::YELLOW()),
                ),
            ]));
        }
        if let Some((why, n, b)) = &r.budget {
            out(Line::from(vec![
                Span::raw(format!("  {}", ui::pad("budget", 9))),
                Span::styled(
                    format!(
                        "ran out of {}: {} ({}) wait for the next read",
                        if why == "time" { "time" } else { "bytes" },
                        plural(*n, "file"),
                        ui::bytes(*b as f64)
                    ),
                    ui::fg(c::YELLOW()),
                ),
            ]));
        }
    }
    let (lead, tail) = ("no local usage data: ", " (--json says why)");
    let room = ui::print_width().saturating_sub(11 + lead.len() + tail.len());
    let names = ui::trunc(&others.join(", "), room.max(12));
    out(Line::from(vec![
        Span::styled(format!("  {}", ui::pad("others", 9)), ui::muted()),
        Span::styled(format!("{lead}{names}{tail}"), ui::faint()),
    ]));
    ExitCode::SUCCESS
}

/// `1 file`, `3 files`.
fn plural(n: u64, what: &str) -> String {
    format!("{n} {what}{}", if n == 1 { "" } else { "s" })
}

/// What a read gained: `3 files · 1.2M · 214 new records`, or `up to date`.
fn gained(files: u64, bytes: u64, records: u64) -> String {
    if records == 0 && bytes == 0 {
        return "up to date".into();
    }
    format!(
        "{} · {} · {}",
        plural(files, "file"),
        ui::bytes(bytes as f64),
        plural(records, "new record")
    )
}

/// The machine's home, from the paths its files live under (for `~`).
fn r_home(hs: Option<&store::HostState>) -> String {
    hs.and_then(|x| x.run.as_ref())
        .map(|r| r.home.clone())
        .unwrap_or_default()
}

// ------------------------------------------------------------------ script, sync

fn script(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let host = match f.host.clone().or_else(|| a.cfg.hosts().first().cloned()) {
        Some(h) => h,
        None => return cli::no_host(),
    };
    print!("{}", super::script_for(&a.cfg, &host));
    if std::io::stderr().is_terminal() {
        hint(&format!(
            "run it yourself: aleph usage script --host {host} | ssh {host} sh -s"
        ));
    }
    ExitCode::SUCCESS
}

fn sync_cmd(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let cfg = &a.cfg;
    let hosts = f.hosts(cfg);
    if hosts.is_empty() {
        return cli::no_host();
    }
    if !super::enabled(cfg) {
        warn(
            "analytics is off — syncing anyway, since you asked (aleph usage on keeps it current)",
        );
    }
    let wait = Duration::from_secs(90);
    let mut reps = sync_now(cfg, &hosts, f.has("--full") || f.has("--rescan"), wait);
    // --until-done: more runs while a machine's budget ran out (a backfill)
    if f.has("--until-done") {
        for _ in 0..50 {
            let behind: Vec<String> = reps
                .iter()
                .filter(|r| r.error.is_none() && r.budget.is_some())
                .map(|r| r.host.clone())
                .collect();
            if behind.is_empty() {
                break;
            }
            for r in sync_now(cfg, &behind, false, wait) {
                if let Some(x) = reps.iter_mut().find(|x| x.host == r.host) {
                    x.files_read += r.files_read;
                    x.bytes += r.bytes;
                    x.records += r.records;
                    x.took_ms += r.took_ms;
                    x.budget = r.budget;
                    x.error = r.error;
                    x.skipped = r.skipped;
                }
            }
        }
    }
    let all_failed = reps.iter().all(|r| r.error.is_some());
    if f.has("--json") {
        json(&json!({"hosts": reps.iter().map(Report::to_json).collect::<Vec<_>>()}));
    } else {
        for r in &reps {
            let col = theme::machine(cfg, &r.host);
            let mut l = Vec::new();
            match &r.error {
                Some(e) => {
                    l.push(Span::styled(
                        "✕ ",
                        ui::fg(c::RED()).add_modifier(Modifier::BOLD),
                    ));
                    l.push(Span::styled(r.host.clone(), Style::new().fg(col)));
                    l.push(Span::styled(format!("  {e}"), ui::fg(c::RED())));
                }
                None if r.excluded => {
                    l.push(Span::styled("– ", ui::muted()));
                    l.push(Span::styled(r.host.clone(), Style::new().fg(col)));
                    l.push(Span::styled(
                        "  excluded (analytics.include / exclude)",
                        ui::muted(),
                    ));
                }
                None => {
                    l.push(Span::styled(
                        "✓ ",
                        ui::fg(c::GREEN()).add_modifier(Modifier::BOLD),
                    ));
                    l.push(Span::styled(r.host.clone(), Style::new().fg(col)));
                    l.push(Span::styled(
                        format!(
                            "  {} · {:.1}s",
                            gained(r.files_read, r.bytes, r.records),
                            r.took_ms as f64 / 1000.0
                        ),
                        ui::faint(),
                    ));
                }
            }
            say(Line::from(l));
            if let Some((why, n, b)) = &r.budget {
                warn(&format!(
                    "{}: ran out of {why} — {} ({}) left for the next read (--until-done reads on)",
                    r.host,
                    plural(*n, "file"),
                    ui::bytes(*b as f64)
                ));
            }
            for (ag, p, why) in &r.skipped {
                warn(&format!("{}: {ag}: {why} ({p})", r.host));
            }
        }
    }
    if all_failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ------------------------------------------------------------------ on / off

fn list(cfg: &Config, key: &str) -> Vec<String> {
    cfg.get(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn toggle(mut a: Args, on: bool) -> ExitCode {
    let f = match flags(&a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let scoped = f.host.is_some() || f.agent.is_some();
    if !scoped {
        if let Err(e) = a
            .cfg
            .set("analytics.enabled", if on { "true" } else { "false" })
        {
            return fail(&format!("couldn't save: {e}"));
        }
        if on {
            ok("analytics on: the agents' logs are read in the background (aleph usage sync now)");
        } else {
            ok("analytics off: nothing more is read (what's stored stays: aleph usage prune)");
        }
        return ExitCode::SUCCESS;
    }
    let glob = format!(
        "{}:{}",
        f.host.as_deref().unwrap_or("*"),
        f.agent.as_deref().unwrap_or("*")
    );
    let mut exc = list(&a.cfg, "analytics.exclude");
    let mut inc = list(&a.cfg, "analytics.include");
    if on {
        exc.retain(|g| *g != glob);
        if !inc.is_empty() && !inc.contains(&glob) {
            inc.push(glob.clone());
        }
    } else if !exc.contains(&glob) {
        exc.push(glob.clone());
    }
    let r = a
        .cfg
        .set("analytics.exclude", &exc.join(","))
        .and_then(|_| a.cfg.set("analytics.include", &inc.join(",")));
    if let Err(e) = r {
        return fail(&format!("couldn't save: {e}"));
    }
    let (h, ag) = (
        f.host.as_deref().unwrap_or("*"),
        f.agent.as_deref().unwrap_or("*"),
    );
    if on {
        if super::allowed(&a.cfg, h, ag) {
            ok(&format!("analytics covers {glob} again"));
        } else {
            warn(&format!(
                "{glob} is still excluded by another rule — analytics.exclude = {}",
                exc.join(",")
            ));
        }
    } else {
        ok(&format!(
            "analytics skips {glob} from now on (stored data stays: aleph usage prune --host {h})"
        ));
    }
    ExitCode::SUCCESS
}

// ------------------------------------------------------------------ prune

fn prune(a: &Args) -> ExitCode {
    let f = match flags(a, 1) {
        Ok(f) => f,
        Err(e) => return fail(&e),
    };
    let dry = f.has("--dry-run") || f.has("-n");
    let host = f
        .host
        .clone()
        .filter(|_| a.rest.iter().any(|x| x.starts_with("--host") || x == "-H"));
    let before = if host.is_some() {
        None
    } else {
        let keep = f
            .keep
            .clone()
            .unwrap_or_else(|| a.cfg.get_or("analytics.keep"));
        match super::parse_days(&keep) {
            Ok(0) => {
                ok("analytics.keep is all: nothing to prune");
                return ExitCode::SUCCESS;
            }
            Ok(d) => Some(crate::cache::now().saturating_sub(d * 86_400)),
            Err(e) => return fail(&e),
        }
    };
    let Some(_lock) = store::lock(Duration::from_secs(30)) else {
        return fail("a sync is running — try again in a moment");
    };
    let p = match store::prune(before, host.as_deref(), dry) {
        Ok(p) => p,
        Err(e) => return fail(&format!("couldn't prune: {e}")),
    };
    if let (Some(h), false) = (&host, dry) {
        let mut st = store::Offsets::load();
        st.hosts.remove(h);
        let _ = st.save();
    }
    if !dry && (p.records_removed > 0 || !p.months_removed.is_empty()) {
        store::rollup(&store::months(), &crate::prices::Table::from_cfg(&a.cfg));
    }
    if f.has("--json") {
        json(
            &json!({"dry_run": dry, "records_removed": p.records_removed, "bytes_freed": p.bytes_freed, "months_removed": p.months_removed}),
        );
        return ExitCode::SUCCESS;
    }
    let what = match &host {
        Some(h) => format!("everything from {h}"),
        None => "records older than analytics.keep".to_string(),
    };
    if p.records_removed == 0 {
        ok(&format!("nothing to prune ({what})"));
    } else {
        ok(&format!(
            "{} {} records ({}) — {what}",
            if dry { "would remove" } else { "removed" },
            p.records_removed,
            ui::bytes(p.bytes_freed as f64)
        ));
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(key: &str, cost: f64, est: bool) -> Row {
        Row {
            key: key.into(),
            input: 12_400,
            output: 340_100,
            cache_read: 88_100_000,
            cache_write_5m: 4_500_000,
            requests: 1203,
            sessions: 14,
            active_ms: 22_320_000,
            cost_reported: if est { 0.0 } else { cost },
            cost_estimated: if est { cost } else { 0.0 },
            ..Default::default()
        }
    }

    #[test]
    fn openrouter_under_the_table() {
        let text = |l: &Line| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        };
        let s = crate::openrouter::Spend {
            usage: Some(40.1),
            today: Some(0.42),
            month: Some(12.75),
            credits: Some(120.0),
            credits_used: Some(40.1),
            limit: Some(50.0),
            limit_left: Some(4.0),
            limit_reset: Some("monthly".into()),
            at: 1_000,
            ..Default::default()
        };
        let v = openrouter_lines(&s, 1_120);
        let (a, b) = (text(&v[0]), text(&v[1]));
        assert!(
            a.starts_with("OpenRouter") && a.contains("today $0.42"),
            "{a}"
        );
        assert!(
            a.contains("this month $12.75") && a.contains("total $40.10"),
            "{a}"
        );
        // a week OpenRouter didn't say isn't made up
        assert!(!a.contains("this week"), "{a}");
        assert!(b.contains("credits $79.90 left of $120"), "{b}");
        assert!(b.contains("limit $4.00 left of $50.00 (monthly)"), "{b}");
        assert!(b.contains("read 2m ago") && !b.contains("stale"), "{b}");
        // the limit nearly gone: red
        assert!(v[1]
            .spans
            .iter()
            .any(|x| x.content == "$4.00" && x.style.fg == Some(c::RED())));
        let old = openrouter_lines(&s, 1_000 + crate::openrouter::STALE + 60);
        assert!(text(&old[1]).contains("stale"));
        // both columns line up with the table's label
        assert_eq!(b.find("credits"), Some(13));
    }

    #[test]
    fn numbers_read_well() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(12_400), "12.4k");
        assert_eq!(compact(340_100), "340k");
        assert_eq!(compact(88_100_000), "88.1M");
        assert_eq!(compact(1_200_000_000), "1.2B");
        assert_eq!(cost_text(&row("m", 142.1, true)), "$142.10 est.");
        assert_eq!(cost_text(&row("m", 3.0, false)), "$3.00");
        assert_eq!(cost_text(&row("m", 0.001, false)), "<$0.01");
        let mut r = row("m", 0.0, true);
        r.unpriced = 3;
        assert_eq!(cost_text(&r), "?");
        r.cost_estimated = 1.0;
        assert_eq!(cost_text(&r), "$1.00+ est.");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
    }

    #[test]
    fn table_is_aligned() {
        let cfg = Config::in_memory(&[]);
        let rows = vec![
            row("claude-opus-5-5", 142.1, true),
            row("gpt-5.1-codex", 3.21, true),
        ];
        let total = super::super::group(&[], By::Total)
            .pop()
            .unwrap_or_default();
        let lines: Vec<String> = table(
            &cfg,
            By::Model,
            Range::days(7),
            &["b31".into(), "gpu2".into()],
            &rows,
            &total,
            110,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
        let text = lines.join("\n");
        assert!(lines[0].starts_with("⏺ Usage · last 7 days"), "{text}");
        assert!(
            lines[1].contains("cache read") && lines[1].contains("cost"),
            "{text}"
        );
        // right-aligned to the same edge
        assert_eq!(lines[2].chars().count(), lines[3].chars().count(), "{text}");
        assert!(
            lines[2].ends_with("$142.10 est.") && lines[3].ends_with("$3.21 est."),
            "{text}"
        );
        for l in &lines {
            assert!(l.chars().count() <= 110, "too wide: {l}");
        }
        assert!(text.contains("total"));
        assert!(text.contains("read b31 never"), "{text}");
        // narrow: columns go, the cost stays
        let narrow: Vec<String> = table(
            &cfg,
            By::Model,
            Range::days(7),
            &["b31".into()],
            &rows,
            &total,
            60,
        )
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
        .collect();
        assert!(
            narrow.iter().all(|l| l.chars().count() <= 60),
            "{narrow:#?}"
        );
        assert!(narrow[2].contains("$142.10"), "{narrow:#?}");
        assert!(!narrow[1].contains("cache write"));
    }

    #[test]
    fn session_and_day_labels() {
        let mut r = row("b31:claude:3f2a9c1e-0b7d-4c55-9a61-2e8f0c4d7a10", 1.0, true);
        r.projects = vec!["~/shop".into()];
        assert_eq!(label(By::Session, &r), "b31 claude 3f2a9c1e ~/shop");
        let d = row("2026-10-22", 1.0, true);
        assert_eq!(label(By::Day, &d), "2026-10-22 Thu");
        assert_eq!(label(By::Model, &row("", 1.0, true)), "(unknown)");
    }
}
