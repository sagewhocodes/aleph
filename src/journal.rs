//! The session journal: a Markdown log per day of what each session did,
//! for standups and notes — `config::data_dir()/journal/YYYY-MM-DD.md`
//! (`~/.local/share/aleph/journal/`; `journal.dir` moves it).
//!
//! What goes in is `journal.include` (all by default):
//!
//! * **prompts** — the prompts sent through Aleph (`sentlog.rs`: the TUI's
//!   send, broadcast and batch, `aleph send`, queue hand-outs, phone and web
//!   replies, a new session's first prompt, hand-offs, races). Text typed
//!   straight into herdr isn't seen; the header says so.
//! * **finals** — each finished turn in the agent's own words (a Done
//!   event's `body`, from its hook), else the turn's title.
//! * **files** — what changed in each session's folder: its commits of the
//!   day (`git log --numstat`, by the repository's own `user.email`) and,
//!   for today, what isn't committed yet. One `sh -s` per machine, off the
//!   UI thread; a folder that's gone is never entered (`cd … || exit`).
//! * **cost** — time worked (the state log's busy time, `statelog.rs`),
//!   tokens and $ per session and per turn (`turncost.rs`, priced like
//!   everything else).
//!
//! Who writes it: the hub-lock holder (the daemon, else the dashboard) —
//! [`Writer`] refreshes today's file every half hour and a minute after a
//! session ends, and finishes yesterday's after midnight (marked final, then
//! left alone). `aleph journal --write` does it now. Reading it: `aleph
//! journal [today|yesterday|DATE] [--write] [--json]` and the dashboard's
//! "Journal" (the palette; Inbox `J`).
//!
//! Showcase mode: what's shown (the dashboard's view, the CLI's print) goes
//! through the lens; the file itself stays real. The demo world keeps a
//! journal of its own (`showcase::sandbox`).

use crate::cli::Args;
use crate::config::{self, Config};
use crate::events::{self, Event, Kind};
use crate::herdr::Session;
use crate::out::*;
use crate::prices;
use crate::restore::Known;
use crate::sentlog::Sent;
use crate::statelog::{St, Stretch};
use crate::turncost::{self, Turn};
use crate::ui::{self, c};
use crate::usage::{self, dash, parse::Record, store};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the hub refreshes today's file.
const EVERY: Duration = Duration::from_secs(30 * 60);
/// How long after a session ends the hub writes again.
const AFTER_END: Duration = Duration::from_secs(60);
/// A machine's git look may take this long.
const GIT_WAIT: Duration = Duration::from_secs(30);
/// What a finished file ends with (it's then left alone).
const FINAL: &str = "<!-- aleph journal: final -->";

// ------------------------------------------------------------------ what goes in

/// `journal.include`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Include {
    pub prompts: bool,
    pub finals: bool,
    pub files: bool,
    pub cost: bool,
}

impl Include {
    pub const ALL: Include = Include {
        prompts: true,
        finals: true,
        files: true,
        cost: true,
    };
    pub const NONE: Include = Include {
        prompts: false,
        finals: false,
        files: false,
        cost: false,
    };

    /// From `journal.include` (pure — tested): `all` (the default) or `on`,
    /// `off` / `none`, a list (`prompts,finals`), or all but some (`-files`).
    pub fn parse(v: &str) -> Include {
        let mut inc = Include::NONE;
        let mut any = false;
        for w in v.split(',').map(|w| w.trim().to_lowercase()) {
            let (minus, name) = match w.strip_prefix('-') {
                Some(n) => (true, n.to_string()),
                None => (false, w.clone()),
            };
            if minus && !any {
                inc = Include::ALL;
            }
            match name.as_str() {
                "" => continue,
                "all" | "on" => inc = Include::ALL,
                "off" | "none" => inc = Include::NONE,
                "prompts" => inc.prompts = !minus,
                "finals" => inc.finals = !minus,
                "files" => inc.files = !minus,
                "cost" => inc.cost = !minus,
                _ => {}
            }
            any = true;
        }
        if any {
            inc
        } else {
            Include::ALL
        }
    }

    pub fn any(&self) -> bool {
        self.prompts || self.finals || self.files || self.cost
    }

    fn names(&self) -> Vec<&'static str> {
        [
            (self.prompts, "prompts"),
            (self.finals, "finals"),
            (self.files, "files"),
            (self.cost, "cost"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, n)| *n)
        .collect()
    }
}

pub fn include(cfg: &Config) -> Include {
    Include::parse(&cfg.get_or("journal.include"))
}

// ------------------------------------------------------------------ where

/// The journal's folder: `journal.dir`, else `data_dir()/journal` (the demo
/// world's own inside either).
pub fn dir(cfg: &Config) -> PathBuf {
    let base = match cfg.get("journal.dir").filter(|d| !d.trim().is_empty()) {
        Some(d) => config::expand(d.trim()),
        None => config::data_dir().join("journal"),
    };
    crate::showcase::sandbox(base)
}

/// `…/journal/2026-10-24.md`.
pub fn path(cfg: &Config, day: i64) -> PathBuf {
    dir(cfg).join(format!("{}.md", usage::day_name(day)))
}

/// The first and the last-plus-one unix second of a local day (UTC offset `tz`).
pub fn bounds(day: i64, tz: i64) -> (u64, u64) {
    let from = (day * 86_400 - tz).max(0) as u64;
    (from, from + 86_400)
}

/// `today`, `yesterday`, `2026-10-24`, `-2` (two days ago) → a local day.
pub fn parse_day(w: &str, today: i64) -> Option<i64> {
    match w.trim().to_lowercase().as_str() {
        "" | "today" => Some(today),
        "yesterday" => Some(today - 1),
        x => {
            if let Some(n) = x.strip_prefix('-').and_then(|n| n.parse::<i64>().ok()) {
                return Some(today - n);
            }
            usage::parse_date(x)
        }
    }
}

/// `Friday 24 October 2026`.
pub fn day_title(day: i64) -> String {
    const WD: [&str; 7] = [
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
    ];
    const MO: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let (y, m, d) = ui::civil(day);
    format!(
        "{} {d} {} {y}",
        WD[day.rem_euclid(7) as usize],
        MO[(m - 1) as usize]
    )
}

/// `14:05` at UTC offset `tz`.
fn clock(t: u64, tz: i64) -> String {
    let s = t as i64 + tz;
    format!(
        "{:02}:{:02}",
        s.rem_euclid(86_400) / 3600,
        s.rem_euclid(3600) / 60
    )
}

// ------------------------------------------------------------------ the day

/// A finished turn, as the journal tells it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Final {
    pub t: u64,
    pub text: String,
    pub turn: Option<Turn>,
}

/// One commit of the day.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub t: u64,
    pub subject: String,
    pub files: Vec<String>,
    pub add: u64,
    pub del: u64,
}

/// What isn't committed (`git diff HEAD`), and files git doesn't track yet.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stat {
    pub files: u64,
    pub add: u64,
    pub del: u64,
    pub new: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Repo {
    pub top: String,
    pub commits: Vec<Commit>,
    pub dirty: Option<Stat>,
    /// an earlier session of the day works in the same repository (its name)
    pub same_as: Option<String>,
}

/// What a session's folder says.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Files {
    /// not looked at (files left out, the demo world, no folder)
    #[default]
    Unknown,
    /// the folder isn't there any more
    Gone,
    NotGit,
    /// the machine didn't answer, or git failed: why
    Failed(String),
    Repo(Repo),
}

/// One session's day.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub host: String,
    pub name: String,
    pub agent: String,
    /// where it works (as the machine has it)
    pub folder: String,
    /// its agent's session ids over the day
    pub natives: Vec<String>,
    pub prompts: Vec<(u64, String)>,
    pub finals: Vec<Final>,
    pub files: Files,
    /// seconds at work
    pub busy: u64,
    pub tokens: u64,
    pub requests: u64,
    pub cost: f64,
    pub estimated: bool,
    /// its first and last moment of the day
    pub first: u64,
    pub last: u64,
}

impl Entry {
    fn touch(&mut self, t: u64) {
        if self.first == 0 || t < self.first {
            self.first = t;
        }
        self.last = self.last.max(t);
    }
    fn active(&self) -> bool {
        !self.prompts.is_empty() || !self.finals.is_empty() || self.busy > 0 || self.requests > 0
    }
}

/// The journal of one day.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Day {
    /// local day number
    pub day: i64,
    pub from: u64,
    pub to: u64,
    /// when it was put together
    pub made: u64,
    /// the day is over (this is how it stays)
    pub done: bool,
    pub include: Option<Include>,
    /// the UTC offset its times are told in
    pub tz: i64,
    pub entries: Vec<Entry>,
}

impl Day {
    fn inc(&self) -> Include {
        self.include.unwrap_or(Include::ALL)
    }
}

/// What a day is put together from (all read before; [`assemble`] is pure).
#[derive(Default)]
pub struct Sources<'a> {
    /// session memory (`sessions.json`): folders, agents, agent ids
    pub known: &'a [Known],
    /// the fleet now (an open working stretch, folders, agent ids)
    pub live: &'a [Session],
    /// the state log
    pub stretches: &'a [Stretch],
    /// the event log
    pub events: &'a [Event],
    /// prompts sent through Aleph
    pub sent: &'a [Sent],
    /// usage records of the sessions' agent ids
    pub recs: &'a [Record],
}

type Key = (String, String);

/// The day from its sources (pure — tested): every session that did
/// something (a prompt, a finished turn, time at work, tokens), oldest
/// first. Files are filled in after ([`fill_files`]).
pub fn assemble(
    day: i64,
    tz: i64,
    now: u64,
    inc: Include,
    src: &Sources,
    table: &prices::Table,
) -> Day {
    let (from, to) = bounds(day, tz);
    let upto = now.min(to);
    let inside = |t: u64| t >= from && t < to;
    let mut m: BTreeMap<Key, Entry> = BTreeMap::new();
    let entry = |m: &mut BTreeMap<Key, Entry>, host: &str, name: &str| -> Key {
        let k = (host.to_string(), name.to_string());
        m.entry(k.clone()).or_insert_with(|| Entry {
            host: host.to_string(),
            name: name.to_string(),
            ..Default::default()
        });
        k
    };
    // a pane's session at a moment: the state log's stretch then, else the
    // fleet now, else session memory
    let name_of = |host: &str, pane: &str, t: u64| -> Option<String> {
        src.stretches
            .iter()
            .find(|s| s.host == host && s.pane == pane && s.start <= t + 60 && s.end + 60 >= t)
            .map(|s| s.name.clone())
            .or_else(|| {
                src.live
                    .iter()
                    .find(|s| s.host == host && s.pane_id == pane)
                    .map(|s| s.name.clone())
            })
            .or_else(|| {
                src.known
                    .iter()
                    .filter(|k| k.host == host && k.pane_id == pane)
                    .max_by_key(|k| k.last_seen)
                    .map(|k| k.name.clone())
            })
            .filter(|n| !n.is_empty())
    };
    // time at work: the state log's stretches, and the one going on now
    for s in src
        .stretches
        .iter()
        .filter(|s| s.end > from && s.start < to)
    {
        if !(s.st.busy() || s.st == St::Needs) || s.name.is_empty() {
            continue;
        }
        let k = entry(&mut m, &s.host, &s.name);
        let e = m.get_mut(&k).unwrap();
        if e.agent.is_empty() {
            e.agent = s.agent.clone();
        }
        if s.st.busy() {
            e.busy += s.end.min(to) - s.start.max(from);
        }
        e.touch(s.start.max(from));
        e.touch(s.end.min(to).saturating_sub(1));
    }
    for s in src.live.iter().filter(|s| s.status == "working") {
        let Some(since) = s.since else { continue };
        let (a, b) = (since.max(from), upto);
        if b > a && !s.name.is_empty() {
            let k = entry(&mut m, &s.host, &s.name);
            let e = m.get_mut(&k).unwrap();
            e.busy += b - a;
            e.touch(a);
            e.touch(b.saturating_sub(1));
        }
    }
    // finished turns (each pair told by herdr and the agent counted once)
    let evs: Vec<Event> = events::merged(
        src.events
            .iter()
            .filter(|e| inside(e.t) && !e.name.is_empty())
            .cloned()
            .collect(),
    );
    let mut done: HashMap<Key, Vec<&Event>> = HashMap::new();
    for e in &evs {
        match e.kind {
            Kind::Done => {
                let k = entry(&mut m, &e.host, &e.name);
                let en = m.get_mut(&k).unwrap();
                en.touch(e.t);
                done.entry(k).or_default().push(e);
            }
            Kind::Blocked | Kind::Task | Kind::Auto => {
                let k = entry(&mut m, &e.host, &e.name);
                m.get_mut(&k).unwrap().touch(e.t);
            }
            _ => {}
        }
    }
    // the prompts sent
    for s in src.sent.iter().filter(|s| inside(s.t)) {
        let name = if s.name.is_empty() {
            name_of(&s.host, &s.pane, s.t).unwrap_or_else(|| s.pane.clone())
        } else {
            s.name.clone()
        };
        if name.is_empty() {
            continue;
        }
        let k = entry(&mut m, &s.host, &name);
        let e = m.get_mut(&k).unwrap();
        e.prompts.push((s.t, s.text.clone()));
        e.touch(s.t);
    }
    // each session's agent ids: the fleet's, memory's, its events'
    for e in m.values_mut() {
        let mut ids: Vec<String> = Vec::new();
        let mut add = |id: &str| {
            if !id.is_empty() && !ids.iter().any(|x| x == id) {
                ids.push(id.to_string());
            }
        };
        for s in src
            .live
            .iter()
            .filter(|s| s.host == e.host && s.name == e.name)
        {
            add(s.native_id.as_deref().unwrap_or(""));
        }
        for k in src
            .known
            .iter()
            .filter(|k| k.host == e.host && k.name == e.name)
        {
            add(&k.agent_session);
        }
        for ev in evs.iter().filter(|x| x.host == e.host && x.name == e.name) {
            add(&ev.native);
        }
        e.natives = ids;
    }
    // tokens and $: the day's requests of its agent ids, priced like the rest
    let reqs: Vec<Record> = store::dedup(
        src.recs
            .iter()
            .filter(|r| !r.is_turn() && inside(r.ts))
            .cloned()
            .collect(),
    );
    for e in m.values_mut() {
        // (its own machine's: a home shared between machines shows the same
        // conversation to each)
        let mine: Vec<Record> = turncost::on_host(&reqs, &e.host)
            .into_iter()
            .filter(|r| e.natives.contains(&r.session))
            .cloned()
            .collect();
        if let (Some(a), Some(b)) = (
            mine.iter().map(|r| r.ts).min(),
            mine.iter().map(|r| r.ts).max(),
        ) {
            e.touch(a);
            e.touch(b);
        }
        for r in store::build(mine, table, tz) {
            e.tokens += r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
            e.requests += r.requests;
            e.cost += r.cost_reported + r.cost_estimated;
            e.estimated |= r.cost_estimated > 0.0;
        }
    }
    // what each turn ended with, and what it took
    for (k, v) in &done {
        let e = m.get_mut(k).unwrap();
        let mut v = v.clone();
        v.sort_by_key(|x| x.t);
        for ev in v {
            let words = if ev.body.trim().is_empty() {
                ev.title.trim()
            } else {
                ev.body.trim()
            };
            let text = one_line(words, 280);
            let native = if ev.native.is_empty() {
                e.natives.first().cloned().unwrap_or_default()
            } else {
                ev.native.clone()
            };
            let stretch = src
                .stretches
                .iter()
                .filter(|s| {
                    s.host == ev.host
                        && s.st.busy()
                        && (s.name == ev.name || (!ev.pane.is_empty() && s.pane == ev.pane))
                        && s.end <= ev.t + 5
                        && s.end + 120 >= ev.t
                })
                .max_by_key(|s| s.end)
                .map(|s| (s.start, s.end));
            let ask = turncost::Ask {
                host: &ev.host,
                native: &native,
                t: ev.t,
                took: ev.took,
                stretch,
            };
            let turn = turncost::of(&ask, src.recs, table, tz);
            // the same words again (a title that didn't change): once
            if e.finals
                .last()
                .is_some_and(|f| f.text == text && turn.is_none())
            {
                continue;
            }
            e.finals.push(Final {
                t: ev.t,
                text,
                turn,
            });
        }
    }
    // what's left: who did something, with its agent and folder
    let mut out: Vec<Entry> = m.into_values().filter(Entry::active).collect();
    for e in out.iter_mut() {
        e.prompts.sort_by_key(|p| p.0);
        let live = src
            .live
            .iter()
            .find(|s| s.host == e.host && s.name == e.name);
        let known = src
            .known
            .iter()
            .filter(|k| k.host == e.host && k.name == e.name)
            .max_by_key(|k| k.last_seen);
        if let Some(s) = live {
            if e.agent.is_empty() {
                e.agent = s.agent.clone();
            }
            e.folder = s.cwd.clone();
        }
        if let Some(k) = known {
            if e.agent.is_empty() {
                e.agent = k.agent.clone();
            }
            if e.folder.is_empty() {
                e.folder = k.cwd.clone();
            }
        }
        if e.folder.is_empty() {
            e.folder = evs
                .iter()
                .filter(|x| x.host == e.host && x.name == e.name && x.kind == Kind::New)
                .map(|x| x.title.clone())
                .find(|t| t.starts_with('/') || t.starts_with('~'))
                .unwrap_or_default();
        }
        if e.folder.is_empty() {
            e.folder = turncost::on_host(&reqs, &e.host)
                .into_iter()
                .filter(|r| e.natives.contains(&r.session) && !r.project.is_empty())
                .max_by_key(|r| r.ts)
                .map(|r| r.project.clone())
                .unwrap_or_default();
        }
        if e.agent.is_empty() {
            e.agent = evs
                .iter()
                .find(|x| x.host == e.host && x.name == e.name && !x.agent.is_empty())
                .map(|x| x.agent.clone())
                .unwrap_or_default();
        }
        if !inc.prompts {
            e.prompts.clear();
        }
        if !inc.finals {
            e.finals.clear();
        }
    }
    out.sort_by(|a, b| (a.first, &a.host, &a.name).cmp(&(b.first, &b.host, &b.name)));
    Day {
        day,
        from,
        to,
        made: now,
        done: now >= to,
        include: Some(inc),
        tz,
        entries: out,
    }
}

/// Words on one line, at most `n` characters.
fn one_line(s: &str, n: usize) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= n {
        return flat;
    }
    let mut v: String = flat.chars().take(n.saturating_sub(1)).collect();
    v.push('…');
    v
}

/// Each session's files from the machines' looks (pure — tested): keyed by
/// (machine, folder as the session has it). A repository an earlier session
/// of the day already showed is named, not repeated.
pub fn fill_files(d: &mut Day, looks: &HashMap<Key, Files>) {
    let mut seen: HashMap<Key, String> = HashMap::new();
    for e in d.entries.iter_mut() {
        let Some(f) = looks.get(&(e.host.clone(), e.folder.clone())) else {
            continue;
        };
        let mut f = f.clone();
        if let Files::Repo(r) = &mut f {
            let k = (e.host.clone(), r.top.clone());
            match seen.get(&k) {
                Some(first) => {
                    r.same_as = Some(first.clone());
                    r.commits.clear();
                    r.dirty = None;
                }
                None => {
                    seen.insert(k, e.name.clone());
                }
            }
        }
        e.files = f;
    }
}

// ------------------------------------------------------------------ git, per machine

/// A folder for `sh`: `'/srv/x'`, `"$HOME"/'x'` for `~/x`.
fn sh_path(p: &str) -> String {
    let q = |s: &str| format!("'{}'", s.replace('\'', "'\\''"));
    match p.strip_prefix("~/") {
        Some(r) => format!("\"$HOME\"/{}", q(r)),
        None if p == "~" => "\"$HOME\"".into(),
        None => q(p),
    }
}

/// The look at these folders over `[since, until)` (`until` None: up to now,
/// with what isn't committed too): one `sh -s` for a machine (pure — tested).
pub fn git_script(folders: &[String], since: u64, until: Option<u64>) -> String {
    let until_arg = until.map(|u| format!(" --until=@{u}")).unwrap_or_default();
    let dirty = if until.is_none() {
        "  echo U\n  git diff HEAD --numstat 2>/dev/null | head -n 2000\n  printf 'n\\t%s\\n' \"$(git ls-files -o --exclude-standard 2>/dev/null | head -n 5000 | wc -l | tr -d ' ')\"\n"
    } else {
        ""
    };
    let mut s = format!(
        "LC_ALL=C; export LC_ALL\n\
         look() {{\n\
         \x20 printf 'D\\t%s\\n' \"$1\"\n\
         \x20 cd \"$2\" 2>/dev/null || {{ echo G; exit 0; }}\n\
         \x20 top=$(git rev-parse --show-toplevel 2>/dev/null) || {{ echo N; exit 0; }}\n\
         \x20 printf 'T\\t%s\\n' \"$top\"\n\
         \x20 me=$(git config user.email 2>/dev/null)\n\
         \x20 git log --no-merges --since=@{since}{until_arg} ${{me:+\"--author=$me\"}} --format='C%x09%h%x09%ct%x09%s' --numstat 2>/dev/null | head -n 4000\n\
         {dirty}\
         \x20 echo E\n\
         }}\n"
    );
    for f in folders {
        let key = format!("'{}'", f.replace('\'', "'\\''"));
        s.push_str(&format!("( look {key} {} )\n", sh_path(f)));
    }
    s
}

/// What [`git_script`] printed → each folder's files (pure — tested).
pub fn parse_git(text: &str) -> HashMap<String, Files> {
    let mut out: HashMap<String, Files> = HashMap::new();
    let mut cur: Option<(String, Repo, bool)> = None;
    let finish = |out: &mut HashMap<String, Files>, cur: &mut Option<(String, Repo, bool)>| {
        if let Some((d, r, _)) = cur.take() {
            out.insert(d, Files::Repo(r));
        }
    };
    for l in text.lines() {
        let f: Vec<&str> = l.splitn(4, '\t').collect();
        match f[0] {
            "D" if f.len() >= 2 => {
                finish(&mut out, &mut cur);
                cur = Some((f[1].to_string(), Repo::default(), false));
            }
            "G" | "N" => {
                if let Some((d, _, _)) = cur.take() {
                    out.insert(
                        d,
                        if f[0] == "G" {
                            Files::Gone
                        } else {
                            Files::NotGit
                        },
                    );
                }
            }
            "T" if f.len() >= 2 => {
                if let Some((_, r, _)) = cur.as_mut() {
                    r.top = f[1].to_string();
                }
            }
            "C" if f.len() >= 4 => {
                if let Some((_, r, _)) = cur.as_mut() {
                    r.commits.push(Commit {
                        hash: f[1].to_string(),
                        t: f[2].trim().parse().unwrap_or(0),
                        subject: f[3].to_string(),
                        ..Default::default()
                    });
                }
            }
            "U" => {
                if let Some((_, r, dirty)) = cur.as_mut() {
                    r.dirty = Some(Stat::default());
                    *dirty = true;
                }
            }
            "n" if f.len() >= 2 => {
                if let Some((_, r, _)) = cur.as_mut() {
                    if let Some(s) = r.dirty.as_mut() {
                        s.new = f[1].trim().parse().unwrap_or(0);
                    }
                }
            }
            "E" => finish(&mut out, &mut cur),
            _ => {
                // a numstat line: added, deleted, file (`-` for a binary)
                let p: Vec<&str> = l.splitn(3, '\t').collect();
                if p.len() != 3 {
                    continue;
                }
                let num = |x: &str| x.trim().parse::<u64>().unwrap_or(0);
                let (a, dl) = (num(p[0]), num(p[1]));
                if let Some((_, r, dirty)) = cur.as_mut() {
                    if *dirty {
                        if let Some(s) = r.dirty.as_mut() {
                            s.files += 1;
                            s.add += a;
                            s.del += dl;
                        }
                    } else if let Some(c) = r.commits.last_mut() {
                        c.files.push(p[2].to_string());
                        c.add += a;
                        c.del += dl;
                    }
                }
            }
        }
    }
    finish(&mut out, &mut cur);
    out
}

/// Every session's folder looked at, one `sh -s` per machine at once (blocking).
fn look_all(d: &Day) -> HashMap<Key, Files> {
    let mut by: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in d.entries.iter().filter(|e| !e.folder.is_empty()) {
        by.entry(e.host.clone())
            .or_default()
            .insert(e.folder.clone());
    }
    let until = d.done.then_some(d.to);
    let handles: Vec<_> = by
        .into_iter()
        .map(|(host, folders)| {
            let folders: Vec<String> = folders.into_iter().collect();
            let script = git_script(&folders, d.from, until);
            std::thread::spawn(move || {
                let o = crate::proc::run(
                    &mut crate::sshcfg::shell(&host, "sh -s"),
                    GIT_WAIT,
                    Some(script.as_bytes()),
                );
                let res = match o {
                    Ok(o) if !o.text().trim().is_empty() => Ok(parse_git(&o.text())),
                    Ok(o) if o.timed_out => Err("timed out".to_string()),
                    Ok(o) => Err(o.reason()),
                    Err(e) => Err(e.to_string()),
                };
                (host, folders, res)
            })
        })
        .collect();
    let mut out = HashMap::new();
    for h in handles {
        let Ok((host, folders, res)) = h.join() else {
            continue;
        };
        match res {
            Ok(m) => {
                for (f, files) in m {
                    out.insert((host.clone(), f), files);
                }
            }
            Err(why) => {
                for f in folders {
                    out.insert((host.clone(), f), Files::Failed(why.clone()));
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ putting it together

/// A day's journal from everything on disk (and the fleet now, `live`),
/// files looked at on each machine (blocking: off the UI thread).
pub fn build(cfg: &Config, day: i64, live: &[Session]) -> Day {
    let tz = ui::utc_offset();
    let now = crate::cache::now();
    let inc = include(cfg);
    let (from, to) = bounds(day, tz);
    let known = crate::restore::load();
    let stretches = crate::statelog::read_from(&crate::statelog::file());
    let evs: Vec<Event> = events::load(events::CAP)
        .into_iter()
        .filter(|e| e.t >= from && e.t < to)
        .collect();
    let sent = if inc.prompts {
        crate::sentlog::between(from, to)
    } else {
        Vec::new()
    };
    // the usage records of every agent id the day's sessions had
    let recs = if inc.cost && usage::enabled(cfg) {
        let mut ids: BTreeSet<String> = BTreeSet::new();
        ids.extend(live.iter().filter_map(|s| s.native_id.clone()));
        ids.extend(
            known
                .iter()
                .filter(|k| k.last_seen >= from && k.first_seen < to)
                .map(|k| k.agent_session.clone()),
        );
        ids.extend(evs.iter().map(|e| e.native.clone()));
        ids.remove("");
        let v: Vec<&str> = ids.iter().map(String::as_str).collect();
        turncost::records(&v, from.saturating_sub(86_400), to)
    } else {
        Vec::new()
    };
    let src = Sources {
        known: &known,
        live,
        stretches: &stretches,
        events: &evs,
        sent: &sent,
        recs: &recs,
    };
    let mut d = assemble(day, tz, now, inc, &src, &prices::Table::from_cfg(cfg));
    // files: on each machine (not in the demo world, nothing real is reached)
    if inc.files && !crate::showcase::demo() && !d.entries.is_empty() {
        let looks = look_all(&d);
        fill_files(&mut d, &looks);
    }
    d
}

/// Write a day's file (atomically); its path.
pub fn write(cfg: &Config, d: &Day) -> std::io::Result<PathBuf> {
    let p = path(cfg, d.day);
    config::write_atomic(&p, markdown(d).as_bytes())?;
    Ok(p)
}

/// Is a day's file there, and finished?
pub fn is_final(cfg: &Config, day: i64) -> bool {
    std::fs::read_to_string(path(cfg, day)).is_ok_and(|t| t.contains(FINAL))
}

/// A day's file as it was written.
pub fn read(cfg: &Config, day: i64) -> Option<String> {
    std::fs::read_to_string(path(cfg, day)).ok()
}

// ------------------------------------------------------------------ Markdown

fn usd(v: f64, est: bool) -> Option<String> {
    if v <= 0.0 || !dash::cost_shown() {
        return None;
    }
    let s = if v >= 100.0 {
        format!("${v:.0}")
    } else if v >= 0.01 {
        format!("${v:.2}")
    } else {
        "<$0.01".into()
    };
    Some(if est { s + "*" } else { s })
}

/// `412k tok · $1.20 · 6 min` (the file's own words: real numbers).
fn turn_words(t: &Turn) -> String {
    let mut v = vec![format!("{} tok", dash::tokens(t.tokens))];
    if let Some(m) = usd(t.cost, t.estimated) {
        v.push(m);
    }
    if t.secs > 0 {
        v.push(turncost::minutes(t.secs));
    }
    v.join(" · ")
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// The day as Markdown (pure — tested).
pub fn markdown(d: &Day) -> String {
    let inc = d.inc();
    let mut o = String::new();
    o.push_str(&format!("# Journal · {}\n\n", day_title(d.day)));
    // the summary line
    let hosts: BTreeSet<&str> = d.entries.iter().map(|e| e.host.as_str()).collect();
    let mut sum: Vec<String> = vec![if d.done {
        "The whole day".to_string()
    } else {
        format!("Updated {}", clock(d.made, d.tz))
    }];
    if !d.entries.is_empty() {
        sum.push(format!(
            "{} on {}",
            plural(d.entries.len(), "session"),
            plural(hosts.len(), "machine")
        ));
        if inc.cost {
            let busy: u64 = d.entries.iter().map(|e| e.busy).sum();
            let tok: u64 = d.entries.iter().map(|e| e.tokens).sum();
            let cost: f64 = d.entries.iter().map(|e| e.cost).sum();
            let est = d.entries.iter().any(|e| e.estimated);
            if busy > 0 {
                sum.push(format!("worked {}", ui::dur(busy)));
            }
            if tok > 0 {
                sum.push(format!("{} tok", dash::tokens(tok)));
            }
            if let Some(m) = usd(cost, est) {
                sum.push(m);
            }
        }
    }
    o.push_str(&format!("_{}_\n\n", sum.join(" · ")));
    // what it can and can't see
    let mut note: Vec<&str> = Vec::new();
    if inc.prompts {
        note.push("Prompts are the ones sent through Aleph: the dashboard, `aleph send`, the queue, phone and web replies, new sessions, hand-offs and races. Text typed straight into an agent in herdr isn't seen here.");
    }
    if inc.cost && d.entries.iter().any(|e| e.estimated) {
        note.push("`*` marks an estimate from list prices.");
    }
    if !note.is_empty() {
        o.push_str(&format!("> {}\n\n", note.join(" ")));
    }
    if d.entries.is_empty() {
        o.push_str(if d.done {
            "_Nothing ran through Aleph's sessions this day._\n\n"
        } else {
            "_Nothing ran through Aleph's sessions yet today._\n\n"
        });
    }
    for e in &d.entries {
        let agent = if e.agent.is_empty() {
            String::new()
        } else {
            format!(" · {}", crate::agents::label(&e.agent))
        };
        o.push_str(&format!("## {} · {}{agent}\n\n", e.name, e.host));
        let mut facts: Vec<String> = Vec::new();
        if !e.folder.is_empty() {
            facts.push(format!("`{}`", ui::tilde_path(&e.folder)));
        }
        if inc.cost {
            if e.busy > 0 {
                facts.push(format!("worked {}", ui::dur(e.busy)));
            }
            if e.tokens > 0 {
                facts.push(format!("{} tok", dash::tokens(e.tokens)));
            }
            if let Some(m) = usd(e.cost, e.estimated) {
                facts.push(m);
            }
        }
        let turns = e.finals.len();
        if turns > 0 {
            facts.push(plural(turns, "turn"));
        }
        if !facts.is_empty() {
            o.push_str(&format!("{}\n\n", facts.join(" · ")));
        }
        if inc.prompts && !e.prompts.is_empty() {
            o.push_str("**Prompts**\n\n");
            for (t, p) in &e.prompts {
                o.push_str(&format!("- {} · {}\n", clock(*t, d.tz), one_line(p, 400)));
            }
            o.push('\n');
        }
        if inc.finals && !e.finals.is_empty() {
            o.push_str("**Finished**\n\n");
            for f in &e.finals {
                let took = match (&f.turn, inc.cost) {
                    (Some(t), true) => format!(" — {}", turn_words(t)),
                    _ => String::new(),
                };
                o.push_str(&format!("- {} · {}{took}\n", clock(f.t, d.tz), f.text));
            }
            o.push('\n');
        }
        if inc.files {
            files_md(&mut o, e, d.tz);
        }
    }
    o.push_str(&if d.done {
        format!("{FINAL}\n")
    } else {
        format!("<!-- aleph journal: updated {} -->\n", d.made)
    });
    o
}

fn files_md(o: &mut String, e: &Entry, tz: i64) {
    match &e.files {
        Files::Unknown => {}
        Files::Gone => o.push_str("**Files** · _the folder is gone_\n\n"),
        Files::NotGit => o.push_str("**Files** · _not a git repository_\n\n"),
        Files::Failed(why) => o.push_str(&format!(
            "**Files** · _couldn't look: {}_\n\n",
            one_line(why, 120)
        )),
        Files::Repo(r) => {
            if let Some(first) = &r.same_as {
                o.push_str(&format!(
                    "**Files** · _the same repository as {first}, above_\n\n"
                ));
                return;
            }
            let files: BTreeSet<&str> = r
                .commits
                .iter()
                .flat_map(|c| c.files.iter().map(String::as_str))
                .collect();
            let (add, del): (u64, u64) = r
                .commits
                .iter()
                .fold((0, 0), |(a, b), c| (a + c.add, b + c.del));
            let dirty = r.dirty.as_ref().filter(|s| s.files > 0 || s.new > 0);
            if r.commits.is_empty() && dirty.is_none() {
                o.push_str("**Files** · _no changes_\n\n");
                return;
            }
            let mut head = vec!["**Files**".to_string()];
            if !r.commits.is_empty() {
                head.push(plural(r.commits.len(), "commit"));
                head.push(plural(files.len(), "file"));
                head.push(format!("+{add} −{del}"));
            }
            o.push_str(&format!("{}\n\n", head.join(" · ")));
            for c in &r.commits {
                o.push_str(&format!(
                    "- `{}` {} {} — {}, +{} −{}\n",
                    c.hash,
                    clock(c.t, tz),
                    one_line(&c.subject, 160),
                    plural(c.files.len(), "file"),
                    c.add,
                    c.del
                ));
            }
            if let Some(s) = dirty {
                let mut v = vec![format!(
                    "- not committed: {}, +{} −{}",
                    plural(s.files as usize, "file"),
                    s.add,
                    s.del
                )];
                if s.new > 0 {
                    v.push(format!("{} new", s.new));
                }
                o.push_str(&format!("{}\n", v.join(" · ")));
            }
            o.push('\n');
        }
    }
}

/// The day as JSON (`aleph journal --json`).
pub fn to_json(d: &Day) -> Value {
    let turn = |t: &Turn| json!({"tokens": t.tokens, "cost": t.cost, "estimated": t.estimated, "secs": t.secs, "requests": t.requests, "agent_timed": t.agent_timed});
    let files = |f: &Files| match f {
        Files::Unknown => Value::Null,
        Files::Gone => json!({"state": "gone"}),
        Files::NotGit => json!({"state": "not_git"}),
        Files::Failed(w) => json!({"state": "failed", "why": w}),
        Files::Repo(r) => json!({
            "state": "repo", "top": r.top, "same_as": r.same_as,
            "commits": r.commits.iter().map(|c| json!({"hash": c.hash, "t": c.t, "subject": c.subject, "files": c.files.len(), "add": c.add, "del": c.del})).collect::<Vec<_>>(),
            "uncommitted": r.dirty.as_ref().map(|s| json!({"files": s.files, "add": s.add, "del": s.del, "new": s.new})),
        }),
    };
    json!({
        "date": usage::day_name(d.day),
        "from": d.from, "to": d.to, "made": d.made, "final": d.done,
        "include": d.inc().names(),
        "sessions": d.entries.iter().map(|e| json!({
            "host": e.host, "name": e.name, "agent": e.agent, "folder": e.folder,
            "agent_sessions": e.natives,
            "prompts": e.prompts.iter().map(|(t, p)| json!({"t": t, "text": p})).collect::<Vec<_>>(),
            "finals": e.finals.iter().map(|f| json!({"t": f.t, "text": f.text, "turn": f.turn.as_ref().map(turn)})).collect::<Vec<_>>(),
            "files": files(&e.files),
            "busy_secs": e.busy, "tokens": e.tokens, "requests": e.requests, "cost": e.cost, "estimated": e.estimated,
            "first": e.first, "last": e.last,
        })).collect::<Vec<_>>(),
    })
}

// ------------------------------------------------------------------ shown (lens)

/// Text on its way to a screen: the showcase lens (names, folders), and its
/// money lens on every `$` amount (scaled, or hidden). The file stays real.
pub fn shown(text: &str) -> String {
    let t = crate::showcase::text(text).into_owned();
    if !crate::showcase::on() {
        return t;
    }
    let mut out = String::with_capacity(t.len());
    let mut rest = t.as_str();
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let n = after
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(after.len());
        match after[..n].parse::<f64>() {
            Ok(v) if n > 0 => {
                out.push_str(&match crate::showcase::money(v) {
                    Some(x) if x >= 100.0 => format!("${x:.0}"),
                    Some(x) => format!("${x:.2}"),
                    None => crate::showcase::MONEY_HIDDEN.to_string(),
                });
                rest = &after[n..];
            }
            _ => {
                out.push('$');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// One Markdown line, styled for a terminal (the dashboard's view, the CLI
/// on a tty): headings in the accent, bold labels, quotes and notes faint,
/// `code` muted. Lists keep their dash.
pub fn styled(l: &str) -> Line<'static> {
    if let Some(h) = l.strip_prefix("# ") {
        return Line::from(Span::styled(
            format!("{} {h}", crate::brand::MARK),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(h) = l.strip_prefix("## ") {
        let mut v = vec![Span::styled("┃ ", ui::fg(c::ACCENT()))];
        let mut parts = h.splitn(2, " · ");
        if let Some(name) = parts.next() {
            v.push(Span::styled(name.to_string(), ui::bold()));
        }
        if let Some(rest) = parts.next() {
            v.push(Span::styled(format!(" · {rest}"), ui::muted()));
        }
        return Line::from(v);
    }
    if l.starts_with("<!--") {
        return Line::raw("");
    }
    if let Some(q) = l.strip_prefix("> ") {
        return Line::from(Span::styled(format!("│ {q}"), ui::faint()));
    }
    if l.starts_with('_') && l.ends_with('_') && l.len() > 1 {
        return Line::from(Span::styled(
            l.trim_matches('_').to_string(),
            ui::muted().add_modifier(Modifier::ITALIC),
        ));
    }
    // inline: **bold**, `code`, _italic_
    let mut v: Vec<Span<'static>> = Vec::new();
    let (body, lead) = match l.strip_prefix("- ") {
        Some(b) => (b, true),
        None => (l, false),
    };
    if lead {
        v.push(Span::styled("  • ", ui::fg(c::ACCENT())));
    }
    let mut rest = body;
    let mut cur = String::new();
    let flush = |v: &mut Vec<Span<'static>>, cur: &mut String| {
        if !cur.is_empty() {
            v.push(Span::raw(std::mem::take(cur)));
        }
    };
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("**") {
            if let Some(e) = r.find("**") {
                flush(&mut v, &mut cur);
                v.push(Span::styled(r[..e].to_string(), ui::bold()));
                rest = &r[e + 2..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix('`') {
            if let Some(e) = r.find('`') {
                flush(&mut v, &mut cur);
                v.push(Span::styled(r[..e].to_string(), ui::fg(c::TEAL())));
                rest = &r[e + 1..];
                continue;
            }
        }
        if let Some(r) = rest.strip_prefix('_') {
            if let Some(e) = r.find('_').filter(|e| *e > 0) {
                flush(&mut v, &mut cur);
                v.push(Span::styled(
                    r[..e].to_string(),
                    ui::muted().add_modifier(Modifier::ITALIC),
                ));
                rest = &r[e + 1..];
                continue;
            }
        }
        let ch = rest.chars().next().unwrap_or(' ');
        cur.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    flush(&mut v, &mut cur);
    Line::from(v)
}

// ------------------------------------------------------------------ the hub's writer

/// The hub's writer (`auto::Hub` owns one): today's file every
/// [`EVERY`] and [`AFTER_END`] after a session ends; yesterday's finished
/// once the day is over. Never blocks: the work is on a thread of its own.
pub struct Writer {
    busy: Arc<AtomicBool>,
    next: Instant,
    /// a session ended: write at this moment
    soon: Option<Instant>,
    /// the day it last wrote (a new one: yesterday gets finished)
    day: Option<i64>,
    sessions: BTreeSet<String>,
}

impl Default for Writer {
    fn default() -> Self {
        Writer {
            busy: Arc::new(AtomicBool::new(false)),
            // let the hub settle (and the first polls land) first
            next: Instant::now() + Duration::from_secs(120),
            soon: None,
            day: None,
            sessions: BTreeSet::new(),
        }
    }
}

impl Writer {
    /// Call after every fleet tick.
    pub fn tick(&mut self, cfg: &Config, fleet: &crate::fleet::Fleet) {
        if !include(cfg).any() {
            return;
        }
        let now_keys: BTreeSet<String> = fleet.sessions().iter().map(|s| s.key()).collect();
        if !self.sessions.is_empty() && self.sessions.iter().any(|k| !now_keys.contains(k)) {
            self.soon = Some(Instant::now() + AFTER_END);
        }
        self.sessions = now_keys;
        let today = ui::local_day(crate::cache::now());
        let new_day = self.day.is_some_and(|d| d != today);
        let due = Instant::now() >= self.next || self.soon.is_some_and(|t| Instant::now() >= t);
        if !(due || new_day) || self.busy.load(Ordering::Relaxed) {
            return;
        }
        let first = self.day.is_none();
        self.day = Some(today);
        self.next = Instant::now() + EVERY;
        self.soon = None;
        self.busy.store(true, Ordering::Relaxed);
        let (cfg, busy) = (cfg.clone(), self.busy.clone());
        let live: Vec<Session> = fleet.sessions().into_iter().cloned().collect();
        std::thread::spawn(move || {
            // yesterday: finished once (after midnight, or at the first write
            // when its file was left unfinished)
            let y = today - 1;
            if (new_day || first) && !is_final(&cfg, y) && (new_day || read(&cfg, y).is_some()) {
                let d = build(&cfg, y, &live);
                if !d.entries.is_empty() || read(&cfg, y).is_some() {
                    if let Err(e) = write(&cfg, &d) {
                        crate::log::warn("journal", &format!("couldn't write {y}: {e}"));
                    }
                }
            }
            let d = build(&cfg, today, &live);
            // an empty day isn't written by itself (aleph journal --write does)
            if !d.entries.is_empty() || read(&cfg, today).is_some() {
                match write(&cfg, &d) {
                    Ok(p) => crate::log::debug(
                        "journal",
                        &format!("{} sessions → {}", d.entries.len(), p.display()),
                    ),
                    Err(e) => crate::log::warn("journal", &format!("couldn't write: {e}")),
                }
            }
            busy.store(false, Ordering::Relaxed);
        });
    }
}

// ------------------------------------------------------------------ aleph journal

pub fn cmd(a: Args) -> ExitCode {
    let pos = a.positional();
    let today = ui::local_day(crate::cache::now());
    if pos.first() == Some(&"path") {
        raw(&dir(&a.cfg).display().to_string());
        return ExitCode::SUCCESS;
    }
    let Some(day) = parse_day(pos.first().copied().unwrap_or("today"), today) else {
        return fail(&format!(
            "{}: a day is today, yesterday, -2 (two days ago) or 2026-10-24",
            pos[0]
        ));
    };
    if day > today {
        return fail("that day hasn't happened yet");
    }
    let inc = include(&a.cfg);
    if !inc.any() {
        err("the journal is off (journal.include = off)");
        hint("aleph settings set journal.include all — prompts, finals, files and cost");
        return ExitCode::from(1);
    }
    let json_out = a.has("--json");
    let write_it = a.has("--write");
    // the fleet as last seen (no poll): what's at work now, folders, agent ids
    let live: Vec<Session> = if day == today {
        crate::fleet::Fleet::new(&a.cfg, None)
            .sessions()
            .into_iter()
            .cloned()
            .collect()
    } else {
        Vec::new()
    };
    let stored = read(&a.cfg, day);
    if json_out || write_it || stored.is_none() {
        let sp = (std::io::stderr().is_terminal() && !json_out)
            .then(|| Spinner::start(format!("putting {} together", usage::day_name(day))));
        let d = build(&a.cfg, day, &live);
        if let Some(sp) = sp {
            sp.stop();
        }
        if json_out {
            json(&to_json(&d));
            if write_it {
                let _ = write(&a.cfg, &d);
            }
            return ExitCode::SUCCESS;
        }
        if write_it {
            return match write(&a.cfg, &d) {
                Ok(p) => {
                    ok(&format!(
                        "{} · {} → {}",
                        usage::day_name(day),
                        plural(d.entries.len(), "session"),
                        crate::doctor::tilde(&p)
                    ));
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&format!("couldn't write the journal: {e}")),
            };
        }
        print_md(&markdown(&d));
        hint("not saved yet — aleph journal --write saves it");
        return ExitCode::SUCCESS;
    }
    print_md(&stored.unwrap_or_default());
    ExitCode::SUCCESS
}

/// Markdown out: styled on a terminal, as it is when piped (masked in
/// showcase mode either way).
fn print_md(md: &str) {
    let tty = std::io::stdout().is_terminal();
    for l in md.lines() {
        if tty {
            if l.starts_with("<!--") {
                continue;
            }
            out(styled(&shown(l)));
        } else {
            raw(&shown(l));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: i64 = 20_750; // 2026-10-24, a Saturday
    fn at(h: u64, m: u64) -> u64 {
        bounds(D, 0).0 + h * 3600 + m * 60
    }

    fn table() -> prices::Table {
        prices::Table::from_cfg(&Config::in_memory(&[]))
    }

    fn stretch(name: &str, pane: &str, a: u64, b: u64, st: St) -> Stretch {
        Stretch {
            start: a,
            end: b,
            host: "b31".into(),
            pane: pane.into(),
            name: name.into(),
            agent: "claude".into(),
            st,
        }
    }

    fn req(ts: u64, session: &str, key: &str, input: u64, cost: f64) -> Record {
        Record {
            ts,
            host: "b31".into(),
            agent: "claude".into(),
            session: session.into(),
            key: key.into(),
            model: "claude-sonnet-4-5".into(),
            input,
            output: 1000,
            cost: Some(cost),
            ..Default::default()
        }
    }

    struct Fixture {
        known: Vec<Known>,
        stretches: Vec<Stretch>,
        events: Vec<Event>,
        sent: Vec<Sent>,
        recs: Vec<Record>,
    }

    fn fixture() -> Fixture {
        Fixture {
            known: vec![Known {
                host: "b31".into(),
                name: "ignite".into(),
                label: "claude_ignite".into(),
                agent: "claude".into(),
                cwd: "/home/u/work/ivy".into(),
                pane_id: "w1:p2".into(),
                first_seen: at(8, 0),
                last_seen: at(18, 0),
                state: "gone".into(),
                agent_session: "s-ignite".into(),
            }],
            stretches: vec![
                stretch("ignite", "w1:p2", at(9, 12), at(9, 31), St::Working),
                stretch("ignite", "w1:p2", at(11, 40), at(12, 5), St::Working),
                // yesterday's: not today's work
                stretch(
                    "ignite",
                    "w1:p2",
                    at(0, 0) - 7200,
                    at(0, 0) - 3600,
                    St::Working,
                ),
                // idle all day: not work
                stretch("lurk", "w1:p9", at(8, 0), at(18, 0), St::Idle),
            ],
            events: vec![
                Event {
                    t: at(9, 31),
                    host: "b31".into(),
                    name: "ignite".into(),
                    kind: Kind::Done,
                    title: "Fix the flaky test".into(),
                    agent: "claude".into(),
                    took: 19 * 60,
                    native: "s-ignite".into(),
                    ..Default::default()
                },
                // the agent's own word on the same turn: one final, in its words
                Event {
                    t: at(9, 31),
                    host: "b31".into(),
                    name: "ignite".into(),
                    kind: Kind::Done,
                    title: "Fixed the flaky test.".into(),
                    body: "Fixed the flaky test: the port was reused.\nAll 41 pass.".into(),
                    agent: "claude".into(),
                    source: "agent".into(),
                    pane: "w1:p2".into(),
                    native: "s-ignite".into(),
                    ..Default::default()
                },
                Event {
                    t: at(12, 5),
                    host: "b31".into(),
                    name: "ignite".into(),
                    kind: Kind::Done,
                    title: "Add retries with backoff".into(),
                    agent: "claude".into(),
                    took: 25 * 60,
                    native: "s-ignite".into(),
                    ..Default::default()
                },
            ],
            sent: vec![
                Sent {
                    t: at(9, 12),
                    host: "b31".into(),
                    pane: "w1:p2".into(),
                    name: String::new(),
                    text: "Fix the flaky test in tests/net.rs".into(),
                },
                Sent {
                    t: at(11, 40),
                    host: "b31".into(),
                    pane: "w1:p2".into(),
                    name: "ignite".into(),
                    text: "Now add a retry\nwith backoff".into(),
                },
            ],
            recs: vec![
                req(at(9, 20), "s-ignite", "k1", 100_000, 0.40),
                req(at(9, 30), "s-ignite", "k2", 200_000, 0.80),
                req(at(11, 50), "s-ignite", "k3", 50_000, 0.30),
                // another day's request of the same conversation
                req(at(0, 0) - 5000, "s-ignite", "k0", 9_000_000, 30.0),
                // the same conversation read on another machine (a shared home)
                Record {
                    host: "gpu2".into(),
                    ..req(at(9, 25), "s-ignite", "kx", 7_000_000, 50.0)
                },
            ],
        }
    }

    fn day_of(f: &Fixture, inc: Include, now: u64) -> Day {
        let src = Sources {
            known: &f.known,
            live: &[],
            stretches: &f.stretches,
            events: &f.events,
            sent: &f.sent,
            recs: &f.recs,
        };
        assemble(D, 0, now, inc, &src, &table())
    }

    #[test]
    fn what_goes_in_is_a_setting() {
        assert_eq!(Include::parse(""), Include::ALL);
        assert_eq!(Include::parse("all"), Include::ALL);
        assert_eq!(Include::parse("off"), Include::NONE);
        assert!(!Include::parse("off").any());
        let p = Include::parse("prompts,finals");
        assert!(p.prompts && p.finals && !p.files && !p.cost);
        let m = Include::parse("-files");
        assert!(m.prompts && m.finals && !m.files && m.cost);
    }

    #[test]
    fn a_fixture_day_in_markdown() {
        let f = fixture();
        let mut d = day_of(&f, Include::ALL, at(23, 0) + 7200);
        assert_eq!(d.entries.len(), 1, "{:?}", d.entries);
        let e = &d.entries[0];
        assert_eq!((e.busy, e.tokens, e.requests), (44 * 60, 353_000, 3));
        assert_eq!(e.folder, "/home/u/work/ivy");
        // the prompt sent by pane alone is the session's
        assert_eq!(e.prompts.len(), 2);
        assert_eq!(e.finals.len(), 2);
        assert!(e.finals[0]
            .text
            .starts_with("Fixed the flaky test: the port"));
        let mut looks = HashMap::new();
        looks.insert(
            ("b31".to_string(), "/home/u/work/ivy".to_string()),
            Files::Repo(Repo {
                top: "/home/u/work/ivy".into(),
                commits: vec![Commit {
                    hash: "a1b2c3d".into(),
                    t: at(9, 30),
                    subject: "Fix flaky port reuse".into(),
                    files: vec!["tests/net.rs".into(), "src/net.rs".into()],
                    add: 40,
                    del: 3,
                }],
                dirty: None,
                same_as: None,
            }),
        );
        fill_files(&mut d, &looks);
        let md = markdown(&d);
        let want = "# Journal · Saturday 24 October 2026\n\n\
_The whole day · 1 session on 1 machine · worked 44m · 353k tok · $1.50_\n\n\
> Prompts are the ones sent through Aleph: the dashboard, `aleph send`, the queue, phone and web replies, new sessions, hand-offs and races. Text typed straight into an agent in herdr isn't seen here.\n\n\
## ignite · b31 · Claude Code\n\n\
`~/work/ivy` · worked 44m · 353k tok · $1.50 · 2 turns\n\n\
**Prompts**\n\n\
- 09:12 · Fix the flaky test in tests/net.rs\n\
- 11:40 · Now add a retry with backoff\n\n\
**Finished**\n\n\
- 09:31 · Fixed the flaky test: the port was reused. All 41 pass. — 302k tok · $1.20 · 19 min\n\
- 12:05 · Add retries with backoff — 51k tok · $0.30 · 25 min\n\n\
**Files** · 1 commit · 2 files · +40 −3\n\n\
- `a1b2c3d` 09:30 Fix flaky port reuse — 2 files, +40 −3\n\n\
<!-- aleph journal: final -->\n";
        assert_eq!(md, want, "\n{md}");
    }

    #[test]
    fn the_switches_leave_parts_out() {
        let f = fixture();
        let inc = Include::parse("finals");
        let md = markdown(&day_of(&f, inc, at(23, 59) + 60));
        assert!(md.contains("**Finished**"), "{md}");
        assert!(
            !md.contains("**Prompts**") && !md.contains("Text typed straight"),
            "{md}"
        );
        assert!(
            !md.contains("tok") && !md.contains('$') && !md.contains("worked"),
            "{md}"
        );
        let md = markdown(&day_of(&f, Include::parse("prompts,cost"), at(23, 59) + 60));
        assert!(
            md.contains("**Prompts**") && md.contains("353k tok"),
            "{md}"
        );
        assert!(!md.contains("**Finished**"), "{md}");
    }

    #[test]
    fn an_empty_day_says_so() {
        let f = Fixture {
            known: vec![],
            stretches: vec![],
            events: vec![],
            sent: vec![],
            recs: vec![],
        };
        let d = day_of(&f, Include::ALL, at(10, 0));
        assert!(d.entries.is_empty() && !d.done);
        let md = markdown(&d);
        assert!(md.contains("_Updated 10:00_"), "{md}");
        assert!(
            md.contains("Nothing ran through Aleph's sessions yet today"),
            "{md}"
        );
        assert!(!md.contains(FINAL));
        assert!(markdown(&day_of(&f, Include::ALL, at(23, 0) + 7200)).contains(FINAL));
    }

    #[test]
    fn git_looks_and_a_missing_folder() {
        let s = git_script(&["/srv/gone".into(), "~/it's here".into()], 1000, None);
        // a folder that's gone is never entered; quotes survive
        assert!(
            s.contains("cd \"$2\" 2>/dev/null || { echo G; exit 0; }"),
            "{s}"
        );
        assert!(s.contains("( look '/srv/gone' '/srv/gone' )"), "{s}");
        assert!(
            s.contains("( look '~/it'\\''s here' \"$HOME\"/'it'\\''s here' )"),
            "{s}"
        );
        assert!(
            s.contains("--since=@1000 ") && s.contains("git diff HEAD --numstat"),
            "{s}"
        );
        // a finished day: up to its end, nothing uncommitted
        let s = git_script(&["/srv/x".into()], 1000, Some(2000));
        assert!(
            s.contains("--until=@2000") && !s.contains("git diff"),
            "{s}"
        );
        let out = "D\t/srv/gone\nG\nD\t/srv/plain\nN\nD\t~/w\nT\t/home/u/w\nC\ta1b2c3d\t1700\tFix it\n3\t1\tsrc/a.rs\n-\t-\tlogo.png\nC\tb2c3d4e\t1800\tTest it\n10\t0\ttests/a.rs\nU\n2\t2\tsrc/a.rs\nn\t3\nE\n";
        let m = parse_git(out);
        assert_eq!(m.get("/srv/gone"), Some(&Files::Gone));
        assert_eq!(m.get("/srv/plain"), Some(&Files::NotGit));
        let Some(Files::Repo(r)) = m.get("~/w") else {
            panic!("{m:?}")
        };
        assert_eq!(r.top, "/home/u/w");
        assert_eq!(r.commits.len(), 2);
        assert_eq!(
            (r.commits[0].files.len(), r.commits[0].add, r.commits[0].del),
            (2, 3, 1)
        );
        assert_eq!(
            r.dirty,
            Some(Stat {
                files: 1,
                add: 2,
                del: 2,
                new: 3
            })
        );
        // the folder gone, in the journal
        let mut d = Day {
            day: D,
            entries: vec![Entry {
                host: "b31".into(),
                name: "x".into(),
                folder: "/srv/gone".into(),
                busy: 60,
                ..Default::default()
            }],
            ..Default::default()
        };
        let looks: HashMap<Key, Files> = m
            .into_iter()
            .map(|(f, v)| (("b31".to_string(), f), v))
            .collect();
        fill_files(&mut d, &looks);
        assert!(markdown(&d).contains("**Files** · _the folder is gone_"));
    }

    #[test]
    fn the_git_look_runs_here_for_real() {
        if crate::proc::which("git").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("aleph-journal-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let o = std::process::Command::new("git")
                .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(o.status.success(), "{o:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@t"]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "Add a's first lines"]);
        std::fs::write(repo.join("a.txt"), "one\n2\nthree\n").unwrap();
        std::fs::write(repo.join("new.txt"), "x\n").unwrap();
        let folders = vec![
            repo.display().to_string(),
            root.join("gone").display().to_string(),
            root.display().to_string(),
        ];
        let script = git_script(&folders, crate::cache::now() - 3600, None);
        let o = crate::proc::run(
            std::process::Command::new("sh").arg("-s"),
            Duration::from_secs(20),
            Some(script.as_bytes()),
        )
        .unwrap();
        let m = parse_git(&o.text());
        let Some(Files::Repo(r)) = m.get(&folders[0]) else {
            panic!("{}\n{m:?}", o.text())
        };
        assert_eq!(r.commits.len(), 1, "{r:?}");
        assert_eq!(r.commits[0].subject, "Add a's first lines");
        assert_eq!((r.commits[0].add, r.commits[0].del), (2, 0));
        assert_eq!(
            r.dirty,
            Some(Stat {
                files: 1,
                add: 2,
                del: 1,
                new: 1
            })
        );
        assert_eq!(m.get(&folders[1]), Some(&Files::Gone));
        // the temp folder itself may sit inside some repository; if not, it says so
        assert!(matches!(
            m.get(&folders[2]),
            Some(Files::NotGit) | Some(Files::Repo(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn two_sessions_in_one_repository_show_it_once() {
        let repo = Files::Repo(Repo {
            top: "/srv/r".into(),
            commits: vec![Commit {
                hash: "a".into(),
                ..Default::default()
            }],
            ..Default::default()
        });
        let e = |name: &str, folder: &str| Entry {
            host: "b31".into(),
            name: name.into(),
            folder: folder.into(),
            busy: 1,
            ..Default::default()
        };
        let mut d = Day {
            entries: vec![e("one", "/srv/r"), e("two", "/srv/r/sub")],
            ..Default::default()
        };
        let mut looks = HashMap::new();
        looks.insert(("b31".to_string(), "/srv/r".to_string()), repo.clone());
        looks.insert(("b31".to_string(), "/srv/r/sub".to_string()), repo);
        fill_files(&mut d, &looks);
        assert!(markdown(&d).contains("the same repository as one, above"));
    }

    #[test]
    fn days_by_name() {
        assert_eq!(parse_day("today", D), Some(D));
        assert_eq!(parse_day("yesterday", D), Some(D - 1));
        assert_eq!(parse_day("-3", D), Some(D - 3));
        assert_eq!(parse_day("2026-10-24", 0), Some(D));
        assert_eq!(parse_day("someday", D), None);
        assert_eq!(day_title(D), "Saturday 24 October 2026");
    }

    #[test]
    fn shown_lines_are_styled() {
        let l = styled("## ignite · b31 · Claude Code");
        assert_eq!(l.spans[1].content, "ignite");
        let l = styled("- `a1b2c3d` 09:30 **Fix** it");
        let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "  • a1b2c3d 09:30 Fix it");
        assert_eq!(shown("costs $1.20 today"), "costs $1.20 today");
    }
}
