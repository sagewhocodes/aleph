//! Session memory and `aleph restore [--host H] [--dry-run] [--all] [--yes] [--days N]`.
//!
//! `~/.local/state/aleph/sessions.json` remembers every session the
//! fleet has seen (host, name, agent, folder, first/last seen), updated after
//! each successful poll. When a session disappears it is marked:
//! * `lost` — it vanished together with every other session on that machine
//!   (herdr's server restarted, the machine rebooted);
//! * `gone` — it vanished while other sessions on the machine kept running
//!   (most likely closed in herdr);
//! * `closed` — closed with `aleph close`.
//!
//! herdr restores agents that have an integration by itself. `aleph restore`
//! recreates the rest — lost sessions (and with `--all`, gone ones too) — in
//! the same folder, with the same name and agent, started with the agent's
//! continue/resume arguments from the registry.

use crate::agentcmd;
use crate::agents::{self, Filter};
use crate::cli::Args;
use crate::config::{self, Config};
use crate::herdr::{self, Herdr, NewOpts, Session};
use crate::out::*;
use crate::probe::{self, Persist, LOCAL};
use crate::ui::{self, c};
use crate::{cache, history, theme};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

/// Don't rewrite the file just to move `last_seen` forward more often than this.
const TOUCH_SECS: u64 = 300;

#[derive(Debug, Clone, PartialEq)]
pub struct Known {
    pub host: String,
    pub name: String,
    pub label: String,
    pub agent: String,
    pub cwd: String,
    pub pane_id: String,
    pub first_seen: u64,
    pub last_seen: u64,
    /// live | lost | gone | closed | restored
    pub state: String,
    /// the agent's own conversation id (herdr's `agent_session`), when known
    pub agent_session: String,
}

impl Known {
    fn to_json(&self) -> Value {
        let mut v = json!({"host": self.host, "name": self.name, "label": self.label, "agent": self.agent,
               "cwd": self.cwd, "pane_id": self.pane_id, "first_seen": self.first_seen,
               "last_seen": self.last_seen, "state": self.state});
        if !self.agent_session.is_empty() {
            v["agent_session"] = json!(self.agent_session);
        }
        v
    }
    fn from_json(v: &Value) -> Option<Known> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        let k = Known {
            host: s("host"),
            name: s("name"),
            label: s("label"),
            agent: s("agent"),
            cwd: s("cwd"),
            pane_id: s("pane_id"),
            first_seen: n("first_seen"),
            last_seen: n("last_seen"),
            state: s("state"),
            agent_session: s("agent_session"),
        };
        (!k.host.is_empty() && !k.name.is_empty()).then_some(k)
    }
}

pub fn file() -> PathBuf {
    if cfg!(test) {
        return std::env::temp_dir()
            .join(format!("ch-sessions-test-{}", std::process::id()))
            .join("sessions.json");
    }
    crate::config::world_state().join("sessions.json")
}

pub fn load() -> Vec<Known> {
    load_from(&file())
}

fn load_from(p: &std::path::Path) -> Vec<Known> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get("sessions").and_then(Value::as_array).cloned())
        .map(|a| a.iter().filter_map(Known::from_json).collect())
        .unwrap_or_default()
}

fn save_to(p: &std::path::Path, v: &[Known]) {
    let doc = json!({"version": 1, "sessions": v.iter().map(Known::to_json).collect::<Vec<_>>()});
    let _ = config::write_atomic(p, doc.to_string().as_bytes());
}

/// Reconcile what we remember about `host` with a successful poll that
/// listed the agents `listed` names (sessions of other agents aren't judged:
/// the poll couldn't see them). Pure — returns true when something changed.
pub fn reconcile(
    known: &mut Vec<Known>,
    host: &str,
    live: &[Session],
    listed: &Filter,
    now: u64,
) -> bool {
    let mut changed = false;
    let judged = |k: &Known| k.host == host && listed.matches(&k.agent);
    let was_live: Vec<(String, String)> = known
        .iter()
        .filter(|k| judged(k) && k.state == "live")
        .map(|k| (k.name.clone(), k.pane_id.clone()))
        .collect();
    // a session that kept its pane: herdr's server didn't restart
    let survivors = live.iter().any(|s| {
        was_live
            .iter()
            .any(|(n, p)| *n == s.name && *p == s.pane_id)
    });
    for s in live {
        // (a pane Aleph didn't start isn't Aleph's to bring back)
        if s.name.is_empty() || s.plain {
            continue;
        }
        match known
            .iter_mut()
            .find(|k| k.host == host && k.name == s.name)
        {
            Some(k) => {
                let fresh = (
                    s.label.as_str(),
                    s.agent.as_str(),
                    s.cwd.as_str(),
                    s.pane_id.as_str(),
                );
                // the conversation id herdr saw (kept when it stops saying)
                let sid = s.native_id.as_deref().unwrap_or("");
                if !sid.is_empty() && sid != k.agent_session {
                    k.agent_session = sid.to_string();
                    changed = true;
                }
                if (
                    k.label.as_str(),
                    k.agent.as_str(),
                    k.cwd.as_str(),
                    k.pane_id.as_str(),
                ) != fresh
                    || k.state != "live"
                    || now.saturating_sub(k.last_seen) >= TOUCH_SECS
                {
                    k.label = s.label.clone();
                    k.agent = s.agent.clone();
                    if s.cwd != "-" && !s.cwd.is_empty() {
                        k.cwd = s.cwd.clone();
                    }
                    k.pane_id = s.pane_id.clone();
                    k.state = "live".into();
                    k.last_seen = now;
                    changed = true;
                }
            }
            None => {
                known.push(Known {
                    host: host.to_string(),
                    name: s.name.clone(),
                    label: s.label.clone(),
                    agent: s.agent.clone(),
                    cwd: s.cwd.clone(),
                    pane_id: s.pane_id.clone(),
                    first_seen: now,
                    last_seen: now,
                    state: "live".into(),
                    agent_session: s.native_id.clone().unwrap_or_default(),
                });
                changed = true;
            }
        }
    }
    for k in known.iter_mut() {
        if judged(k) && k.state == "live" && !live.iter().any(|s| s.name == k.name) {
            k.state = if survivors { "gone" } else { "lost" }.into();
            changed = true;
        }
    }
    changed
}

/// The fleet's hook: remember what a successful poll of `host` saw.
pub fn seen(host: &str, live: &[Session], listed: &Filter) {
    let p = file();
    let mut v = load_from(&p);
    if reconcile(&mut v, host, live, listed, cache::now()) {
        save_to(&p, &v);
    }
}

fn set_state(host: &str, name: &str, state: &str) {
    let p = file();
    let mut v = load_from(&p);
    let mut hit = false;
    for k in v.iter_mut().filter(|k| k.host == host && k.name == name) {
        k.state = state.into();
        hit = true;
    }
    if hit {
        save_to(&p, &v);
    }
}

/// `aleph close` closed it on purpose: never offer it for restore.
pub fn mark_closed(host: &str, name: &str) {
    set_state(host, name, "closed");
}

/// What `aleph restore` would recreate on `host`.
pub fn candidates<'a>(
    known: &'a [Known],
    host: &str,
    live: &[Session],
    probe: Option<&probe::HostProbe>,
    all: bool,
    since: u64,
) -> Vec<&'a Known> {
    known
        .iter()
        .filter(|k| k.host == host && k.last_seen >= since)
        .filter(|k| k.state == "lost" || (all && k.state == "gone"))
        .filter(|k| !live.iter().any(|s| s.name == k.name))
        .filter(|k| {
            all || match (agents::find(&k.agent), probe) {
                // herdr restores agents with an installed integration itself
                (Some(a), Some(p)) => p.persist(a) != Persist::Yes,
                _ => true,
            }
        })
        .collect()
}

/// The command a restored session starts with (for display).
fn start_line(k: &Known) -> String {
    let a = agents::find(&k.agent);
    let exe = a.map_or(k.agent.as_str(), |a| a.exe());
    let args = a
        .map(|a| a.resume_for(Some(&k.agent_session)))
        .unwrap_or_default();
    match a.filter(|_| !args.is_empty()) {
        Some(a) => a.command_line(&args),
        None => format!(
            "{exe}  (fresh start — {} can't continue a conversation)",
            a.map_or(exe, |a| a.name)
        ),
    }
}

struct HostState {
    alias: String,
    target: config::Target,
    live: Result<Vec<Session>, String>,
    probe: probe::HostProbe,
}

fn look(cfg: &Config, hosts: &[String]) -> Vec<HostState> {
    let sp = Spinner::start(format!("Checking sessions on {}", hosts.join(", ")));
    let handles: Vec<_> = hosts
        .iter()
        .map(|h| {
            let t = cfg.target_for(h, herdr::machine_for);
            let h = h.clone();
            std::thread::spawn(move || {
                let ph = h.clone();
                let pr = std::thread::spawn(move || probe::run(&ph));
                let live = Herdr::new(&t).sessions();
                let probe = pr.join().unwrap_or_default();
                HostState {
                    alias: h,
                    target: t,
                    live,
                    probe,
                }
            })
        })
        .collect();
    let v = handles.into_iter().filter_map(|h| h.join().ok()).collect();
    sp.stop();
    v
}

pub fn cmd(a: Args) -> ExitCode {
    let hosts: Vec<String> = agentcmd::hosts_for(&a)
        .into_iter()
        .filter(|h| h != LOCAL)
        .collect();
    if hosts.is_empty() {
        return crate::cli::no_host();
    }
    let has = |f: &str| a.rest.iter().any(|x| x == f);
    let dry = has("--dry-run") || has("-n");
    let all = has("--all");
    let assume = has("--yes") || has("-y");
    let days: u64 = a
        .rest
        .iter()
        .position(|x| x == "--days")
        .and_then(|i| a.rest.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let since = cache::now().saturating_sub(days * 86400);
    let json_out = has("--json");
    if !dry && !assume && !io::stdin().is_terminal() {
        return fail("aleph restore asks before starting sessions — run it in a terminal, pass --yes, or look first with --dry-run");
    }

    let mut states = look(&a.cfg, &hosts);
    // herdr not running (reboot) but the machine answers: start it first —
    // agents with an integration come back by themselves
    for st in states.iter_mut() {
        if st.live.is_err() && st.probe.ok() && st.probe.herdr.is_some() {
            if dry {
                st.live = Ok(vec![]);
                continue;
            }
            let h = Herdr::new(&st.target);
            let sp = Spinner::start(format!("Starting herdr on {}", st.alias));
            let r = h.start_server();
            if r.is_ok() {
                // give herdr a moment to resume integrated agents
                std::thread::sleep(Duration::from_secs(3));
                st.live = h.sessions();
            }
            sp.stop();
            if let Err(e) = r {
                warn(&format!("{}: {e}", st.alias));
            }
        }
    }
    let mut known = load();
    for st in &states {
        if let Ok(live) = &st.live {
            reconcile(&mut known, &st.alias, live, &Filter::All, cache::now());
        }
    }
    save_to(&file(), &known);

    let mut plan: Vec<(&HostState, Known)> = Vec::new();
    for st in &states {
        match &st.live {
            Ok(live) => {
                for k in candidates(&known, &st.alias, live, Some(&st.probe), all, since) {
                    plan.push((st, k.clone()));
                }
            }
            Err(e) => warn(&format!("{}: {e} — skipped", st.alias)),
        }
    }
    if json_out {
        json(&Value::Array(
            plan.iter()
                .map(|(_, k)| {
                    let mut v = k.to_json();
                    v["args"] = json!(agents::find(&k.agent)
                        .map(|a| a.resume_for(Some(&k.agent_session)))
                        .unwrap_or_default());
                    v
                })
                .collect(),
        ));
        if dry {
            return ExitCode::SUCCESS;
        }
    }
    if plan.is_empty() {
        ok(&format!(
            "nothing to restore — every session seen in the last {days} day{} is running{}",
            if days == 1 { "" } else { "s" },
            if all {
                ""
            } else {
                " (or herdr restores it itself)"
            }
        ));
        let gone = known
            .iter()
            .filter(|k| hosts.contains(&k.host) && k.state == "gone" && k.last_seen >= since)
            .count();
        if gone > 0 && !all {
            hint(&format!(
                "{gone} session{} ended while the machine kept running (closed?) — aleph restore --all --dry-run shows them",
                if gone == 1 { "" } else { "s" }
            ));
        }
        return ExitCode::SUCCESS;
    }
    if !json_out {
        say(Line::from(vec![
            Span::styled("⏺ ", ui::fg(c::ACCENT())),
            Span::styled(
                format!(
                    "{} session{} to restore",
                    plan.len(),
                    if plan.len() == 1 { "" } else { "s" }
                ),
                ui::bold(),
            ),
            Span::styled("  · same folder, name and agent", ui::faint()),
        ]));
        let wh = plan
            .iter()
            .map(|(s, _)| ui::width(&s.alias))
            .max()
            .unwrap_or(4);
        let wn = plan
            .iter()
            .map(|(_, k)| ui::width(&k.name))
            .max()
            .unwrap_or(6);
        let now = cache::now();
        for (st, k) in &plan {
            let short = herdr::short_path(&k.cwd, "");
            say(Line::from(vec![
                Span::raw("  "),
                Span::styled("▌", Style::new().fg(theme::machine(&a.cfg, &st.alias))),
                Span::styled(ui::pad(&st.alias, wh), ui::muted()),
                Span::raw("  "),
                Span::styled(ui::pad(&k.name, wn), ui::bold()),
                Span::raw("  "),
                Span::styled(ui::pad(&short, 30), ui::muted()),
                Span::styled(
                    format!(
                        "  {} · seen {}",
                        start_line(k),
                        ui::ago(now.saturating_sub(k.last_seen))
                    ),
                    ui::faint(),
                ),
            ]));
        }
    }
    if dry {
        hint("aleph restore (without --dry-run) starts them");
        return ExitCode::SUCCESS;
    }
    if !assume && !yes("Start them now?") {
        return ExitCode::from(1);
    }
    let recent = history::recent_names(7);
    let mut bad = 0;
    for (st, k) in &plan {
        let ag = agents::find(&k.agent);
        let live_names: Vec<String> = st
            .live
            .as_ref()
            .map(|v| v.iter().map(|s| s.name.clone()).collect())
            .unwrap_or_default();
        let o = NewOpts {
            dir: (k.cwd.starts_with('/')).then(|| k.cwd.as_bytes().to_vec()),
            args: ag
                .map(|a| a.resume_for(Some(&k.agent_session)))
                .unwrap_or_default(),
            name: crate::names::valid(&k.name).then(|| k.name.clone()),
            agent: ag.map_or(k.agent.clone(), |a| a.id.to_string()),
            tile: 1,
            naming: a.cfg.get_or("naming"),
            prefix: a.cfg.get_or("tab_prefix"),
            pre_hook: a.cfg.get("hook.pre_new"),
            live: live_names,
            recent: recent.clone(),
            ..Default::default()
        };
        let sp = Spinner::start(format!("Restoring {} on {}", k.name, st.alias));
        let r = Herdr::new(&st.target).new_session(&o, &mut |_, l| sp.set(l));
        sp.stop();
        match r {
            Ok(v) => {
                let got = v.first().map(|s| s.name.clone()).unwrap_or_default();
                set_state(&st.alias, &k.name, "restored");
                ok(&format!(
                    "{} · {}{} is back",
                    st.alias,
                    k.name,
                    if !got.is_empty() && got != k.name {
                        format!(" (as {got})")
                    } else {
                        String::new()
                    }
                ));
            }
            Err(e) => {
                bad += 1;
                err(&format!("{} · {}: {e}", st.alias, k.name));
            }
        }
    }
    if bad > 0 {
        ExitCode::from(1)
    } else {
        hint(
            "aleph ls shows them · aleph agents setup lets herdr restore integrated agents itself",
        );
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: Filter = Filter::All;

    fn s(name: &str, pane: &str, agent: &str) -> Session {
        Session {
            name: name.into(),
            pane_id: pane.into(),
            agent: agent.into(),
            label: format!("{agent}_{name}"),
            cwd: format!("/srv/{name}"),
            status: "idle".into(),
            ..Default::default()
        }
    }

    #[test]
    fn lost_versus_gone() {
        let mut k = Vec::new();
        let first = [
            s("ignite", "w1:p1", "claude"),
            s("tune", "w1:p2", "codex"),
            s("map", "w1:p3", "claude"),
        ];
        assert!(reconcile(&mut k, "b31", &first, &ALL, 1000));
        assert_eq!(k.len(), 3);
        // nothing changed, and too soon to touch last_seen: no write
        assert!(!reconcile(&mut k, "b31", &first, &ALL, 1010));
        // `map` closed while the others kept their panes → gone
        reconcile(&mut k, "b31", &first[..2], &ALL, 1020);
        assert_eq!(k.iter().find(|x| x.name == "map").unwrap().state, "gone");
        // server restart: nothing kept its pane → lost
        reconcile(&mut k, "b31", &[], &ALL, 1030);
        assert!(k
            .iter()
            .filter(|x| x.name != "map")
            .all(|x| x.state == "lost"));
        // a poll that only lists Claude Code doesn't judge codex sessions
        let mut k2 = Vec::new();
        reconcile(&mut k2, "b31", &first, &ALL, 1000);
        let claude_only = Filter::Only(vec!["claude"]);
        reconcile(&mut k2, "b31", &[first[0].clone()], &claude_only, 1010);
        assert_eq!(k2.iter().find(|x| x.name == "tune").unwrap().state, "live");
        assert_eq!(k2.iter().find(|x| x.name == "map").unwrap().state, "gone");
        // …and one that only lists codex doesn't judge Claude Code's
        let mut k3 = Vec::new();
        reconcile(&mut k3, "b31", &first, &ALL, 1000);
        reconcile(&mut k3, "b31", &[], &Filter::Only(vec!["codex"]), 1010);
        assert_eq!(k3.iter().find(|x| x.name == "tune").unwrap().state, "lost");
        assert!(k3
            .iter()
            .filter(|x| x.agent == "claude")
            .all(|x| x.state == "live"));
        // other hosts are untouched
        reconcile(&mut k2, "gpu2", &[], &ALL, 1020);
        assert!(k2.iter().all(|x| x.host == "b31"));
    }

    #[test]
    fn restore_candidates() {
        let mut k = Vec::new();
        let first = [s("ignite", "w1:p1", "claude"), s("tune", "w1:p2", "codex")];
        reconcile(&mut k, "b31", &first, &ALL, 1000);
        reconcile(&mut k, "b31", &[], &ALL, 2000);
        let p = probe::parse(
            "b31",
            "CCA\tclaude\t/b/claude\t2.1\nCCI\tclaude: current (v11) (/h/c)\nCCI\tcodex: not installed (/h/x)\nCCO\tLinux\n",
        );
        // claude has its integration: herdr brings it back itself
        let c = candidates(&k, "b31", &[], Some(&p), false, 0);
        assert_eq!(
            c.iter().map(|x| x.name.as_str()).collect::<Vec<_>>(),
            vec!["tune"]
        );
        assert_eq!(candidates(&k, "b31", &[], Some(&p), true, 0).len(), 2);
        // already running again (restored by herdr or by hand)
        assert!(candidates(
            &k,
            "b31",
            &[s("tune", "w1:p9", "codex")],
            Some(&p),
            false,
            0
        )
        .is_empty());
        // too old
        assert!(candidates(&k, "b31", &[], Some(&p), false, 5000).is_empty());
        assert!(start_line(&k[1]).starts_with("codex resume --last"));
    }

    #[test]
    fn file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ch-sess-{}", std::process::id()));
        let p = dir.join("sessions.json");
        let mut k = Vec::new();
        reconcile(&mut k, "b31", &[s("ignite", "w1:p1", "claude")], &ALL, 1000);
        save_to(&p, &k);
        assert_eq!(load_from(&p), k);
        let _ = std::fs::remove_dir_all(dir);
    }
}
