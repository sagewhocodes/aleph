//! Plain-CLI commands: ls, status, new, resume, send, keys, rename, close,
//! tile, hub, last, recent, history, preset, settings, version. Every command
//! that lists sessions works across the whole fleet (`hosts=`), or one host
//! with `--host`. `--json` gives machine-readable output; piped output is TSV.

use crate::config::{self, Config, Target};
use crate::fleet::{Conn, Fleet};
use crate::herdr::{self, Herdr, NewOpts, Session};
use crate::out::*;
use crate::term::{self, Tui};
use crate::ui::{self, c};
use crate::{history, launch, names, path, proc, sshcfg, theme, tui};
use ratatui::prelude::*;
use serde_json::json;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::{Duration, Instant};

pub struct Args<'a> {
    pub cfg: Config,
    pub flag: Option<String>,
    pub rest: &'a [String],
}

impl Args<'_> {
    pub(crate) fn has(&self, f: &str) -> bool {
        self.rest.iter().any(|a| a == f)
    }
    pub(crate) fn positional(&self) -> Vec<&str> {
        let mut v = Vec::new();
        let mut i = 0;
        while i < self.rest.len() {
            let a = self.rest[i].as_str();
            if a == "-n" {
                i += 2;
                continue;
            }
            if !a.starts_with('-') {
                v.push(a);
            }
            i += 1;
        }
        v
    }
}

pub fn target(cfg: &Config, flag: &Option<String>) -> Option<Target> {
    cfg.target(flag.as_deref(), herdr::machine_for)
}

pub fn no_host() -> ExitCode {
    err("no default host yet");
    hint("aleph host pick — choose one from ~/.ssh/config");
    hint("aleph host add <alias> <user@hostname> — define a new one");
    ExitCode::from(1)
}

/// The fleet (or the `--host` machine), polled once.
pub fn fleet_now(cfg: &Config, flag: &Option<String>, label: &str) -> Fleet {
    let only = flag.as_deref().or(None);
    let mut f = Fleet::new(cfg, only);
    let sp = Spinner::start(label.to_string());
    f.poll_blocking(Duration::from_secs(20));
    sp.stop();
    f
}

/// Launch something full-screen from the plain CLI (exec herdr at the end).
pub fn launch_cli(cfg: &Config, t: &Target, job: launch::Job) -> ExitCode {
    if !term::interactive() {
        return fail("starting or attaching a session needs a terminal");
    }
    let tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) => return fail(&format!("terminal: {e}")),
    };
    let (tui, o) = launch::run(tui, t, job, false, theme::machine(cfg, &t.ssh));
    launched(cfg, t, tui, o)
}

/// How a CLI launch ended. Showcase mode opens the pane in the dashboard's
/// Focus on the same screen (herdr's own isn't masked).
pub fn launched(cfg: &Config, t: &Target, tui: Tui, o: launch::Outcome) -> ExitCode {
    if let launch::Outcome::Focus(pane) = &o {
        return crate::tui::run_dashboard_in(
            tui,
            cfg.clone(),
            crate::tui::Start {
                focus: Some(format!("{}:{pane}", t.ssh)),
                ..Default::default()
            },
        );
    }
    drop(tui);
    match o {
        launch::Outcome::Cancelled => ExitCode::from(130),
        launch::Outcome::Detached(_) | launch::Outcome::Focus(_) => ExitCode::SUCCESS,
        launch::Outcome::Failed(e) => {
            err(&format!("couldn't start the session on {}: {e}", t.ssh));
            hint("aleph doctor explains what's wrong");
            ExitCode::from(1)
        }
    }
}

// ------------------------------------------------------------------ ls / status

fn status_cell(s: &Session) -> (String, Color) {
    if let Some(r) = &s.limit {
        return (crate::limits::label(r), c::PEACH());
    }
    let (g, col, label) = ui::status(&s.status, Instant::now());
    let g = if s.status == "working" { "✻" } else { g };
    let age = s
        .since
        .map(|t| format!(" · {}", ui::dur(crate::cache::now().saturating_sub(t))))
        .unwrap_or_default();
    (format!("{g} {label}{age}"), col)
}

fn host_line(h: &crate::fleet::Host) -> Line<'static> {
    let mut v = vec![
        Span::raw("  "),
        Span::styled("▌", Style::new().fg(h.color)),
        Span::styled(
            h.alias.clone(),
            Style::new().fg(h.color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !h.addr.is_empty() {
        v.push(Span::styled(format!("  {}", h.addr), ui::faint()));
    }
    match &h.conn {
        Conn::Up => {
            v.push(Span::styled("  ● herdr", ui::fg(c::GREEN())));
            if let Some(l) = h.latency {
                v.push(Span::styled(format!(" · {}ms", l.as_millis()), ui::faint()));
            }
        }
        Conn::Down(e) if crate::fleet::not_running(e) => {
            v.push(Span::styled("  ○ herdr not running", ui::muted()));
        }
        Conn::Down(e) => {
            v.push(Span::styled(
                format!("  ✕ {}", ui::trunc(e, 50)),
                ui::fg(c::RED()),
            ));
            if h.stale {
                if let Some(t) = h.fetched {
                    v.push(Span::styled(
                        format!(
                            " · cached {}",
                            ui::ago(crate::cache::now().saturating_sub(t))
                        ),
                        ui::faint(),
                    ));
                }
            }
        }
        Conn::Unknown => v.push(Span::styled("  …", ui::faint())),
    }
    if h.paused {
        v.push(Span::styled("  ⏸ paused", ui::fg(c::YELLOW())));
    }
    Line::from(v)
}

/// A paused machine isn't watched: say so, and how to resume it.
pub fn warn_paused(f: &Fleet) {
    for h in f.paused() {
        warn(&format!(
            "{} is paused: the dashboard and the daemon don't watch it (no states, questions or alerts from it)",
            h.alias
        ));
        hint(&format!(
            "Z on it in Fleet resumes it · or {} settings unset host.{}.paused",
            crate::doctor::me(),
            h.alias
        ));
    }
}

pub fn session_lines(f: &Fleet) -> Vec<Line<'static>> {
    let all = f.sessions();
    let nw = all
        .iter()
        .map(|s| ui::width(s.display()))
        .max()
        .unwrap_or(8)
        .max(8);
    let mut lines = Vec::new();
    for h in &f.hosts {
        lines.push(host_line(h));
        if h.sessions.is_empty() {
            if h.conn == Conn::Up {
                lines.push(Line::from(Span::styled("     no sessions", ui::faint())));
            }
            continue;
        }
        for s in &h.sessions {
            let (st, col) = status_cell(s);
            let dim = h.stale;
            let base = if dim { ui::faint() } else { Style::new() };
            lines.push(Line::from(vec![
                Span::styled("  ▏ ", Style::new().fg(h.color)),
                Span::styled(format!("{} ", s.glyph()), Style::new().fg(s.color())),
                Span::styled(ui::pad(s.display(), nw), base.add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(
                    ui::pad(&st, 18),
                    if dim { ui::faint() } else { ui::fg(col) },
                ),
                Span::raw("  "),
                Span::styled(ui::pad(&h.short_path(&s.cwd), 28), ui::muted()),
                Span::styled(
                    if s.title.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", ui::trunc(&s.title, 40))
                    },
                    ui::faint(),
                ),
            ]));
        }
    }
    lines
}

pub fn ls(a: Args) -> ExitCode {
    if a.cfg.fleet().is_empty() && a.flag.is_none() {
        return no_host();
    }
    let json_out = a.has("--json");
    let plain = a.has("--plain") || a.has("-p");
    let watch = a.has("--watch") || a.has("-w");
    if term::interactive() && !plain && !json_out && !watch {
        // the master dashboard: navigate, see details + live output, enter to attach
        return tui::run_dashboard(
            a.cfg,
            tui::Start {
                view: tui::View::Fleet,
                flag: a.flag.clone(),
                ..Default::default()
            },
        );
    }
    let mut f = fleet_now(&a.cfg, &a.flag, "Looking for sessions");
    if json_out {
        json(&f.to_json(&a.cfg));
        return ExitCode::SUCCESS;
    }
    if !io::stdout().is_terminal() {
        for s in f.sessions() {
            crate::out::raw(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                s.host,
                s.display(),
                s.status,
                s.cwd,
                s.pane_id,
                s.since.map(|t| t.to_string()).unwrap_or_default(),
                s.title
            ));
        }
        return ExitCode::SUCCESS;
    }
    loop {
        let n = f.sessions().len();
        let blocked = f.sessions().iter().filter(|s| s.blocked()).count();
        if watch {
            print!("\x1b[H\x1b[2J");
        }
        out(Line::from(vec![
            Span::styled("⏺ ", ui::fg(c::ACCENT())),
            Span::styled(
                format!(
                    "{n} session{} on {} machine{}",
                    if n == 1 { "" } else { "s" },
                    f.hosts.len(),
                    if f.hosts.len() == 1 { "" } else { "s" }
                ),
                ui::bold(),
            ),
            Span::styled(
                if blocked > 0 {
                    format!("  ·  {blocked} need you")
                } else {
                    String::new()
                },
                ui::fg(c::RED()).add_modifier(Modifier::BOLD),
            ),
        ]));
        for l in session_lines(&f) {
            out(l);
        }
        if !watch {
            break;
        }
        out(Line::from(Span::styled(
            format!("\n  refreshing every {}s · ctrl+c to stop", a.cfg.refresh()),
            ui::faint(),
        )));
        let _ = io::stdout().flush();
        std::thread::sleep(Duration::from_secs(a.cfg.refresh()));
        f.poll_blocking(Duration::from_secs(20));
    }
    if !watch {
        warn_paused(&f);
    }
    if !plain && !watch {
        hint("aleph ls in a terminal opens the dashboard · aleph resume <name> attaches");
    }
    ExitCode::SUCCESS
}

fn counts(f: &Fleet) -> serde_json::Value {
    let all = f.sessions();
    let n = |st: &str| all.iter().filter(|s| s.status == st).count();
    json!({
        "total": all.len(),
        "working": n("working"),
        "blocked": n("blocked"),
        "done": n("done"),
        "idle": n("idle"),
        "hosts": f.hosts.iter().map(|h| json!({
            "alias": h.alias, "reachable": h.conn == Conn::Up, "sessions": h.sessions.len(),
            "blocked": h.count("blocked"), "working": h.count("working"), "paused": h.paused,
        })).collect::<Vec<_>>(),
        // not watched (no states, questions or alerts from them)
        "paused": f.paused().iter().map(|h| h.alias.clone()).collect::<Vec<_>>(),
        "needs_you": all.iter().filter(|s| s.blocked()).map(|s| json!({"host": s.host, "name": s.name, "display": s.display(), "title": s.title})).collect::<Vec<_>>(),
        "generated_at": crate::cache::now(),
    })
}

pub fn short(v: &serde_json::Value) -> String {
    let g = |k: &str| v[k].as_u64().unwrap_or(0);
    let mut s = Vec::new();
    if g("blocked") > 0 {
        s.push(format!("▲{}", g("blocked")));
    }
    if g("working") > 0 {
        s.push(format!("✻{}", g("working")));
    }
    if g("done") > 0 {
        s.push(format!("✓{}", g("done")));
    }
    if g("idle") > 0 {
        s.push(format!("●{}", g("idle")));
    }
    if s.is_empty() {
        "–".into()
    } else {
        s.join(" ")
    }
}

/// Fresh status written by `aleph watch`, if any (no network needed).
fn watched_status(cfg: &Config) -> Option<serde_json::Value> {
    let p = config::world_cache().join("status.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()?;
    let age = crate::cache::now().saturating_sub(v["generated_at"].as_u64()?);
    (age <= cfg.num("watch_interval").max(5) * 3).then_some(v)
}

pub fn status(a: Args) -> ExitCode {
    let watch = a.has("--watch") || a.has("-w");
    loop {
        let v = match (a.has("--live") || a.flag.is_some(), watched_status(&a.cfg)) {
            (false, Some(v)) if !watch => v,
            _ => counts(&fleet_now(&a.cfg, &a.flag, "Checking sessions")),
        };
        if a.has("--json") {
            json(&v);
        } else if a.has("--short") {
            crate::out::raw(&short(&v));
        } else {
            if watch {
                print!("\x1b[H\x1b[2J");
            }
            let g = |k: &str| v[k].as_u64().unwrap_or(0);
            out(Line::from(vec![
                Span::styled("⏺ ", ui::fg(c::ACCENT())),
                Span::styled(format!("{} sessions", g("total")), ui::bold()),
                Span::styled(
                    format!("   ✻ {} working", g("working")),
                    ui::fg(c::WORKING()),
                ),
                Span::styled(
                    format!("   ▲ {} need you", g("blocked")),
                    ui::fg(c::NEEDS()),
                ),
                Span::styled(format!("   ✓ {} done", g("done")), ui::fg(c::DONE())),
                Span::styled(format!("   ● {} idle", g("idle")), ui::fg(c::IDLE())),
            ]));
            for n in v["needs_you"].as_array().cloned().unwrap_or_default() {
                out(Line::from(vec![
                    Span::styled("  ▲ ", ui::fg(c::NEEDS())),
                    Span::styled(n["display"].as_str().unwrap_or("").to_string(), ui::bold()),
                    Span::styled(
                        format!(" on {}", n["host"].as_str().unwrap_or("")),
                        ui::muted(),
                    ),
                    Span::styled(
                        format!("  {}", n["title"].as_str().unwrap_or("")),
                        ui::faint(),
                    ),
                ]));
            }
            for h in v["paused"].as_array().cloned().unwrap_or_default() {
                let h = h.as_str().unwrap_or("");
                warn(&format!("{h} is paused: not watched (Z on it in Fleet resumes it · {} settings unset host.{h}.paused)", crate::doctor::me()));
            }
        }
        if !watch {
            return ExitCode::SUCCESS;
        }
        let _ = io::stdout().flush();
        std::thread::sleep(Duration::from_secs(a.cfg.refresh()));
    }
}

// ------------------------------------------------------------------ resume / send / keys / rename / close

/// Find one session by name across the fleet. Prints a helpful error otherwise.
/// `find`, without the error lines (None: no such session running).
pub fn find_quiet(cfg: &Config, flag: &Option<String>, q: &str) -> Option<(Target, Session)> {
    let f = fleet_now(cfg, flag, "Looking for sessions");
    let all: Vec<Session> = f.sessions().into_iter().cloned().collect();
    let s = herdr::pick(&all, q)?.clone();
    let t = f
        .host(&s.host)
        .map(|h| h.target.clone())
        .unwrap_or_else(|| cfg.target_for(&s.host, herdr::machine_for));
    Some((t, s))
}

pub fn find(cfg: &Config, flag: &Option<String>, q: &str) -> Result<(Target, Session), ExitCode> {
    let f = fleet_now(cfg, flag, "Looking for sessions");
    let all: Vec<Session> = f.sessions().into_iter().cloned().collect();
    match herdr::pick(&all, q) {
        Some(s) => {
            let t = f
                .host(&s.host)
                .map(|h| h.target.clone())
                .unwrap_or_else(|| cfg.target_for(&s.host, herdr::machine_for));
            Ok((t, s.clone()))
        }
        None => {
            err(&format!("no session '{q}'"));
            if !all.is_empty() {
                let names: Vec<String> = all
                    .iter()
                    .map(|s| format!("{}:{}", s.host, s.name))
                    .collect();
                hint(&format!("running: {}", names.join(", ")));
            }
            Err(ExitCode::from(1))
        }
    }
}

pub fn resume(a: Args) -> ExitCode {
    if a.cfg.fleet().is_empty() && a.flag.is_none() {
        return no_host();
    }
    let q = a.positional().first().map(|s| s.to_string());
    let f = fleet_now(&a.cfg, &a.flag, "Looking for sessions");
    let all: Vec<Session> = f.sessions().into_iter().cloned().collect();
    if all.is_empty() {
        let down: Vec<&str> = f
            .hosts
            .iter()
            .filter(|h| matches!(h.conn, Conn::Down(_)))
            .map(|h| h.alias.as_str())
            .collect();
        if !down.is_empty() {
            err(&format!("can't reach herdr on {}", down.join(", ")));
            hint("aleph doctor explains what's wrong");
        } else {
            head("No sessions running");
            hint("aleph new — or aleph -i to pick a folder first");
        }
        return ExitCode::from(1);
    }
    if q.is_none() && all.len() > 1 && term::interactive() {
        return tui::run_dashboard(
            a.cfg,
            tui::Start {
                view: tui::View::Fleet,
                flag: a.flag.clone(),
                ..Default::default()
            },
        );
    }
    let s = match herdr::pick(&all, q.as_deref().unwrap_or("1")) {
        Some(s) => s.clone(),
        None => {
            err(&format!("no session '{}'", q.unwrap_or_default()));
            let names: Vec<String> = all
                .iter()
                .map(|s| format!("{}:{}", s.host, s.name))
                .collect();
            hint(&format!("running: {}", names.join(", ")));
            return ExitCode::from(1);
        }
    };
    let t = f
        .host(&s.host)
        .map(|h| h.target.clone())
        .unwrap_or_else(|| a.cfg.target_for(&s.host, herdr::machine_for));
    launch_cli(
        &a.cfg,
        &t,
        launch::Job::Attach {
            pane: s.pane_id.clone(),
            label: s.display().into(),
        },
    )
}

pub fn send(a: Args) -> ExitCode {
    // --after idle | 30m | 14:30: queue it for that session instead;
    // --wait [--until state]… [--timeout 10m]: wait for what it does
    let mut after: Option<&str> = None;
    let mut wait = false;
    let mut until: Vec<String> = Vec::new();
    let mut timeout: Option<Duration> = None;
    let mut p: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < a.rest.len() {
        match a.rest[i].as_str() {
            "--after" => {
                after = a.rest.get(i + 1).map(String::as_str);
                i += 1;
            }
            "--wait" => wait = true,
            "--until" => {
                if let Some(u) = a.rest.get(i + 1) {
                    until.push(u.clone());
                }
                wait = true;
                i += 1;
            }
            "--timeout" => {
                timeout = a
                    .rest
                    .get(i + 1)
                    .and_then(|d| crate::log::parse_duration(d));
                i += 1;
            }
            x if x.starts_with("--after=") => after = Some(&x[8..]),
            // a prompt's variable (`@name --var topic=LUTs`)
            "--var" => i += 1,
            x if !x.starts_with('-') => p.push(x),
            _ => {}
        }
        i += 1;
    }
    if p.len() < 2 {
        return fail("usage: aleph send <name> <text…> [--after idle | 30m | 14:30]");
    }
    let (t, s) = match find(&a.cfg, &a.flag, p[0]) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let mut text = p[1..].join(" ");
    // `@review`: a saved prompt, filled in for this session
    let ctx = crate::prompts::Ctx::of(&s);
    match crate::prompts::expand(&a.cfg, &text, &ctx, &crate::prompts::given(a.rest)) {
        Some(Ok(t)) => text = t,
        Some(Err(e)) => return fail(&e),
        None => {}
    }
    if let Some(w) = after {
        let due = match crate::queue::parse_after(w, crate::cache::now(), ui::utc_offset()) {
            Ok(d) => d,
            Err(e) => return fail(&e),
        };
        let id = crate::queue::add(crate::queue::Task {
            host: s.host.clone(),
            prompt: text,
            to: Some(s.name.clone()),
            after: due,
            ..Default::default()
        });
        ok(&format!(
            "queued #{id} for {} on {} — {}",
            s.display(),
            s.host,
            match due {
                Some(t) => format!("sent at {} (or once it's free after that)", ui::clock(t)),
                None => "sent once it's free".into(),
            }
        ));
        if !crate::queue::watch_running(&a.cfg) {
            hint("queued messages go out while aleph watch (or the dashboard) runs — aleph watch --install");
        }
        return ExitCode::SUCCESS;
    }
    if wait {
        let sp = Spinner::start(format!("Sent to {} — waiting for it", s.display()));
        let r = Herdr::new(&t).prompt_wait(&s.pane_id, &text, &until, timeout);
        sp.stop();
        return match r {
            Ok(st) => {
                ok(&format!(
                    "{} is {}",
                    s.display(),
                    if st.is_empty() { "done" } else { st.as_str() }
                ));
                ExitCode::SUCCESS
            }
            Err(e) => {
                err(&e);
                ExitCode::from(1)
            }
        };
    }
    match Herdr::new(&t).prompt(&s.pane_id, &text) {
        Ok(()) => {
            ok(&format!("sent to {} on {}", s.display(), t.ssh));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

/// `aleph wait <session> [--until working|blocked|done|idle]… [--timeout 10m]`
pub fn wait(a: Args) -> ExitCode {
    let mut until: Vec<String> = Vec::new();
    let mut timeout: Option<Duration> = None;
    let mut q: Option<&str> = None;
    let mut i = 0;
    while i < a.rest.len() {
        match a.rest[i].as_str() {
            "--until" => {
                if let Some(u) = a.rest.get(i + 1) {
                    until.push(u.clone());
                }
                i += 1;
            }
            "--timeout" => {
                timeout = a
                    .rest
                    .get(i + 1)
                    .and_then(|d| crate::log::parse_duration(d));
                i += 1;
            }
            "--json" => {}
            x if q.is_none() => q = Some(x),
            _ => {}
        }
        i += 1;
    }
    let Some(q) = q else {
        return fail("usage: aleph wait <name | host:name> [--until working|blocked|done|idle]… [--timeout 10m]");
    };
    let (t, s) = match find(&a.cfg, &a.flag, q) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let sp = Spinner::start(format!("Waiting for {}", s.display()));
    let r = Herdr::new(&t).wait(&s.pane_id, &until, timeout);
    sp.stop();
    match r {
        Ok(st) if a.has("--json") => {
            json(&serde_json::json!({"host": s.host, "name": s.name, "state": st}));
            ExitCode::SUCCESS
        }
        Ok(st) => {
            ok(&format!("{} is {st}", s.display()));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

/// `aleph reply <event id | host:name> <text…>`: a prompt to the session an
/// Inbox event is about (ids from `aleph inbox --json`).
pub fn reply(a: Args) -> ExitCode {
    let p = a.positional();
    if p.len() < 2 {
        return fail("usage: aleph reply <event id | host:name> <text…>");
    }
    let q = p[0];
    let target = if q.len() == 16 && q.chars().all(|c| c.is_ascii_hexdigit()) {
        match crate::events::load(2000).into_iter().find(|e| e.id() == q) {
            Some(e) if !e.name.is_empty() => format!("{}:{}", e.host, e.name),
            Some(_) => return fail("that event is about a machine, not a session"),
            None => return fail(&format!("no event {q} (aleph inbox --json lists them)")),
        }
    } else {
        q.to_string()
    };
    let (t, s) = match find(&a.cfg, &a.flag, &target) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let text = p[1..].join(" ");
    match Herdr::new(&t).prompt(&s.pane_id, &text) {
        Ok(()) => {
            ok(&format!("sent to {} on {}", s.display(), t.ssh));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

pub fn keys(a: Args) -> ExitCode {
    let p = a.positional();
    if p.len() < 2 {
        return fail("usage: aleph keys <name> <key…>   e.g. aleph keys ignite 1   ·   enter · esc · up · ctrl+c");
    }
    let (t, s) = match find(&a.cfg, &a.flag, p[0]) {
        Ok(x) => x,
        Err(c) => return c,
    };
    match Herdr::new(&t).send_keys(&s.pane_id, &p[1..]) {
        Ok(()) => {
            ok(&format!("sent {} to {}", p[1..].join(" "), s.display()));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

/// New tab label for a renamed session: keep the prefix and the `:folder` part.
pub fn renamed_label(label: &str, new: &str) -> String {
    let (base, folder) = match label.split_once(':') {
        Some((b, f)) => (b, Some(f)),
        None => (label, None),
    };
    let prefix = match base.split_once('_') {
        Some((p, _)) => format!("{p}_"),
        None => String::new(),
    };
    match folder {
        Some(f) => format!("{prefix}{new}:{f}"),
        None => format!("{prefix}{new}"),
    }
}

pub fn rename(a: Args) -> ExitCode {
    let p = a.positional();
    if p.len() != 2 {
        return fail("usage: aleph rename <name> <new-name>");
    }
    if !names::valid(p[1]) {
        return fail("names are lowercase letters, digits, - and _ (start with a letter, ≤ 32)");
    }
    let (t, s) = match find(&a.cfg, &a.flag, p[0]) {
        Ok(x) => x,
        Err(c) => return c,
    };
    let h = Herdr::new(&t);
    let label = renamed_label(&s.label, p[1]);
    let r1 = h.rename_tab(&s.tab_id, &label);
    let r2 = h.rename_agent(&s.pane_id, p[1]);
    let _ = h.report_tokens(&s.pane_id, &[("name", p[1])]);
    match (r1, r2) {
        (Err(e), _) => {
            err(&e);
            ExitCode::from(1)
        }
        (Ok(()), r) => {
            ok(&format!("{} → {label}", s.display()));
            if let Err(e) = r {
                hint(&format!("herdr kept the agent's old name ({e})"));
            }
            ExitCode::SUCCESS
        }
    }
}

/// `aleph close`: after `close.grace` (`closing.rs`).
pub fn close(a: Args) -> ExitCode {
    crate::closing::cmd(a)
}

/// Native tiling: move sessions (same machine) into one herdr tab as a grid.
pub fn tile(a: Args) -> ExitCode {
    let p = a.positional();
    if p.len() < 2 {
        return fail("usage: aleph tile <name> <name> [name…]   (sessions on the same machine)");
    }
    let f = fleet_now(&a.cfg, &a.flag, "Looking for sessions");
    let all: Vec<Session> = f.sessions().into_iter().cloned().collect();
    let mut picked: Vec<Session> = Vec::new();
    for q in &p {
        match herdr::pick(&all, q) {
            Some(s) => picked.push(s.clone()),
            None => return fail(&format!("no session '{q}'")),
        }
    }
    let host = picked[0].host.clone();
    if picked.iter().any(|s| s.host != host) {
        return fail("herdr tiles panes within one machine — pick sessions on the same host (the wall tiles across machines: aleph wall)");
    }
    let t = f
        .host(&host)
        .map(|h| h.target.clone())
        .unwrap_or_else(|| a.cfg.target_for(&host, herdr::machine_for));
    match tile_into(&t, &picked) {
        Ok(label) => {
            ok(&format!(
                "tiled {} sessions into {label} on {host}",
                picked.len()
            ));
            hint(&format!("aleph --host {host} resume {}", picked[0].name));
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&e);
            ExitCode::from(1)
        }
    }
}

/// Move `v` into one new tab as an even grid (herdr's own panes).
pub fn tile_into(t: &Target, v: &[Session]) -> Result<String, String> {
    let Some(tree) = crate::layouts::grid(v.len()) else {
        return Err("nothing to tile".into());
    };
    push_into(t, &tree.map(&mut |i| v[*i].clone()))
}

/// Move the sessions of `tree` (one machine's) into one new herdr tab in
/// that shape: `aleph tile`, and the dashboard's Tile pushed into herdr.
/// The tab's label.
pub fn push_into(t: &Target, tree: &crate::layouts::Tree<Session>) -> Result<String, String> {
    let label = format!(
        "tile:{}",
        tree.leaves()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join("+")
    );
    let label: String = label.chars().take(60).collect();
    Herdr::new(t).push_tree(&tree.map(&mut |s| s.pane_id.clone()), &label)?;
    Ok(label)
}

pub fn hub(a: Args) -> ExitCode {
    if !term::interactive() {
        return fail("aleph hub needs a terminal");
    }
    let saved = herdr::profiles();
    let missing: Vec<String> = a
        .cfg
        .hosts()
        .into_iter()
        .filter(|h| !saved.iter().any(|(t, s)| t == h || s == h))
        .collect();
    let mut cfg = a.cfg;
    for m in &missing {
        warn(&format!(
            "{m} isn't saved in herdr — it won't appear in the hub"
        ));
        crate::onboard::run(&mut cfg, m, m);
    }
    head(
        "herdr hub — every machine in one window · ctrl+b w switches machines · ctrl+b q detaches",
    );
    let e = proc::exec(herdr::bin(), &[] as &[&str]);
    fail(&format!("couldn't run herdr: {e}"))
}

// ------------------------------------------------------------------ new / last

pub struct NewArgs {
    pub opts: NewOpts,
    pub host: Option<String>,
    /// `--openrouter <model>` (already in `opts.args`, the agent's way)
    pub openrouter: Option<String>,
}

/// Parse `aleph new` arguments (pure — tested).
pub fn parse_new(rest: &[String], cfg: &Config) -> Result<NewArgs, String> {
    let mut o = NewOpts {
        tile: 1,
        naming: cfg.get_or("naming"),
        prefix: cfg.get_or("tab_prefix"),
        pre_hook: cfg.get("hook.pre_new"),
        ..Default::default()
    };
    let mut host: Option<String> = None;
    let mut dir: Option<String> = None;
    let mut agent: Option<String> = None;
    let mut openrouter: Option<String> = None;
    let mut extra: Vec<String> = Vec::new();
    let mut i = 0;
    let need = |i: usize, f: &str| -> Result<String, String> {
        rest.get(i + 1)
            .cloned()
            .ok_or_else(|| format!("{f} needs a value"))
    };
    while i < rest.len() {
        let a = rest[i].as_str();
        match a {
            "--" => {
                extra.extend(rest[i + 1..].iter().cloned());
                break;
            }
            "--dir" | "-d" => {
                dir = Some(need(i, a)?);
                i += 1;
            }
            "--name" | "-n" => {
                let n = need(i, a)?;
                if !names::valid(&n) {
                    return Err(
                        "names are lowercase letters, digits, - and _ (start with a letter, ≤ 32)"
                            .into(),
                    );
                }
                o.name = Some(n);
                i += 1;
            }
            "--agent" | "-a" => {
                agent = Some(need(i, a)?);
                i += 1;
            }
            "--prompt" | "-m" => {
                o.prompt = Some(need(i, a)?);
                i += 1;
            }
            "--tile" | "-t" => {
                o.tile = need(i, a)?
                    .parse()
                    .ok()
                    .filter(|n| (1..=9).contains(n))
                    .ok_or("--tile takes 1-9")?;
                i += 1;
            }
            "--worktree" | "-w" => o.worktree = Some(String::new()),
            x if x.starts_with("--worktree=") => {
                o.worktree = Some(x["--worktree=".len()..].to_string())
            }
            "--host" | "-H" => {
                host = Some(need(i, a)?);
                i += 1;
            }
            // an OpenRouter model, passed the agent's own way (`--model` is
            // the agent's own flag, passed through as it always was)
            "--openrouter" => {
                openrouter = Some(need(i, a)?);
                i += 1;
            }
            x if x.starts_with("--openrouter=") => {
                openrouter = Some(x["--openrouter=".len()..].to_string())
            }
            x if x.starts_with('@') => {
                let p = cfg
                    .preset(&x[1..])
                    .ok_or_else(|| format!("no preset '{}' (aleph preset ls)", &x[1..]))?;
                host = host.or(p.host);
                dir = dir.or(p.dir);
                agent = agent.or(p.agent);
                if p.worktree && o.worktree.is_none() {
                    o.worktree = Some(String::new());
                }
                o.prompt = o.prompt.take().or(p.prompt);
                if let Some(args) = p.args {
                    extra.extend(split_words(&args));
                }
            }
            _ => extra.push(rest[i].clone()),
        }
        i += 1;
    }
    let h = host
        .clone()
        .or_else(|| cfg.get("ssh_host"))
        .unwrap_or_default();
    let dir = dir.or_else(|| crate::brand::env("DIR"));
    if let Some(d) = &dir {
        if !d.starts_with('/') {
            return Err(
                "--dir / CC_DIR must be an absolute path on the remote (~ would expand locally)"
                    .into(),
            );
        }
        o.dir = Some(path::normalize(d.as_bytes()));
    }
    // --agent / the preset, else the machine's or the global default
    let agent = agent.unwrap_or_else(|| crate::agents::configured(cfg, &h));
    o.agent = crate::agents::resolve(&agent)?.to_string();
    if let Some(def) = cfg.host_get(&h, "args") {
        let mut v = split_words(&def);
        v.extend(extra);
        extra = v;
    }
    // `--openrouter qwen/qwen3-coder`: the agent's own way of taking it, first
    if let Some(m) = &openrouter {
        match crate::openrouter::model_args(&o.agent, m) {
            Some(mut a) => {
                a.extend(extra);
                extra = a;
            }
            None if crate::openrouter::takes_model(&o.agent) => {
                return Err(format!(
                    "--openrouter {m}: a model id is provider/model, no spaces (aleph new --openrouter qwen/qwen3-coder)"
                ))
            }
            None => {
                return Err(format!(
                    "--openrouter: {} can't be started on an OpenRouter model ({} can)",
                    crate::agents::label(&o.agent),
                    crate::openrouter::model_agents().join(", ")
                ))
            }
        }
    }
    o.args = extra;
    Ok(NewArgs {
        opts: o,
        host,
        openrouter,
    })
}

/// Minimal shell-like word splitting for config values ('…' and "…" quotes).
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut q: Option<char> = None;
    let mut any = false;
    for ch in s.chars() {
        match (q, ch) {
            (Some(x), c) if c == x => q = None,
            (Some(_), c) => cur.push(c),
            (None, '\'') | (None, '"') => {
                q = Some(ch);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

pub fn fill_names(o: &mut NewOpts) {
    if o.live.is_empty() {
        // cached snapshot: instant, and new_session adds the host's live names
        let f = Fleet::new(&Config::load(), None);
        o.live = f.live_names();
    }
    if o.recent.is_empty() {
        o.recent = history::recent_names(7);
    }
}

/// A machine's standing terminal (`shell.ready`), started in the background.
pub fn start_ready_shell(cfg: &Config, t: &crate::config::Target) -> Result<(), String> {
    let mut o = parse_new(&["--host".into(), t.ssh.clone()], cfg)?.opts;
    o.agent = crate::agents::SHELL.into();
    o.name = Some(crate::agents::READY_SHELL.into());
    o.no_focus = true;
    fill_names(&mut o);
    Herdr::new(t).new_session(&o, &mut |_, _| {}).map(|_| ())
}

/// `aleph shell [host] [dir] [--detach]`: a plain terminal (no agent) in
/// herdr on a machine, for running commands — `new --agent shell` with the
/// machine and folder as words.
pub fn shell(a: Args) -> ExitCode {
    let cfg = a.cfg.clone();
    let mut flag = a.flag.clone();
    let mut rest: Vec<String> = vec!["--agent".into(), crate::agents::SHELL.into()];
    for w in a.rest {
        if w.starts_with('-') {
            rest.push(w.clone());
        } else if flag.is_none() && (cfg.fleet().contains(w) || crate::sshcfg::is_local(w)) {
            flag = Some(w.clone());
        } else if !rest.iter().any(|x| x == "--dir") {
            rest.push("--dir".into());
            rest.push(w.clone());
        } else {
            return fail(&format!("one machine and one folder: what is '{w}'?"));
        }
    }
    new(Args {
        cfg,
        flag,
        rest: &rest,
    })
}

pub fn new(a: Args) -> ExitCode {
    // --detach: start it and return (scripts, Raycast); --json with it prints what started
    let detach = a.has("--detach");
    let as_json = detach && a.has("--json");
    let rest: Vec<String> = match a.rest.iter().position(|x| x == "--") {
        Some(i) => a.rest[..i]
            .iter()
            .filter(|x| *x != "--detach" && *x != "--json")
            .chain(a.rest[i..].iter())
            .cloned()
            .collect(),
        None => a
            .rest
            .iter()
            .filter(|x| *x != "--detach" && *x != "--json")
            .cloned()
            .collect(),
    };
    // a saved prompt's variables (`-m @explain --var topic=LUTs`) aren't new's own
    let vars = crate::prompts::given(&rest);
    let mut keep = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--var" {
            i += 2;
            continue;
        }
        keep.push(rest[i].clone());
        i += 1;
    }
    let rest = keep;
    let mut na = match parse_new(&rest, &a.cfg) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let flag = na.host.take().or(a.flag.clone());
    let Some(t) = target(&a.cfg, &flag) else {
        return no_host();
    };
    // an OpenRouter model: checked against the list kept (a day old at most),
    // never refused for it (the list may be behind); the key: said when it
    // won't go along
    if let Some(m) = &na.openrouter {
        let id = m.trim_start_matches("openrouter/");
        if let Some((_, all)) = crate::openrouter::models_cached().filter(|x| !x.1.is_empty()) {
            if !all.iter().any(|x| x.id == id) {
                let near = crate::openrouter::search(&all, id);
                match near.first() {
                    Some(x) => warn(&format!(
                        "{id} isn't in OpenRouter's model list — did you mean {}?",
                        x.id
                    )),
                    None => warn(&format!("{id} isn't in OpenRouter's model list")),
                }
            }
        }
        if !crate::openrouter::wants(&a.cfg, &na.opts.agent) {
            hint(&format!(
                "{} isn't in openrouter.agents: the session won't get the OpenRouter key from Aleph",
                crate::agents::label(&na.opts.agent)
            ));
        }
    }
    // `-m @review`: a saved prompt, filled in for where it starts
    if let Some(p) = na.opts.prompt.clone() {
        let ctx = crate::prompts::Ctx {
            host: t.ssh.clone(),
            dir: na
                .opts
                .dir
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default(),
            agent: na.opts.agent.clone(),
            ..Default::default()
        };
        match crate::prompts::expand(&a.cfg, &p, &ctx, &vars) {
            Some(Ok(x)) => na.opts.prompt = Some(x),
            Some(Err(e)) => return fail(&e),
            None => {}
        }
    }
    // this computer's herdr: start where you are unless told otherwise
    if crate::sshcfg::is_local(&t.ssh) && na.opts.dir.is_none() {
        if let Ok(d) = std::env::current_dir() {
            na.opts.dir = Some(path::normalize(d.as_os_str().as_encoded_bytes()));
        }
    }
    fill_names(&mut na.opts);
    if !detach {
        return launch_cli(&a.cfg, &t, launch::Job::New(na.opts));
    }
    // started from a script or Raycast: herdr's view stays where you left it
    na.opts.no_focus = true;
    let sp = Spinner::start(format!("Starting on {}", t.ssh));
    let r = Herdr::new(&t).new_session(&na.opts, &mut |_, msg| sp.set(msg));
    sp.stop();
    match r {
        Ok(v) => {
            launch::after_start(&t, &na.opts, &v);
            if as_json {
                json(&serde_json::json!({
                    "host": t.ssh,
                    "agent": na.opts.agent,
                    "sessions": v.iter().map(|s| serde_json::json!({
                        "name": s.name, "label": s.label, "pane_id": s.pane,
                        "hue": s.hue, "mark": s.mark, "dir": s.dir,
                    })).collect::<Vec<_>>(),
                    // flat fields for the common single-session case
                    "name": v.first().map(|s| s.name.clone()),
                    "pane_id": v.first().map(|s| s.pane.clone()),
                    // what the start couldn't do (the OpenRouter key left out)
                    "note": v.first().and_then(|s| s.note.clone()),
                }));
            } else {
                for s in &v {
                    let wher = if s.dir.is_empty() {
                        "herdr's current folder".to_string()
                    } else {
                        s.dir.clone()
                    };
                    ok(&format!(
                        "started {} ({}) on {} in {wher}",
                        s.name,
                        crate::agents::label(&na.opts.agent),
                        t.ssh
                    ));
                }
                if let Some(n) = v.first().and_then(|s| s.note.as_ref()) {
                    warn(n);
                }
                if let Some(s) = v.first() {
                    hint(&format!("aleph --host {} resume {}", t.ssh, s.name));
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            err(&format!("couldn't start the session on {}: {e}", t.ssh));
            ExitCode::from(1)
        }
    }
}

/// `aleph local [agent] [new flags…]`: an agent (or `shell`) in this
/// computer's own herdr, in the current folder.
pub fn local(a: Args) -> ExitCode {
    if proc::which(&herdr::bin()).is_none() {
        err("herdr isn't installed on this computer");
        hint("install it from https://herdr.dev, then run this again");
        return ExitCode::from(1);
    }
    let mut rest: Vec<String> = Vec::new();
    let mut it = a.rest.iter();
    if let Some(first) = a.rest.first() {
        if !first.starts_with('-') && !first.starts_with('@') {
            rest.push("--agent".into());
            rest.push(first.clone());
            it.next();
        }
    }
    rest.extend(it.cloned());
    new(Args {
        cfg: a.cfg,
        flag: Some(crate::sshcfg::LOCAL.to_string()),
        rest: &rest,
    })
}

pub fn last(a: Args) -> ExitCode {
    let Some(e) = history::load(1).into_iter().next() else {
        err("no sessions in history yet");
        return ExitCode::from(1);
    };
    let t = a.cfg.target_for(&e.host, herdr::machine_for);
    let agent = if e.agent.is_empty() {
        crate::agents::default_agent_for(&a.cfg, &e.host)
            .id
            .to_string()
    } else {
        e.agent.clone()
    };
    let mut o = NewOpts {
        dir: (!e.dir.is_empty()).then(|| e.dir.as_bytes().to_vec()),
        args: e.args.clone(),
        agent,
        tile: 1,
        naming: a.cfg.get_or("naming"),
        prefix: a.cfg.get_or("tab_prefix"),
        pre_hook: a.cfg.get("hook.pre_new"),
        ..Default::default()
    };
    fill_names(&mut o);
    head(&format!(
        "New {} session in {} on {}",
        crate::agents::label(&o.agent),
        e.dir,
        e.host
    ));
    launch_cli(&a.cfg, &t, launch::Job::New(o))
}

// ------------------------------------------------------------------ history / recent

pub fn history_cmd(a: Args) -> ExitCode {
    let n = a
        .rest
        .iter()
        .position(|x| x == "-n")
        .and_then(|i| a.rest.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let v = history::load(n);
    if a.has("--json") {
        json(&serde_json::Value::Array(
            v.iter().map(|e| e.to_json()).collect(),
        ));
        return ExitCode::SUCCESS;
    }
    if v.is_empty() {
        head("No sessions started yet");
        return ExitCode::SUCCESS;
    }
    let now = crate::cache::now();
    for e in &v {
        let col = theme::machine(&a.cfg, &e.host);
        out(Line::from(vec![
            Span::styled(
                format!("  {:>8}  ", ui::ago(now.saturating_sub(e.t))),
                ui::faint(),
            ),
            Span::styled("▌", Style::new().fg(col)),
            Span::styled(ui::pad(&e.host, 8), Style::new().fg(col)),
            Span::styled(ui::pad(&e.name, 12), ui::bold()),
            Span::styled(e.dir.clone(), ui::muted()),
            Span::styled(
                e.prompt
                    .as_ref()
                    .map(|p| format!("  “{}”", ui::trunc(p, 40)))
                    .unwrap_or_default(),
                ui::faint(),
            ),
        ]));
    }
    ExitCode::SUCCESS
}

pub fn recent(a: Args) -> ExitCode {
    let v = history::recent_dirs(a.flag.as_deref());
    if a.has("--json") {
        json(&serde_json::Value::Array(
            v.iter()
                .map(|r| json!({"host": r.host, "dir": r.dir, "count": r.count, "last": r.last, "score": r.score}))
                .collect(),
        ));
        return ExitCode::SUCCESS;
    }
    if !io::stdout().is_terminal() {
        for r in &v {
            crate::out::raw(&format!("{}\t{}\t{}", r.host, r.dir, r.count));
        }
        return ExitCode::SUCCESS;
    }
    let now = crate::cache::now();
    for r in v.iter().take(20) {
        let col = theme::machine(&a.cfg, &r.host);
        out(Line::from(vec![
            Span::styled("  ▌", Style::new().fg(col)),
            Span::styled(ui::pad(&r.host, 8), Style::new().fg(col)),
            Span::styled(ui::pad(&r.dir, 48), Style::new()),
            Span::styled(
                format!("{}× · {}", r.count, ui::ago(now.saturating_sub(r.last))),
                ui::faint(),
            ),
        ]));
    }
    ExitCode::SUCCESS
}

// ------------------------------------------------------------------ presets

// ------------------------------------------------------------------ settings

/// Every known setting with its value, then the rest of the file
/// (`aleph settings list --json`, the API's `config.list`).
pub fn settings_json(cfg: &Config) -> serde_json::Value {
    let mut m = serde_json::Map::new();
    for k in config::KNOWN {
        m.insert(k.key.into(), json!(cfg.get_or(k.key)));
    }
    for (k, v) in cfg.entries() {
        m.insert(k, json!(v));
    }
    serde_json::Value::Object(m)
}

pub fn settings(mut a: Args) -> ExitCode {
    let sub = a.rest.first().map(String::as_str).unwrap_or("list");
    match sub {
        "list" | "ls" => {
            if a.has("--json") {
                json(&settings_json(&a.cfg));
                return ExitCode::SUCCESS;
            }
            out(Line::from(vec![
                Span::styled("⏺ ", ui::fg(c::ACCENT())),
                Span::styled("settings", ui::bold()),
                Span::styled(format!("  ·  {}", crate::doctor::tilde(&a.cfg.path)), ui::faint()),
            ]));
            // the longest key, whole, and a gap after each column
            let kw = config::KNOWN.iter().map(|k| ui::width(k.key)).max().unwrap_or(20);
            for k in config::KNOWN {
                let set = a.cfg.get(k.key);
                let v = set.clone().unwrap_or_else(|| k.default.to_string());
                out(Line::from(vec![
                    Span::styled(format!("  {}  ", ui::pad(k.key, kw)), if set.is_some() { ui::bold() } else { Style::new() }),
                    Span::styled(format!("{}  ", ui::pad(if v.is_empty() { "–" } else { &v }, 18)), if set.is_some() { ui::fg(c::ACCENT()) } else { ui::muted() }),
                    Span::styled(k.help.to_string(), ui::faint()),
                ]));
            }
            let other: Vec<(String, String)> = a.cfg.entries().into_iter().filter(|(k, _)| config::known(k).is_none()).collect();
            if !other.is_empty() {
                out(Line::raw(""));
                for (k, v) in other {
                    out(Line::from(vec![Span::styled(format!("  {}", ui::pad(&k, 28)), Style::new()), Span::styled(v, ui::muted())]));
                }
            }
            ExitCode::SUCCESS
        }
        "get" => {
            let Some(k) = a.rest.get(1) else { return fail("usage: aleph settings get <key>") };
            // as written: a `secret:NAME` reference stays one (aleph secret get NAME)
            println!("{}", a.cfg.get_raw(k).unwrap_or_else(|| a.cfg.get_or(k)));
            ExitCode::SUCCESS
        }
        "set" => {
            let (Some(k), Some(_)) = (a.rest.get(1), a.rest.get(2)) else {
                return fail("usage: aleph settings set <key> <value>");
            };
            let v = a.rest[2..].join(" ");
            if let Err(e) = config::check(k, &v) {
                return fail(&e);
            }
            if config::known(k).is_none() && !k.contains('.') {
                warn(&format!("'{k}' isn't a known setting — saved anyway"));
            }
            match a.cfg.set(k, &a.rest[2..].join(" ")) {
                Ok(()) => {
                    ok(&format!("{k} = {}", a.rest[2..].join(" ")));
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&format!("couldn't save: {e}")),
            }
        }
        "unset" | "reset" => {
            let Some(k) = a.rest.get(1) else { return fail("usage: aleph settings unset <key>") };
            let _ = a.cfg.unset(k);
            ok(&format!("{k} reset to default ({})", config::known(k).map(|k| k.default).unwrap_or("unset")));
            ExitCode::SUCCESS
        }
        "path" => {
            println!("{}", a.cfg.path.display());
            ExitCode::SUCCESS
        }
        "edit" => {
            let ed = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            if !a.cfg.path.exists() {
                let _ = a.cfg.set("refresh", &a.cfg.get_or("refresh"));
            }
            let st = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{ed} {}", proc::sh_quote(&a.cfg.path.to_string_lossy())))
                .status();
            match st {
                Ok(s) if s.success() => ExitCode::SUCCESS,
                _ => fail(&format!("couldn't run {ed}")),
            }
        }
        _ => fail("usage: aleph settings [list | get <key> | set <key> <value> | unset <key> | edit | path]"),
    }
}

// ------------------------------------------------------------------ version / completion data

pub fn version(as_json: bool) -> ExitCode {
    if as_json {
        // what `aleph daemon deploy` and `aleph update` read on other machines
        json(&serde_json::json!({
            "name": crate::brand::BIN,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "exe": std::env::current_exe().ok(),
        }));
        return ExitCode::SUCCESS;
    }
    let mut lines: Vec<Line> = vec![Line::from(vec![
        Span::styled(
            format!("{} aleph ", crate::brand::MARK),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ),
        Span::raw(env!("CARGO_PKG_VERSION")),
    ])];
    let v = |prog: &str, args: &[&str]| -> String {
        let o = proc::cap(prog, args, Duration::from_secs(5));
        o.text().lines().next().unwrap_or("").trim().to_string()
    };
    // the agent this wrapper stands in for (`wraps`), then herdr
    let w = crate::agents::Wraps::from_cfg(&Config::load());
    let cw = ui::width(&w.exe).max(6);
    if let Some(p) = w.path() {
        lines.push(Line::from(vec![
            Span::styled(format!("  {}  ", ui::pad(&w.exe, cw)), ui::faint()),
            Span::raw(v(&p.to_string_lossy(), &["--version"])),
        ]));
    }
    lines.push(Line::from(vec![
        Span::styled(format!("  {}  ", ui::pad("herdr", cw)), ui::faint()),
        Span::raw(v(&herdr::bin(), &["--version"])),
    ]));
    // on a colour terminal: the mark beside it
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && crate::term::color_out() && !c::mono() {
        let mark = crate::logo::mark(12, 6, c::ACCENT());
        lines.insert(
            1,
            Line::from(Span::styled(
                "  coding agents across your machines",
                ui::muted(),
            )),
        );
        let top = (mark.len().saturating_sub(lines.len())) / 2;
        for (i, m) in mark.into_iter().enumerate() {
            let mut spans = m.spans;
            spans.push(Span::raw("   "));
            if let Some(t) = i.checked_sub(top).and_then(|j| lines.get(j)) {
                spans.extend(t.spans.iter().cloned());
            }
            out(Line::from(spans));
        }
        return ExitCode::SUCCESS;
    }
    for l in lines {
        out(l);
    }
    ExitCode::SUCCESS
}

/// `claude __complete <what>` — fast, offline lists for shell completion.
pub fn complete(a: Args) -> ExitCode {
    match a.rest.first().map(String::as_str) {
        Some("hosts") => {
            let mut v = a.cfg.hosts();
            for x in sshcfg::aliases() {
                if !v.contains(&x) {
                    v.push(x);
                }
            }
            for h in v {
                println!("{h}");
            }
        }
        Some("sessions") => {
            let f = Fleet::new(&a.cfg, None);
            for s in f.sessions() {
                println!("{}", s.name);
            }
        }
        Some("presets") => {
            for p in a.cfg.presets() {
                println!("@{}", p.name);
            }
        }
        Some("agents") => {
            // every agent id and executable name (install.sh: does --name shadow an agent?)
            let mut seen: Vec<&str> = Vec::new();
            for ag in crate::agents::ALL {
                for w in std::iter::once(ag.id).chain(ag.exes.iter().copied()) {
                    if !seen.contains(&w) {
                        seen.push(w);
                        println!("{w}");
                    }
                }
            }
        }
        Some("settings") => {
            for k in config::KNOWN {
                println!("{}", k.key);
            }
        }
        // `new --openrouter …`: the model list kept (offline, never fetched here)
        Some("openrouter-models") => {
            for m in crate::openrouter::models_cached()
                .map(|x| x.1)
                .unwrap_or_default()
            {
                println!("{}", m.id);
            }
        }
        _ => {
            for c in crate::COMMANDS {
                println!("{}", c.0);
            }
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_arg_parsing() {
        let cfg = Config::in_memory(&[
            "ssh_host=b31",
            "preset.px.host=gpu2",
            "preset.px.dir=/srv/px",
            "preset.px.args=--model opus",
            "preset.px.worktree=1",
            "host.gpu2.agent=codex",
        ]);
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let n = parse_new(
            &s(&[
                "--dir", "/x/", "--name", "tune", "-m", "fix it", "--tile", "4", "--", "--resume",
            ]),
            &cfg,
        )
        .unwrap();
        assert_eq!(n.opts.dir.as_deref(), Some(&b"/x"[..]));
        assert_eq!(
            (
                n.opts.name.as_deref(),
                n.opts.prompt.as_deref(),
                n.opts.tile
            ),
            (Some("tune"), Some("fix it"), 4)
        );
        assert_eq!(n.opts.args, vec!["--resume"]);
        assert_eq!(n.opts.agent, "claude");
        let p = parse_new(&s(&["@px", "extra"]), &cfg).unwrap();
        assert_eq!(p.host.as_deref(), Some("gpu2"));
        assert_eq!(p.opts.dir.as_deref(), Some(&b"/srv/px"[..]));
        assert_eq!(p.opts.worktree.as_deref(), Some(""));
        assert_eq!(p.opts.agent, "codex");
        assert_eq!(p.opts.args, vec!["--model", "opus", "extra"]);
        assert!(parse_new(&s(&["--dir", "rel"]), &cfg).is_err());
        assert!(parse_new(&s(&["--name", "Bad Name"]), &cfg).is_err());
        assert!(parse_new(&s(&["@nope"]), &cfg).is_err());
        assert_eq!(
            parse_new(&s(&["--agent", "cursor-agent"]), &cfg)
                .unwrap()
                .opts
                .agent,
            "cursor"
        );
        let e = parse_new(&s(&["--agent", "clade"]), &cfg).err().unwrap();
        assert!(e.contains("did you mean 'claude'"), "{e}");
        // an OpenRouter model: the agent's own flag, before the rest
        let o = parse_new(
            &s(&[
                "--agent",
                "opencode",
                "--openrouter",
                "qwen/qwen3-coder",
                "--",
                "-c",
            ]),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            o.opts.args,
            vec!["--model", "openrouter/qwen/qwen3-coder", "-c"]
        );
        let o = parse_new(&s(&["--agent", "pi", "--openrouter=z-ai/glm-4.6"]), &cfg).unwrap();
        assert_eq!(
            o.opts.args,
            vec!["--provider", "openrouter", "--model", "z-ai/glm-4.6"]
        );
        // --model stays the agent's own, passed through
        let o = parse_new(&s(&["--agent", "opencode", "--model", "anthropic/x"]), &cfg).unwrap();
        assert_eq!(o.opts.args, vec!["--model", "anthropic/x"]);
        let e = parse_new(
            &s(&["--agent", "claude", "--openrouter", "qwen/qwen3-coder"]),
            &cfg,
        )
        .err()
        .unwrap();
        assert!(
            e.contains("Claude Code can't") && e.contains("OpenCode"),
            "{e}"
        );
        assert!(parse_new(&s(&["--agent", "kilo", "--openrouter", "two words"]), &cfg).is_err());
        let d = Config::in_memory(&["ssh_host=b31", "agent=codex"]);
        assert_eq!(parse_new(&[], &d).unwrap().opts.agent, "codex");
        assert_eq!(
            parse_new(&s(&["--worktree=feat/x"]), &cfg)
                .unwrap()
                .opts
                .worktree
                .as_deref(),
            Some("feat/x")
        );
    }

    #[test]
    fn words_and_labels() {
        assert_eq!(
            split_words(r#"--model opus --x 'a b' "" c"#),
            vec!["--model", "opus", "--x", "a b", "", "c"]
        );
        assert_eq!(renamed_label("claude_old:px", "new"), "claude_new:px");
        assert_eq!(renamed_label("claude_old", "new"), "claude_new");
        assert_eq!(renamed_label("misc", "new"), "new");
        let v = json!({"blocked": 1, "working": 2, "done": 0, "idle": 3});
        assert_eq!(short(&v), "▲1 ✻2 ●3");
    }
}
