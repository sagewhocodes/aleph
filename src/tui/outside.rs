//! Agents running outside herdr on a machine — a terminal tab of their own,
//! not one of herdr's panes — listed under it in Fleet, so every running
//! agent shows up somewhere.
//!
//! They come from the stats sampler's process list (`Stats::agent_list`,
//! which knows whether herdr started each one); each one's folder is looked
//! up off the UI thread when the set changes and every minute (`readlink
//! /proc/<pid>/cwd`, or `lsof` on macOS), and its tokens and context come
//! from its own logs (the usage store), matched by folder. Aleph can see
//! them, not drive them: herdr has no hold on them.

use super::Dash;
use crate::telemetry::AgentProc;
use crate::ui::{self, c};
use ratatui::prelude::*;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// Look the folders up again this often (and whenever the set changes).
const EVERY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct State {
    hosts: HashMap<String, Host>,
}

#[derive(Default)]
struct Host {
    /// the pids the folders were asked for
    pids: Vec<String>,
    cwd: HashMap<String, String>,
    /// pid → the session file it has open (or its folder's newest, for Claude Code)
    file: HashMap<String, String>,
    at: Option<Instant>,
    rx: Option<Receiver<Looked>>,
    /// the login user there (other users' agents aren't listed)
    user: Option<String>,
}

/// What one look at a machine's processes found: (pid → folder, pid → session file).
type Looked = (HashMap<String, String>, HashMap<String, String>);

/// One agent outside herdr.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outside {
    pub host: String,
    pub p: AgentProc,
    pub cwd: Option<String>,
    /// its conversation's id, from the session file it's writing
    pub conv: Option<String>,
}

impl Outside {
    pub fn key(&self) -> String {
        format!("{}|{}", self.host, self.p.pid)
    }
}

/// Stop an agent outside herdr: SIGTERM to its process on its machine (as
/// ctrl+c would), waiting for it to go; its terminal stays. Off the UI thread.
pub(crate) fn stop(d: &mut Dash, o: Outside) {
    let label = crate::agents::label(&o.p.agent).to_string();
    d.toast(
        super::Level::Info,
        format!("stopping {label} (pid {}) on {}…", o.p.pid, o.host),
    );
    let host = o.host.clone();
    d.spawn(Some(host.clone()), move || {
        match crate::adopt::stop(&host, &o.p.pid)? {
            true => Ok(format!("stopped {label} (pid {}) on {host}", o.p.pid)),
            false => Err(format!(
                "{label} (pid {}) on {host} is still running 10 s later",
                o.p.pid
            )),
        }
    });
}

/// The conversation its usage logs say it's in (the logs' own session id).
fn logs_id(d: &Dash, o: &Outside) -> Option<String> {
    let cwd = o.cwd.as_deref()?;
    let data = d.usage.data.as_ref()?;
    let started = crate::cache::now().saturating_sub(o.p.age.unwrap_or(0) + 120);
    data.live
        .iter()
        .filter(|(_, l)| l.host == o.host && l.agent == o.p.agent && l.last >= started)
        .filter(|(_, l)| same_folder(&l.project, cwd))
        .max_by_key(|(_, l)| l.last)
        .map(|(id, _)| id.clone())
}

fn found(o: &Outside) -> crate::adopt::Found {
    crate::adopt::Found {
        host: o.host.clone(),
        pid: o.p.pid.clone(),
        agent: o.p.agent.clone(),
        cmd: o.p.cmd.clone(),
        cwd: o.cwd.clone().unwrap_or_default(),
    }
}

/// Enter on its row: what reopening it in herdr will run, and a yes.
pub(crate) fn ask_reopen(d: &mut Dash, key: &str) {
    let Some(o) = find(d, key) else {
        return;
    };
    // what's known now; the look at the process itself (its open session
    // file, its exact options) comes when it's carried out
    let p = crate::adopt::plan(&found(&o), None, logs_id(d, &o));
    let label = crate::agents::label(&o.p.agent);
    d.mode = super::Mode::Confirm {
        title: "Reopen it in herdr?".into(),
        text: format!(
            "{label} (pid {}) on {} runs outside herdr. This stops it and starts it again in a herdr tab{} — {}, with the options it had: {}. Aleph looks at the process first for its open session file, so it's the same conversation.",
            o.p.pid,
            o.host,
            if p.dir.is_empty() { String::new() } else { format!(" in {}", p.dir) },
            p.how,
            p.command()
        ),
        on_yes: super::Pending::Reopen(o),
    };
}

/// Capture, stop, start in herdr (off the UI thread).
pub(crate) fn reopen(d: &mut Dash, o: Outside) {
    let label = crate::agents::label(&o.p.agent).to_string();
    let logs = logs_id(d, &o);
    let (cfg, t, f) = (d.cfg.clone(), d.target_of(&o.host), found(&o));
    d.toast(
        super::Level::Info,
        format!("reopening {label} (pid {}) in herdr…", o.p.pid),
    );
    d.spawn(Some(o.host.clone()), move || {
        let cap = crate::adopt::capture(&f.host, &f.pid, &f.agent).ok();
        let p = crate::adopt::plan(&f, cap.as_ref(), logs);
        let name = crate::adopt::reopen(&cfg, &t, &p, &mut |_| {})?;
        Ok(format!(
            "{label} runs in herdr now as {name}, {} · {}",
            p.how,
            p.command()
        ))
    });
}

/// The login user on `host` (ssh's resolved user; this computer's own).
fn login_user(host: &str) -> Option<String> {
    crate::sshcfg::resolve(host)
        .map(|r| r.user)
        .filter(|u| !u.is_empty())
}

/// The agents outside herdr on `host`, the longest-running first.
pub(crate) fn list(d: &Dash, host: &str) -> Vec<Outside> {
    let Some(st) = d.stats(host) else {
        return Vec::new();
    };
    let hs = d.outside.hosts.get(host);
    let user = hs.and_then(|h| h.user.clone());
    st.agent_list
        .iter()
        .filter(|p| !p.in_herdr)
        .filter(|p| {
            user.as_ref()
                .is_none_or(|u| p.user.is_empty() || p.user == *u)
        })
        .map(|p| Outside {
            host: host.to_string(),
            p: p.clone(),
            cwd: hs.and_then(|h| h.cwd.get(&p.pid).cloned()),
            conv: hs
                .and_then(|h| h.file.get(&p.pid))
                .and_then(|f| crate::adopt::id_in_path(&p.agent, f)),
        })
        .collect()
}

/// One of them by its key (`host|pid`).
pub(crate) fn find(d: &Dash, key: &str) -> Option<Outside> {
    let (h, _) = key.split_once('|')?;
    list(d, h).into_iter().find(|o| o.key() == key)
}

/// Call every frame: folders for new agents, and again every minute.
pub(crate) fn pump(d: &mut Dash) {
    let aliases: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
    for a in aliases {
        let pids: Vec<String> = {
            let mut v: Vec<String> = d
                .stats(&a)
                .map(|st| {
                    st.agent_list
                        .iter()
                        .filter(|p| !p.in_herdr)
                        .map(|p| p.pid.clone())
                        .collect()
                })
                .unwrap_or_default();
            v.sort();
            v
        };
        let h = d.outside.hosts.entry(a.clone()).or_default();
        if let Some(rx) = &h.rx {
            if let Ok(m) = rx.try_recv() {
                // showcase: their folders are masked like herdr sessions'
                for c in m.0.values() {
                    crate::showcase::learn_folder(c);
                }
                h.cwd = m.0;
                h.file = m.1;
                h.rx = None;
            }
        }
        if pids.is_empty() || h.rx.is_some() {
            continue;
        }
        let due = h.pids != pids || h.at.is_none_or(|t| t.elapsed() > EVERY);
        if !due {
            continue;
        }
        h.pids = pids.clone();
        h.at = Some(Instant::now());
        let (tx, rx) = channel();
        h.rx = Some(rx);
        let need_user = h.user.is_none();
        let host = a.clone();
        std::thread::spawn(move || {
            let _ = tx.send(folders(&host, &pids));
        });
        if need_user {
            h.user = login_user(&a);
        }
    }
}

/// The lookup script: `pid<TAB>folder<TAB>session file` per pid (pure — tested).
/// The file is one it has open (Codex keeps its rollout open); else, for Claude
/// Code, which opens its only to write, its folder's newest.
pub(crate) fn script(pids: &[String]) -> String {
    let list: Vec<&str> = pids
        .iter()
        .map(String::as_str)
        .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .collect();
    format!(
        "LC_ALL=C; for p in {}; do c=$(readlink \"/proc/$p/cwd\" 2>/dev/null); \
         [ -n \"$c\" ] || c=$(lsof -a -d cwd -p \"$p\" -Fn 2>/dev/null | sed -n 's/^n//p' | head -n 1); \
         f=$(ls -l \"/proc/$p/fd\" 2>/dev/null | sed -n 's/.* -> \\(.*\\.jsonl\\)$/\\1/p' | head -n 1); \
         [ -n \"$f\" ] || f=$(lsof -p \"$p\" -Fn 2>/dev/null | sed -n 's/^n\\(.*\\.jsonl\\)$/\\1/p' | head -n 1); \
         if [ -z \"$f\" ]; then d=$(printf %s \"$c\" | sed 's/[^A-Za-z0-9]/-/g'); \
         f=$(ls -t \"$HOME/.claude/projects/$d\"/*.jsonl 2>/dev/null | head -n 1); fi; \
         printf '%s\\t%s\\t%s\\n' \"$p\" \"$c\" \"$f\"; done\n",
        list.join(" ")
    )
}

/// `pid<TAB>…` lines → pid → column `col` (1 the folder, 2 the session file; pure — tested).
pub(crate) fn parse(text: &str, col: usize) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let p = f.next()?;
            let v = f.nth(col - 1)?.trim();
            (!v.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
                .then(|| (p.to_string(), v.to_string()))
        })
        .collect()
}

fn folders(host: &str, pids: &[String]) -> Looked {
    let mut cmd = crate::sshcfg::shell(host, "sh -s");
    let input = script(pids);
    match crate::proc::run(&mut cmd, Duration::from_secs(10), Some(input.as_bytes())) {
        Ok(o) => {
            let t = o.text();
            (parse(&t, 1), parse(&t, 2))
        }
        Err(_) => Default::default(),
    }
}

/// Its numbers from its own logs: its own conversation when the file it writes
/// says which, else the latest of that agent in that folder since the process
/// started, never one a session in herdr is having.
pub(crate) fn usage_for<'a>(d: &'a Dash, o: &Outside) -> Option<&'a crate::usage::dash::Live> {
    let data = d.usage.data.as_ref()?;
    if let Some(l) = o.conv.as_ref().and_then(|c| data.live.get(c)) {
        return Some(l);
    }
    let cwd = o.cwd.as_deref()?;
    let started = crate::cache::now().saturating_sub(o.p.age.unwrap_or(0) + 120);
    let in_herdr: Vec<&str> = d
        .fleet
        .sessions()
        .into_iter()
        .filter_map(|s| s.native_id.as_deref())
        .collect();
    data.live
        .iter()
        .filter(|(id, _)| !in_herdr.contains(&id.as_str()))
        .map(|(_, l)| l)
        .filter(|l| l.host == o.host && l.agent == o.p.agent && l.last >= started)
        .filter(|l| same_folder(&l.project, cwd))
        .max_by_key(|l| l.last)
}

/// A log's folder (`~/…` under the home there) against an absolute one.
pub(crate) fn same_folder(project: &str, cwd: &str) -> bool {
    if project.is_empty() {
        return false;
    }
    match project.strip_prefix('~') {
        Some(rest) => !rest.is_empty() && cwd.ends_with(rest),
        None => project.trim_end_matches('/') == cwd.trim_end_matches('/'),
    }
}

/// A Fleet row: the tree guide, the agent, `outside herdr · up 3h`, its
/// folder and numbers.
pub(crate) fn line(
    d: &Dash,
    o: &Outside,
    guide: &str,
    w: usize,
    (nw, mw, sw): (usize, usize, usize),
    selected: bool,
) -> Line<'static> {
    let short = |p: &str| -> String {
        d.fleet
            .host(&o.host)
            .map(|h| h.short_path(p))
            .unwrap_or_else(|| p.to_string())
    };
    let label = crate::agents::label(&o.p.agent);
    let state = match o.p.age {
        Some(a) => format!("○ outside · up {}", ui::dur(a)),
        None => "○ outside herdr".to_string(),
    };
    let state = ui::trunc(&state, sw);
    let mut v: Vec<Span> = vec![
        Span::styled(format!("  {guide}  "), ui::fg(c::LINE())),
        Span::raw("  "),
        crate::logo::glyph(crate::logo::Kind::Agent, &o.p.agent),
        Span::raw(" "),
        Span::styled(ui::pad(&ui::trunc(&label, nw), nw), ui::muted()),
        Span::raw("  "),
    ];
    if mw > 0 {
        let model = usage_for(d, o)
            .map(|l| crate::timeline::short_model(&l.model))
            .unwrap_or_default();
        v.push(Span::styled(
            ui::pad(&ui::trunc(&model, mw), mw),
            ui::muted(),
        ));
        v.push(Span::raw("  "));
    }
    v.push(Span::styled(ui::pad(&state, sw), ui::faint()));
    let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
    let room = w.saturating_sub(used + 1);
    if room > 6 {
        let folder = o.cwd.as_deref().map(short).unwrap_or_else(|| "…".into());
        // the folder keeps room for its last part; the numbers get the rest
        let keep = ui::width(&folder).min(20);
        let chip: Vec<Span> = usage_for(d, o)
            .map(|l| super::usage::chip(l, room.saturating_sub(keep + 2).min(34)))
            .unwrap_or_default();
        let cw: usize = chip.iter().map(|x| ui::width(&x.content)).sum();
        let fw = ui::width(&folder).min(room.saturating_sub(if cw > 0 { cw + 2 } else { 0 }));
        v.push(Span::styled(ui::trunc_left(&folder, fw), ui::muted()));
        if cw > 0 {
            let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
            if used + cw + 2 <= w {
                v.push(Span::raw(" ".repeat(w - used - cw)));
                v.extend(chip);
            }
        }
    }
    let mut l = Line::from(v);
    if selected {
        l = l.style(ui::selected());
    }
    l
}

/// The panel beside the list for one of them.
pub(crate) fn info(d: &Dash, o: &Outside, w: usize) -> Vec<Line<'static>> {
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), ui::faint()),
            Span::styled(v, ui::muted()),
        ])
    };
    let mut v = vec![
        Line::from(vec![
            crate::logo::glyph(crate::logo::Kind::Agent, &o.p.agent),
            Span::styled(format!(" {}", crate::agents::label(&o.p.agent)), ui::bold()),
            Span::styled("  outside herdr", ui::faint()),
        ]),
        Line::raw(""),
        kv(
            "folder",
            o.cwd
                .as_deref()
                .map(|c| {
                    let s = d
                        .fleet
                        .host(&o.host)
                        .map(|h| h.short_path(c))
                        .unwrap_or_else(|| c.to_string());
                    ui::trunc_left(&s, w.saturating_sub(12))
                })
                .unwrap_or_else(|| "looking…".into()),
        ),
        kv(
            "process",
            format!(
                "pid {}{}",
                o.p.pid,
                o.p.age
                    .map(|a| format!(" · up {}", ui::dur(a)))
                    .unwrap_or_default()
            ),
        ),
    ];
    if !o.p.cmd.is_empty() {
        v.push(kv("command", ui::trunc(&o.p.cmd, w.saturating_sub(12))));
    }
    if let Some(l) = usage_for(d, o) {
        let mut model = l.model.clone();
        if !l.effort.is_empty() {
            model = format!("{model} {}", l.effort);
        }
        v.push(kv("model", model));
        let mut sp = vec![Span::styled(format!("{:<10}", "usage"), ui::faint())];
        sp.extend(super::usage::chip(l, w.saturating_sub(12)));
        v.push(Line::from(sp));
    }
    v.push(Line::raw(""));
    for l in ui::wrap(
        "It runs in a terminal of its own, not in herdr: Aleph can see it, not attach to it or answer it. n starts a session here that Aleph can drive.",
        w.saturating_sub(2),
    ) {
        v.push(Line::from(Span::styled(l, ui::faint())));
    }
    v
}

/// The herdr sessions and outside agents of a machine share one tree: is
/// this row the last of it? (pure — tested)
pub(crate) fn guide(last: bool) -> &'static str {
    if last {
        "└─"
    } else {
        "├─"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_script_and_parse() {
        let s = script(&["123".into(), "45; rm -rf /".into(), "77".into()]);
        assert!(s.contains("for p in 123 77;"), "{s}");
        assert!(!s.contains("rm -rf"), "{s}");
        let m = parse("123\t/home/selene/hunt\n77\t\nx\t/etc\n", 1);
        assert_eq!(m.len(), 1);
        assert_eq!(m["123"], "/home/selene/hunt");
    }

    #[test]
    fn folders_match_logs() {
        assert!(same_folder("~/work/hunt", "/home/selene/work/hunt"));
        assert!(same_folder("/srv/train/", "/srv/train"));
        assert!(!same_folder("~/work/hunt", "/home/selene/work/hunter"));
        assert!(!same_folder("", "/x"));
        assert!(!same_folder("~", "/home/selene"));
        assert_eq!(guide(true), "└─");
    }
}
