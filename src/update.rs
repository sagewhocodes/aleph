//! `aleph update [--check] [--yes] [--json]` — keep the fleet current.
//!
//! herdr's `--machine` calls need the same herdr version on both ends, so the
//! local herdr is the reference: every fleet machine is compared with it. Also
//! reports outdated herdr integrations, agent CLI versions across machines and
//! aleph itself. `--check` only reports; otherwise it offers
//! `herdr update` on machines that are behind (over ssh) and refreshing the
//! outdated integrations, asking once for each.

use crate::agentcmd;
use crate::agents;
use crate::cli::Args;
use crate::config::Config;
use crate::out::*;
use crate::probe::{self, HostProbe, IState, LOCAL};
use crate::theme;
use crate::ui::{self, c};
use ratatui::prelude::*;
use serde_json::json;
use std::cmp::Ordering;
use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Herdr {
    Same,
    /// remote is older than this machine
    Behind,
    /// remote is newer than this machine
    Ahead,
    Missing,
    /// no local herdr to compare with
    Unknown,
    Unreachable(String),
}

impl Herdr {
    fn word(&self) -> &'static str {
        match self {
            Herdr::Same => "same",
            Herdr::Behind => "behind",
            Herdr::Ahead => "ahead",
            Herdr::Missing => "missing",
            Herdr::Unknown => "unknown",
            Herdr::Unreachable(_) => "unreachable",
        }
    }
}

/// Compare dotted versions numerically (`0.10.0` > `0.9.5`); non-numeric
/// parts compare as text.
pub fn cmp_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<&str> = a.split(['.', '-', '+']).collect();
    let pb: Vec<&str> = b.split(['.', '-', '+']).collect();
    for i in 0..pa.len().max(pb.len()) {
        let (x, y) = (
            pa.get(i).copied().unwrap_or("0"),
            pb.get(i).copied().unwrap_or("0"),
        );
        let o = match (x.parse::<u64>(), y.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            _ => x.cmp(y),
        };
        if o != std::cmp::Ordering::Equal {
            return o;
        }
    }
    std::cmp::Ordering::Equal
}

/// Pure: how a host's herdr compares with the local one.
pub fn compare(local: Option<&str>, p: &HostProbe) -> Herdr {
    if let Some(e) = &p.error {
        return Herdr::Unreachable(e.clone());
    }
    let Some(r) = p.herdr_version() else {
        return Herdr::Missing;
    };
    match local {
        None => Herdr::Unknown,
        Some(l) => match cmp_versions(r, l) {
            Ordering::Equal => Herdr::Same,
            Ordering::Less => Herdr::Behind,
            Ordering::Greater => Herdr::Ahead,
        },
    }
}

/// Pure: per agent id, the newest version seen and the hosts running an older one.
/// (agent, newest version, [(host, older version)])
pub type Lag = (&'static agents::Agent, String, Vec<(String, String)>);

pub fn lagging(probes: &[HostProbe]) -> Vec<Lag> {
    let mut out = Vec::new();
    for ag in agents::ALL {
        let seen: Vec<(String, String)> = probes
            .iter()
            .filter_map(|p| p.agent(ag.id).map(|f| (p.host.clone(), f.version.clone())))
            .filter(|(_, v)| v.chars().next().is_some_and(|c| c.is_ascii_digit()))
            .collect();
        let Some(max) = seen
            .iter()
            .map(|x| x.1.clone())
            .max_by(|a, b| cmp_versions(a, b))
        else {
            continue;
        };
        let behind: Vec<(String, String)> = seen
            .into_iter()
            .filter(|(_, v)| cmp_versions(v, &max) == Ordering::Less)
            .collect();
        out.push((ag, max, behind));
    }
    out
}

fn title(h: &str) -> String {
    if h == LOCAL {
        "this machine".into()
    } else {
        h.to_string()
    }
}

fn chip(cfg: &Config, h: &str, w: usize) -> Vec<Span<'static>> {
    vec![
        Span::raw("  "),
        Span::styled(
            "▌",
            Style::new().fg(if h == LOCAL {
                c::ACCENT()
            } else {
                theme::machine(cfg, h)
            }),
        ),
        Span::styled(ui::pad(&title(h), w), ui::bold()),
        Span::raw("  "),
    ]
}

fn section(t: &str) {
    out(Line::from(vec![
        Span::styled("⏺ ", ui::fg(c::ACCENT())),
        Span::styled(t.to_string(), ui::bold()),
    ]));
}

pub fn cmd(a: Args) -> ExitCode {
    let has = |f: &str| a.rest.iter().any(|x| x == f);
    let check = has("--check") || has("--json");
    let assume = has("--yes") || has("-y");
    let mut hosts: Vec<String> = agentcmd::hosts_for(&a)
        .into_iter()
        .filter(|h| h != LOCAL)
        .collect();
    hosts.insert(0, LOCAL.into());
    if !check && !assume && !io::stdin().is_terminal() {
        return fail("aleph update asks before changing anything — run it in a terminal, pass --yes, or just look with --check");
    }
    let sp = Spinner::start(format!(
        "Checking this machine and {} host{}",
        hosts.len() - 1,
        if hosts.len() == 2 { "" } else { "s" }
    ));
    // daemons deployed to machines (aleph daemon deploy): their versions, meanwhile
    let deployed: Vec<String> = crate::deploy::deploys().keys().cloned().collect();
    let dv = std::thread::spawn(move || crate::deploy::remote_versions(&deployed));
    let probes = probe::run_all(&hosts);
    let daemons = dv.join().unwrap_or_default();
    sp.stop();
    let ours = env!("CARGO_PKG_VERSION");
    let local = probes[0].herdr_version().map(str::to_string);
    let remotes = &probes[1..];
    let cmp: Vec<Herdr> = remotes
        .iter()
        .map(|p| compare(local.as_deref(), p))
        .collect();
    let outdated: Vec<agentcmd::Step> = agentcmd::plan(&probes, &[])
        .into_iter()
        .filter(|s| s.refresh)
        .collect();
    let lag = lagging(remotes);

    if has("--json") {
        json(&json!({
            "aleph": env!("CARGO_PKG_VERSION"),
            "local": {"herdr": local},
            "hosts": remotes.iter().zip(&cmp).map(|(p, h)| json!({
                "alias": p.host,
                "reachable": p.ok(),
                "error": p.error,
                "herdr": p.herdr_version(),
                "herdr_status": h.word(),
                "outdated_integrations": outdated.iter().filter(|s| s.host == p.host).map(|s| s.integration).collect::<Vec<_>>(),
                "agents": p.agents.iter().map(|f| json!({"id": f.id, "version": f.version})).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "local_outdated_integrations": outdated.iter().filter(|s| s.host == LOCAL).map(|s| s.integration).collect::<Vec<_>>(),
            "daemons": daemons.iter().map(|(h, r)| json!({
                "host": h,
                "version": r.as_ref().ok(),
                "status": crate::deploy::compare(ours, r),
                "error": r.as_ref().err(),
            })).collect::<Vec<_>>(),
        }));
        return ExitCode::SUCCESS;
    }

    let w = probes
        .iter()
        .map(|p| ui::width(&title(&p.host)))
        .max()
        .unwrap_or(8);
    // ---- herdr
    section("herdr");
    let mut l = chip(&a.cfg, LOCAL, w);
    l.push(match &local {
        Some(v) => Span::styled(ui::pad(v, 9), ui::bold()),
        None => Span::styled(ui::pad("–", 9), ui::fg(c::YELLOW())),
    });
    l.push(Span::styled(
        if local.is_some() {
            "the reference: herdr --machine needs the same version on both ends"
        } else {
            "herdr isn't installed here — https://herdr.dev"
        },
        ui::faint(),
    ));
    out(Line::from(l));
    for (p, h) in remotes.iter().zip(&cmp) {
        let mut l = chip(&a.cfg, &p.host, w);
        let v = p.herdr_version().unwrap_or("–").to_string();
        let (text, col) = match h {
            Herdr::Same => ("✓ matches".to_string(), c::GREEN()),
            Herdr::Behind => ("↑ behind — herdr update there".into(), c::YELLOW()),
            Herdr::Ahead => (
                "↑ newer than this machine — herdr update here".into(),
                c::YELLOW(),
            ),
            Herdr::Missing => (
                format!("– not installed — aleph host include {}", p.host),
                c::YELLOW(),
            ),
            Herdr::Unknown => ("? nothing to compare with".into(), c::MUTED()),
            Herdr::Unreachable(e) => (format!("✕ {}", ui::trunc(e, 60)), c::RED()),
        };
        if !matches!(h, Herdr::Unreachable(_)) {
            l.push(Span::styled(ui::pad(&v, 9), ui::muted()));
        }
        l.push(Span::styled(text, ui::fg(col)));
        out(Line::from(l));
    }
    println!();

    // ---- integrations
    section("herdr integrations");
    if outdated.is_empty() {
        out(Line::from(Span::styled(
            "  ✓ none outdated",
            ui::fg(c::GREEN()),
        )));
    }
    for s in &outdated {
        let p = probes.iter().find(|p| p.host == s.host);
        let detail = p
            .and_then(|p| p.integ(s.integration))
            .map(|i| match &i.state {
                IState::Outdated { have, want } => format!("{have} → {want}"),
                _ => String::new(),
            })
            .unwrap_or_default();
        let mut l = chip(&a.cfg, &s.host, w);
        l.push(Span::styled(
            format!("↑ {}", s.agent.name),
            ui::fg(c::YELLOW()),
        ));
        l.push(Span::styled(format!("  {detail}"), ui::faint()));
        out(Line::from(l));
    }
    println!();

    // ---- agents
    section("agents");
    if lag.is_empty() {
        out(Line::from(Span::styled(
            "  no agents found on the fleet — aleph agents --all",
            ui::faint(),
        )));
    }
    let wn = lag.iter().map(|x| ui::width(x.0.name)).max().unwrap_or(8);
    for (ag, max, behind) in &lag {
        let mut l = vec![
            Span::raw("  "),
            Span::styled(ui::pad(ag.name, wn), ui::bold()),
            Span::raw("  "),
            Span::styled(ui::pad(max, 10), ui::muted()),
        ];
        if behind.is_empty() {
            let n = remotes.iter().filter(|p| p.agent(ag.id).is_some()).count();
            l.push(Span::styled(
                format!(
                    "✓ same everywhere ({n} machine{})",
                    if n == 1 { "" } else { "s" }
                ),
                ui::fg(c::GREEN()),
            ));
        } else {
            l.push(Span::styled(
                format!(
                    "↑ older on {}",
                    behind
                        .iter()
                        .map(|(h, v)| format!("{h} ({v})"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                ui::fg(c::YELLOW()),
            ));
            l.push(Span::styled(format!("  · {}", ag.install), ui::faint()));
        }
        out(Line::from(l));
    }
    println!();

    // ---- aleph
    section("aleph");
    out(Line::from(vec![
        Span::raw("  "),
        Span::styled(env!("CARGO_PKG_VERSION"), ui::bold()),
        Span::styled(
            "  aleph self-update --check looks for a newer release",
            ui::faint(),
        ),
    ]));
    // TODO(self-update): call crate::selfupdate's check function here once it
    // lands and show "↑ x.y.z available" inline.
    println!();
    if !daemons.is_empty() {
        section("aleph daemons on other machines (aleph daemon deploy)");
        let wd = daemons
            .iter()
            .map(|(h, _)| ui::width(h))
            .max()
            .unwrap_or(8)
            .max(w);
        for (h, r) in &daemons {
            let mut l = chip(&a.cfg, h, wd);
            let v = r.as_deref().unwrap_or("–");
            match crate::deploy::compare(ours, r) {
                "same" => {
                    l.push(Span::styled(ui::pad(v, 9), ui::muted()));
                    l.push(Span::styled("✓ same as here", ui::fg(c::GREEN())));
                }
                "behind" => {
                    l.push(Span::styled(ui::pad(v, 9), ui::muted()));
                    l.push(Span::styled(
                        format!("↑ behind — aleph daemon deploy {h}"),
                        ui::fg(c::YELLOW()),
                    ));
                }
                "ahead" => {
                    l.push(Span::styled(ui::pad(v, 9), ui::muted()));
                    l.push(Span::styled(
                        "↑ newer than this one — update aleph here",
                        ui::fg(c::YELLOW()),
                    ));
                }
                _ => l.push(Span::styled(
                    format!(
                        "✕ {}",
                        ui::trunc(r.as_ref().err().map_or("", |e| e.as_str()), 60)
                    ),
                    ui::fg(c::RED()),
                )),
            }
            out(Line::from(l));
        }
        println!();
    }

    let behind: Vec<&HostProbe> = remotes
        .iter()
        .zip(&cmp)
        .filter(|(_, h)| **h == Herdr::Behind)
        .map(|(p, _)| p)
        .collect();
    let ahead = cmp.contains(&Herdr::Ahead);
    let n_todo = behind.len() + usize::from(ahead) + outdated.len();
    if n_todo == 0 {
        ok("everything that can be compared is current");
        let w = crate::agents::Wraps::from_cfg(&a.cfg);
        if w.subcommands(false).iter().any(|c| c == "update") {
            hint(&format!(
                "{} updates itself: command {} update",
                w.name(),
                w.exe
            ));
        }
        return ExitCode::SUCCESS;
    }
    if check {
        hint("aleph update (without --check) offers the updates");
        return ExitCode::SUCCESS;
    }

    let mut bad = 0;
    if ahead
        && local.is_some()
        && (assume
            || yes("A machine runs a newer herdr than this one — update this machine's herdr?"))
    {
        let sp = Spinner::start("Updating herdr here".into());
        let r = probe::sh(
            LOCAL,
            &update_script(a.cfg.flag("herdr.update_handoff")),
            Duration::from_secs(300),
        );
        sp.stop();
        bad += report(LOCAL, r);
    }
    if !behind.is_empty() {
        let names: Vec<String> = behind.iter().map(|p| p.host.clone()).collect();
        say(Line::from(Span::styled(
            format!(
                "  herdr update installs herdr's latest release on {} (the running server keeps going until it restarts).",
                names.join(", ")
            ),
            ui::faint(),
        )));
        say(Line::from(Span::styled(
            "  To pin the exact version of this machine instead: aleph host include <host> (herdr machine add).",
            ui::faint(),
        )));
        if assume || yes(&format!("Run herdr update on {}?", names.join(", "))) {
            let sp = Spinner::start(format!("Updating herdr on {}", names.join(", ")));
            let handles: Vec<_> = names
                .iter()
                .map(|h| {
                    let h = h.clone();
                    let handoff = a.cfg.flag("herdr.update_handoff");
                    std::thread::spawn(move || {
                        let r = probe::sh(&h, &update_script(handoff), Duration::from_secs(300));
                        (h, r)
                    })
                })
                .collect();
            let res: Vec<_> = handles.into_iter().filter_map(|h| h.join().ok()).collect();
            sp.stop();
            for (h, r) in res {
                bad += report(&h, r);
            }
        }
    }
    if !outdated.is_empty() {
        agentcmd::print_plan(&a.cfg, &outdated);
        if assume || yes("Refresh these integrations?") {
            let (_, b) = agentcmd::apply(&outdated);
            bad += b;
        }
    }
    if bad > 0 {
        ExitCode::from(1)
    } else {
        hint("aleph update --check confirms it");
        ExitCode::SUCCESS
    }
}

/// `handoff`: herdr moves its running server to the new version live, so sessions
/// carry on (`herdr.update_handoff`).
fn update_script(handoff: bool) -> String {
    let how = if handoff { " --handoff" } else { "" };
    format!("LC_ALL=C; export LC_ALL\nherdr update{how} </dev/null 2>&1 | tail -n 3\nprintf 'CCV\\t%s\\n' \"$(herdr --version 2>/dev/null | head -n 1)\"\n")
}

/// Print the outcome of an update; 1 if it failed.
fn report(h: &str, r: Result<crate::proc::Out, String>) -> usize {
    let o = match r {
        Ok(o) => o,
        Err(e) => {
            err(&format!("{}: {e}", title(h)));
            return 1;
        }
    };
    let v = o
        .text()
        .lines()
        .find_map(|l| l.strip_prefix("CCV\t").map(probe::version_of))
        .unwrap_or_default();
    if o.ok() && !v.is_empty() {
        ok(&format!("{} runs herdr {v}", title(h)));
        if h != LOCAL {
            crate::chan::forget(h);
        }
        0
    } else {
        err(&format!(
            "{}: herdr update failed — {}",
            title(h),
            o.reason()
        ));
        1
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_live_handoff_only_when_asked_for() {
        assert!(update_script(true).contains("herdr update --handoff </dev/null"));
        assert!(update_script(false).contains("herdr update </dev/null"));
    }

    use super::*;

    #[test]
    fn herdr_comparison() {
        assert_eq!(cmp_versions("0.10.0", "0.9.5"), Ordering::Greater);
        assert_eq!(cmp_versions("0.7.5", "0.9.5"), Ordering::Less);
        assert_eq!(cmp_versions("0.9.5", "0.9.5"), Ordering::Equal);
        assert_eq!(cmp_versions("1.0", "1.0.0"), Ordering::Equal);

        let p = |t: &str| probe::parse("x", t);
        let new = p("CCH\t/b/herdr\therdr 0.9.5\nCCO\tLinux\n");
        let old = p("CCH\t/b/herdr\therdr 0.7.5\nCCO\tLinux\n");
        let none = p("CCO\tLinux\n");
        assert_eq!(compare(Some("0.9.5"), &new), Herdr::Same);
        assert_eq!(compare(Some("0.9.5"), &old), Herdr::Behind);
        assert_eq!(compare(Some("0.7.5"), &new), Herdr::Ahead);
        assert_eq!(compare(Some("0.9.5"), &none), Herdr::Missing);
        assert_eq!(compare(None, &new), Herdr::Unknown);
        assert!(matches!(compare(Some("1"), &p("")), Herdr::Unreachable(_)));
    }

    #[test]
    fn agent_lag() {
        let a = probe::parse(
            "b31",
            "CCA\tcodex\t/b/codex\tcodex-cli 0.46.0\nCCA\tclaude\t/b/claude\t2.1.0\nCCO\tL\n",
        );
        let b = probe::parse("gpu2", "CCA\tcodex\t/b/codex\tcodex-cli 0.40.1\nCCO\tL\n");
        let v = lagging(&[a, b]);
        let codex = v.iter().find(|x| x.0.id == "codex").unwrap();
        assert_eq!(codex.1, "0.46.0");
        assert_eq!(codex.2, vec![("gpu2".to_string(), "0.40.1".to_string())]);
        let claude = v.iter().find(|x| x.0.id == "claude").unwrap();
        assert!(claude.2.is_empty());
    }
}
