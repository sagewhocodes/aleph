//! What the dashboard shows of usage, computed in one go off the UI thread
//! ([`load`]): the Monitor › Usage view (totals, a chart by day or hour, a
//! breakdown, an hour × weekday heatmap, insights, plan limits, sources) and
//! the live numbers other views put beside a session (tokens, context, $)
//! and a machine (today).

use super::parse::Record;
use super::{group, rows, store, By, Filter, Range, Row};
use crate::config::Config;
use crate::prices;
use crate::ui;
use serde_json::Value;
use std::collections::HashMap;

/// One agent session's numbers, from its own logs.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Live {
    pub host: String,
    pub agent: String,
    /// its latest request's model, effort and folder (`~/…` under the home)
    pub model: String,
    /// every model it used, in the order it first did (a switch mid-session:
    /// more than one; the main thread's, not its subagents')
    pub models: Vec<String>,
    pub effort: String,
    pub project: String,
    pub tokens: u64,
    pub output: u64,
    pub requests: u64,
    pub cost: f64,
    /// some of `cost` is an estimate from the price table
    pub estimated: bool,
    /// the latest request's context: input + cache read + cache write
    pub ctx: Option<u64>,
    /// the model's context window, when it's known
    pub window: Option<u64>,
    /// the context at each of its latest requests (up to 60, oldest first)
    pub ctx_series: Vec<u64>,
    /// its latest requests' (time, tokens), subagents' too, from the last
    /// [`RECENT`] seconds before its newest (oldest first): Fleet's
    /// tokens-per-minute bars
    pub recent: Vec<(u64, u64)>,
    pub first: u64,
    pub last: u64,
}

impl Live {
    /// Context used, 0–100, when the window is known.
    pub fn ctx_pct(&self) -> Option<u64> {
        let (c, w) = (self.ctx?, self.window?);
        (w > 0).then(|| (c * 100 / w).min(100))
    }
}

/// Everything the Usage view draws.
#[derive(Clone, Debug, Default)]
pub struct Data {
    pub range: Option<Range>,
    pub by: Option<By>,
    pub host: Option<String>,
    /// over the range
    pub total: Row,
    /// today · 7 days · 30 days (with the machine filter)
    pub kpis: Vec<(&'static str, Row)>,
    /// the chart's columns: (label, tokens per series), oldest first
    pub columns: Vec<(String, Vec<u64>)>,
    /// the chart's series (agents, biggest first; the rest as `other`)
    pub series: Vec<String>,
    /// hours, not days (a one-day range)
    pub hourly: bool,
    pub breakdown: Vec<Row>,
    /// tokens by weekday (Monday first) × local hour
    pub heat: [[u64; 24]; 7],
    pub insights: Vec<(String, String)>,
    /// the newest rate-limit snapshot per machine and agent (store::limits)
    pub limits: Value,
    /// per machine: how its logs were last read
    pub reads: Vec<super::ReadState>,
    /// agent session id → its numbers (this month and last)
    pub live: HashMap<String, Live>,
    /// machine → today's totals
    pub today: HashMap<String, Row>,
    /// account id → its label (`accounts::label`)
    pub names: HashMap<String, String>,
    /// what the filters can pick, over the range (with no filter): agents and
    /// their accounts, machines, projects, each with its cost
    pub options: Vec<Opt>,
    /// the same range just before, filtered alike (▲▼ against it)
    pub prev: Row,
    /// the screens' charts, stats and insights (`explore`)
    pub more: super::explore::More,
}

/// One choice for a filter.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Opt {
    /// `a` an agent, `c` one of its accounts, `h` a machine, `p` a project
    pub kind: char,
    pub id: String,
    pub label: String,
    /// an account's agent (it's listed under it)
    pub parent: String,
    pub cost: f64,
    pub tokens: u64,
}

/// The chart keeps this many series; the rest are summed as `other`.
const SERIES: usize = 5;

/// The model's context window (tokens), only for the families where it's
/// certain: Claude 3 and 4 (200K; 1M with the `[1m]` suffix Claude Code
/// logs) and Gemini 2.5. Others get no context % rather than a wrong one.
pub fn context_window(model: &str) -> Option<u64> {
    let m = model.to_lowercase();
    if m.contains("[1m]") {
        return Some(1_000_000);
    }
    let n = prices::normalize(&m);
    let claude_200k = [
        "claude-3",
        "claude-opus-4",
        "claude-sonnet-4",
        "claude-haiku-4",
    ];
    if claude_200k.iter().any(|p| n.starts_with(p)) {
        Some(200_000)
    } else if n.starts_with("gemini-2.5") {
        Some(1_048_576)
    } else {
        None
    }
}

/// Compute it all (reads the store; seconds at most on years of logs).
pub fn load(cfg: &Config, range: Range, by: By, filter: &Filter) -> Data {
    load_split(cfg, range, by, filter, By::Account, By::Account)
}

/// [`load`], the breakup split by `split`, the breakdown's tree by `tree`.
pub fn load_split(
    cfg: &Config,
    range: Range,
    by: By,
    filter: &Filter,
    split: By,
    tree: By,
) -> Data {
    let filter = filter.clone();
    let (all, ix) = super::rows_ix(cfg, range, &Filter::default());
    let rs: Vec<store::Row> = if filter.is_empty() {
        all.clone()
    } else {
        rows(cfg, range, &filter)
    };
    let host = filter.hosts.first().cloned();
    let total = group(&rs, By::Total).pop().unwrap_or_default();
    let breakdown = group(&rs, by);
    let kpis: Vec<(&'static str, Row)> = [
        ("today", Range::today()),
        ("7 days", Range::days(7)),
        ("30 days", Range::days(30)),
    ]
    .into_iter()
    .map(|(n, r)| {
        let v = if r == range {
            rs.clone()
        } else {
            rows(cfg, r, &filter)
        };
        (n, group(&v, By::Total).pop().unwrap_or_default())
    })
    .collect();
    // the chart's series: the range's biggest agents
    let by_agent = group(&rs, By::Agent);
    let mut series: Vec<String> = by_agent
        .iter()
        .take(SERIES)
        .map(|r| r.key.clone())
        .collect();
    if by_agent.len() > SERIES {
        series.push("other".into());
    }
    let slot = |agent: &str| {
        series
            .iter()
            .position(|s| s == agent)
            .unwrap_or(series.len().saturating_sub(1))
    };
    // the hours come from the rows (each is an hour's); raw records only for
    // the sessions' live numbers, this month's and last (whatever the range)
    let tz = ui::utc_offset();
    let recs = records(&months_of(Range::days(1), 1));
    let hourly = range.from == range.to || range.since.is_some();
    let mut columns: Vec<(String, Vec<u64>)> = Vec::new();
    // hours: one column per hour of the range (a day: its 24)
    let hour_cols: Vec<(i64, u8)> = if hourly {
        let mut v = Vec::new();
        for d in range.from..=range.to {
            for h in 0..24u8 {
                if range.keeps(d, h, tz) && (range.since.is_some() || d == range.from) {
                    v.push((d, h));
                }
            }
        }
        v
    } else {
        Vec::new()
    };
    if hourly {
        columns = hour_cols
            .iter()
            .map(|(_, h)| (format!("{h:02}"), vec![0; series.len()]))
            .collect();
    }
    // the whole range (all time: from the first day with data)
    let first = if range.from <= 0 {
        first_day(&rs).min(range.to)
    } else {
        range.from
    };
    if !hourly {
        for d in first..=range.to {
            columns.push((short_day(d), vec![0; series.len()]));
        }
    }
    let mut heat = [[0u64; 24]; 7];
    for r in &rs {
        let t = r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
        // 1970-01-01 was a Thursday: Monday = 0
        let wd = (r.day + 3).rem_euclid(7) as usize;
        heat[wd][(r.hour as usize).min(23)] += t;
        if hourly && !series.is_empty() {
            if let Some(i) = hour_cols.iter().position(|x| *x == (r.day, r.hour)) {
                columns[i].1[slot(&r.agent)] += t;
            }
        }
    }
    if !hourly && !series.is_empty() {
        for r in &rs {
            let i = (r.day - first) as usize;
            if let Some(c) = columns.get_mut(i) {
                c.1[slot(&r.agent)] +=
                    r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
            }
        }
    }
    let insights = insights(&rs, &heat, &total);
    let live = live(&recs, &prices::Table::from_cfg(cfg), tz);
    let today_rows = rows(cfg, Range::today(), &Filter::default());
    let today = group(&today_rows, By::Host)
        .into_iter()
        .map(|r| (r.key.clone(), r))
        .collect();
    let book = super::accounts::Book::load();
    let names: HashMap<String, String> = book
        .accounts
        .keys()
        .map(|id| (id.clone(), super::accounts::label(cfg, &book, id)))
        .collect();
    // the filters' choices: every agent (its accounts under it), machine and project of the range
    let mut options = Vec::new();
    for (a, accts) in super::split(&all, By::Agent, By::Account) {
        options.push(Opt {
            kind: 'a',
            id: a.key.clone(),
            label: crate::agents::label(&a.key),
            cost: a.cost(),
            tokens: a.tokens(),
            ..Default::default()
        });
        if accts.len() > 1 || accts.iter().any(|x| !x.key.is_empty()) {
            for c in accts {
                options.push(Opt {
                    kind: 'c',
                    label: names
                        .get(&c.key)
                        .cloned()
                        .unwrap_or_else(|| "account unknown".into()),
                    id: c.key.clone(),
                    parent: a.key.clone(),
                    cost: c.cost(),
                    tokens: c.tokens(),
                });
            }
        }
    }
    for h in group(&all, By::Host) {
        options.push(Opt {
            kind: 'h',
            label: h.key.clone(),
            id: h.key.clone(),
            cost: h.cost(),
            tokens: h.tokens(),
            ..Default::default()
        });
    }
    for p in group(&all, By::Project)
        .into_iter()
        .filter(|p| !p.key.is_empty())
    {
        options.push(Opt {
            kind: 'p',
            label: ix.name(&p.key),
            id: p.key.clone(),
            cost: p.cost(),
            tokens: p.tokens(),
            ..Default::default()
        });
    }
    let more = super::explore::more(cfg, range, &filter, split, by, tree, &rs, &names);
    let prev = group(&rows(cfg, range.prev(), &filter), By::Total)
        .pop()
        .unwrap_or_default();
    Data {
        range: Some(range),
        by: Some(by),
        host,
        total,
        kpis,
        columns,
        series,
        hourly,
        breakdown,
        heat,
        insights,
        limits: store::limits(),
        reads: super::reads(&cfg.fleet()),
        live,
        today,
        names,

        more,
        options,
        prev,
    }
}

fn first_day(rs: &[store::Row]) -> i64 {
    rs.iter().map(|r| r.day).min().unwrap_or(i64::MAX)
}

/// `Mon 20` for a local day.
fn short_day(d: i64) -> String {
    const W: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let (_, _, dd) = ui::civil(d);
    format!("{} {dd}", W[(d + 3).rem_euclid(7) as usize])
}

/// The months a range covers, plus `extra` months before it (live numbers
/// for sessions that began last month).
fn months_of(range: Range, extra: usize) -> Vec<String> {
    let mut v = range.months();
    let now = store::month_of(crate::cache::now());
    if !v.contains(&now) {
        v.push(now);
    }
    let all = store::months();
    if let Some(first) = v.iter().min().cloned() {
        let before: Vec<String> = all
            .iter()
            .filter(|m| **m < first)
            .rev()
            .take(extra)
            .cloned()
            .collect();
        v.extend(before);
    }
    v.sort();
    v.dedup();
    v
}

fn records(months: &[String]) -> Vec<Record> {
    let mut v = Vec::new();
    for m in months {
        v.extend(store::read_month(m));
    }
    store::dedup(v)
}

/// How far back `Live::recent` reaches (from a session's newest request),
/// and how many requests it keeps at most.
pub const RECENT: u64 = 2 * 3600;
const RECENT_MAX: usize = 600;

/// Per agent session: totals, cost and the latest context (pure — tested).
pub fn live(recs: &[Record], table: &prices::Table, tz: i64) -> HashMap<String, Live> {
    let mut out: HashMap<String, Live> = HashMap::new();
    // costs as the store computes them (reported, else the price table)
    for r in store::build(recs.to_vec(), table, tz) {
        if r.session.is_empty() {
            continue;
        }
        let e = out.entry(r.session.clone()).or_insert_with(|| Live {
            host: r.host.clone(),
            agent: r.agent.clone(),
            ..Default::default()
        });
        e.tokens += r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
        e.output += r.output;
        e.requests += r.requests;
        e.cost += r.cost_reported + r.cost_estimated;
        e.estimated |= r.cost_estimated > 0.0;
        if r.first > 0 && (e.first == 0 || r.first < e.first) {
            e.first = r.first;
        }
        e.last = e.last.max(r.last);
    }
    // the context at each request, in order (the main thread's), and its models
    let mut series: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    let mut used: HashMap<&str, Vec<(u64, &str)>> = HashMap::new();
    for r in recs
        .iter()
        .filter(|r| !r.is_turn() && !r.session.is_empty() && !r.sidechain())
    {
        series.entry(r.session.as_str()).or_default().push((
            r.ts,
            r.input + r.cache_read + r.cache_write_5m + r.cache_write_1h,
        ));
        if !r.model.is_empty() {
            used.entry(r.session.as_str())
                .or_default()
                .push((r.ts, r.model.as_str()));
        }
    }
    for (s, mut v) in used {
        if let Some(l) = out.get_mut(s) {
            v.sort_by_key(|x| x.0);
            for (_, m) in v {
                if !l.models.iter().any(|x| x == m) {
                    l.models.push(m.to_string());
                }
            }
        }
    }
    for (s, mut v) in series {
        if let Some(l) = out.get_mut(s) {
            v.sort_by_key(|x| x.0);
            let from = v.len().saturating_sub(60);
            l.ctx_series = v[from..].iter().map(|x| x.1).collect();
        }
    }
    // every request's tokens near its newest (subagents' too: they spend)
    let mut tok: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    for r in recs
        .iter()
        .filter(|r| !r.is_turn() && !r.session.is_empty())
    {
        tok.entry(r.session.as_str())
            .or_default()
            .push((r.ts, r.tokens()));
    }
    for (s, mut v) in tok {
        if let Some(l) = out.get_mut(s) {
            v.sort_by_key(|x| x.0);
            let newest = v.last().map_or(0, |x| x.0);
            v.retain(|x| x.0 + RECENT >= newest);
            let from = v.len().saturating_sub(RECENT_MAX);
            l.recent = v.split_off(from);
        }
    }
    // the latest request of each session: its model, effort and context
    let mut latest: HashMap<&str, &Record> = HashMap::new();
    // (a subagent's requests have their own context: the main thread's count)
    for r in recs
        .iter()
        .filter(|r| !r.is_turn() && !r.session.is_empty() && !r.sidechain())
    {
        let e = latest.entry(r.session.as_str()).or_insert(r);
        if r.ts >= e.ts {
            *e = r;
        }
    }
    for (s, r) in latest {
        if let Some(l) = out.get_mut(s) {
            l.model = r.model.clone();
            l.effort = r.effort.clone();
            l.project = r.project.clone();
            l.ctx = Some(r.input + r.cache_read + r.cache_write_5m + r.cache_write_1h);
            l.window = context_window(&r.model);
        }
    }
    out
}

/// A few facts worth a glance (Claude Code's /stats style).
fn insights(rs: &[store::Row], heat: &[[u64; 24]; 7], total: &Row) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = Vec::new();
    if rs.is_empty() {
        return v;
    }
    let days = group(rs, By::Day);
    if let Some(b) = days.iter().max_by_key(|r| r.tokens()) {
        v.push((
            "busiest day".into(),
            format!("{} · {}", b.key, tokens(b.tokens())),
        ));
    }
    let active = days.iter().filter(|r| r.tokens() > 0).count();
    v.push((
        "active days".into(),
        format!("{active} of {}", days.len().max(active)),
    ));
    if let Some(m) = group(rs, By::Model).first() {
        v.push((
            "most used model".into(),
            format!("{} · {}", m.key, row_money(m)),
        ));
    }
    if let Some(p) = group(rs, By::Project).first().filter(|p| !p.key.is_empty()) {
        v.push((
            "top project".into(),
            format!(
                "{} · {}",
                super::projects::name_of(&p.key),
                tokens(p.tokens())
            ),
        ));
    }
    if let Some(s) = group(rs, By::Session)
        .iter()
        .max_by_key(|r| r.active_secs())
    {
        v.push(("longest session".into(), ui::dur(s.active_secs())));
    }
    // the busiest hour of the week
    let mut best = (0u64, 0usize, 0usize);
    for (d, row) in heat.iter().enumerate() {
        for (h, n) in row.iter().enumerate() {
            if *n > best.0 {
                best = (*n, d, h);
            }
        }
    }
    if best.0 > 0 {
        const W: [&str; 7] = [
            "Mondays",
            "Tuesdays",
            "Wednesdays",
            "Thursdays",
            "Fridays",
            "Saturdays",
            "Sundays",
        ];
        v.push((
            "busiest hour".into(),
            format!("{} {:02}:00", W[best.1], best.2),
        ));
    }
    let cached = total.cache_read as f64;
    let fresh = (total.input + total.cache_write()) as f64;
    if cached + fresh > 0.0 {
        v.push((
            "cache hits".into(),
            format!("{:.0}% of input", 100.0 * cached / (cached + fresh)),
        ));
    }
    v
}

/// `1.2M`, `340k`, `912`.
pub fn tokens(n: u64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.1}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e4 {
        format!("{:.0}k", f / 1e3)
    } else if f >= 1e3 {
        format!("{:.1}k", f / 1e3)
    } else {
        format!("{n}")
    }
}

/// `$4.20`, `$4.20 est.`, `$0.03`.
static SHOW_COST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `analytics.cost = off` hides the $ estimates (they read as "—").
pub fn show_cost(on: bool) {
    SHOW_COST.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Are $ shown at all (`analytics.cost`)?
pub fn cost_shown() -> bool {
    SHOW_COST.load(std::sync::atomic::Ordering::Relaxed)
}

/// Today's spend as the header and the status bar show it: `$0.85*`,
/// `$106*` (`*` = some of it estimated), scaled or hidden in showcase mode.
pub fn money_short(v: f64, estimated: bool) -> String {
    let Some(v) = crate::showcase::money(v) else {
        return crate::showcase::MONEY_HIDDEN.into();
    };
    let s = if v >= 100.0 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    };
    if estimated {
        s + "*"
    } else {
        s
    }
}

/// A row's cost: `free` when it all ran on free models, else [`money`].
pub fn row_money(r: &crate::usage::Row) -> String {
    if r.all_free() {
        "free".into()
    } else {
        money(r.cost(), r.estimated())
    }
}

pub fn money(v: f64, estimated: bool) -> String {
    if v <= 0.0 || !SHOW_COST.load(std::sync::atomic::Ordering::Relaxed) {
        return "—".into();
    }
    // showcase: scaled, or hidden (`showcase.money`)
    let Some(v) = crate::showcase::money(v) else {
        return crate::showcase::MONEY_HIDDEN.into();
    };
    let s = if v >= 100.0 {
        format!("${v:.0}")
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else {
        "<$0.01".into()
    };
    if estimated {
        format!("{s} est.")
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(ts: u64, session: &str, model: &str, input: u64, cache_read: u64, out: u64) -> Record {
        Record {
            ts,
            host: "b31".into(),
            agent: "claude".into(),
            session: session.into(),
            key: format!("{session}-{ts}"),
            model: model.into(),
            input,
            cache_read,
            output: out,
            ..Default::default()
        }
    }

    #[test]
    fn windows() {
        assert_eq!(context_window("claude-opus-5-5[1m]"), Some(1_000_000));
        assert_eq!(context_window("claude-sonnet-4-5-20250929"), Some(200_000));
        assert_eq!(context_window("gemini-2.5-pro"), Some(1_048_576));
        assert_eq!(context_window("gpt-5-codex"), None);
        // newer families: unknown here, so no context % at all
        assert_eq!(context_window("claude-opus-5-5"), None);
    }

    #[test]
    fn live_takes_the_latest_context() {
        let recs = vec![
            rec(100, "s1", "claude-sonnet-4-5", 10, 1000, 50),
            rec(200, "s1", "claude-sonnet-4-5", 20, 120_000, 80),
            rec(150, "s2", "gpt-5-codex", 500, 0, 10),
        ];
        let l = live(&recs, &prices::Table::default(), 0);
        let s1 = &l["s1"];
        assert_eq!(s1.tokens, 10 + 1000 + 50 + 20 + 120_000 + 80);
        assert_eq!(s1.ctx, Some(120_020));
        assert_eq!(s1.ctx_pct(), Some(60));
        assert_eq!(s1.requests, 2);
        assert_eq!(s1.last, 200);
        assert_eq!(s1.ctx_series, vec![1010, 120_020]);
        // each request's tokens, for the rows' tokens-per-minute bars
        assert_eq!(s1.recent, vec![(100, 1060), (200, 120_100)]);
        assert_eq!(l["s2"].ctx_pct(), None);
        // only the last RECENT seconds before the newest are kept
        let recs = vec![
            rec(1, "s3", "m", 1, 0, 0),
            rec(10 + RECENT, "s3", "m", 2, 0, 0),
        ];
        assert_eq!(
            live(&recs, &prices::Table::default(), 0)["s3"].recent,
            vec![(10 + RECENT, 2)]
        );
    }

    #[test]
    fn formats() {
        assert_eq!(tokens(912), "912");
        assert_eq!(tokens(1_234), "1.2k");
        assert_eq!(tokens(340_000), "340k");
        assert_eq!(tokens(1_234_567), "1.2M");
        assert_eq!(money(4.2, true), "$4.20 est.");
        assert_eq!(money(0.0, false), "—");
        assert_eq!(money(0.004, false), "<$0.01");
        assert_eq!(short_day(20_383), "Wed 22");
    }
}
