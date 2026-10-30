//! The local store, under `state_dir()/usage/`:
//!
//! * `records-YYYY-MM.tsv` — one line per request or turn (`parse::to_line`),
//!   by UTC month, append-only. Duplicates are harmless: the rollup keeps the
//!   last line per (machine, key).
//! * `offsets.json` — per machine: each log file's device, inode, size,
//!   mtime, next offset and reader state, and what the last run did.
//! * `limits.json` — the newest rate-limit snapshot per machine and agent.
//! * `rollup.json` — per month: sums by day × machine × agent × session ×
//!   model × effort × folder (days in local time), rebuilt when the month's
//!   records, the UTC offset or the prices change.

use super::extract::AgentSeen;
use super::parse::{self, Limit, Record};
use crate::prices;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Gaps between requests up to this long count as active time.
pub const IDLE_GAP: u64 = 300;

pub fn dir() -> PathBuf {
    crate::config::world_state().join("usage")
}

fn month_file(m: &str) -> PathBuf {
    dir().join(format!("records-{m}.tsv"))
}

/// `2026-10` for a unix time (UTC).
pub fn month_of(ts: u64) -> String {
    let (y, m, _) = crate::ui::civil((ts / 86_400) as i64);
    format!("{y:04}-{m:02}")
}

/// Every month that has records, oldest first.
pub fn months() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir())
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_prefix("records-")
                        .and_then(|r| r.strip_suffix(".tsv"))
                        .filter(|m| m.len() == 7)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The first and last unix second of a UTC month (`2026-10`).
pub fn month_bounds(m: &str) -> Option<(u64, u64)> {
    let (y, mo) = m.split_once('-')?;
    let (y, mo): (i64, u32) = (y.parse().ok()?, mo.parse().ok()?);
    if !(1..=12).contains(&mo) {
        return None;
    }
    let start = parse::days_from_civil(y, mo, 1);
    let (ny, nm) = if mo == 12 { (y + 1, 1) } else { (y, mo + 1) };
    let end = parse::days_from_civil(ny, nm, 1);
    Some((
        (start.max(0) * 86_400) as u64,
        (end.max(0) * 86_400) as u64 - 1,
    ))
}

/// Append records to their month files; the months touched.
pub fn append(recs: &[Record]) -> io::Result<Vec<String>> {
    let mut by: BTreeMap<String, String> = BTreeMap::new();
    for r in recs {
        let s = by.entry(month_of(r.ts)).or_default();
        s.push_str(&parse::to_line(r));
        s.push('\n');
    }
    fs::create_dir_all(dir())?;
    for (m, text) in &by {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(month_file(m))?;
        f.write_all(text.as_bytes())?;
    }
    Ok(by.into_keys().collect())
}

/// A month's records as written (duplicates included).
pub fn read_month(m: &str) -> Vec<Record> {
    fs::read_to_string(month_file(m))
        .map(|t| t.lines().filter_map(parse::from_line).collect())
        .unwrap_or_default()
}

/// A month's records whose line holds one of `needles` (an agent session's
/// id, say): the rest isn't parsed, so one session's records come quickly
/// out of a big month (`turncost`, the journal).
pub fn read_month_matching(m: &str, needles: &[&str]) -> Vec<Record> {
    if needles.is_empty() {
        return Vec::new();
    }
    fs::read_to_string(month_file(m))
        .map(|t| {
            t.lines()
                .filter(|l| needles.iter().any(|n| l.contains(n)))
                .filter_map(parse::from_line)
                .collect()
        })
        .unwrap_or_default()
}

fn file_size(m: &str) -> u64 {
    fs::metadata(month_file(m)).map(|x| x.len()).unwrap_or(0)
}

// ------------------------------------------------------------------ the lock

/// Held while a sync or a prune changes the store (other Aleph processes
/// sync too: the daemon, the dashboard, the CLI).
pub struct Lock(#[allow(dead_code)] fs::File);

/// Take the store lock, waiting up to `wait`.
pub fn lock(wait: Duration) -> Option<Lock> {
    fs::create_dir_all(dir()).ok()?;
    let f = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir().join(".lock"))
        .ok()?;
    let t0 = Instant::now();
    loop {
        if f.try_lock().is_ok() {
            return Some(Lock(f));
        }
        if t0.elapsed() >= wait {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ------------------------------------------------------------------ offsets

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileState {
    pub agent: String,
    pub reader: String,
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: u64,
    pub off: u64,
    pub ctx: String,
}

/// What one run on a machine did (`aleph usage sources`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunInfo {
    pub at: u64,
    pub took_ms: u64,
    pub error: Option<String>,
    pub hostname: String,
    pub home: String,
    pub files_read: u64,
    pub bytes: u64,
    pub records: u64,
    pub budget: Option<(String, u64, u64)>,
    pub agents: Vec<AgentSeen>,
    pub skipped: Vec<(String, String, String)>,
    pub has_zstd: bool,
    pub has_sqlite: bool,
}

#[derive(Clone, Debug, Default)]
pub struct HostState {
    pub files: BTreeMap<String, FileState>,
    pub run: Option<RunInfo>,
    /// the last run that worked
    pub ok_at: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Offsets {
    pub hosts: BTreeMap<String, HostState>,
}

fn gs(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("").to_string()
}
fn gu(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

fn agent_json(a: &AgentSeen) -> Value {
    json!({"agent": a.agent, "roots": a.roots, "listed": a.listed, "unchanged": a.unchanged,
           "to_read": a.to_read, "zst_skipped": a.zst_skipped})
}

fn agent_from(v: &Value) -> AgentSeen {
    AgentSeen {
        agent: gs(v, "agent"),
        roots: v
            .get("roots")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        listed: gu(v, "listed"),
        unchanged: gu(v, "unchanged"),
        to_read: gu(v, "to_read"),
        zst_skipped: gu(v, "zst_skipped"),
    }
}

impl RunInfo {
    pub fn to_json(&self) -> Value {
        json!({
            "at": self.at, "took_ms": self.took_ms, "error": self.error, "hostname": self.hostname, "home": self.home,
            "files_read": self.files_read, "bytes": self.bytes, "records": self.records,
            "budget": self.budget.as_ref().map(|(w, f, b)| json!({"reason": w, "files": f, "bytes": b})),
            "agents": self.agents.iter().map(agent_json).collect::<Vec<_>>(),
            "skipped": self.skipped.iter().map(|(a, p, w)| json!({"agent": a, "path": p, "why": w})).collect::<Vec<_>>(),
            "zstd": self.has_zstd, "sqlite3": self.has_sqlite,
        })
    }
    fn from_json(v: &Value) -> RunInfo {
        RunInfo {
            at: gu(v, "at"),
            took_ms: gu(v, "took_ms"),
            error: v.get("error").and_then(Value::as_str).map(str::to_string),
            hostname: gs(v, "hostname"),
            home: gs(v, "home"),
            files_read: gu(v, "files_read"),
            bytes: gu(v, "bytes"),
            records: gu(v, "records"),
            budget: v
                .get("budget")
                .filter(|b| b.is_object())
                .map(|b| (gs(b, "reason"), gu(b, "files"), gu(b, "bytes"))),
            agents: v
                .get("agents")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(agent_from).collect())
                .unwrap_or_default(),
            skipped: v
                .get("skipped")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|s| (gs(s, "agent"), gs(s, "path"), gs(s, "why")))
                        .collect()
                })
                .unwrap_or_default(),
            has_zstd: v.get("zstd").and_then(Value::as_bool).unwrap_or(false),
            has_sqlite: v.get("sqlite3").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

impl Offsets {
    pub fn path() -> PathBuf {
        dir().join("offsets.json")
    }

    pub fn load() -> Offsets {
        let Ok(t) = fs::read_to_string(Self::path()) else {
            return Offsets::default();
        };
        let Ok(v) = serde_json::from_str::<Value>(&t) else {
            crate::log::warn(
                "usage",
                "offsets.json didn't parse: every log is read again",
            );
            return Offsets::default();
        };
        let mut o = Offsets::default();
        if let Some(hs) = v.get("hosts").and_then(Value::as_object) {
            for (h, hv) in hs {
                let mut st = HostState::default();
                if let Some(fs) = hv.get("files").and_then(Value::as_object) {
                    for (p, f) in fs {
                        st.files.insert(
                            p.clone(),
                            FileState {
                                agent: gs(f, "a"),
                                reader: gs(f, "k"),
                                dev: gu(f, "d"),
                                ino: gu(f, "i"),
                                size: gu(f, "s"),
                                mtime: gu(f, "m"),
                                off: gu(f, "o"),
                                ctx: gs(f, "c"),
                            },
                        );
                    }
                }
                st.run = hv
                    .get("run")
                    .filter(|r| r.is_object())
                    .map(RunInfo::from_json);
                st.ok_at = gu(hv, "ok_at");
                o.hosts.insert(h.clone(), st);
            }
        }
        o
    }

    pub fn save(&self) -> io::Result<()> {
        let mut hosts = serde_json::Map::new();
        for (h, st) in &self.hosts {
            let mut files = serde_json::Map::new();
            for (p, f) in &st.files {
                files.insert(
                    p.clone(),
                    json!({"a": f.agent, "k": f.reader, "d": f.dev, "i": f.ino, "s": f.size,
                           "m": f.mtime, "o": f.off, "c": f.ctx}),
                );
            }
            hosts.insert(
                h.clone(),
                json!({"files": files, "run": st.run.as_ref().map(RunInfo::to_json), "ok_at": st.ok_at}),
            );
        }
        let v = json!({"v": 1, "hosts": hosts});
        crate::config::write_atomic(&Self::path(), v.to_string().as_bytes())
    }
}

// ------------------------------------------------------------------ limits

/// A snapshot's slot: its limit id, and whose account it is when that's
/// known (two logins of one agent on one machine each keep their own).
fn slot_of(l: &Limit) -> String {
    let id = if l.id.is_empty() { "-" } else { l.id.as_str() };
    if l.account.len() >= 8 {
        format!("{id}@{}", &l.account[..8])
    } else {
        id.to_string()
    }
}

/// The newest snapshot per machine, agent, limit id and account.
pub fn save_limits(host: &str, agent: &str, ls: &[Limit]) {
    if ls.is_empty() {
        return;
    }
    let p = dir().join("limits.json");
    let mut v = raw_limits();
    for l in ls {
        let key = slot_of(l);
        // a slot from before accounts had their own, for this account: this one replaces it
        if !l.account.is_empty() {
            if let Some(ids) = v
                .get_mut(host)
                .and_then(|a| a.get_mut(agent))
                .and_then(Value::as_object_mut)
            {
                ids.retain(|k, s| {
                    *k == key
                        || s.get("account").and_then(Value::as_str) != Some(l.account.as_str())
                });
            }
        }
        let slot = &mut v[host][agent][key.as_str()];
        // a reading never gives way to an older one, nor to the bare account and plan (no windows)
        let had = slot
            .get("windows")
            .and_then(Value::as_array)
            .is_some_and(|w| !w.is_empty());
        if slot.get("ts").and_then(Value::as_u64).unwrap_or(0) > l.ts
            || (had && l.windows.is_empty())
        {
            continue;
        }
        *slot = json!({
            "ts": l.ts, "plan": l.plan,
            "windows": l.windows.iter().map(|w| json!({"used_percent": w.used_percent, "minutes": w.minutes, "resets_at": w.resets_at})).collect::<Vec<_>>(),
        });
        if !l.source.is_empty() {
            slot["source"] = json!(l.source);
        }
        if !l.account.is_empty() {
            slot["account"] = json!(l.account);
        }
    }
    let doc = json!({"v": 1, "hosts": v});
    let _ = crate::config::write_atomic(&p, doc.to_string().as_bytes());
}

/// Forget the plan read from `agent`'s own settings on `host` (it isn't installed there any more).
pub fn drop_cached_limits(host: &str, agent: &str) {
    let p = dir().join("limits.json");
    let mut v = raw_limits();
    let Some(ids) = v
        .get_mut(host)
        .and_then(|a| a.get_mut(agent))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    let before = ids.len();
    ids.retain(|_, s| s.get("source").and_then(Value::as_str) != Some("cache"));
    if ids.len() == before {
        return;
    }
    if ids.is_empty() {
        if let Some(a) = v.get_mut(host).and_then(Value::as_object_mut) {
            a.remove(agent);
        }
    }
    let doc = json!({"v": 1, "hosts": v});
    let _ = crate::config::write_atomic(&p, doc.to_string().as_bytes());
}

/// `limits.json`'s hosts: host → agent → id → snapshot (for the dashboard:
/// "Limits taken from logs"). The file is `{"v": 1, "hosts": …}`; before
/// it had a version, the hosts were the whole file.
pub fn limits() -> Value {
    current(raw_limits())
}

/// `limits.json`'s hosts as saved (no merging).
pub fn raw_limits() -> Value {
    let v: Value = fs::read_to_string(dir().join("limits.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    match v.get("hosts") {
        Some(h) if v.get("v").is_some() && h.is_object() => h.clone(),
        _ => v,
    }
}

/// The limits worth showing: a log's snapshot gives way to a newer one the
/// agent gave itself (live, or its own cache) on that machine, one saved
/// before accounts were recorded gives way to a newer one with its account
/// (the same login, then), and one account seen on several machines is one
/// limit, the newest, with `hosts` listing them all (its own first).
pub fn current(mut v: Value) -> Value {
    let ts = |s: &Value| s.get("ts").and_then(Value::as_u64).unwrap_or(0);
    let told = |s: &Value| {
        s.get("source")
            .and_then(Value::as_str)
            .is_some_and(|x| !x.is_empty())
    };
    let known = |s: &Value| {
        s.get("account")
            .and_then(Value::as_str)
            .is_some_and(|x| !x.is_empty())
    };
    let Some(hosts) = v.as_object_mut() else {
        return v;
    };
    for agents in hosts.values_mut().filter_map(Value::as_object_mut) {
        for ids in agents.values_mut().filter_map(Value::as_object_mut) {
            let newest_told = ids.values().filter(|s| told(s)).map(ts).max();
            if let Some(t) = newest_told {
                ids.retain(|_, s| told(s) || ts(s) > t);
            }
            let newest_known = ids.values().filter(|s| known(s)).map(ts).max();
            if let Some(t) = newest_known {
                ids.retain(|_, s| known(s) || ts(s) > t);
            }
        }
    }
    // one account: keep the newest, note where else it was seen
    let mut best: BTreeMap<(String, String), (String, String, u64)> = BTreeMap::new();
    let mut seen: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (host, agents) in hosts.iter() {
        for (agent, ids) in agents.as_object().into_iter().flatten() {
            for (id, s) in ids.as_object().into_iter().flatten() {
                let Some(acct) = s
                    .get("account")
                    .and_then(Value::as_str)
                    .filter(|a| !a.is_empty())
                else {
                    continue;
                };
                let k = (agent.clone(), acct.to_string());
                seen.entry(k.clone()).or_default().push(host.clone());
                if best.get(&k).is_none_or(|b| ts(s) > b.2) {
                    best.insert(k, (host.clone(), id.clone(), ts(s)));
                }
            }
        }
    }
    for (k, (host, id, _)) in &best {
        let all = &seen[k];
        if all.len() < 2 {
            continue;
        }
        for h in all.iter().filter(|h| *h != host) {
            if let Some(ids) = hosts
                .get_mut(h)
                .and_then(|a| a.get_mut(&k.0))
                .and_then(Value::as_object_mut)
            {
                ids.retain(|_, s| s.get("account").and_then(Value::as_str) != Some(k.1.as_str()));
            }
        }
        let mut list = vec![host.clone()];
        list.extend(all.iter().filter(|h| *h != host).cloned());
        hosts[host][&k.0][id]["hosts"] = json!(list);
    }
    // what's left empty goes
    for agents in hosts.values_mut().filter_map(Value::as_object_mut) {
        agents.retain(|_, ids| ids.as_object().is_some_and(|o| !o.is_empty()));
    }
    hosts.retain(|_, a| a.as_object().is_some_and(|o| !o.is_empty()));
    v
}

// ------------------------------------------------------------------ rollup

/// One rollup row: sums for an hour × machine × agent × session × model ×
/// effort × folder × account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    /// local day number (days since 1970-01-01, local time)
    pub day: i64,
    /// the local hour in it (0–23)
    pub hour: u8,
    pub host: String,
    pub agent: String,
    pub session: String,
    pub model: String,
    pub effort: String,
    pub project: String,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    pub reasoning: u64,
    pub requests: u64,
    pub cost_reported: f64,
    pub cost_estimated: f64,
    /// requests with neither a reported cost nor a known price
    pub unpriced: u64,
    pub active_ms: u64,
    pub turn_ms: u64,
    pub web: u64,
    pub first: u64,
    pub last: u64,
    /// the account (`accounts`; "" not known)
    pub acct: String,
    /// `acct` is a guess: the records didn't say (`accounts::Book::guess`)
    pub guess: bool,
    /// the agent's turn records in it: prompts it finished
    pub prompts: u64,
    /// requests the provider charged nothing for (`Record::free`): counted in
    /// tokens and requests, their cost a known $0
    pub free: u64,
    /// its folder's project (`projects::Index`, filled when queried; not kept)
    pub pid: String,
}

impl Row {
    fn to_json(&self) -> Value {
        json!([
            self.day,
            self.host,
            self.agent,
            self.session,
            self.model,
            self.effort,
            self.project,
            self.input,
            self.output,
            self.cache_read,
            self.cache_write_5m,
            self.cache_write_1h,
            self.reasoning,
            self.requests,
            self.cost_reported,
            self.cost_estimated,
            self.unpriced,
            self.active_ms,
            self.turn_ms,
            self.web,
            self.first,
            self.last,
            self.acct,
            self.guess,
            self.hour,
            self.prompts,
            self.free
        ])
    }
    fn from_json(v: &Value) -> Option<Row> {
        let a = v.as_array()?;
        if a.len() < 22 {
            return None;
        }
        let s = |i: usize| a[i].as_str().unwrap_or("").to_string();
        let n = |i: usize| a[i].as_u64().unwrap_or(0);
        let f = |i: usize| a[i].as_f64().unwrap_or(0.0);
        Some(Row {
            day: a[0].as_i64()?,
            host: s(1),
            agent: s(2),
            session: s(3),
            model: s(4),
            effort: s(5),
            project: s(6),
            input: n(7),
            output: n(8),
            cache_read: n(9),
            cache_write_5m: n(10),
            cache_write_1h: n(11),
            reasoning: n(12),
            requests: n(13),
            cost_reported: f(14),
            cost_estimated: f(15),
            unpriced: n(16),
            active_ms: n(17),
            turn_ms: n(18),
            web: n(19),
            first: n(20),
            last: n(21),
            acct: a.get(22).and_then(Value::as_str).unwrap_or("").to_string(),
            guess: a.get(23).and_then(Value::as_bool).unwrap_or(false),
            hour: a.get(24).and_then(Value::as_u64).unwrap_or(0) as u8,
            prompts: a.get(25).and_then(Value::as_u64).unwrap_or(0),
            free: a.get(26).and_then(Value::as_u64).unwrap_or(0),
            pid: String::new(),
        })
    }
}

/// The last line per (machine, key) wins: streamed duplicates, resumed
/// sessions and re-reads collapse. Two aliases for one machine (the same
/// hostname and home, `same_machine`) are one machine here, so a request
/// read through both counts once.
pub fn dedup(recs: Vec<Record>) -> Vec<Record> {
    dedup_on(recs, &same_machine())
}

/// [`dedup`] with the aliases to fold given (alias → the one it's counted as).
pub fn dedup_on(recs: Vec<Record>, canon: &HashMap<String, String>) -> Vec<Record> {
    let mut at: HashMap<(String, String), usize> = HashMap::new();
    let mut out: Vec<Option<Record>> = Vec::with_capacity(recs.len());
    for r in recs {
        let m = canon.get(&r.host).unwrap_or(&r.host).clone();
        let k = (m, r.key.clone());
        if let Some(&i) = at.get(&k) {
            out[i] = None;
        }
        at.insert(k, out.len());
        out.push(Some(r));
    }
    out.into_iter().flatten().collect()
}

/// Aliases that are one machine (the extractor said the same hostname and
/// home for both): each → the first of them, by name.
pub fn same_machine() -> HashMap<String, String> {
    let st = Offsets::load();
    let mut by: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (h, s) in &st.hosts {
        if let Some(r) = s.run.as_ref().filter(|r| !r.hostname.is_empty()) {
            by.entry((r.hostname.clone(), r.home.clone()))
                .or_default()
                .push(h.clone());
        }
    }
    let mut m = HashMap::new();
    for hs in by.values().filter(|v| v.len() > 1) {
        let first = hs.iter().min().cloned().unwrap_or_default();
        for h in hs {
            m.insert(h.clone(), first.clone());
        }
    }
    m
}

/// (machine, agent, session)
type SessionKey = (String, String, String);

/// Sum a month's records into rows (days at UTC offset `tz` seconds); a
/// request without an account gets the book's guess, marked.
pub fn build(recs: Vec<Record>, table: &prices::Table, tz: i64) -> Vec<Row> {
    build_with(recs, table, tz, &super::accounts::Book::default())
}

/// [`build`] with the accounts known (their guesses for requests that didn't say).
pub fn build_with(
    recs: Vec<Record>,
    table: &prices::Table,
    tz: i64,
    book: &super::accounts::Book,
) -> Vec<Row> {
    build_all(recs, table, tz, book).0
}

/// One prompt: what the agent did from being asked to finishing (its own
/// turn record: Claude Code's `turn_duration`, Codex's `task_complete`), and
/// the requests it made in that time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Prompt {
    /// when it finished (unix seconds) and how long it took
    pub ts: u64,
    pub secs: u64,
    /// its local day and hour (when it finished)
    pub day: i64,
    pub hour: u8,
    pub host: String,
    pub agent: String,
    pub session: String,
    pub acct: String,
    pub guess: bool,
    pub project: String,
    pub model: String,
    pub tokens: u64,
    pub output: u64,
    pub requests: u64,
    pub cost: f64,
    pub estimated: bool,
    /// its folder's project (filled when queried; not kept)
    pub pid: String,
}

impl Prompt {
    fn to_json(&self) -> Value {
        json!([
            self.ts,
            self.secs,
            self.day,
            self.hour,
            self.host,
            self.agent,
            self.session,
            self.acct,
            self.guess,
            self.project,
            self.model,
            self.tokens,
            self.output,
            self.requests,
            self.cost,
            self.estimated
        ])
    }
    fn from_json(v: &Value) -> Option<Prompt> {
        let a = v.as_array()?;
        if a.len() < 16 {
            return None;
        }
        let s = |i: usize| a[i].as_str().unwrap_or("").to_string();
        let n = |i: usize| a[i].as_u64().unwrap_or(0);
        Some(Prompt {
            ts: n(0),
            secs: n(1),
            day: a[2].as_i64()?,
            hour: n(3) as u8,
            host: s(4),
            agent: s(5),
            session: s(6),
            acct: s(7),
            guess: a[8].as_bool().unwrap_or(false),
            project: s(9),
            model: s(10),
            tokens: n(11),
            output: n(12),
            requests: n(13),
            cost: a[14].as_f64().unwrap_or(0.0),
            estimated: a[15].as_bool().unwrap_or(false),
            pid: String::new(),
        })
    }
}

/// A turn record this far past its window still holds a request (the
/// machine's clock writes both, but the record comes a moment after).
const TURN_SLACK: u64 = 3;

/// [`build_with`], and the prompts: each turn record with the requests
/// inside its window, priced like the rows.
pub fn build_all(
    recs: Vec<Record>,
    table: &prices::Table,
    tz: i64,
    book: &super::accounts::Book,
) -> (Vec<Row>, Vec<Prompt>) {
    let recs = dedup(recs);
    let (mut reqs, turns): (Vec<Record>, Vec<Record>) =
        recs.into_iter().partition(|r| !r.is_turn());
    reqs.sort_by(|a, b| {
        (&a.host, &a.agent, &a.session, a.ts).cmp(&(&b.host, &b.agent, &b.session, b.ts))
    });
    let day = |ts: u64| (ts as i64 + tz).div_euclid(86_400);
    let hour = |ts: u64| ((ts as i64 + tz).rem_euclid(86_400) / 3600) as u8;
    type Key = (
        i64,
        u8,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    );
    let mut rows: BTreeMap<Key, Row> = BTreeMap::new();
    // per session: (model, effort, project, account, guessed) of its latest request, for its turns
    type Last = (u64, String, String, String, String, bool);
    let mut last_of: HashMap<SessionKey, Vec<Last>> = HashMap::new();
    // per session: each request's (time, tokens, output, cost, estimated), for its prompts
    type Priced = (u64, u64, u64, f64, bool);
    let mut priced: HashMap<SessionKey, Vec<Priced>> = HashMap::new();
    let mut guessed: HashMap<(String, String, String), Option<String>> = HashMap::new();
    let mut prev: Option<(String, String, String, u64)> = None;
    let mut pricer = table.pricer();
    for r in &reqs {
        let sk = (r.host.clone(), r.agent.clone(), r.session.clone());
        let gap = match &prev {
            Some((h, a, s, t)) if (h, a, s) == (&sk.0, &sk.1, &sk.2) => r.ts.saturating_sub(*t),
            _ => u64::MAX,
        };
        prev = Some((sk.0.clone(), sk.1.clone(), sk.2.clone(), r.ts));
        // its account: what it said, else the book's guess (one per session and folder)
        let (acct, guess) = if r.acct.is_empty() {
            let g = guessed
                .entry((r.host.clone(), r.root.clone(), r.session.clone()))
                .or_insert_with(|| book.guess(r));
            match g {
                Some(a) => (a.clone(), true),
                None => (String::new(), false),
            }
        } else {
            (r.acct.clone(), false)
        };
        last_of.entry(sk.clone()).or_default().push((
            r.ts,
            r.model.clone(),
            r.effort.clone(),
            r.project.clone(),
            acct.clone(),
            guess,
        ));
        let k: Key = (
            day(r.ts),
            hour(r.ts),
            r.host.clone(),
            r.agent.clone(),
            r.session.clone(),
            r.model.clone(),
            r.effort.clone(),
            r.project.clone(),
            acct.clone(),
        );
        let row = rows.entry(k).or_insert_with(|| Row {
            day: day(r.ts),
            hour: hour(r.ts),
            host: r.host.clone(),
            agent: r.agent.clone(),
            session: r.session.clone(),
            model: r.model.clone(),
            effort: r.effort.clone(),
            project: r.project.clone(),
            first: r.ts,
            acct: acct.clone(),
            guess,
            ..Default::default()
        });
        row.input += r.input;
        row.output += r.output;
        row.cache_read += r.cache_read;
        row.cache_write_5m += r.cache_write_5m;
        row.cache_write_1h += r.cache_write_1h;
        row.reasoning += r.reasoning;
        row.web += r.web;
        row.requests += 1;
        row.first = row.first.min(r.ts);
        row.last = row.last.max(r.ts);
        if gap <= IDLE_GAP {
            row.active_ms += gap * 1000;
        }
        let (c, est) = match r.cost {
            Some(c) => {
                row.cost_reported += c;
                (c, false)
            }
            // free: a known $0 (counted as reported, never priced as the paid model)
            None if r.free() => {
                row.free += 1;
                (0.0, false)
            }
            None => match pricer.at(&r.model, &r.effort, r.ts) {
                Some(p) => {
                    let c = prices::cost(&p, &r.price_tokens(), r.fast(), r.us());
                    row.cost_estimated += c;
                    (c, true)
                }
                None => {
                    row.unpriced += 1;
                    (0.0, false)
                }
            },
        };
        priced
            .entry(sk)
            .or_default()
            .push((r.ts, r.tokens(), r.output, c, est));
    }
    let mut prompts = Vec::new();
    for t in &turns {
        let sk = (t.host.clone(), t.agent.clone(), t.session.clone());
        let (model, effort, project, acct, guess) = last_of
            .get(&sk)
            .and_then(|v| {
                v.iter()
                    .rev()
                    .find(|(ts, ..)| *ts <= t.ts)
                    .or_else(|| v.first())
                    .map(|(_, m, e, p, a, g)| (m.clone(), e.clone(), p.clone(), a.clone(), *g))
            })
            .unwrap_or_default();
        let k: Key = (
            day(t.ts),
            hour(t.ts),
            t.host.clone(),
            t.agent.clone(),
            t.session.clone(),
            model.clone(),
            effort.clone(),
            project.clone(),
            acct.clone(),
        );
        let row = rows.entry(k).or_insert_with(|| Row {
            day: day(t.ts),
            hour: hour(t.ts),
            host: t.host.clone(),
            agent: t.agent.clone(),
            session: t.session.clone(),
            model: model.clone(),
            effort,
            project: project.clone(),
            first: t.ts,
            last: t.ts,
            acct: acct.clone(),
            guess,
            ..Default::default()
        });
        row.turn_ms += t.turn_ms;
        row.prompts += 1;
        // the prompt: its session's requests from when it started to when it finished
        let secs = t.turn_ms.div_ceil(1000);
        let lo = t.ts.saturating_sub(secs + TURN_SLACK);
        let hi = t.ts + TURN_SLACK;
        let mut p = Prompt {
            ts: t.ts,
            secs,
            day: day(t.ts),
            hour: hour(t.ts),
            host: t.host.clone(),
            agent: t.agent.clone(),
            session: t.session.clone(),
            acct,
            guess,
            project,
            model,
            ..Default::default()
        };
        if let Some(v) = priced.get(&sk) {
            let start = v.partition_point(|x| x.0 < lo);
            for x in v[start..].iter().take_while(|x| x.0 <= hi) {
                p.tokens += x.1;
                p.output += x.2;
                p.cost += x.3;
                p.estimated |= x.4;
                p.requests += 1;
            }
        }
        prompts.push(p);
    }
    prompts.sort_by_key(|p| p.ts);
    (rows.into_values().collect(), prompts)
}

/// The rollup's shape (hourly rows with accounts, prompts): a change makes
/// every month be summed again.
const ROLLUP: &str = "r3";

struct MonthRollup {
    src: u64,
    /// what the price log held for its records when it was summed
    dated: String,
    rows: Vec<Row>,
    prompts: Vec<Prompt>,
}

/// The latest request a month's rows hold.
fn last_of(rows: &[Row]) -> u64 {
    rows.iter().map(|r| r.last).max().unwrap_or(0)
}

/// The rollup rows of these months: from `rollup2.json` where it's current,
/// rebuilt (and saved) where the records, the offset or the prices changed. A
/// price that moved only touches the months with requests after it, and a model
/// newly priced only the months where it had none.
pub fn rollup(months: &[String], table: &prices::Table) -> Vec<Row> {
    summed(months, table)
        .into_iter()
        .flat_map(|m| m.rows)
        .collect()
}

/// The prompts of these months (each turn record and its requests), as [`rollup`] keeps them.
pub fn prompts(months: &[String], table: &prices::Table) -> Vec<Prompt> {
    summed(months, table)
        .into_iter()
        .flat_map(|m| m.prompts)
        .collect()
}

fn summed(months: &[String], table: &prices::Table) -> Vec<MonthRollup> {
    let tz = crate::ui::utc_offset();
    // the prices, and which aliases are one machine: either changes the sums
    let same: BTreeMap<String, String> = same_machine().into_iter().collect();
    // the accounts' sightings: the guesses for requests that didn't say
    let book = super::accounts::Book::load();
    let stamp = format!("{ROLLUP}|{}|{same:?}|{}", table.stamp(), book.stamp());
    // rollup2.json: an older build keeps its rollup.json (rows without accounts)
    let p = dir().join("rollup2.json");
    let v: Value = fs::read_to_string(&p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    let same = v.get("tz").and_then(Value::as_i64) == Some(tz)
        && v.get("prices").and_then(Value::as_str) == Some(stamp.as_str());
    let mut have: BTreeMap<String, MonthRollup> = BTreeMap::new();
    if same {
        if let Some(ms) = v.get("months").and_then(Value::as_object) {
            for (m, mv) in ms {
                let list = |k: &str| {
                    mv.get(k)
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                };
                have.insert(
                    m.clone(),
                    MonthRollup {
                        src: gu(mv, "src"),
                        dated: gs(mv, "dated"),
                        rows: list("rows").iter().filter_map(Row::from_json).collect(),
                        prompts: list("prompts")
                            .iter()
                            .filter_map(Prompt::from_json)
                            .collect(),
                    },
                );
            }
        }
    }
    let mut changed = !same;
    for m in months {
        let size = file_size(m);
        let fresh = have.get(m).is_some_and(|r| {
            r.src == size
                && r.dated == table.dated_stamp(last_of(&r.rows))
                && !r
                    .rows
                    .iter()
                    .any(|x| x.unpriced > 0 && table.get(&x.model).is_some())
        });
        if !fresh {
            let (rows, prompts) = if size == 0 {
                (Vec::new(), Vec::new())
            } else {
                build_all(read_month(m), table, tz, &book)
            };
            have.insert(
                m.clone(),
                MonthRollup {
                    src: size,
                    dated: table.dated_stamp(last_of(&rows)),
                    rows,
                    prompts,
                },
            );
            changed = true;
        }
    }
    // months whose records are gone
    let on_disk = self::months();
    let before = have.len();
    have.retain(|m, _| on_disk.contains(m));
    changed |= have.len() != before;
    if changed {
        let mut ms = serde_json::Map::new();
        for (m, r) in &have {
            ms.insert(
                m.clone(),
                json!({"src": r.src,
                       "dated": r.dated,
                       "rows": r.rows.iter().map(Row::to_json).collect::<Vec<_>>(),
                       "prompts": r.prompts.iter().map(Prompt::to_json).collect::<Vec<_>>()}),
            );
        }
        let v = json!({"v": 1, "tz": tz, "prices": stamp, "months": ms});
        let _ = crate::config::write_atomic(&p, v.to_string().as_bytes());
    }
    months.iter().filter_map(|m| have.remove(m)).collect()
}

// ------------------------------------------------------------------ re-reads

/// The readers' generation: a reader that learned to read more reads its
/// logs again once, from the start. 2: the timeline's steps (every log);
/// 3: Codex 0.164's helper threads (its logs of the last week).
pub const GEN: u64 = 3;

/// What a host at generation `g` forgets of `files` to read them again.
pub fn regen(g: u64, files: &mut BTreeMap<String, FileState>, now: u64) {
    if g < 2 {
        files.clear();
    } else if g < 3 {
        files.retain(|_, f| f.agent != "codex" || f.mtime + 7 * 86_400 < now);
    }
}

/// Each machine's generation, and whether its re-read is still catching up
/// (`gens.json`: an older build never touches it).
pub fn gens() -> HashMap<String, (u64, bool)> {
    let v: Value = fs::read_to_string(dir().join("gens.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    v.as_object()
        .into_iter()
        .flatten()
        .map(|(h, x)| {
            (
                h.clone(),
                (
                    x.get("gen").and_then(Value::as_u64).unwrap_or(1),
                    x.get("catching_up")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
            )
        })
        .collect()
}

pub fn save_gens(g: &HashMap<String, (u64, bool)>) {
    let v: serde_json::Map<String, Value> = g
        .iter()
        .map(|(h, (n, c))| (h.clone(), json!({"gen": n, "catching_up": c})))
        .collect();
    let _ = crate::config::write_atomic(
        &dir().join("gens.json"),
        Value::Object(v).to_string().as_bytes(),
    );
}

/// Rewrite a month's records with one line per (machine, key), the last (a
/// re-read wrote them again); a line this build can't read is kept as it is.
pub fn compact(m: &str) -> io::Result<u64> {
    let p = dir().join(format!("records-{m}.tsv"));
    let Ok(text) = fs::read_to_string(&p) else {
        return Ok(0);
    };
    let lines: Vec<&str> = text.lines().collect();
    let mut last: HashMap<(String, String), usize> = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(r) = parse::from_line(l) {
            last.insert((r.host, r.key), i);
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut dropped = 0;
    for (i, l) in lines.iter().enumerate() {
        let keep = match parse::from_line(l) {
            Some(r) => last.get(&(r.host, r.key)) == Some(&i),
            None => true,
        };
        if keep {
            out.push_str(l);
            out.push('\n');
        } else {
            dropped += 1;
        }
    }
    if dropped > 0 {
        crate::config::write_atomic(&p, out.as_bytes())?;
    }
    Ok(dropped)
}

// ------------------------------------------------------------------ prune

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pruned {
    pub months_removed: Vec<String>,
    pub records_removed: u64,
    pub bytes_freed: u64,
}

/// Drop records older than `before` (unix seconds), or every record of
/// `host`. `dry` only counts.
pub fn prune(before: Option<u64>, host: Option<&str>, dry: bool) -> io::Result<Pruned> {
    let mut p = Pruned::default();
    for m in months() {
        let Some((start, end)) = month_bounds(&m) else {
            continue;
        };
        let file = month_file(&m);
        let size = file_size(&m);
        let whole_month_old = before.is_some_and(|b| end < b) && host.is_none();
        if whole_month_old {
            p.records_removed += fs::read_to_string(&file)
                .map(|t| t.lines().count() as u64)
                .unwrap_or(0);
            p.months_removed.push(m.clone());
            p.bytes_freed += size;
            if !dry {
                fs::remove_file(&file)?;
            }
            continue;
        }
        let touches = host.is_some() || before.is_some_and(|b| start < b);
        if !touches {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap_or_default();
        let mut keep = String::new();
        let mut dropped = 0u64;
        for l in text.lines() {
            let gone = match parse::from_line(l) {
                Some(r) => {
                    host.is_some_and(|h| r.host == h)
                        || (host.is_none() && before.is_some_and(|b| r.ts < b))
                }
                None => true,
            };
            if gone {
                dropped += 1;
            } else {
                keep.push_str(l);
                keep.push('\n');
            }
        }
        if dropped == 0 {
            continue;
        }
        p.records_removed += dropped;
        p.bytes_freed += size.saturating_sub(keep.len() as u64);
        if keep.is_empty() {
            p.months_removed.push(m.clone());
            if !dry {
                fs::remove_file(&file)?;
            }
        } else if !dry {
            crate::config::write_atomic(&file, keep.as_bytes())?;
        }
    }
    Ok(p)
}

#[cfg(test)]
mod tests {

    #[test]
    fn limits_worth_showing() {
        let v = json!({
            "b31": {
                "codex": {
                    "codex": {"ts": 100, "plan": "prolite", "windows": [{"used_percent": 69.0}]},
                    "old-id": {"ts": 90, "plan": "prolite", "windows": []}
                },
                "claude": {"claude": {"ts": 50, "plan": "Max 5x", "windows": [], "source": "cache"}}
            },
            "local": {
                "codex": {
                    "codex": {"ts": 300, "plan": "prolite", "windows": [{"used_percent": 1.0}], "source": "live", "account": "A"}
                }
            },
            "dex42": {
                "codex": {"codex": {"ts": 250, "plan": "prolite", "windows": [], "source": "live", "account": "A"}},
                "pi": {"x": {"ts": 1, "plan": "", "windows": []}}
            }
        });
        let c = current(v);
        // b31 asked nothing live: its log snapshots stay; Claude's cache too
        assert_eq!(c["b31"]["codex"].as_object().unwrap().len(), 2);
        assert_eq!(c["b31"]["claude"]["claude"]["plan"], "Max 5x");
        // one account on local and dex42: the newest, with both machines
        assert_eq!(
            c["local"]["codex"]["codex"]["hosts"],
            json!(["local", "dex42"])
        );
        assert!(c["dex42"].get("codex").is_none(), "{c}");
        assert!(c["dex42"]["pi"]["x"].is_object());
        // a live answer replaces that machine's older log snapshots
        let c = current(json!({"h": {"codex": {
            "codex": {"ts": 10, "windows": []},
            "live": {"ts": 20, "windows": [], "source": "live"},
            "newer-log": {"ts": 30, "windows": []}
        }}}));
        let ids: Vec<_> = c["h"]["codex"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(ids, ["live", "newer-log"]);
        // a slot from before accounts were recorded (no account) gives way
        // to a newer one with its account, even a log's: one Codex, not two
        let c = current(json!({
            "local": {"codex": {
                "codex": {"ts": 10, "plan": "prolite", "windows": [{"used_percent": 76.0}]},
                "codex@3a": {"ts": 50, "plan": "prolite", "windows": [], "account": "3a"}
            }},
            "b31": {"codex": {
                "codex": {"ts": 5, "plan": "prolite", "windows": []},
                "codex@3a": {"ts": 90, "plan": "prolite", "windows": [], "account": "3a", "source": "live"}
            }}
        }));
        assert!(c.get("local").is_none(), "{c}");
        let ids: Vec<_> = c["b31"]["codex"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(ids, ["codex@3a"]);
        assert_eq!(
            c["b31"]["codex"]["codex@3a"]["hosts"],
            json!(["b31", "local"])
        );
    }

    use super::*;

    fn rec(host: &str, session: &str, key: &str, ts: u64, model: &str, input: u64) -> Record {
        Record {
            ts,
            host: host.into(),
            agent: "claude".into(),
            session: session.into(),
            key: key.into(),
            model: model.into(),
            project: "~/p".into(),
            input,
            output: 10,
            ..Default::default()
        }
    }

    #[test]
    fn months_and_bounds() {
        assert_eq!(month_of(1_792_395_846), "2026-10");
        let (a, b) = month_bounds("2026-10").unwrap();
        assert_eq!(month_of(a), "2026-10");
        assert_eq!(month_of(b), "2026-10");
        assert_eq!(month_of(b + 1), "2026-11");
        assert_eq!(month_of(a - 1), "2026-09");
        assert!(month_bounds("2026-13").is_none());
        let (a, b) = month_bounds("2026-12").unwrap();
        assert_eq!(
            (month_of(a), month_of(b + 1)),
            ("2026-12".into(), "2027-01".into())
        );
    }

    #[test]
    fn last_line_per_key_wins() {
        let mut a = rec("b31", "s", "k1", 100, "m", 1);
        let b = rec("b31", "s", "k2", 110, "m", 2);
        let mut a2 = a.clone();
        a2.input = 5;
        a.output = 1;
        let none = HashMap::new();
        let d = dedup_on(vec![a.clone(), b.clone(), a2.clone()], &none);
        assert_eq!(d.len(), 2);
        assert!(d.contains(&a2));
        // another machine's request with the same key is its own
        let other_host = rec("gpu2", "s", "k1", 100, "m", 1);
        let d = dedup_on(
            vec![a.clone(), b.clone(), a2.clone(), other_host.clone()],
            &none,
        );
        assert_eq!(d.len(), 3);
        // …unless that's a second alias for the same machine: it counts once
        let same: HashMap<String, String> = [
            ("b31".to_string(), "b31".to_string()),
            ("gpu2".to_string(), "b31".to_string()),
        ]
        .into();
        let d = dedup_on(vec![a, b, a2, other_host.clone()], &same);
        assert_eq!(d.len(), 2);
        assert!(d.contains(&other_host));
    }

    #[test]
    fn a_request_costs_what_its_model_cost_when_it_ran() {
        let line = |from: u64, input: f64| {
            format!(
                "{{\"m\":\"gpt-9\",\"from\":{from},\"p\":[{input},1,1,1,1],\"src\":\"litellm\"}}\n"
            )
        };
        let moved = 1_792_400_000;
        let log = format!("{}{}", line(0, 2.0), line(moved, 3.0));
        let t = prices::Table::default()
            .with_dated(std::sync::Arc::new(crate::pricelog::Dated::parse(&log)));
        let recs = vec![
            rec("b31", "s1", "k1", moved - 10_000, "gpt-9", 1_000_000),
            rec("b31", "s2", "k2", moved + 10_000, "gpt-9", 1_000_000),
        ];
        let rows = build(recs, &t, 0);
        let cost = |s: &str| rows.iter().find(|r| r.session == s).unwrap().cost_estimated;
        assert!((cost("s1") - 2.0).abs() < 1e-3, "{}", cost("s1"));
        assert!((cost("s2") - 3.0).abs() < 1e-3, "{}", cost("s2"));
        // a month that ended before the change isn't summed again for it
        assert_eq!(t.dated_stamp(moved - 1), "0:0");
        assert_ne!(t.dated_stamp(moved + 1), "0:0");
    }

    #[test]
    fn rollup_rows_cost_and_active_time() {
        let t = prices::Table::default();
        let base = 1_792_395_846;
        let mut recs = vec![
            rec("b31", "s1", "k1", base, "claude-opus-5-5", 1_000_000),
            rec("b31", "s1", "k2", base + 60, "claude-opus-5-5", 0),
            // a long pause: not active time
            rec("b31", "s1", "k3", base + 4000, "claude-opus-5-5", 0),
            rec("b31", "s2", "k4", base + 10, "gpt-6-astra", 100),
        ];
        let mut reported = rec("b31", "s3", "k5", base, "whatever", 1);
        reported.cost = Some(0.5);
        recs.push(reported);
        recs.push(Record {
            ts: base + 70,
            host: "b31".into(),
            agent: "claude".into(),
            session: "s1".into(),
            key: "turn1".into(),
            turn_ms: 70_000,
            ..Default::default()
        });
        let rows = build(recs, &t, 0);
        let s1: Vec<&Row> = rows.iter().filter(|r| r.session == "s1").collect();
        // a row an hour: the pause put its third request in the next one
        assert_eq!(s1.len(), 2, "same hour, model, effort, folder: one row");
        let r = s1[0];
        assert_eq!(r.requests, 2);
        assert_eq!(s1[1].requests, 1);
        assert_eq!(r.active_ms + s1[1].active_ms, 60_000);
        assert_eq!(r.turn_ms, 70_000);
        assert_eq!(r.prompts, 1);
        // 1M input at $4 plus 30 output tokens at $20/M
        let est: f64 = s1.iter().map(|r| r.cost_estimated).sum();
        assert!((est - (4.0 + 30.0 * 20e-6)).abs() < 1e-9, "{est}");
        assert_eq!(r.unpriced, 0);
        let s2 = rows.iter().find(|r| r.session == "s2").unwrap();
        assert_eq!((s2.unpriced, s2.cost_estimated), (1, 0.0));
        let s3 = rows.iter().find(|r| r.session == "s3").unwrap();
        assert_eq!((s3.cost_reported, s3.unpriced), (0.5, 0));
        // days follow the UTC offset
        let late = vec![rec("h", "s", "k", 1_792_368_000 - 1, "m", 1)];
        let utc = build(late.clone(), &t, 0)[0].day;
        let ahead = build(late, &t, 3600)[0].day;
        assert_eq!(ahead, utc + 1);
    }

    #[test]
    fn rows_survive_json() {
        let r = Row {
            day: 20_748,
            host: "b31".into(),
            agent: "codex".into(),
            session: "s".into(),
            model: "gpt-5".into(),
            effort: "high".into(),
            project: "~/x".into(),
            input: 1,
            output: 2,
            cache_read: 3,
            cache_write_5m: 4,
            cache_write_1h: 5,
            reasoning: 6,
            requests: 7,
            cost_reported: 0.25,
            cost_estimated: 1.5,
            unpriced: 8,
            active_ms: 9,
            turn_ms: 10,
            web: 11,
            first: 12,
            last: 13,
            acct: "a3f2c1aa00000000".into(),
            guess: true,
            hour: 14,
            prompts: 3,
            free: 2,
            pid: String::new(),
        };
        assert_eq!(Row::from_json(&r.to_json()), Some(r.clone()));
        // a row from before accounts (22 values) still reads, with none
        let mut old = r.to_json();
        old.as_array_mut().unwrap().truncate(22);
        let o = Row::from_json(&old).unwrap();
        assert!(o.acct.is_empty() && !o.guess && o.last == 13 && o.hour == 0 && o.prompts == 0);
    }

    #[test]
    fn hours_and_prompts() {
        let t = prices::Table::default();
        // three requests in one prompt (a turn record of 100 s ending at 1_000_100),
        // one an hour later in no prompt
        let mut v = vec![
            rec("h", "s", "a", 1_000_010, "claude-opus-5-5", 100),
            rec("h", "s", "b", 1_000_050, "claude-opus-5-5", 100),
            rec("h", "s", "c", 1_000_099, "claude-opus-5-5", 100),
            rec("h", "s", "d", 1_004_000, "claude-opus-5-5", 100),
        ];
        v.push(Record {
            ts: 1_000_100,
            host: "h".into(),
            agent: "claude".into(),
            session: "s".into(),
            key: "t1".into(),
            turn_ms: 100_000,
            ..Default::default()
        });
        let (rows, prompts) = build_all(v, &t, 0, &crate::usage::accounts::Book::default());
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        assert_eq!((p.requests, p.secs, p.tokens), (3, 100, 330));
        assert!(p.cost > 0.0 && p.estimated);
        // rows by the hour: 1_000_010 is 13:46 UTC, 1_004_000 is 14:53
        let hours: Vec<u8> = rows.iter().map(|r| r.hour).collect();
        assert!(hours.contains(&13) && hours.contains(&14), "{hours:?}");
        assert_eq!(rows.iter().map(|r| r.prompts).sum::<u64>(), 1);
        assert_eq!(rows.iter().map(|r| r.requests).sum::<u64>(), 4);
        let back: Vec<Prompt> = prompts
            .iter()
            .filter_map(|p| Prompt::from_json(&p.to_json()))
            .collect();
        assert_eq!(back, prompts);
    }

    #[test]
    fn a_request_without_an_account_gets_the_guess_marked() {
        use crate::usage::accounts::{Account, Book};
        let t = prices::Table::default();
        let mut b = Book::default();
        b.note(Account {
            id: "aaaa000000000000".into(),
            agent: "claude".into(),
            ..Default::default()
        });
        b.sight("h", "r1", "aaaa000000000000", 5_000, 6_000);
        let mut known = rec("h", "s1", "k1", 5_500, "m", 1);
        known.acct = "aaaa000000000000".into();
        known.root = "r1".into();
        let mut old = rec("h", "s2", "k2", 1_000, "m", 1);
        old.root = "r1".into();
        let other = rec("gpu2", "s3", "k3", 1_000, "m", 1);
        let rows = build_with(vec![known, old, other], &t, 0, &b);
        let by = |s: &str| rows.iter().find(|r| r.session == s).unwrap();
        assert_eq!(
            (by("s1").acct.as_str(), by("s1").guess),
            ("aaaa000000000000", false)
        );
        assert_eq!(
            (by("s2").acct.as_str(), by("s2").guess),
            ("aaaa000000000000", true)
        );
        assert_eq!((by("s3").acct.as_str(), by("s3").guess), ("", false));
    }
}
