//! Steps: what an agent did, for the timeline — the prompts it was given
//! (by you, another agent, itself), its tool calls (start, end, how they
//! went), web searches, compactions, errors, and the subagents it started
//! (each a lane of its session). Read from the agents' own logs with their
//! usage (`extract.rs`): ids, tool names, times and numbers, never what was
//! said. Kept in `steps-YYYY-MM.tsv` (UTC months; apart from the usage
//! records, which an older build would prune), for `timeline.keep` days.
//!
//! Wire lines (the extractor's, tab-separated):
//!
//! | tag | fields |
//! |---|---|
//! | `p` | ts, session, lane, key, by (u you · a an agent · s the agent itself) |
//! | `ks` | ts, session, lane, call id, tool name, class (tool · web · mcp · agent) |
//! | `ke` | ts, session, lane, call id, status (ok · err · denied), duration ms (0: not logged) |
//! | `c` | ts, session, lane, key, tokens before, after |
//! | `e` | ts, session, lane, key, kind (api · interrupted) |
//! | `sl` | ts, session, lane, the call that started it, its type |

use super::parse::{hash, parse_ts};
use super::store;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};

/// What a step is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Prompt,
    ToolStart,
    ToolEnd,
    Compact,
    Error,
    Lane,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Prompt => "p",
            Kind::ToolStart => "ks",
            Kind::ToolEnd => "ke",
            Kind::Compact => "c",
            Kind::Error => "e",
            Kind::Lane => "sl",
        }
    }
    fn of(tag: &str) -> Option<Kind> {
        Some(match tag {
            "p" => Kind::Prompt,
            "ks" => Kind::ToolStart,
            "ke" => Kind::ToolEnd,
            "c" => Kind::Compact,
            "e" => Kind::Error,
            "sl" => Kind::Lane,
            _ => return None,
        })
    }
}

/// One step.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub kind: Kind,
    /// unix ms (the logs give ms; a tool call often takes less than a second)
    pub ms: u64,
    pub host: String,
    pub agent: String,
    pub session: String,
    /// "" the main agent, else a subagent's id
    pub lane: String,
    /// the agent's id for it, hashed (a tool call's start and end share it)
    pub key: String,
    /// a tool's name; a prompt's sender (u a s); a status (ok err denied); an
    /// error's kind; a lane's type
    pub name: String,
    /// a tool's class; a lane's parent call (hashed)
    pub class: String,
    /// a tool call's logged duration (ms); a compaction's tokens before
    pub n1: u64,
    /// a compaction's tokens after
    pub n2: u64,
}

/// A name kept: letters, digits and `. _ : -`, 48 at most.
fn word(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
        .take(48)
        .collect()
}

/// Milliseconds of a log's timestamp (ISO with fractions, or epoch s / ms).
fn ms_of(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.bytes().all(|b| b.is_ascii_digit()) && !t.is_empty() {
        let v: u64 = t.parse().ok()?;
        return Some(if v > 100_000_000_000 { v } else { v * 1000 });
    }
    let secs = parse_ts(t)?;
    // the fraction, when there is one
    let frac = t
        .split_once('.')
        .map(|(_, r)| {
            r.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
        })
        .unwrap_or_default();
    let ms = match frac.len() {
        0 => 0,
        n => frac[..n.min(3)].parse::<u64>().unwrap_or(0) * 10u64.pow(3 - n.min(3) as u32),
    };
    Some(secs * 1000 + ms)
}

/// A wire line (tag first) → a step for `host` / `agent`.
pub fn from_wire(line: &str, host: &str, agent: &str) -> Option<Step> {
    let f: Vec<&str> = line.split('\t').collect();
    let kind = Kind::of(f.first()?)?;
    if f.len() < 5 {
        return None;
    }
    let g = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
    let n = |i: usize| g(i).parse::<u64>().unwrap_or(0);
    let key = g(4);
    if key.is_empty() && kind != Kind::Lane {
        return None;
    }
    // a lane's link may come without a time (read from the subagent's meta file)
    let ms = match ms_of(g(1)) {
        Some(m) => m,
        None if kind == Kind::Lane => 0,
        None => return None,
    };
    let session = word(g(2));
    let lane = word(g(3));
    let hk = |k: &str| hash(&[agent, k]);
    let s = Step {
        kind,
        ms,
        host: host.to_string(),
        agent: agent.to_string(),
        session: session.clone(),
        lane: lane.clone(),
        key: match kind {
            // the lane itself is the key; its parent call is hashed like the call's own key
            Kind::Lane => hk(&format!("lane:{session}:{lane}")),
            _ => hk(key),
        },
        name: match kind {
            Kind::Prompt => match g(5) {
                "a" | "s" => g(5).to_string(),
                _ => "u".into(),
            },
            Kind::ToolEnd => match g(5) {
                "err" | "denied" => g(5).to_string(),
                _ => "ok".into(),
            },
            Kind::Compact => String::new(),
            _ => word(g(5)),
        },
        class: match kind {
            Kind::ToolStart => match g(6) {
                "web" | "mcp" | "agent" => g(6).to_string(),
                _ => "tool".into(),
            },
            Kind::Lane if !key.is_empty() => hk(key),
            _ => String::new(),
        },
        // a call's logged time; a compaction's tokens before and after
        n1: match kind {
            Kind::ToolEnd => n(6),
            Kind::Compact => n(5),
            _ => 0,
        },
        n2: if kind == Kind::Compact { n(6) } else { 0 },
    };
    Some(s)
}

/// The store's line for a step.
pub fn to_line(s: &Step) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        s.kind.tag(),
        s.ms,
        s.host,
        s.agent,
        s.session,
        s.lane,
        s.key,
        s.name,
        s.class,
        s.n1,
        s.n2
    )
}

/// A store line → a step.
pub fn from_line(l: &str) -> Option<Step> {
    let f: Vec<&str> = l.split('\t').collect();
    if f.len() < 11 {
        return None;
    }
    let n = |i: usize| f[i].parse::<u64>().unwrap_or(0);
    Some(Step {
        kind: Kind::of(f[0])?,
        ms: f[1].parse().ok()?,
        host: f[2].into(),
        agent: f[3].into(),
        session: f[4].into(),
        lane: f[5].into(),
        key: f[6].into(),
        name: f[7].into(),
        class: f[8].into(),
        n1: n(9),
        n2: n(10),
    })
}

fn file(month: &str) -> std::path::PathBuf {
    store::dir().join(format!("steps-{month}.tsv"))
}

/// The months there are steps for, oldest first.
pub fn months() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(store::dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_prefix("steps-")
                .and_then(|r| r.strip_suffix(".tsv"))
                .map(str::to_string)
        })
        .collect();
    v.sort();
    v
}

/// Add steps to their months' files.
pub fn append(steps: &[Step]) -> io::Result<()> {
    let mut by: BTreeMap<String, String> = BTreeMap::new();
    for s in steps {
        let l = by.entry(store::month_of(s.ms / 1000)).or_default();
        l.push_str(&to_line(s));
        l.push('\n');
    }
    fs::create_dir_all(store::dir())?;
    for (m, text) in by {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file(&m))?;
        f.write_all(text.as_bytes())?;
    }
    Ok(())
}

/// A month's steps, the last per (machine, kind, key) kept (a re-read writes
/// them again).
pub fn read_month(m: &str) -> Vec<Step> {
    let text = fs::read_to_string(file(m)).unwrap_or_default();
    let mut at: HashMap<(String, Kind, String), usize> = HashMap::new();
    let mut out: Vec<Option<Step>> = Vec::new();
    for s in text.lines().filter_map(from_line) {
        let k = (s.host.clone(), s.kind, s.key.clone());
        if let Some(&i) = at.get(&k) {
            out[i] = None;
        }
        at.insert(k, out.len());
        out.push(Some(s));
    }
    out.into_iter().flatten().collect()
}

/// A month's steps whose line holds one of `needles` (a session's id): the
/// rest isn't parsed (the timeline, read again every few seconds).
pub fn read_month_matching(m: &str, needles: &[&str]) -> Vec<Step> {
    if needles.is_empty() {
        return Vec::new();
    }
    let text = fs::read_to_string(file(m)).unwrap_or_default();
    let mut at: HashMap<(String, Kind, String), usize> = HashMap::new();
    let mut out: Vec<Option<Step>> = Vec::new();
    for s in text
        .lines()
        .filter(|l| needles.iter().any(|n| l.contains(n)))
        .filter_map(from_line)
    {
        let k = (s.host.clone(), s.kind, s.key.clone());
        if let Some(&i) = at.get(&k) {
            out[i] = None;
        }
        at.insert(k, out.len());
        out.push(Some(s));
    }
    out.into_iter().flatten().collect()
}

/// One session's steps (every lane), oldest first: this month and the ones
/// it may have started in.
pub fn of_session(host: &str, session: &str, months_back: usize) -> Vec<Step> {
    let all = months();
    let mut v: Vec<Step> = all
        .iter()
        .rev()
        .take(months_back.max(1))
        .flat_map(|m| read_month(m))
        .filter(|s| s.host == host && (s.session == session || s.lane == session))
        .collect();
    v.sort_by_key(|s| s.ms);
    v
}

/// Was the turn that ended by `at_ms` started by a subagent reporting back,
/// with others still to report? No news then: the last report is. A subagent
/// whose call ended when it did never reports, so only those still at work
/// after their call count. Anything unclear answers no: a real end must
/// never stay quiet.
pub fn woken_by_subagent(steps: &[Step], at_ms: u64) -> bool {
    let main = |s: &&Step| s.lane.is_empty() && s.ms <= at_ms;
    let woken = steps
        .iter()
        .filter(main)
        .filter(|s| s.kind == Kind::Prompt)
        .max_by_key(|s| s.ms)
        .is_some_and(|p| p.name == "a");
    if !woken {
        return false;
    }
    let reports: HashSet<&str> = steps
        .iter()
        .filter(main)
        .filter(|s| s.kind == Kind::Prompt && s.name == "a")
        .map(|s| s.key.as_str())
        .collect();
    let calls: HashSet<&str> = steps
        .iter()
        .filter(main)
        .filter(|s| s.kind == Kind::ToolStart && s.class == "agent")
        .map(|s| s.key.as_str())
        .collect();
    let ended: HashMap<&str, u64> = steps
        .iter()
        .filter(main)
        .filter(|s| s.kind == Kind::ToolEnd)
        .map(|s| (s.key.as_str(), s.ms))
        .collect();
    let mut last: HashMap<&str, u64> = HashMap::new();
    for s in steps.iter().filter(|s| !s.lane.is_empty()) {
        let at = last.entry(s.lane.as_str()).or_default();
        *at = (*at).max(s.ms);
    }
    let background: HashSet<&str> = steps
        .iter()
        .filter(|s| s.kind == Kind::Lane && calls.contains(s.class.as_str()))
        .filter(|s| {
            let worked_until = last.get(s.lane.as_str()).copied().unwrap_or(0);
            ended
                .get(s.class.as_str())
                .is_none_or(|end| worked_until > end + 1000)
        })
        .map(|s| s.lane.as_str())
        .collect();
    background.len() > reports.len()
}

/// Did the turn that ended by `at_ms` end in a compaction: the main agent's last
/// compaction just before it, and nothing done since? (A compaction in the
/// middle of work is followed by the work, or by the time an answer takes.)
pub fn ended_in_compaction(steps: &[Step], at_ms: u64) -> bool {
    const JUST: u64 = 6_000;
    let Some(c) = steps
        .iter()
        .filter(|s| s.kind == Kind::Compact && s.lane.is_empty() && s.ms <= at_ms)
        .map(|s| s.ms)
        .max()
    else {
        return false;
    };
    // (its own lines land with it: `/compact`'s output, in the same moment)
    at_ms.saturating_sub(c) <= JUST
        && !steps
            .iter()
            .any(|s| s.lane.is_empty() && s.kind != Kind::Lane && s.ms > c + 2_000 && s.ms <= at_ms)
}

/// Drop whole months older than `before` (unix s); the rest is kept.
pub fn prune(before: u64) -> usize {
    let mut n = 0;
    for m in months() {
        if store::month_bounds(&m).is_some_and(|(_, end)| end < before)
            && fs::remove_file(file(&m)).is_ok()
        {
            n += 1;
        }
    }
    n
}

/// Rewrite a month's file with one line per (machine, kind, key) (after a re-read).
pub fn compact(m: &str) -> io::Result<()> {
    let keep = read_month(m);
    let text: String = keep.iter().map(|s| to_line(s) + "\n").collect();
    crate::config::write_atomic(&file(m), text.as_bytes())
}

// ------------------------------------------------------------------ words

/// What reads a session's words on its machine: per log, `W <raw id> <text>`
/// for each prompt (its first line) and tool call (its target: a file, a
/// command, a pattern, a url, a query), 120 characters at most. POSIX awk,
/// on the raw lines (no scanner: only these few keys are looked at).
const WORDS_AWK: &str = r##"
function unq(v) { gsub(/\\n|\\t|\\r/, " ", v); gsub(/\\"/, "\"", v); gsub(/\\\\/, "\\", v); return v }
# the string value of a key in the raw line (escaped quotes kept inside)
function str(k,   p, s, i, c, out, esc) {
  p = "\"" k "\":\""; i = index($0, p); if (i == 0) return ""
  s = substr($0, i + length(p)); out = ""; esc = 0
  for (i = 1; i <= length(s) && i <= 600; i++) {
    c = substr(s, i, 1)
    if (esc) { out = out "\\" c; esc = 0; continue }
    if (c == "\\") { esc = 1; continue }
    if (c == "\"") break
    out = out c
  }
  return out
}
function say(id, t) { t = unq(t); gsub(/[\t\r\n]/, " ", t); sub(/^ +/, "", t); if (id != "" && t != "") print "W\t" id "\t" substr(t, 1, 120) }
function target(   t) {
  t = str("file_path"); if (t == "") t = str("command"); if (t == "") t = str("pattern"); if (t == "") t = str("url")
  if (t == "") t = str("query"); if (t == "") t = str("path"); if (t == "") t = str("description"); if (t == "") t = str("prompt")
  return t
}
K == "claude" && index($0, "\"type\":\"user\"") && !index($0, "\"tool_use_id\"") && !index($0, "\"isMeta\":true") {
  id = str("promptId"); if (id == "") id = str("uuid"); t = str("content"); if (t == "") t = str("text"); say(id, t); next
}
K == "claude" && index($0, "\"type\":\"tool_use\"") { id = str("id"); i = index($0, "\"type\":\"tool_use\""); line = $0; $0 = substr($0, i); say(str("id"), target()); $0 = line; next }
K == "codex" && index($0, "\"task_started\"") { turn = str("turn_id"); next }
K == "codex" && index($0, "\"user_message\"") { say(turn, str("message")); next }
K == "codex" && index($0, "\"role\":\"user\"") && index($0, "\"input_text\"") && turn != "" && !said[turn]++ { t = str("text"); if (t !~ /^<(environment_context|user_instructions)/) say(turn, t); else said[turn] = 0; next }
K == "codex" && (index($0, "\"function_call\"") || index($0, "\"custom_tool_call\"")) { a = str("arguments"); if (a == "") a = str("input"); say(str("call_id"), a); next }
"##;

/// A session's words, read now on its machine (prompts' first lines, tool
/// calls' targets), secrets masked; never written anywhere. Keyed like the
/// steps (`hash([agent, id])`). Claude Code and Codex for now.
pub fn words(host: &str, agent: &str, session: &str) -> Result<HashMap<String, String>, String> {
    if session.is_empty() {
        return Ok(HashMap::new());
    }
    // showcase's demo world: made up from its steps (no machine is asked)
    if crate::showcase::demo() {
        let mine: Vec<Step> = months()
            .into_iter()
            .rev()
            .take(2)
            .flat_map(|m| read_month_matching(&m, &[session]))
            .filter(|s| s.host == host && s.agent == agent)
            .collect();
        if let Some(w) = crate::showcase::timeline_words(&mine) {
            return Ok(w);
        }
    }
    if !matches!(agent, "claude" | "codex") {
        return Ok(HashMap::new());
    }
    let st = store::Offsets::load();
    let files: Vec<String> = st
        .hosts
        .get(host)
        .map(|h| {
            h.files
                .iter()
                .filter(|(p, f)| f.agent == agent && p.contains(session))
                .map(|(p, _)| p.clone())
                .collect()
        })
        .unwrap_or_default();
    if files.is_empty() {
        return Ok(HashMap::new());
    }
    let q = |p: &str| format!("'{}'", p.replace('\'', "'\\''"));
    let mut script = String::from("export LC_ALL=C\nAWK='");
    script.push_str(&WORDS_AWK.replace('\'', ""));
    script.push_str("'\n");
    for f in &files {
        let cat = if f.ends_with(".zst") {
            "zstd -dc"
        } else {
            "cat"
        };
        script.push_str(&format!(
            "{cat} {} 2>/dev/null | awk -v K={agent} \"$AWK\"\n",
            q(f)
        ));
    }
    let o = crate::proc::run(
        &mut crate::sshcfg::shell(host, "sh -s"),
        std::time::Duration::from_secs(20),
        Some(script.as_bytes()),
    )
    .map_err(|e| e.to_string())?;
    if !o.ok() {
        return Err(o.reason());
    }
    Ok(parse_words(&o.text(), agent))
}

/// `W <id> <text>` lines → hashed id → the text, masked and cut short.
pub fn parse_words(text: &str, agent: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '\t');
            if f.next() != Some("W") {
                return None;
            }
            let id = f.next()?.trim();
            let t = f.next()?.trim();
            (!id.is_empty() && !t.is_empty()).then(|| {
                let t = crate::sentlog::scrub(t);
                (hash(&[agent, id]), t.chars().take(120).collect())
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session's log as wire lines, each `(tag, seconds, lane, key, name, class)`.
    fn log(lines: &[(&str, u64, &str, &str, &str, &str)]) -> Vec<Step> {
        lines
            .iter()
            .filter_map(|(tag, at, lane, key, name, class)| {
                let line = format!("{tag}\t{at}\tsess\t{lane}\t{key}\t{name}\t{class}");
                from_wire(&line, "b31", "claude")
            })
            .collect()
    }

    const LAUNCH_TWO: &[(&str, u64, &str, &str, &str, &str)] = &[
        // you ask; the agent starts two subagents in the background and stops
        ("p", 100, "", "you", "u", ""),
        ("ks", 101, "", "call-a", "Agent", "agent"),
        ("ke", 102, "", "call-a", "ok", "0"),
        ("ks", 103, "", "call-b", "Agent", "agent"),
        ("ke", 104, "", "call-b", "ok", "0"),
        ("sl", 0, "lane-a", "call-a", "Explore", ""),
        ("sl", 0, "lane-b", "call-b", "Explore", ""),
        ("ks", 150, "lane-a", "a1", "Read", "tool"),
        ("ks", 160, "lane-b", "b1", "Read", "tool"),
    ];

    #[test]
    fn a_turn_woken_by_a_subagent_stays_quiet_until_the_last_report() {
        let mut v = log(LAUNCH_TWO);
        // the turn you asked for ends: that's always news
        assert!(!woken_by_subagent(&v, 110_000));
        // the first report wakes it, the other still out: quiet
        v.extend(log(&[("p", 170, "", "report-1", "a", "")]));
        assert!(woken_by_subagent(&v, 175_000));
        // the second, the last: this is the session finishing
        v.extend(log(&[("p", 180, "", "report-2", "a", "")]));
        assert!(!woken_by_subagent(&v, 185_000));
        // you ask again, with nothing out: news
        v.extend(log(&[("p", 200, "", "you-2", "u", "")]));
        assert!(!woken_by_subagent(&v, 205_000));
    }

    #[test]
    fn a_turn_that_only_compacted_is_known() {
        // /compact: its prompt, the boundary, its output, then at rest
        let v = log(&[
            ("p", 100, "", "you", "u", ""),
            ("p", 200, "", "slash", "u", ""),
            ("c", 206, "", "boundary", "", ""),
            ("p", 206, "", "output", "u", ""),
        ]);
        assert!(ended_in_compaction(&v, 208_000));
        // long after it: another turn's end
        assert!(!ended_in_compaction(&v, 300_000));
        // work after a compaction in mid-turn: a real end
        let mut w = v.clone();
        w.extend(log(&[("ks", 210, "", "t1", "Read", "tool")]));
        assert!(!ended_in_compaction(&w, 212_000));
        // a subagent's compaction isn't the session's
        let v = log(&[("c", 206, "lane-a", "b", "", "")]);
        assert!(!ended_in_compaction(&v, 208_000));
        assert!(!ended_in_compaction(&[], 208_000));
    }

    #[test]
    fn subagents_that_finished_inside_their_call_never_report() {
        // the call ran as long as the subagent did: nothing is out afterwards,
        // so a later wake (a peer's message, say) isn't held back by them
        let v = log(&[
            ("p", 100, "", "you", "u", ""),
            ("ks", 101, "", "call-a", "Agent", "agent"),
            ("sl", 0, "lane-a", "call-a", "Explore", ""),
            ("ks", 110, "lane-a", "a1", "Read", "tool"),
            ("ke", 120, "", "call-a", "ok", "0"),
            ("p", 130, "", "peer-1", "a", ""),
        ]);
        assert!(!woken_by_subagent(&v, 135_000));
    }

    #[test]
    fn a_subagents_own_subagent_is_not_waited_for() {
        // it reports to the subagent that started it, never to the main agent
        let v = log(&[
            ("p", 100, "", "you", "u", ""),
            ("ks", 101, "", "call-a", "Agent", "agent"),
            ("ke", 102, "", "call-a", "ok", "0"),
            ("sl", 0, "lane-a", "call-a", "Explore", ""),
            ("ks", 110, "lane-a", "call-n", "Agent", "agent"),
            ("sl", 0, "lane-n", "call-n", "Explore", ""),
            ("ks", 120, "lane-n", "n1", "Read", "tool"),
            ("p", 130, "", "report-1", "a", ""),
        ]);
        assert!(!woken_by_subagent(&v, 135_000));
    }

    #[test]
    fn nothing_known_is_never_quiet() {
        assert!(!woken_by_subagent(&[], 1_000));
        // only a lane's prompts: the main agent's turn isn't known to be woken
        let v = log(&[("p", 100, "lane-a", "x", "a", "")]);
        assert!(!woken_by_subagent(&v, 105_000));
    }

    #[test]
    fn steps_from_the_wire_and_back() {
        let p = from_wire(
            "p\t2026-10-22T09:00:01.250Z\tsess\t\tprompt-1\tu",
            "b31",
            "claude",
        )
        .unwrap();
        assert_eq!(
            (p.kind, p.ms % 1000, p.name.as_str()),
            (Kind::Prompt, 250, "u")
        );
        let ks = from_wire(
            "ks\t2026-10-22T09:00:02Z\tsess\t\ttoolu_1\tBash\ttool",
            "b31",
            "claude",
        )
        .unwrap();
        let ke = from_wire(
            "ke\t2026-10-22T09:00:05Z\tsess\t\ttoolu_1\terr\t0",
            "b31",
            "claude",
        )
        .unwrap();
        assert_eq!(ks.key, ke.key, "a call's start and end share their key");
        assert_eq!((ke.name.as_str(), ks.class.as_str()), ("err", "tool"));
        let web = from_wire(
            "ks\t1792659602000\tsess\t\tc1\tWebSearch\tweb",
            "b31",
            "claude",
        )
        .unwrap();
        assert_eq!((web.ms, web.class.as_str()), (1_792_659_602_000, "web"));
        let c = from_wire(
            "c\t2026-10-22T09:10:00Z\tsess\t\tu9\t150000\t30000",
            "b31",
            "claude",
        )
        .unwrap();
        assert_eq!((c.n1, c.n2), (150_000, 30_000));
        let l = from_wire("sl\t\tsess\ta1b2c3\ttoolu_9\tExplore", "b31", "claude").unwrap();
        assert_eq!(
            (l.lane.as_str(), l.name.as_str(), l.ms),
            ("a1b2c3", "Explore", 0)
        );
        assert_eq!(
            l.class,
            hash(&["claude", "toolu_9"]),
            "its parent call, keyed as the call is"
        );
        // names stay names: no spaces, quotes, text
        let odd = from_wire("ks\t1\tsess\t\tk\trm -rf \"x\"\ttool", "h", "a").unwrap();
        assert_eq!(odd.name, "rm-rfx");
        for s in [p, ks, ke, web, c, l, odd] {
            assert_eq!(from_line(&to_line(&s)).unwrap(), s);
        }
    }

    /// The words of a fixture log, as its machine would print them.
    fn words_of(agent: &str, file: &str) -> HashMap<String, String> {
        let f = crate::usage::extract::tests::fixtures().join(file);
        let o = std::process::Command::new("awk")
            .args([
                "-v",
                &format!("K={agent}"),
                &WORDS_AWK.replace('\'', ""),
                f.to_str().unwrap(),
            ])
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        parse_words(&String::from_utf8_lossy(&o.stdout), agent)
    }

    #[test]
    fn words_live_prompts_and_tool_targets() {
        let w = words_of(
            "claude",
            "claude/projects/-home-u-shop/7a7a0000-0000-4000-8000-000000000001.jsonl",
        );
        assert_eq!(
            w.get(&hash(&["claude", "pr-0001"])).map(String::as_str),
            Some("SECRET prompt, with \"timestamp\":\"x\" inside")
        );
        assert_eq!(
            w.get(&hash(&["claude", "toolu_T1"])).map(String::as_str),
            Some("SECRET command")
        );
        assert!(
            !w.values().any(|t| t.contains("a note the agent wrote")),
            "a meta line isn't a prompt"
        );
        let w = words_of("codex", "codex/sessions/2026/10/22/rollout-2026-10-22T09-00-00-019a0001-aaaa-7bbb-8ccc-000000000001.jsonl");
        assert!(
            w.get(&hash(&["codex", "call_1"]))
                .is_some_and(|t| t.contains("echo")),
            "{w:?}"
        );
        // secrets are masked on the way
        let m = parse_words(
            "W\tk\texport TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            "claude",
        );
        assert!(
            !m.values()
                .any(|t| t.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789")),
            "{m:?}"
        );
        assert!(from_wire("ks\tnot-a-time\ts\t\tk\tBash\ttool", "h", "a").is_none());
        assert!(from_wire("zz\t1\ts\t\tk", "h", "a").is_none());
    }
}
