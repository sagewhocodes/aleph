//! Remote control: may a phone, Raycast or the web act on a session
//! (approve, reply, queue)?
//!
//! `remote.allow` and `remote.deny` are comma lists of `host:agent` globs
//! (`b31:*`, `*:claude`, `*:*`). The deny list wins. With nothing allowed —
//! the default — remote actions are off everywhere and only notifications go
//! out. Questions matching `approve_deny` (rm -rf, sudo, git push, …) are
//! never answerable remotely: they wait for someone at a terminal.

use crate::config::Config;

pub fn globs(cfg: &Config, key: &str) -> Vec<String> {
    cfg.get(key)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn glob(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        None => pat == s,
        Some((a, b)) => {
            s.len() >= a.len() + b.len() && s.starts_with(a) && glob_rest(b, &s[a.len()..])
        }
    }
}

fn glob_rest(pat: &str, s: &str) -> bool {
    if pat.is_empty() {
        return true;
    }
    (0..=s.len())
        .filter(|&i| s.is_char_boundary(i))
        .any(|i| glob(pat, &s[i..]))
}

/// Does a `host:agent` glob list match this session?
pub fn matches(list: &[String], host: &str, agent: &str) -> bool {
    list.iter().any(|g| {
        let (h, a) = g.split_once(':').unwrap_or((g.as_str(), "*"));
        glob(h, host) && glob(a, agent)
    })
}

/// May remote channels act on `agent` on `host`?
pub fn allowed(cfg: &Config, host: &str, agent: &str) -> bool {
    matches(&globs(cfg, "remote.allow"), host, agent)
        && !matches(&globs(cfg, "remote.deny"), host, agent)
}

/// Why this question can't be answered from a phone: the deny list, or a form
/// of several questions (a phone's buttons answer one at most).
pub fn not_from_phone(cfg: &Config, question: &str) -> Option<String> {
    terminal_only(cfg, question).or_else(|| {
        crate::ask::is_form(question).then(|| "a form of several questions".to_string())
    })
}

/// Must this question be answered at a terminal (the `approve_deny` list)?
pub fn terminal_only(cfg: &Config, question: &str) -> Option<String> {
    crate::auto::deny_list(cfg)
        .into_iter()
        .find(|d| crate::auto::pmatch(d, question))
}

// ------------------------------------------------------------------ matrices
// machines × agents, as Settings and setup show `remote.allow` and
// `analytics.include` / `analytics.exclude`

/// The cells `allow` minus `deny` turns on. `empty_all`: an empty allow list
/// means every cell (analytics.include), not none (remote.allow).
pub fn grid(
    allow: &[String],
    deny: &[String],
    hosts: &[String],
    agents: &[String],
    empty_all: bool,
) -> Vec<Vec<bool>> {
    hosts
        .iter()
        .map(|h| {
            agents
                .iter()
                .map(|a| {
                    ((empty_all && allow.is_empty()) || matches(allow, h, a))
                        && !matches(deny, h, a)
                })
                .collect()
        })
        .collect()
}

/// A matrix as `host:agent` globs: `*:*` when everything is on,
/// `*:<agent>` for a whole column, `<host>:*` for a whole row. `keep`:
/// entries about machines outside the matrix stay.
pub fn to_globs(
    hosts: &[String],
    agents: &[String],
    on: &[Vec<bool>],
    keep: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = keep
        .iter()
        .filter(|g| {
            let h = g.split_once(':').map_or(g.as_str(), |x| x.0);
            !hosts.iter().any(|x| glob(h, x))
        })
        .cloned()
        .collect();
    let all = !hosts.is_empty() && on.iter().all(|r| r.iter().all(|x| *x));
    if all {
        out.push("*:*".into());
        return out;
    }
    let cols: Vec<bool> = (0..agents.len())
        .map(|j| !hosts.is_empty() && on.iter().all(|r| r.get(j).copied().unwrap_or(false)))
        .collect();
    for (j, a) in agents.iter().enumerate() {
        if cols[j] {
            out.push(format!("*:{a}"));
        }
    }
    for (i, h) in hosts.iter().enumerate() {
        let row = &on[i];
        if !row.is_empty() && row.iter().all(|x| *x) {
            out.push(format!("{h}:*"));
            continue;
        }
        for (j, a) in agents.iter().enumerate() {
            if row.get(j).copied().unwrap_or(false) && !cols[j] {
                out.push(format!("{h}:{a}"));
            }
        }
    }
    out
}

/// The (include, exclude) values for an analytics matrix: everything on is
/// both empty (every machine and agent, the ones added later too); nothing
/// on excludes everything; else include what's ticked.
pub fn include_exclude(
    hosts: &[String],
    agents: &[String],
    on: &[Vec<bool>],
    keep: &[String],
) -> (String, String) {
    if !on.iter().any(|r| r.iter().any(|x| *x)) {
        return (String::new(), "*:*".into());
    }
    let g = to_globs(hosts, agents, on, keep);
    if g == ["*:*"] {
        return (String::new(), String::new());
    }
    (g.join(","), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allow_and_deny_globs() {
        let off = Config::in_memory(&[]);
        assert!(!allowed(&off, "b31", "claude"));
        let c = Config::in_memory(&["remote.allow=b31:*, *:codex", "remote.deny=b31:cursor"]);
        assert!(allowed(&c, "b31", "claude"));
        assert!(allowed(&c, "gpu2", "codex"));
        assert!(!allowed(&c, "gpu2", "claude"));
        assert!(!allowed(&c, "b31", "cursor"));
        let all = Config::in_memory(&["remote.allow=*:*"]);
        assert!(allowed(&all, "local", "pi"));
        assert!(matches(&["gpu*:c*".into()], "gpu2", "claude"));
        assert!(!matches(&["gpu*:c*".into()], "b31", "claude"));
    }

    #[test]
    fn risky_questions_stay_at_the_terminal() {
        let c = Config::in_memory(&[]);
        assert!(terminal_only(&c, "Run: sudo rm -rf /tmp/x ?").is_some());
        assert!(terminal_only(&c, "Run: pytest -x ?").is_none());
    }
}
