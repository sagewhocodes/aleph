//! `aleph watch [--once] [--interval S] [--install | --uninstall] [--dry-run]`
//!
//! Polls every fleet machine (each on its own schedule, so a dead host never
//! delays the others) and notifies when a session starts waiting on you —
//! and, with `notify_done=true`, when a working session finishes. Never on the
//! first poll of a host (that's just the current state), never with
//! `notify=false`. After every poll it writes `~/.cache/aleph/status.json`
//! (the `aleph status --json` shape), which `aleph status` and Raycast read.
//!
//! Delivery: macOS `terminal-notifier` (grouped per session; clicking opens a
//! Warp tab attached to it) or `osascript`; Linux `notify-send`.
//! `--install` / `--uninstall` are `aleph daemon install` / `uninstall`
//! (`daemonctl.rs`): a launchd agent on macOS, a systemd user unit on Linux.

use crate::cli::{self, Args};
use crate::fleet::{Conn, Fleet, Transition};
use crate::herdr::{short_path, Session};
use crate::out::*;
use crate::proc::{self, sh_quote};
use crate::shortcuts;
use crate::ui::{self, c};
use ratatui::prelude::*;
use serde_json::json;
use std::io::{self, IsTerminal};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

const T_NOTIFY: Duration = Duration::from_secs(10);

pub fn cmd(mut a: Args) -> ExitCode {
    let has = |f: &str| a.rest.iter().any(|x| x == f);
    let dry = has("--dry-run") || has("-n");
    if has("--install") {
        return crate::daemonctl::install(dry);
    }
    if has("--uninstall") {
        return crate::daemonctl::uninstall(dry);
    }
    let mut interval = a.cfg.num("watch_interval");
    let mut i = 0;
    while i < a.rest.len() {
        let x = a.rest[i].as_str();
        let v = if x == "--interval" {
            i += 1;
            a.rest.get(i).map(String::as_str)
        } else {
            x.strip_prefix("--interval=")
        };
        if x == "--interval" || x.starts_with("--interval=") {
            match v.and_then(|v| v.parse::<u64>().ok()) {
                Some(n) => interval = n,
                None => return fail("--interval needs a number of seconds"),
            }
        }
        i += 1;
    }
    let interval = Duration::from_secs(interval.max(5));
    let tty = io::stderr().is_terminal();
    // the API socket is also the single-instance lock: one daemon per computer
    let shared = crate::daemon::Shared::new();
    if !has("--once") {
        if let Some(code) = take_socket(&shared, tty) {
            return code;
        }
        // the HTTP API and web console, when api.http = on
        crate::http::start(&a.cfg, &shared);
    }
    let mut f = Fleet::new(&a.cfg, a.flag.as_deref());
    // the Inbox under the rules of 2026-10-26, once
    crate::events::tidy_once();
    f.scan_limits = true;
    if f.hosts.is_empty() {
        return cli::no_host();
    }
    // (showcase's demo world answers at once: polled every second)
    f.interval = crate::showcase::poll_every(interval);
    f.listen(&a.cfg);
    let notify = a.cfg.flag("notify");
    let notify_done = a.cfg.flag("notify_done");

    // what the last run saw (on disk, marked stale) answers until the first
    // poll is back: a client never meets an empty fleet while this starts
    shared.publish(f.to_json(&a.cfg), false);
    let sp = Spinner::start("Checking sessions".into());
    f.poll_blocking(Duration::from_secs(45));
    sp.stop();
    write_status(&f);
    shared.publish(f.to_json(&a.cfg), false);

    if has("--once") {
        let blocked: Vec<Session> = f
            .sessions()
            .into_iter()
            .filter(|s| s.blocked())
            .cloned()
            .collect();
        if has("--json") {
            json(&counts(&f));
        }
        for s in &blocked {
            let n = note(s, Kind::NeedsYou);
            log_note(&n);
            if notify {
                deliver(&n);
            }
        }
        if blocked.is_empty() && !has("--json") {
            ok("nothing needs you");
        }
        return ExitCode::SUCCESS;
    }

    let names: Vec<String> = f.hosts.iter().map(|h| h.alias.clone()).collect();
    crate::log::info(
        "daemon",
        &format!(
            "started {} — watching {}",
            env!("CARGO_PKG_VERSION"),
            names.join(", ")
        ),
    );
    head(&format!(
        "watching {} every {}s{}",
        names.join(", "),
        f.interval.as_secs(),
        if tty { " — ctrl+c to stop" } else { "" }
    ));
    if !notify {
        warn("notify=false — only status.json is kept up to date");
    }
    let mut last = f.changed_at;
    let mut failed = false;
    // auto-approve, phone notifications, the task queue and self-heal — only
    // while this process holds the hub lock (one hub per computer; hub.machine
    // picks the computer)
    let mut hub = crate::auto::Hub::new();
    // the daemon's router also does the desktop notifications
    hub.router = crate::router::Router::new(true);
    let mut lock: Option<crate::auto::HubLock> = None;
    let mut lock_try = std::time::Instant::now() - Duration::from_secs(60);
    let me = Upgrade::start();
    let mut autostart = Autostart::default();
    // token/time analytics: the agents' logs, read in the background now and then
    let mut usage = crate::usage::Background::new();
    // the settings as they were read, to notice a change (aleph settings, a
    // dashboard, a profile switched) and read them again; the first look
    // reads them again anyway (they may have moved while this started)
    let mut cfg_stamp: Option<u64> = None;
    let mut cfg_look = std::time::Instant::now();
    // a step that holds the loop up (a slow machine, a sink that hangs) says so
    let slow = |what: &str, t0: std::time::Instant| {
        let d = t0.elapsed();
        if d > Duration::from_secs(5) {
            crate::log::warn(
                "daemon",
                &format!("{what} took {:.1} s (the loop waited)", d.as_secs_f64()),
            );
        }
    };
    loop {
        std::thread::sleep(Duration::from_millis(250));
        if cfg_stamp.is_none() || cfg_look.elapsed() > Duration::from_secs(2) {
            cfg_look = std::time::Instant::now();
            // taken before the read: a change while reading shows next time
            let s = a.cfg.stamp();
            if cfg_stamp != Some(s) {
                a.cfg = crate::config::Config::load();
                crate::theme::assign_machine_colors(&mut a.cfg);
                f.outages = crate::fleet::Outages::from_cfg(&a.cfg);
                // a machine paused or resumed (a dashboard's Z, aleph settings)
                f.follow_paused(&a.cfg);
                if cfg_stamp.is_some() {
                    crate::log::info("daemon", "the settings changed: read them again");
                }
                cfg_stamp = Some(s);
            }
        }
        let t0 = std::time::Instant::now();
        usage.tick(&a.cfg, &f.unreachable());
        slow("usage", t0);
        // `cargo install` / `aleph update` replaced the binary: carry on as
        // the new one (an old watch would run the hub with yesterday's rules)
        if me.replaced() {
            // the listener's ssh forwards are our children: stop them first
            f.stop_listening();
            me.reexec();
            f.listen(&a.cfg);
        }
        // showcase's demo world went on or off: start again in the other world
        if crate::showcase::world_changed() {
            f.stop_listening();
            crate::showcase::restart();
            f.listen(&a.cfg);
        }
        // a machine changed (`hosts.refresh`: it was just set up): look again
        let t0 = std::time::Instant::now();
        for h in shared.take_refresh() {
            looked_again(&mut f, &h);
        }
        slow("looking at a machine again", t0);
        // hosts that haven't answered yet this run: their first result is
        // just the current state, not news
        let fresh: Vec<String> = f
            .hosts
            .iter()
            .filter(|h| h.conn == Conn::Unknown)
            .map(|h| h.alias.clone())
            .collect();
        let t0 = std::time::Instant::now();
        let trans = f.tick();
        slow("the fleet's poll", t0);
        if lock.is_none()
            && lock_try.elapsed() > Duration::from_secs(5)
            && crate::auto::hub_here(&a.cfg)
        {
            lock_try = std::time::Instant::now();
            lock = crate::auto::HubLock::try_take();
            if lock.is_some() {
                shared.publish(f.to_json(&a.cfg), true);
                crate::log::info(
                    "daemon",
                    "running the hub (auto-approve, queue, notifications)",
                );
            }
        }
        if f.changed_at != last {
            shared.publish(f.to_json(&a.cfg), lock.is_some());
        }
        if lock.is_none() {
            // another process runs the hub (and its notifications); closes
            // due while nobody does are claimed here (`closing.rs`)
            hub.closer.tick(&a.cfg, &f, false);
            if f.changed_at != last {
                last = f.changed_at;
                write_status(&f);
            }
            continue;
        }
        let t0 = std::time::Instant::now();
        hub.tick(&a.cfg, &f, &trans, &fresh);
        slow("the hub", t0);
        let t0 = std::time::Instant::now();
        autostart.tick(&a.cfg, &f);
        slow("autostart", t0);
        for t in trans {
            if fresh.contains(&t.session.host) {
                continue;
            }
            let Some(n) = note_for(&t, notify_done) else {
                continue;
            };
            // the router delivers it (its desktop sink); this is the log line
            log_note(&n);
            let _ = (notify, &mut failed);
        }
        if f.changed_at != last {
            last = f.changed_at;
            write_status(&f);
        }
    }
}

/// `hosts.refresh` reached the daemon: a machine was just set up. It joins
/// this fleet if the settings now have it (a machine added since the daemon
/// started), its herdr version is asked again, and it's polled now.
pub fn looked_again(f: &mut Fleet, h: &str) {
    let cfg = crate::config::Config::load();
    if f.host(h).is_none() && cfg.fleet().iter().any(|x| x == h) {
        f.add_host(&cfg, h);
    }
    if let Some(x) = f.host_mut(h) {
        x.herdr_info = None;
    }
    f.refresh(Some(h));
    crate::log::info(
        "daemon",
        &format!("{h} changed (set up) — looking at it again"),
    );
}

/// Take the daemon's socket, or say who has it. `Some(code)`: don't run.
/// Started by hand (a terminal), a second daemon stops; started as a
/// service, it waits for the socket to come free (so the service manager
/// doesn't restart it in a loop).
fn take_socket(shared: &std::sync::Arc<crate::daemon::Shared>, tty: bool) -> Option<ExitCode> {
    use crate::daemon::{bind, serve, Bind};
    let mut said = false;
    loop {
        match bind() {
            Bind::Ours(s) => {
                crate::log::info("daemon", &format!("API on {}", s.path().display()));
                serve(s, shared.clone());
                return None;
            }
            Bind::Taken(pid) => {
                let who = pid.map(|p| format!(" (pid {p})")).unwrap_or_default();
                let dash = crate::daemon::request("daemon.status", serde_json::json!({}))
                    .ok()
                    .and_then(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
                    .is_some_and(|k| k == "dashboard");
                if tty {
                    if dash {
                        crate::out::err(&format!(
                            "the dashboard{who} runs the hub here while it's open"
                        ));
                        crate::out::hint("close it first — or install the daemon as a service (aleph watch --install): it takes over when the dashboard closes");
                    } else {
                        crate::out::err(&format!("a daemon is already running here{who}"));
                        crate::out::hint("aleph daemon status — one daemon per computer runs the hub, notifications and the API");
                    }
                    return Some(ExitCode::from(1));
                }
                if !said {
                    said = true;
                    crate::log::warn(
                        "daemon",
                        &format!("another daemon runs here{who}; waiting for it to stop"),
                    );
                }
                std::thread::sleep(Duration::from_secs(5));
            }
            Bind::Failed(e) => {
                // carry on without the API rather than not watching at all
                crate::log::warn("daemon", &format!("no API socket: {e}"));
                warn(&format!("no API socket: {e}"));
                return None;
            }
        }
    }
}

// ------------------------------------------------------------------ autostart

/// Presets with `autostart = true`: started once per daemon run, as soon as
/// every machine they use answers. Only presets whose panes all have names
/// (launched with reuse, so a restart never starts them twice).
#[derive(Default)]
struct Autostart {
    done: Vec<String>,
    checked: Option<std::time::Instant>,
}

impl Autostart {
    fn tick(&mut self, cfg: &crate::config::Config, f: &crate::fleet::Fleet) {
        if self
            .checked
            .is_some_and(|t| t.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        self.checked = Some(std::time::Instant::now());
        let tags = crate::presets::fleet_tags(cfg);
        for p in crate::presets::all(cfg).into_iter().filter(|p| p.autostart) {
            if self.done.contains(&p.name) {
                continue;
            }
            if p.panes.iter().any(|x| x.name.is_none()) {
                crate::log::warn(
                    "autostart",
                    &format!("@{}: every pane needs a name to autostart (so a restart can't start it twice)", p.name),
                );
                self.done.push(p.name.clone());
                continue;
            }
            let plan = crate::presets::plan(&p, &tags, cfg.get("ssh_host").as_deref(), &[]);
            let up = plan.launches.iter().all(|l| {
                f.host(&l.host).is_some_and(|h| h.conn == Conn::Up)
                    || crate::sshcfg::is_local(&l.host)
            });
            if plan.launches.is_empty() || !up {
                continue;
            }
            self.done.push(p.name.clone());
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let (started, errors) = crate::presets::launch(&cfg, &plan, true, &mut |_| {});
                crate::log::info(
                    "autostart",
                    &format!(
                        "@{}: {} running, {} problem(s)",
                        p.name,
                        started.len(),
                        errors.len()
                    ),
                );
                for e in errors {
                    crate::log::warn("autostart", &format!("@{}: {e}", p.name));
                }
            });
        }
    }
}

// ------------------------------------------------------------------ upgrades

/// Notices when this process's own binary is replaced on disk.
pub(crate) struct Upgrade {
    exe: Option<std::path::PathBuf>,
    stamp: std::cell::Cell<Option<std::time::SystemTime>>,
    checked: std::cell::Cell<std::time::Instant>,
}

impl Upgrade {
    pub(crate) fn start() -> Upgrade {
        let exe = std::env::current_exe().ok();
        let stamp = exe.as_ref().and_then(|p| mtime(p));
        Upgrade {
            exe,
            stamp: std::cell::Cell::new(stamp),
            checked: std::cell::Cell::new(std::time::Instant::now()),
        }
    }

    /// Checked every ~10 s: the binary changed on disk and the new one runs
    /// (a half-written file must not replace us).
    pub(crate) fn replaced(&self) -> bool {
        if self.checked.get().elapsed() < Duration::from_secs(10) {
            return false;
        }
        self.checked.set(std::time::Instant::now());
        let (Some(exe), Some(old)) = (&self.exe, self.stamp.get()) else {
            return false;
        };
        match mtime(exe) {
            Some(new) if new != old => {}
            _ => return false,
        }
        crate::proc::run(
            std::process::Command::new(exe).arg("version"),
            Duration::from_secs(10),
            None,
        )
        .is_ok_and(|o| o.ok())
    }

    /// exec the new binary with the same arguments. Same pid, so launchd /
    /// systemd don't notice; returns only if that failed.
    fn reexec(&self) {
        let Some(exe) = &self.exe else { return };
        say(Line::from(Span::styled(
            "aleph was updated — restarting the watch on the new build",
            ui::muted(),
        )));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let e = std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .exec();
            warn(&format!("couldn't restart on the new build: {e}"));
            // don't retry this file every 10 s
            self.stamp.set(mtime(exe));
        }
    }
}

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

// ------------------------------------------------------------------ status.json

/// Same shape as `aleph status --json`.
pub fn counts(f: &Fleet) -> serde_json::Value {
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
            "blocked": h.count("blocked"), "working": h.count("working"),
        })).collect::<Vec<_>>(),
        "needs_you": all.iter().filter(|s| s.blocked()).map(|s| json!({"host": s.host, "name": s.name, "display": s.display(), "title": s.title})).collect::<Vec<_>>(),
        "generated_at": crate::cache::now(),
    })
}

fn write_status(f: &Fleet) {
    let _ = crate::config::write_atomic(
        &crate::config::world_cache().join("status.json"),
        counts(f).to_string().as_bytes(),
    );
}

// ------------------------------------------------------------------ notifications

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    NeedsYou,
    Finished,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub kind: Kind,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub host: String,
    pub name: String,
    pub pane: String,
    /// what a click runs (None: open a Warp tab attached to it)
    pub click: Option<String>,
}

/// Is this transition worth a notification?
pub fn note_for(t: &Transition, notify_done: bool) -> Option<Note> {
    let to = t.session.status.as_str();
    if to == "blocked" && t.from != "blocked" {
        return Some(note(&t.session, Kind::NeedsYou));
    }
    if notify_done && t.from == "working" && (to == "done" || to == "idle") {
        return Some(note(&t.session, Kind::Finished));
    }
    None
}

pub fn note(s: &Session, kind: Kind) -> Note {
    let name = if s.name.is_empty() {
        s.display().to_string()
    } else {
        s.name.clone()
    };
    let body = if !s.title.trim().is_empty() {
        s.title.trim().to_string()
    } else if !s.cwd.is_empty() {
        short_path(&s.cwd, "")
    } else {
        s.display().to_string()
    };
    Note {
        kind,
        title: match kind {
            Kind::NeedsYou => format!("{} needs you", crate::agents::label(&s.agent)),
            Kind::Finished => format!("{} finished", crate::agents::label(&s.agent)),
        },
        subtitle: format!("{} · {}", s.host, name),
        body,
        host: s.host.clone(),
        name,
        pane: s.pane_id.clone(),
        click: None,
    }
}

fn log_note(n: &Note) {
    let (g, col) = match n.kind {
        Kind::NeedsYou => ("▲", c::NEEDS()),
        Kind::Finished => ("✓", c::TEAL()),
    };
    say(Line::from(vec![
        Span::styled(format!("{g} "), ui::fg(col)),
        Span::styled(n.subtitle.clone(), ui::bold()),
        Span::styled(format!("  {}", n.body), ui::muted()),
    ]));
}

/// The shell command terminal-notifier runs on click: write a Warp tab
/// config attached to the session, then open it.
pub fn attach_command(n: &Note, warp_dir: &Path) -> String {
    let file = warp_dir.join("claude_attach.toml");
    let cfg = shortcuts::warp_tab(
        &format!("{} {}", n.title, n.subtitle),
        "claude_attach",
        &format!(
            "aleph --host {} resume {}",
            sh_quote(&n.host),
            sh_quote(&n.name)
        ),
    );
    let cfg = cfg.replace(
        "aleph shortcuts install rewrites this file; delete this line to keep your edits",
        "written when you click a aleph watch notification",
    );
    let lines: Vec<String> = cfg.lines().map(sh_quote).collect();
    format!(
        "mkdir -p {} && printf '%s\\n' {} > {} && open 'warp://tab_config/claude_attach'",
        sh_quote(&warp_dir.to_string_lossy()),
        lines.join(" "),
        sh_quote(&file.to_string_lossy())
    )
}

fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// (program, args) that show `n` on this platform.
pub fn delivery(
    n: &Note,
    os: &str,
    terminal_notifier: bool,
    warp_dir: &Path,
) -> Option<(String, Vec<String>)> {
    // showcase mode: the words people see masked (the click still goes to the real one)
    let masked;
    let n = if crate::showcase::on() {
        masked = Note {
            title: crate::showcase::text(&n.title).into_owned(),
            subtitle: crate::showcase::text(&n.subtitle).into_owned(),
            body: crate::showcase::text(&n.body).into_owned(),
            ..n.clone()
        };
        &masked
    } else {
        n
    };
    match os {
        "macos" if terminal_notifier => {
            let mut v = vec![
                "-title".into(),
                n.title.clone(),
                "-subtitle".into(),
                n.subtitle.clone(),
                "-message".into(),
                n.body.clone(),
                "-group".into(),
                format!("aleph-{}-{}", n.host, n.pane),
                "-execute".into(),
                n.click
                    .clone()
                    .unwrap_or_else(|| attach_command(n, warp_dir)),
            ];
            if let Some(icon) = app_icon() {
                v.extend(["-appIcon".into(), icon]);
            }
            if n.kind == Kind::NeedsYou {
                v.extend(["-sound".into(), "default".into()]);
            }
            Some(("terminal-notifier".into(), v))
        }
        "macos" => Some((
            "osascript".into(),
            vec![
                "-e".into(),
                format!(
                    "display notification {} with title {} subtitle {}",
                    applescript_str(&n.body),
                    applescript_str(&n.title),
                    applescript_str(&n.subtitle)
                ),
            ],
        )),
        "linux" => Some((
            "notify-send".into(),
            vec![
                "-a".into(),
                "aleph".into(),
                "-u".into(),
                if n.kind == Kind::NeedsYou {
                    "critical"
                } else {
                    "normal"
                }
                .into(),
                n.title.clone(),
                format!("{}\n{}", n.subtitle, n.body),
            ],
        )),
        _ => None,
    }
}

/// Aleph's icon for terminal-notifier (written once next to the cache).
fn app_icon() -> Option<String> {
    if cfg!(test) {
        return None;
    }
    let p = crate::config::cache_dir().join("aleph-128.png");
    if !p.exists() {
        let png: &[u8] = include_bytes!("../assets/brand/png/aleph-app-128.png");
        crate::config::write_atomic(&p, png).ok()?;
    }
    Some(p.to_string_lossy().into_owned())
}

/// What a click on a notification runs (`notify.desktop.click`): the session
/// in Aleph's Focus in a new terminal tab, Raycast's Aleph inbox at that
/// event (its launch context `{host, name, kind, t}`), or a Warp tab
/// attached in herdr.
pub fn click_command(click: &str, e: &crate::events::Event) -> Option<String> {
    let (host, name) = (e.host.as_str(), e.name.as_str());
    match click {
        "raycast" => {
            let ctx = json!({"host": host, "name": name, "kind": e.kind.as_str(), "t": e.t});
            // percent-encoded: nothing in it needs quoting from the shell
            Some(format!(
                "open 'raycast://extensions/aleph/aleph/inbox?context={}'",
                url_component(&ctx.to_string())
            ))
        }
        "warp" => None,
        _ => {
            let me = std::env::current_exe()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| crate::brand::BIN.into());
            let me = sh_quote(&me);
            Some(if name.is_empty() {
                format!("{me} open-tab -- {me} --host {}", sh_quote(host))
            } else {
                format!(
                    "{me} open-tab -- {me} focus {}",
                    sh_quote(&format!("{host}:{name}"))
                )
            })
        }
    }
}

/// `s` for a URL's query: every byte but `A–Z a–z 0–9 - . _ ~` as `%XX`.
fn url_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The router's desktop sink: an event as a notification (Aleph's title,
/// the machine and session under it; a click opens it in Focus).
/// Take a session's banner back from Notification Center (its question was
/// answered, it ended, or it got old): macOS with terminal-notifier only.
pub fn withdraw(host: &str, pane: &str) {
    if !cfg!(target_os = "macos") || cfg!(test) || pane.is_empty() {
        return;
    }
    let Some(tn) = proc::which("terminal-notifier") else {
        return;
    };
    let group = format!("aleph-{host}-{pane}");
    std::thread::spawn(move || {
        let _ = proc::run(
            std::process::Command::new(tn).args(["-remove", &group]),
            std::time::Duration::from_secs(5),
            None,
        );
    });
}

pub fn deliver_event(cfg: &crate::config::Config, e: &crate::events::Event) -> bool {
    use crate::events::Kind as K;
    let agent = crate::agents::label(&e.agent);
    let what = match e.kind {
        K::Blocked => format!("{agent} needs you"),
        K::Done => format!("{agent} finished"),
        K::Limit => format!("{agent} hit its usage limit"),
        K::Error => format!("{} is unreachable", e.host),
        K::HostUp => format!("{} is back", e.host),
        _ => crate::notify::title_of(e),
    };
    let n = Note {
        kind: if e.kind == K::Blocked {
            Kind::NeedsYou
        } else {
            Kind::Finished
        },
        title: format!("{} · {what}", crate::brand::NAME),
        subtitle: if e.name.is_empty() {
            e.host.clone()
        } else {
            format!("{} · {}", e.host, e.name)
        },
        body: if e.body.is_empty() {
            e.title.clone()
        } else {
            // showcase: the agent's words like its screen (`showcase.screens`)
            crate::showcase::screen_text(&e.body).into_owned()
        },
        host: e.host.clone(),
        name: e.name.clone(),
        pane: e.name.clone(),
        click: click_command(&cfg.get_or("notify.desktop.click"), e),
    };
    deliver(&n)
}

fn deliver(n: &Note) -> bool {
    let tn = proc::which("terminal-notifier").is_some();
    let Some((prog, args)) = delivery(n, std::env::consts::OS, tn, &shortcuts::warp_dir()) else {
        return false;
    };
    let o = proc::cap(&prog, &args, T_NOTIFY);
    if o.ok() || !tn {
        return o.ok();
    }
    // terminal-notifier is there but may not notify (macOS took its
    // permission: exit 3, "tccutil reset UserNotification …"): osascript's
    // notification instead, and the log says why once
    static SAID: std::sync::Once = std::sync::Once::new();
    SAID.call_once(|| {
        crate::log::warn(
            "notify",
            &format!(
                "terminal-notifier didn't notify (exit {:?}: {}) — using osascript; allow terminal-notifier in System Settings › Notifications to get its clickable banners back",
                o.code,
                o.stderr.trim().lines().last().unwrap_or("")
            ),
        )
    });
    let Some((prog, args)) = delivery(n, std::env::consts::OS, false, &shortcuts::warp_dir())
    else {
        return false;
    };
    proc::cap(&prog, &args, T_NOTIFY).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sess(status: &str) -> Session {
        Session {
            host: "b31".into(),
            pane_id: "w1:p2".into(),
            label: "codex_temper".into(),
            name: "temper".into(),
            agent: "codex".into(),
            status: status.into(),
            title: "Run the cargo benches".into(),
            cwd: "/home/selene/px".into(),
            ..Default::default()
        }
    }

    fn tr(from: &str, to: &str) -> Transition {
        Transition {
            session: sess(to),
            from: from.into(),
            took: 0,
        }
    }

    #[test]
    fn when_to_notify() {
        let n = note_for(&tr("working", "blocked"), false).unwrap();
        assert_eq!(n.kind, Kind::NeedsYou);
        assert_eq!(n.title, "Codex needs you");
        assert_eq!(n.subtitle, "b31 · temper");
        assert_eq!(n.body, "Run the cargo benches");
        assert!(
            note_for(&tr("", "blocked"), false).is_some(),
            "new session already blocked"
        );
        assert!(note_for(&tr("working", "done"), false).is_none());
        let d = note_for(&tr("working", "done"), true).unwrap();
        assert_eq!(
            (d.kind, d.title.as_str()),
            (Kind::Finished, "Codex finished")
        );
        assert!(note_for(&tr("working", "idle"), true).is_some());
        assert!(note_for(&tr("blocked", "idle"), true).is_none());
        assert!(note_for(&tr("idle", "working"), true).is_none());
        // no title → folder
        let mut s = sess("blocked");
        s.title.clear();
        assert_eq!(note(&s, Kind::NeedsYou).body, "/home/selene/px");
    }

    #[test]
    fn a_click_opens_raycast_at_the_event() {
        use crate::events::{Event, Kind as K};
        let e = Event {
            t: 1_792_670_000,
            host: "b31".into(),
            name: "it's ok".into(),
            kind: K::Blocked,
            ..Default::default()
        };
        let c = click_command("raycast", &e).unwrap();
        let q = c
            .strip_prefix("open 'raycast://extensions/aleph/aleph/inbox?context=")
            .and_then(|r| r.strip_suffix('\''))
            .unwrap();
        // nothing the shell (or a URL) would read
        assert!(
            q.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b)),
            "{q}"
        );
        let (b, mut out, mut i) = (q.as_bytes(), Vec::new(), 0);
        while i < b.len() {
            if b[i] == b'%' {
                out.push(u8::from_str_radix(&q[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v,
            json!({"host": "b31", "name": "it's ok", "kind": "blocked", "t": 1_792_670_000u64})
        );
        assert!(click_command("warp", &e).is_none());
        assert!(click_command("aleph", &e)
            .unwrap()
            .contains("focus 'b31:it'\\''s ok'"));
    }

    #[test]
    fn delivery_commands() {
        let n = note(&sess("blocked"), Kind::NeedsYou);
        let w = Path::new("/h/.warp/tab_configs");
        let (p, a) = delivery(&n, "macos", true, w).unwrap();
        assert_eq!(p, "terminal-notifier");
        let at = |k: &str| a[a.iter().position(|x| x == k).unwrap() + 1].clone();
        assert_eq!(at("-group"), "aleph-b31-w1:p2");
        assert_eq!(at("-title"), "Codex needs you");
        let ex = at("-execute");
        assert!(ex.contains("aleph --host b31 resume temper"), "{ex}");
        assert!(ex.ends_with(
            "> /h/.warp/tab_configs/claude_attach.toml && open 'warp://tab_config/claude_attach'"
        ));
        // the -execute command really writes a valid tab config
        if proc::which("sh").is_some() {
            let d = std::env::temp_dir().join(format!("chw-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            let cmd =
                attach_command(&n, &d).replace(" && open 'warp://tab_config/claude_attach'", "");
            let o = proc::cap("sh", &["-c", &cmd], Duration::from_secs(5));
            assert!(o.ok(), "{}", o.stderr);
            let t = std::fs::read_to_string(d.join("claude_attach.toml")).unwrap();
            assert!(
                t.contains("commands = [\"aleph --host b31 resume temper\"]"),
                "{t}"
            );
            assert!(t.contains(shortcuts::MARK));
            let _ = std::fs::remove_dir_all(&d);
        }
        let mut q = n.clone();
        q.body = r#"say "hi" \ now"#.into();
        let (p, a) = delivery(&q, "macos", false, w).unwrap();
        assert_eq!(p, "osascript");
        assert_eq!(
            a[1],
            r#"display notification "say \"hi\" \\ now" with title "Codex needs you" subtitle "b31 · temper""#
        );
        let (p, a) = delivery(&n, "linux", false, w).unwrap();
        assert_eq!(p, "notify-send");
        assert_eq!(a[a.len() - 2], "Codex needs you");
        assert_eq!(a[a.len() - 1], "b31 · temper\nRun the cargo benches");
        assert!(delivery(&n, "windows", false, w).is_none());
    }
}
