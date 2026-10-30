//! `aleph doctor` (and the Doctor view): binaries, shell wiring and the agent
//! the wrapper stands in for, the default host (resolves? key login? what the
//! server allows), every registry agent installed on it with its herdr
//! integration (one `probe` round trip), herdr's view of it, and the Warp /
//! Raycast shortcuts that point here.
//!
//! Checks stream out as `Line`s (section heads + labelled results) so both the
//! CLI and the dashboard can render them as they arrive.

use crate::agents::{self, Detect, Wraps};
use crate::config::{expand, home, Config, Target};
use crate::probe::{self, IState};
use crate::ui::{self, c};
use crate::{herdr, proc, sshcfg};
use ratatui::prelude::*;
use std::fs;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum St {
    Head,
    Ok,
    Warn,
    Bad,
    Info,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub st: St,
    /// short name of the check (section title for `Head`)
    pub label: String,
    pub text: String,
    pub fix: Vec<String>,
    /// what `f` (Settings › Health) and `aleph doctor --fix` can do about it
    pub action: Option<Fix>,
}

/// A fix Aleph can carry out itself (always after asking).
#[derive(Clone, Debug, PartialEq)]
pub enum Fix {
    /// copy your key to the machine (one password prompt, ssh's own)
    KeySetup(String),
    /// save the machine in herdr (offers to install herdr there first)
    MachineAdd(String),
    InstallAgent {
        agent: String,
        host: String,
    },
    /// `herdr integration install <name>` on the machine
    Integration {
        host: String,
        name: String,
    },
    /// colour herdr's sidebar by session (`aleph theme herdr --apply`)
    HerdrTheme,
    /// install the daemon as a service (`aleph daemon install`)
    DaemonInstall,
    /// restart the daemon through its service (`aleph daemon restart`)
    DaemonRestart,
    /// start herdr's server on the machine (in the background; restores the
    /// sessions Aleph remembers there when it had stopped with them)
    StartHerdr(String),
    /// give the machine its CPU temperature driver: install the package
    /// that has it (`package`, when this kernel lacks it), load it, keep it
    /// loaded at boot (`sudo` asks its own password, so it needs the terminal)
    CpuSensor {
        host: String,
        module: String,
        package: Option<String>,
    },
}

impl Fix {
    /// What it does, for the confirm.
    pub fn label(&self) -> String {
        match self {
            Fix::KeySetup(h) => format!("set up key login to {h} (ssh asks the password once)"),
            Fix::MachineAdd(h) => {
                format!("save {h} in herdr (installs herdr there if needed, after asking)")
            }
            Fix::InstallAgent { agent, host } => {
                format!("install {} on {host}", agents::label(agent))
            }
            Fix::Integration { host, name } => {
                format!("install herdr's {name} integration on {host}")
            }
            Fix::HerdrTheme => "colour herdr's sidebar by session (a backup is kept)".into(),
            Fix::DaemonInstall => {
                "install the daemon as a service (it runs at login and comes back if it stops)"
                    .into()
            }
            Fix::DaemonRestart => "restart the daemon".into(),
            Fix::StartHerdr(h) => format!("start herdr's server on {h} (in the background)"),
            Fix::CpuSensor {
                host,
                module,
                package: Some(pkg),
            } => format!(
                "install {pkg} on {host} and load {module} for CPU temperatures (sudo asks its password there)"
            ),
            Fix::CpuSensor { host, module, .. } => format!(
                "load {module} on {host} for CPU temperatures (sudo asks its password there)"
            ),
        }
    }

    /// The command a fix that needs the terminal runs over ssh (None: it
    /// has its own flow).
    pub fn remote_command(&self) -> Option<(String, String)> {
        match self {
            Fix::CpuSensor {
                host,
                module,
                package,
            } => Some((host.clone(), sensor_command(module, package.as_deref()))),
            _ => None,
        }
    }

    /// Carry out a fix that needs no terminal of its own (None: it does —
    /// key setup, saving a machine and installs run interactively).
    pub fn run_quiet(&self, cfg: &Config) -> Option<Result<String, String>> {
        // showcase's demo world fixes nothing of a machine (the daemon is real)
        if !matches!(self, Fix::DaemonInstall | Fix::DaemonRestart) {
            if let Some(e) = crate::showcase::refused() {
                return Some(Err(e));
            }
        }
        match self {
            Fix::Integration { host, name } => {
                let cmd = format!(
                    "{}; herdr integration install {} </dev/null 2>&1",
                    crate::chan::PATH_PRELUDE,
                    proc::sh_quote(name)
                );
                let o = proc::run(
                    &mut sshcfg::shell(host, &cmd),
                    Duration::from_secs(60),
                    None,
                );
                Some(match o {
                    Ok(o) if o.ok() => Ok(format!("{name} integration installed on {host}")),
                    Ok(o) => Err(format!(
                        "{host}: {}",
                        first_line(&format!("{}{}", o.text(), o.stderr))
                    )),
                    Err(e) => Err(format!("{host}: {e}")),
                })
            }
            Fix::HerdrTheme => Some(crate::herdrtheme::apply_sidebar(cfg)),
            Fix::DaemonInstall => Some(crate::daemonctl::install_quiet()),
            Fix::DaemonRestart => Some(crate::daemonctl::restart_quiet()),
            Fix::StartHerdr(h) => Some(start_herdr(cfg, h)),
            _ => None,
        }
    }
}

/// What gives a machine its CPU temperature driver: the package that has
/// it (when this kernel lacks it), the driver loaded now and at every boot.
pub fn sensor_command(module: &str, package: Option<&str>) -> String {
    let install = package
        .map(|p| format!("sudo apt-get install -y {p} && "))
        .unwrap_or_default();
    format!(
        "{install}sudo modprobe {module} && echo {module} | sudo tee /etc/modules-load.d/aleph-{module}.conf >/dev/null && echo \"{module} loaded, and at every boot\""
    )
}

/// `Fix::StartHerdr`: sessions Aleph remembers as lost there come back with
/// the server (`restore`); otherwise just the server.
pub fn start_herdr(cfg: &Config, h: &str) -> Result<String, String> {
    let lost = crate::restore::load()
        .iter()
        .any(|k| k.host == h && k.state == "lost");
    if lost {
        return crate::auto::heal_now(h).map(|m| format!("{h}: {m}"));
    }
    herdr::Herdr::new(&cfg.target_for(h, herdr::machine_for))
        .start_server()
        .map(|()| format!("herdr's server is running on {h}"))
}

/// Run a fix that needs the terminal over ssh (`ssh -t`, so `sudo` can ask).
pub fn run_remote(f: &Fix) -> Result<(), String> {
    if let Some(e) = crate::showcase::refused() {
        return Err(e);
    }
    let Some((host, cmd)) = f.remote_command() else {
        return Err("that fix has its own flow".into());
    };
    let mut args: Vec<String> = vec!["-t".into()];
    args.extend(sshcfg::interactive());
    args.push(host);
    args.push(cmd);
    let args: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_ref()).collect();
    match proc::interactive(&sshcfg::ssh_bin(), &args) {
        Ok(true) => Ok(()),
        Ok(false) => Err("it didn't work — see above".into()),
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Default, Debug, Clone, Copy)]
pub struct Summary {
    pub ok: usize,
    pub warn: usize,
    pub bad: usize,
}

struct Out<'a> {
    emit: &'a mut dyn FnMut(Line),
    sum: Summary,
}

impl Out<'_> {
    fn put(&mut self, st: St, label: &str, text: impl Into<String>, fix: &[String]) {
        self.put_fix(st, label, text, fix, None);
    }
    fn put_fix(
        &mut self,
        st: St,
        label: &str,
        text: impl Into<String>,
        fix: &[String],
        action: Option<Fix>,
    ) {
        match st {
            St::Ok => self.sum.ok += 1,
            St::Warn => self.sum.warn += 1,
            St::Bad => self.sum.bad += 1,
            _ => {}
        }
        (self.emit)(Line {
            st,
            label: label.into(),
            text: text.into(),
            fix: fix.to_vec(),
            action,
        });
    }
    fn head(&mut self, t: &str) {
        self.put(St::Head, t, "", &[]);
    }
    fn ok(&mut self, label: &str, t: impl Into<String>) {
        self.put(St::Ok, label, t, &[]);
    }
    fn info(&mut self, label: &str, t: impl Into<String>, fix: &[String]) {
        self.put(St::Info, label, t, fix);
    }
    fn warn(&mut self, label: &str, t: impl Into<String>, fix: &[String]) {
        self.put(St::Warn, label, t, fix);
    }
    fn bad(&mut self, label: &str, t: impl Into<String>, fix: &[String]) {
        self.put(St::Bad, label, t, fix);
    }
}

fn v(s: &[&str]) -> Vec<String> {
    s.iter().map(|x| x.to_string()).collect()
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// "2.1.299 (Claude Code)" → "2.1.299"; "herdr 0.9.5" → "0.9.5"
pub fn version(s: &str) -> String {
    let l = first_line(s);
    let l = l.split(" (").next().unwrap_or(&l).to_string();
    l.rsplit(' ').next().unwrap_or(&l).to_string()
}

pub fn tilde(p: &Path) -> String {
    let h = home();
    match p.strip_prefix(&h) {
        Ok(r) => format!("~/{}", r.display()),
        Err(_) => p.display().to_string(),
    }
}

/// System resolver with a deadline. None = couldn't check.
fn lookup(host: &str) -> Option<Result<String, String>> {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Some(Ok(host.into()));
    }
    let (tx, rx) = mpsc::channel();
    let h = host.to_string();
    std::thread::spawn(move || {
        let r = (h.as_str(), 22)
            .to_socket_addrs()
            .map_err(|e| e.to_string())
            .and_then(|mut it| {
                it.next()
                    .map(|a| a.ip().to_string())
                    .ok_or_else(|| "no addresses".into())
            });
        let _ = tx.send(r);
    });
    rx.recv_timeout(Duration::from_secs(8))
        .ok()
        .or(Some(Err("lookup timed out".into())))
}

/// The folder browser's freshness check needs `stat` (GNU or BSD).
const STAT_CHECK: &str = "if stat -c %Y / >/dev/null 2>&1 || stat -f %m / >/dev/null 2>&1; then printf 'CCS\\tstat\\tok\\n'; fi";

/// What's on a machine — herdr, every registry agent on the PATH (versions
/// asked in parallel, each capped at a few seconds), herdr's integrations and
/// the OS, in one `probe` round trip — plus whether `stat` works there.
pub fn remote(host: &str) -> (probe::HostProbe, bool) {
    let t0 = std::time::Instant::now();
    let script = format!("{}\n{STAT_CHECK}\n", probe::script());
    let (mut p, stat) = match probe::sh(host, &script, Duration::from_secs(45)) {
        Ok(o) => {
            let text = o.text();
            let mut p = probe::parse(host, &text);
            if p.error.is_some() && !o.ok() {
                p.error = Some(o.reason());
            }
            (p, text.lines().any(|l| l == "CCS\tstat\tok"))
        }
        Err(e) => (
            probe::HostProbe {
                host: host.to_string(),
                error: Some(e),
                ..Default::default()
            },
            false,
        ),
    };
    p.took = t0.elapsed();
    (p, stat)
}

/// The command people type for us in hints: always `aleph` (it works in
/// every shell and in scripts; `al` is the interactive shortcut).
pub fn me() -> &'static str {
    crate::brand::BIN
}

/// The first shell function the install block defines (`al() { … }` → `al`;
/// the pre-rename `# >>> claude-herdr >>>` block counts too).
pub fn wrapper_name(rc: &str) -> Option<String> {
    let mut on = false;
    for l in rc.lines() {
        match l.trim() {
            crate::brand::RC_BEGIN | crate::brand::LEGACY_RC_BEGIN => on = true,
            crate::brand::RC_END | crate::brand::LEGACY_RC_END => on = false,
            t if on => {
                if let Some((name, _)) = t.split_once("()") {
                    let name = name.trim_start_matches("function ").trim();
                    if !name.is_empty() && !name.contains(char::is_whitespace) {
                        return Some(name.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Every function the managed block defines (`al`, `cc`, `claude`, …).
pub fn rc_block_functions(rc: &str) -> Vec<String> {
    let mut on = false;
    let mut block = String::new();
    for l in rc.lines() {
        match l.trim() {
            crate::brand::RC_BEGIN | crate::brand::LEGACY_RC_BEGIN => on = true,
            crate::brand::RC_END | crate::brand::LEGACY_RC_END => on = false,
            t if on => {
                block.push_str(t);
                block.push('\n');
            }
            _ => {}
        }
    }
    crate::brand::rc_functions(&block)
}

pub fn warp_dirs() -> Vec<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"));
    [
        home().join(".warp/tab_configs"),
        home().join(".warp-preview/tab_configs"),
        data.join("warp-terminal/tab_configs"),
        data.join("warp-terminal-preview/tab_configs"),
    ]
    .into_iter()
    .filter(|d| d.is_dir())
    .collect()
}

fn tab_config_exists(name: &str) -> bool {
    let want = format!("{}.toml", name.to_lowercase());
    warp_dirs().iter().any(|d| {
        fs::read_dir(d)
            .map(|rd| {
                rd.filter_map(Result::ok)
                    .any(|e| e.file_name().to_string_lossy().to_lowercase() == want)
            })
            .unwrap_or(false)
    })
}

/// The invocations of this tool inside a file (tab config `commands`,
/// scripts): `aleph …` (or the pre-rename `claude-herdr …`), or a shell
/// function (whatever `names` holds).
fn our_cmds(text: &str, names: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('#') {
            continue;
        }
        for piece in l.split(['"', '\'', ';', '&', '|']) {
            let p = piece.trim();
            let words: Vec<&str> = p.split_whitespace().collect();
            if let Some(i) = words.iter().position(|w| {
                names.contains(w)
                    || w.ends_with("/aleph")
                    || *w == "aleph"
                    || *w == "al"
                    || w.ends_with("/claude-herdr")
                    || *w == "claude-herdr"
            }) {
                if words[..i].iter().all(|w| w.contains('=')) {
                    out.push(p.to_string());
                }
            }
        }
    }
    out
}

/// Host referenced by an invocation (`--host X`, `-H X`, CC_SSH_HOST=X, CC_MACHINE=X).
fn host_in(cmd: &str) -> Option<String> {
    let w: Vec<&str> = cmd.split_whitespace().collect();
    for (i, x) in w.iter().enumerate() {
        if (*x == "--host" || *x == "-H") && i + 1 < w.len() {
            return Some(w[i + 1].to_string());
        }
        for k in ["CC_SSH_HOST=", "CC_MACHINE="] {
            if let Some(h) = x.strip_prefix(k) {
                return Some(h.to_string());
            }
        }
    }
    None
}

fn app(names: &[&str]) -> Option<PathBuf> {
    names.iter().find_map(|n| {
        [PathBuf::from("/Applications"), home().join("Applications")]
            .into_iter()
            .map(|d| d.join(n))
            .find(|p| p.is_dir())
    })
}

fn no_host(o: &mut Out) {
    o.head("Host");
    o.bad(
        "default host",
        "not set",
        &v(&[
            "aleph host pick   — choose from ~/.ssh/config",
            "aleph host add <alias> <user@hostname>   — define a new one",
        ]),
    );
}

/// One part of a fleet-wide check (the dashboard runs them side by side).
#[derive(Clone, Debug)]
pub enum Part {
    Local,
    /// the daemon: its service, socket, hub, logs, API (`daemonctl.rs`)
    Daemon,
    Host(Target),
    /// no machines yet
    NoHost,
    Shortcuts,
}

/// The parts of a check of the whole fleet: this computer, its daemon, every
/// machine (`only`: just that one), then the shortcuts. `local` (this
/// computer's own herdr, e.g. `fleet.default = "local"`) is no ssh machine:
/// the first part checks it.
pub fn parts(cfg: &Config, only: Option<&str>) -> Vec<Part> {
    let mut v = vec![Part::Local, Part::Daemon];
    let remote = |h: &str| !sshcfg::is_local(h);
    match only {
        // `--host H`: that machine, in the fleet or not
        Some(h) if remote(h) => v.push(Part::Host(cfg.target_for(h, herdr::machine_for))),
        Some(_) => {}
        None => {
            for h in cfg.hosts().iter().filter(|h| remote(h)) {
                v.push(Part::Host(cfg.target_for(h, herdr::machine_for)));
            }
        }
    }
    if only.is_none() && cfg.hosts().is_empty() {
        v.push(Part::NoHost);
    }
    v.push(Part::Shortcuts);
    v
}

/// Run one part; its lines go to `emit`.
pub fn run_part(cfg: &Config, p: &Part, emit: &mut dyn FnMut(Line)) -> Summary {
    let mut o = Out {
        emit,
        sum: Summary::default(),
    };
    // showcase's demo world: its machines' checks
    if let Some(lines) = crate::showcase::doctor(p) {
        for l in lines {
            o.put_fix(l.st, &l.label, l.text, &l.fix, l.action);
        }
        return o.sum;
    }
    let aliases = sshcfg::aliases();
    match p {
        Part::Local => local(&mut o, cfg),
        Part::Daemon => {
            o.head("Daemon");
            for l in crate::daemonctl::doctor_lines(cfg) {
                o.put_fix(l.st, &l.label, l.text, &l.fix, l.action);
            }
        }
        Part::Host(t) => host(&mut o, cfg, t, &aliases),
        Part::NoHost => no_host(&mut o),
        Part::Shortcuts => shortcuts(&mut o, cfg, &aliases),
    }
    o.sum
}

/// Every part at once (a thread each); lines come out in part order, each
/// part as soon as it and the ones before it are done.
/// The report as `aleph doctor --json` prints it: `{ok, warn, fail,
/// sections: [{section, checks: [{status, label, text, fix}]}]}`;
/// `progress` hears the report so far after every part.
pub fn report_json(
    cfg: &Config,
    parts: &[Part],
    progress: &mut dyn FnMut(&serde_json::Value),
) -> (serde_json::Value, Summary) {
    let mut sections: Vec<serde_json::Value> = Vec::new();
    let mut sum = Summary::default();
    let sum_all = run_fleet(cfg, parts, &mut |l| {
        if l.st == St::Head {
            if !sections.is_empty() {
                progress(
                    &serde_json::json!({"ok": sum.ok, "warn": sum.warn, "fail": sum.bad, "sections": sections, "running": true}),
                );
            }
            sections.push(serde_json::json!({"section": l.label, "checks": []}));
        } else if let Some(s) = sections.last_mut() {
            let st = match l.st {
                St::Ok => {
                    sum.ok += 1;
                    "ok"
                }
                St::Warn => {
                    sum.warn += 1;
                    "warn"
                }
                St::Bad => {
                    sum.bad += 1;
                    "fail"
                }
                _ => "info",
            };
            if let Some(a) = s["checks"].as_array_mut() {
                // action: what `doctor --fix` (or f in the dashboard) would do for it
                a.push(
                    serde_json::json!({"status": st, "label": l.label, "text": l.text, "fix": l.fix,
                    "action": l.action.as_ref().map(Fix::label)}),
                );
            }
        }
    });
    (
        serde_json::json!({"ok": sum_all.ok, "warn": sum_all.warn, "fail": sum_all.bad, "sections": sections}),
        sum_all,
    )
}

pub fn run_fleet(cfg: &Config, parts: &[Part], emit: &mut dyn FnMut(Line)) -> Summary {
    let rx: Vec<mpsc::Receiver<(Vec<Line>, Summary)>> = parts
        .iter()
        .map(|p| {
            let (tx, rx) = mpsc::channel();
            let p = p.clone();
            std::thread::spawn(move || {
                let cfg = Config::load();
                let mut lines = Vec::new();
                let s = run_part(&cfg, &p, &mut |l| lines.push(l));
                let _ = tx.send((lines, s));
            });
            rx
        })
        .collect();
    let _ = cfg;
    let mut sum = Summary::default();
    for r in rx {
        let Ok((lines, s)) = r.recv() else { continue };
        for l in lines {
            emit(l);
        }
        sum.ok += s.ok;
        sum.warn += s.warn;
        sum.bad += s.bad;
    }
    sum
}

/// `aleph doctor --fix`: each fix, asked one by one (nothing runs unasked).
pub fn fix_all(cfg: &Config, fixes: &[Fix]) {
    use crate::out::*;
    if fixes.is_empty() {
        ok("nothing for --fix to do");
        return;
    }
    for f in fixes {
        let q = f.label();
        let mut q = q.chars();
        let q: String = q
            .next()
            .map(|c| c.to_uppercase().collect::<String>() + q.as_str())
            .unwrap_or_default();
        if !yes(&format!("{q}?")) {
            continue;
        }
        match f.run_quiet(cfg) {
            Some(Ok(m)) => ok(&m),
            Some(Err(e)) => err(&e),
            None => match f {
                Fix::KeySetup(h) => {
                    if let Err(e) = sshcfg::setup_key(h) {
                        err(&e);
                    }
                }
                Fix::MachineAdd(h) => {
                    let mut c = Config::load();
                    crate::onboard::run(&mut c, h, &herdr::machine_for(h));
                }
                Fix::InstallAgent { agent, host } => {
                    crate::agentcmd::install_flow(
                        cfg,
                        agent,
                        std::slice::from_ref(host),
                        crate::agentcmd::InstallOpts::default(),
                    );
                }
                f @ Fix::CpuSensor { .. } => {
                    if let Err(e) = run_remote(f) {
                        err(&e);
                    }
                }
                _ => {}
            },
        }
    }
}

/// What `agent explain --json` says about a pane, in a line.
pub fn explain_text(v: &serde_json::Value) -> String {
    let mut objs = Vec::new();
    fn walk<'a>(
        v: &'a serde_json::Value,
        out: &mut Vec<&'a serde_json::Map<String, serde_json::Value>>,
    ) {
        match v {
            serde_json::Value::Object(m) => {
                out.push(m);
                m.values().for_each(|x| walk(x, out));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    walk(v, &mut objs);
    let pick = |k: &str| {
        objs.iter()
            .find_map(|m| m.get(k).and_then(|x| x.as_str()).filter(|s| !s.is_empty()))
    };
    let state = pick("state")
        .or_else(|| pick("agent_status"))
        .unwrap_or("?");
    match pick("reason")
        .or_else(|| pick("explanation"))
        .or_else(|| pick("detail"))
    {
        Some(r) => format!("{state} — {r}"),
        None => state.to_string(),
    }
}

/// `aleph doctor --deep`: herdr's own view — every saved machine as herdr
/// sees it (`machine status`, which never prompts), and why herdr can't tell
/// what an agent is doing (`agent explain`) where it can't.
pub fn deep(parts: &[Part], emit: &mut dyn FnMut(Line)) -> Summary {
    let mut o = Out {
        emit,
        sum: Summary::default(),
    };
    o.head("herdr's own checks");
    let st = proc::cap(
        &herdr::bin(),
        &["machine", "status", "--json"],
        Duration::from_secs(30),
    );
    match serde_json::from_slice::<serde_json::Value>(&st.stdout) {
        Ok(serde_json::Value::Array(v)) => {
            for m in v {
                let label = m.get("label").and_then(|x| x.as_str()).unwrap_or("?");
                match m.get("status").and_then(|x| x.as_str()).unwrap_or("?") {
                    "reachable" => o.ok(label, "herdr reaches it (machine status)"),
                    s => {
                        let e = m
                            .get("error")
                            .and_then(|x| x.as_str())
                            .map(|e| format!(" — {e}"))
                            .unwrap_or_default();
                        o.warn(
                            label,
                            format!("herdr says {s}{e}"),
                            &[format!("herdr machine reconnect {label}")],
                        )
                    }
                }
            }
        }
        _ => o.info(
            "machine status",
            "herdr didn't answer it (herdr older than 0.9?)",
            &[],
        ),
    }
    for p in parts {
        let Part::Host(t) = p else { continue };
        let h = herdr::Herdr::new(t);
        let Ok(ss) = h.sessions() else { continue };
        for s in ss
            .iter()
            .filter(|s| matches!(s.status.as_str(), "unknown" | "?" | ""))
        {
            match h.explain(&s.pane_id) {
                Ok(v) => o.info(&format!("{}:{}", t.ssh, s.name), explain_text(&v), &[]),
                Err(e) => o.info(
                    &format!("{}:{}", t.ssh, s.name),
                    format!("explain: {e}"),
                    &[],
                ),
            }
        }
    }
    o.sum
}

/// The Settings section a check belongs to (its Health panel shows it).
pub fn section_for(head: &str, label: &str) -> &'static str {
    if head == "Daemon" {
        return "Daemon & API";
    }
    if head == "This Mac" || head == "This computer" {
        return match label {
            "hub" => "Daemon & API",
            "ssh" | "ssh-copy-id" => "Network",
            "herdr" => "Machines",
            _ => "Integrations",
        };
    }
    if head.starts_with("On ") {
        return if agents::ALL.iter().any(|a| a.name == label) || label == "agents" {
            "Agents"
        } else {
            "Machines"
        };
    }
    if head.starts_with("Host") || head.starts_with("herdr") {
        return "Machines";
    }
    if head == "Shortcuts" {
        return "Integrations";
    }
    "Advanced"
}

/// This computer: Aleph, the shell wiring, the wrapper, herdr, the hub, ssh.
fn local(o: &mut Out, cfg: &Config) {
    o.head(if cfg!(target_os = "macos") {
        "This Mac"
    } else {
        "This computer"
    });
    let me = std::env::current_exe()
        .map(|p| tilde(&p))
        .unwrap_or_else(|_| "?".into());
    o.ok("aleph", format!("{}  ·  {me}", env!("CARGO_PKG_VERSION")));
    let rc = std::env::var_os("ZDOTDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home)
        .join(".zshrc");
    let rc_text = fs::read_to_string(&rc).unwrap_or_default();
    let fname = wrapper_name(&rc_text);
    match &fname {
        _ if rc_text.contains("_cc_sessions") => o.warn(
            "zsh",
            format!("{} still has the old shell-function wrapper", tilde(&rc)),
            &v(&["rerun ./install.sh"]),
        ),
        Some(n) => o.ok("zsh", format!("{n} → aleph  ·  {}", tilde(&rc))),
        None => o.warn(
            "zsh",
            format!("no shell function runs aleph in {}", tilde(&rc)),
            &v(&["./install.sh   (adds `al`; --name <fn> picks another, --wrap-claude makes `claude` go through Aleph)"]),
        ),
    }
    // the opt-in wrapper (`claude() { ALEPH_WRAP=1 aleph …; }`): its agent's
    // own commands and calls inside herdr panes go to that agent
    let w = Wraps::from_cfg(cfg);
    let wrapper = rc_block_functions(&rc_text)
        .into_iter()
        .find(|n| crate::brand::is_agent_program(n));
    if let Some(fname) = wrapper {
        match w.path() {
            Some(p) => {
                let ver = proc::cap(
                    p.to_str().unwrap_or(&w.exe),
                    &["--version"],
                    Duration::from_secs(15),
                );
                o.ok(
                    &w.name(),
                    format!(
                        "{}  ·  {}  ·  `{fname} -p …` and calls inside herdr panes run it",
                        version(&ver.text()),
                        tilde(&p)
                    ),
                );
            }
            None => {
                let mut fix: Vec<String> = w.install_hint().map(String::from).into_iter().collect();
                fix.push(format!(
                "or: aleph settings set wraps <agent>   — the agent `{fname}` stands in for (now {})",
                w.exe
            ));
                o.warn(
                &w.name(),
                format!("not found on PATH — `{fname} -p …`, its own subcommands and calls inside herdr panes can't reach it"),
                &fix,
            );
            }
        }
    } else {
        o.info(
            "wrapper",
            "off — `claude` is Claude Code itself (opt in: ./install.sh --wrap-claude)",
            &[],
        );
    }
    match proc::which(&herdr::bin()) {
        Some(p) => {
            let ver = proc::cap(&herdr::bin(), &["--version"], Duration::from_secs(10));
            o.ok(
                "herdr",
                format!("{}  ·  {}", version(&ver.text()), tilde(&p)),
            );
        }
        None => o.bad(
            "herdr",
            "not found on PATH",
            &v(&["brew install herdr   (or: curl -fsSL https://herdr.dev/install.sh | sh)"]),
        ),
    }
    // running a herdr newer than Aleph knows: what it asks of it may have moved
    let status = proc::cap(
        &herdr::bin(),
        &["status", "server", "--json"],
        Duration::from_secs(10),
    );
    if let Some(p) = herdr::ServerStatus::parse(&status.text())
        .and_then(|s| s.protocol)
        .filter(|p| *p != herdr::TESTED_PROTOCOL)
    {
        o.warn(
            "herdr protocol",
            format!(
                "herdr speaks protocol {p}, and this Aleph was built for {}",
                herdr::TESTED_PROTOCOL
            ),
            &v(&["aleph self-update, or stay on the herdr you had until there's one for it"]),
        );
    }
    match proc::which(&sshcfg::ssh_bin()) {
        Some(_) => {
            let ver = proc::cap(&sshcfg::ssh_bin(), &["-V"], Duration::from_secs(5));
            o.ok("ssh", first_line(&format!("{}{}", ver.stderr, ver.text())));
        }
        None => o.bad("ssh", "not found on PATH", &[]),
    }
    if proc::which("ssh-copy-id").is_none() {
        o.info(
            "ssh-copy-id",
            "not found — `aleph host key` uses a built-in fallback",
            &[],
        );
    }
}

/// One machine: its ssh settings and login, herdr and the agents on it,
/// and herdr's view of it.
fn host(o: &mut Out, cfg: &Config, t: &Target, aliases: &[String]) {
    o.head(&format!("Host · {}", t.ssh));
    let mach = if t.machine != t.ssh {
        format!(" · herdr machine {}", t.machine)
    } else {
        String::new()
    };
    o.ok(
        "default",
        format!(
            "{}{mach}  ·  from {}",
            t.ssh,
            if t.source == "config" {
                tilde(&cfg.path)
            } else {
                t.source.to_string()
            }
        ),
    );
    if aliases.contains(&t.ssh) {
        o.ok(
            "ssh alias",
            format!("defined in {}", tilde(&sshcfg::user_config())),
        );
    } else {
        o.warn(
            "ssh alias",
            "not in your ssh config — used as a raw hostname",
            &[format!("aleph host add {} <user@hostname>", t.ssh)],
        );
    }
    let Some(res) = sshcfg::resolve(&t.ssh) else {
        o.bad(
            "ssh config",
            format!("ssh can't parse its settings for '{}'", t.ssh),
            &v(&["ssh -G <host>   shows the syntax error"]),
        );
        return;
    };
    o.ok(
        "connects to",
        format!(
            "{}{}",
            res.short(),
            res.proxy
                .as_ref()
                .map(|p| format!("  ·  via {p}"))
                .unwrap_or_default()
        ),
    );
    if res.proxy.is_none() {
        match lookup(&res.hostname) {
            Some(Ok(ip)) if ip != res.hostname => o.ok("dns", format!("{} → {ip}", res.hostname)),
            Some(Ok(_)) | None => {}
            Some(Err(e)) => o.bad(
                "dns",
                format!("{} doesn't resolve ({e})", res.hostname),
                &v(&["check HostName, your VPN, or /etc/hosts"]),
            ),
        }
    }
    let (reachable, keys_allowed) = match sshcfg::methods(&t.ssh) {
        Ok(m) => {
            let keys = m.iter().any(|x| x == "publickey" || x == "none");
            if keys {
                o.ok("auth methods", m.join(", "));
            } else {
                o.bad(
                    "auth methods",
                    format!("{} only — every connection would ask for a password", m.join(", ")),
                    &v(&[
                        "herdr and `aleph -i` need key login: enable PubkeyAuthentication in the server's sshd_config,",
                        "or reach it through a jump host that allows keys (ProxyJump). Stored passwords (sshpass) aren't supported — insecure.",
                    ]),
                );
            }
            (true, keys)
        }
        Err(p) => {
            net_fail(o, t, &res, &p);
            (false, false)
        }
    };
    let mut key_ok = false;
    if reachable && keys_allowed {
        let p = sshcfg::probe(&t.ssh);
        if p.ok() {
            key_ok = true;
            o.ok("key login", "works — no password prompts");
        } else if p.fail == Some(sshcfg::Fail::Auth) {
            o.put_fix(
                St::Bad,
                "key login",
                "no key accepted — you'd type a password every time",
                &[format!("aleph host key {}   — copies your key once (one password prompt), then never again", t.ssh)],
                Some(Fix::KeySetup(t.ssh.clone())),
            );
            keys(o, &res);
        } else {
            net_fail(o, t, &res, &p);
        }
    }

    // ------------------------------------------------------------ remote
    if key_ok {
        o.head(&format!("On {}", t.ssh));
        let (p, stat) = remote(&t.ssh);
        if let Some(e) = &p.error {
            o.warn("probe", format!("couldn't inspect the host: {e}"), &[]);
        } else {
            match &p.herdr {
                Some((hp, hv)) => o.ok("herdr", format!("{hv}  ·  {hp}")),
                None => o.put_fix(
                    St::Bad,
                    "herdr",
                    "not installed",
                    &[format!(
                        "herdr machine add {}   — offers to install it",
                        t.ssh
                    )],
                    Some(Fix::MachineAdd(t.ssh.clone())),
                ),
            }
            agent_rows(o, cfg, &t.ssh, &p);
            if let Some(m) = p.tool("moshi-hook") {
                let what = if p.moshi.is_empty() {
                    "installed, hooking no agent yet: Aleph sends everything".to_string()
                } else {
                    format!(
                        "tells the phone about {} itself; Aleph adds machine news (host.{}.moshi)",
                        p.moshi.join(", "),
                        t.ssh
                    )
                };
                o.info("moshi-hook", format!("{}  ·  {what}", m.path), &[]);
            }
            if cfg.get_or("attach_via") == "mosh" && p.tool("mosh-server").is_none() {
                o.warn(
                    "mosh",
                    "attach_via = mosh, but there's no mosh-server here — attaches use ssh",
                    &v(&["install mosh on the machine, or aleph settings set attach_via ssh"]),
                );
            }
            if !stat {
                o.warn(
                    "stat",
                    "missing — the folder browser refreshes by age only",
                    &[],
                );
            }
            if !p.os.is_empty() {
                o.info("system", p.os.clone(), &[]);
            }
            if let Some(why) = p.cpu_sensor() {
                let bmc = p.sensors.as_ref().is_some_and(|x| x.ipmi);
                let also = if bmc {
                    " (its BMC reads them too, but only as root)"
                } else {
                    ""
                };
                let (text, module, package) = match &why {
                    probe::CpuSensor::Load(m) => (
                        format!("no CPU sensor: the {m} driver is there but not loaded{also}"),
                        *m,
                        None,
                    ),
                    probe::CpuSensor::Install { module, package } => (
                        format!(
                            "no CPU sensor: this kernel's {module} driver comes in {package}, which isn't installed — lm-sensors only reads the drivers the kernel has{also}"
                        ),
                        *module,
                        Some(package.clone()),
                    ),
                    probe::CpuSensor::Absent(m) => (
                        format!("no CPU sensor: this kernel has no {m} driver{also}"),
                        *m,
                        None,
                    ),
                };
                if matches!(why, probe::CpuSensor::Absent(_)) {
                    o.info("CPU temperature", text, &[]);
                } else {
                    // the hint is the command itself (Raycast copies it); f runs it
                    o.put_fix(
                        St::Warn,
                        "CPU temperature",
                        text,
                        &[format!(
                            "ssh -t {} '{}'",
                            t.ssh,
                            sensor_command(module, package.as_deref())
                        )],
                        Some(Fix::CpuSensor {
                            host: t.ssh.clone(),
                            module: module.to_string(),
                            package,
                        }),
                    );
                }
            }
        }
    }

    // ------------------------------------------------------------ herdr link
    o.head(&format!("herdr · {}", t.ssh));
    if proc::which(&herdr::bin()).is_some() {
        if herdr::has_machine(&t.machine) {
            o.ok("machine", format!("'{}' is saved in herdr", t.machine));
            match herdr::Herdr::new(t).sessions() {
                Ok(s) if s.is_empty() => o.ok(
                    "session",
                    format!("running on {} · no agent sessions yet", t.ssh),
                ),
                Ok(s) => {
                    o.ok(
                        "session",
                        format!(
                            "running on {} · {} agent session{}",
                            t.ssh,
                            s.len(),
                            if s.len() == 1 { "" } else { "s" }
                        ),
                    );
                    o.info(
                        "sessions",
                        s.iter()
                            .map(|x| herdr::base_label(&x.label))
                            .collect::<Vec<_>>()
                            .join("  "),
                        &[],
                    );
                }
                Err(e) if crate::fleet::not_running(&e) => o.put_fix(
                    St::Warn,
                    "session",
                    "herdr's server isn't running there",
                    &v(&["f starts it (sessions Aleph remembers there come back too)"]),
                    Some(Fix::StartHerdr(t.ssh.clone())),
                ),
                Err(e) => o.warn("session", format!("not answering: {e}"), &[]),
            }
        } else {
            o.put_fix(
                St::Bad,
                "machine",
                format!("'{}' isn't saved in herdr", t.machine),
                &[
                    format!("herdr machine add {}", t.ssh),
                    format!("or: aleph host set {} — offers to do it", t.ssh),
                ],
                Some(Fix::MachineAdd(t.ssh.clone())),
            );
        }
    }
}

/// One row per registry agent installed on `host`, with its herdr
/// integration; a problem when the agent new sessions start isn't there.
fn agent_rows(o: &mut Out, cfg: &Config, host: &str, p: &probe::HostProbe) {
    let def = agents::default_agent_for(cfg, host);
    let installed = p.installed();
    for (a, f) in &installed {
        let mut text = format!("{}  ·  {}", f.version, f.path);
        if a.id == def.id {
            text.push_str("  ·  default");
        }
        let setup = format!("aleph --host {host} agents setup {}", a.id);
        let integ = a.integration().map(|name| Fix::Integration {
            host: host.to_string(),
            name: name.to_string(),
        });
        match a.integration().and_then(|n| p.integ(n)).map(|i| &i.state) {
            Some(IState::Current(v)) => o.ok(
                a.name,
                format!("{text}  ·  integration {v}").trim_end().to_string(),
            ),
            Some(IState::Outdated { have, want }) => o.put_fix(
                St::Warn,
                a.name,
                format!("{text}  ·  integration outdated ({have} → {want})"),
                &[setup],
                integ,
            ),
            // a warning where it matters (herdr is blind without it, or it's
            // the agent new sessions start); a note otherwise
            Some(IState::Missing) => {
                let blind = a.detect() == Detect::NeedsIntegration;
                let why = if blind {
                    "herdr can't see its state without it"
                } else {
                    "herdr restores its sessions with it"
                };
                let fix = [format!("{setup}   — {why}")];
                let text = format!("{text}  ·  no herdr integration");
                let st = if blind || a.id == def.id {
                    St::Warn
                } else {
                    St::Info
                };
                o.put_fix(st, a.name, text, &fix, integ)
            }
            // no integration for it in this herdr: `aleph restore` recreates its sessions
            _ => o.ok(a.name, text),
        }
    }
    if !installed.iter().any(|(a, _)| a.id == def.id) {
        let mut fix = vec![
            format!("aleph --host {host} agents install {}", def.id),
            def.install_on(host),
        ];
        if let Some((a, _)) = installed.first() {
            fix.push(format!("or: aleph settings set agent {}", a.id));
        }
        o.put_fix(
            St::Bad,
            def.name,
            "not installed — it's the default agent, so new sessions would fail",
            &fix,
            Some(Fix::InstallAgent {
                agent: def.id.to_string(),
                host: host.to_string(),
            }),
        );
    }
    if installed.is_empty() {
        o.info(
            "agents",
            "none of herdr's agents is on the PATH",
            &v(&["aleph agents --all lists them, with how to install each"]),
        );
    }
}

fn net_fail(o: &mut Out, t: &Target, res: &sshcfg::Resolved, p: &sshcfg::Probe) {
    use sshcfg::Fail::*;
    let fix: Vec<String> = match p.fail {
        Some(Dns) => v(&["check HostName in ~/.ssh/config, your VPN, or /etc/hosts"]),
        Some(Unreachable) | Some(Timeout) => {
            v(&["is the machine on, and on this network/VPN? is a firewall in the way?"])
        }
        Some(Refused) => vec![format!(
            "sshd isn't listening on {}:{} — wrong Port, or sshd stopped",
            res.hostname, res.port
        )],
        Some(HostKeyUnknown) => vec![format!(
            "ssh {}   — once, to check and trust its host key",
            t.ssh
        )],
        Some(HostKeyChanged) => vec![
            "the server's identity changed — confirm with whoever runs it before trusting it"
                .into(),
            format!("then: ssh-keygen -R {}", res.hostname),
        ],
        _ => vec![format!("ssh -v {}   for details", t.ssh)],
    };
    o.bad(
        "connection",
        format!("{} — {}", p.explain(), p.detail),
        &fix,
    );
}

fn keys(o: &mut Out, res: &sshcfg::Resolved) {
    let ids = sshcfg::identities(res);
    if ids.is_empty() {
        o.info("ssh key", "none found — `aleph host key` creates one", &[]);
        return;
    }
    let agent = sshcfg::agent();
    if let sshcfg::Agent::Keys(n) = agent {
        o.info(
            "ssh agent",
            format!("{n} key{} loaded", if n == 1 { "" } else { "s" }),
            &[],
        );
    }
    for k in ids {
        if sshcfg::loose_perms(&k) {
            o.bad(
                "ssh key",
                format!("{} is readable by others — ssh ignores it", tilde(&k)),
                &[format!("chmod 600 {}", tilde(&k))],
            );
        } else if sshcfg::encrypted(&k) && !matches!(agent, sshcfg::Agent::Keys(_)) {
            let add = if cfg!(target_os = "macos") {
                "ssh-add --apple-use-keychain"
            } else {
                "ssh-add"
            };
            o.warn(
                "ssh key",
                format!(
                    "{} has a passphrase and isn't loaded in the agent",
                    tilde(&k)
                ),
                &[format!("{add} {}", tilde(&k))],
            );
        } else {
            o.info(
                "ssh key",
                format!("{} — not authorized on the server yet?", tilde(&k)),
                &[],
            );
        }
    }
}

fn shortcuts(o: &mut Out, cfg: &Config, aliases: &[String]) {
    o.head("Shortcuts");
    // what the scripts call: aleph, `claude`, or the function's own name
    let rc = std::env::var_os("ZDOTDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(home)
        .join(".zshrc");
    let rc_funcs = rc_block_functions(&fs::read_to_string(rc).unwrap_or_default());
    let mut names = vec!["aleph", "al", "claude"];
    for n in &rc_funcs {
        names.push(n);
    }
    let known =
        |h: &str| sshcfg::is_local(h) || aliases.iter().any(|a| a == h) || herdr::has_machine(h);
    if cfg!(target_os = "macos") {
        match app(&["Warp.app", "WarpPreview.app"]) {
            Some(a) => {
                let plist = a.join("Contents/Info.plist");
                let x = proc::cap(
                    "plutil",
                    &[
                        std::ffi::OsStr::new("-convert"),
                        "xml1".as_ref(),
                        "-o".as_ref(),
                        "-".as_ref(),
                        plist.as_os_str(),
                    ],
                    Duration::from_secs(5),
                );
                let name = a.file_name().unwrap().to_string_lossy().to_string();
                if x.text().contains("<string>warp</string>")
                    || x.text().contains("<string>warppreview</string>")
                {
                    o.ok("Warp", format!("{name} handles warp:// links"));
                } else {
                    o.info(
                        "Warp",
                        format!("{name} found (couldn't confirm the warp:// scheme)"),
                        &[],
                    );
                }
            }
            None => o.info(
                "Warp",
                "not installed — tab-config shortcuts unavailable",
                &[],
            ),
        }
    }
    let mut n = 0;
    for d in warp_dirs() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = fs::read_to_string(&f) else {
                continue;
            };
            let cmds = our_cmds(&text, &names);
            if cmds.is_empty() {
                continue;
            }
            n += 1;
            let stem = f.file_stem().unwrap().to_string_lossy().to_string();
            for cmd in cmds {
                let what = format!("{stem} → {cmd}");
                match host_in(&cmd) {
                    Some(h) if !known(&h) => o.bad(
                        "tab config",
                        what,
                        &[format!("'{h}' isn't an ssh alias or herdr machine — fix {} or: aleph host add {h} <user@host>", tilde(&f))],
                    ),
                    _ if cmd.contains("aleph-browse") => o.warn("tab config", what, &v(&["aleph-browse was merged into aleph — use `aleph -i`"])),
                    _ => o.ok("tab config", what),
                }
            }
        }
    }
    if n == 0 {
        o.info(
            "tab config",
            "no Warp tab configs run aleph — see extras/warp/",
            &[],
        );
    }
    if cfg!(target_os = "macos") && app(&["Raycast.app"]).is_none() {
        o.info("Raycast", "not installed", &[]);
    }
    let mut r = 0;
    for d in cfg.raycast_dirs() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.is_file())
            .collect();
        files.sort();
        for f in files {
            let Ok(text) = fs::read_to_string(&f) else {
                continue;
            };
            if !text.contains("@raycast.") {
                continue;
            }
            let urls: Vec<String> = text
                .match_indices("warp://tab_config/")
                .map(|(i, m)| {
                    text[i + m.len()..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || "._-".contains(*c))
                        .collect()
                })
                .collect();
            let cmds = our_cmds(&text, &names);
            if urls.is_empty() && cmds.is_empty() {
                continue;
            }
            r += 1;
            let name = f.file_name().unwrap().to_string_lossy().to_string();
            use std::os::unix::fs::PermissionsExt;
            let exec = f
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            let missing: Vec<&str> = ["schemaVersion", "title", "mode"]
                .into_iter()
                .filter(|k| !text.contains(&format!("@raycast.{k}")))
                .collect();
            if !exec {
                o.bad(
                    "Raycast",
                    format!("{name} isn't executable"),
                    &[format!("chmod +x {}", tilde(&f))],
                );
            } else if !missing.is_empty() {
                o.bad(
                    "Raycast",
                    format!("{name} is missing @raycast.{}", missing.join(", @raycast.")),
                    &[],
                );
            } else if let Some(u) = urls.iter().find(|u| !tab_config_exists(u)) {
                o.bad(
                    "Raycast",
                    format!("{name} opens tab config '{u}', which doesn't exist"),
                    &v(&["create it in ~/.warp/tab_configs/ — see extras/warp/"]),
                );
            } else if let Some(h) = cmds.iter().filter_map(|c| host_in(c)).find(|h| !known(h)) {
                o.bad("Raycast", format!("{name} uses unknown host '{h}'"), &[]);
            } else {
                let what = urls
                    .first()
                    .map(|u| format!("tab config {u}"))
                    .or_else(|| cmds.first().cloned())
                    .unwrap_or_default();
                o.ok("Raycast", format!("{name} → {what}"));
            }
        }
    }
    if r == 0 {
        let dirs: Vec<String> = cfg.raycast_dirs().iter().map(|d| tilde(d)).collect();
        o.info(
            "Raycast",
            format!("no script commands in {}", dirs.join(", ")),
            &v(&["example: extras/raycast/claude-b31.sh  ·  other folders: raycast_dirs=… in the config"]),
        );
    }
    o.info(
        "quicklinks",
        "stored inside Raycast — can't be checked; Raycast rejects warp:// quicklinks",
        &[],
    );
    let hc = expand("~/.config/herdr/config.toml");
    if fs::read_to_string(&hc)
        .map(|s| s.contains("$mark"))
        .unwrap_or(false)
    {
        o.ok("sidebar", "herdr colours sessions by glyph");
    } else {
        o.put_fix(
            St::Info,
            "sidebar",
            "herdr sidebar colours not set up (optional)",
            &v(&["aleph theme herdr --apply   — adds them, keeping a backup"]),
            Some(Fix::HerdrTheme),
        );
    }
}

// ------------------------------------------------------------------ a check, in pieces

/// How a check reads at a glance: a problem, a warning, something optional
/// with a fix (a tip), a pass, or a plain note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fail,
    Warn,
    Tip,
    Ok,
    Note,
}

impl Kind {
    pub fn of(l: &Line) -> Kind {
        match l.st {
            St::Bad => Kind::Fail,
            St::Warn => Kind::Warn,
            St::Ok => Kind::Ok,
            _ if !l.fix.is_empty() || l.action.is_some() => Kind::Tip,
            _ => Kind::Note,
        }
    }
    /// Problems first, then warnings, then tips; passes and notes keep their order.
    pub fn rank(self) -> u8 {
        match self {
            Kind::Fail => 0,
            Kind::Warn => 1,
            Kind::Tip => 2,
            Kind::Ok | Kind::Note => 3,
        }
    }
    pub fn color(self) -> Color {
        match self {
            Kind::Fail => c::RED(),
            Kind::Warn => c::YELLOW(),
            Kind::Tip => c::BLUE(),
            Kind::Ok => c::GREEN(),
            Kind::Note => c::FAINT(),
        }
    }
    pub fn mark(self) -> (&'static str, Style) {
        let st = ui::fg(self.color());
        match self {
            Kind::Fail => ("✗", st.add_modifier(Modifier::BOLD)),
            Kind::Warn => ("!", st.add_modifier(Modifier::BOLD)),
            Kind::Tip => ("+", st.add_modifier(Modifier::BOLD)),
            Kind::Ok => ("✓", st),
            Kind::Note => ("·", st),
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            Kind::Fail => "problem",
            Kind::Warn => "warning",
            Kind::Tip => "tip",
            Kind::Ok => "passed",
            Kind::Note => "note",
        }
    }
}

/// How many checks of each kind.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub fail: usize,
    pub warn: usize,
    pub tip: usize,
    pub ok: usize,
    pub note: usize,
}

impl Counts {
    pub fn of<'a>(lines: impl IntoIterator<Item = &'a Line>) -> Counts {
        let mut n = Counts::default();
        for l in lines.into_iter().filter(|l| l.st != St::Head) {
            match Kind::of(l) {
                Kind::Fail => n.fail += 1,
                Kind::Warn => n.warn += 1,
                Kind::Tip => n.tip += 1,
                Kind::Ok => n.ok += 1,
                Kind::Note => n.note += 1,
            }
        }
        n
    }
    /// The worst kind there is (a pass when there's nothing wrong).
    pub fn worst(&self) -> Kind {
        if self.fail > 0 {
            Kind::Fail
        } else if self.warn > 0 {
            Kind::Warn
        } else {
            Kind::Ok
        }
    }
    /// `✗ 1  ! 1  + 2  ✓ 10`, each in its colour (only the kinds there are;
    /// a lone `✓ n` when all is well).
    pub fn spans(&self) -> Vec<Span<'static>> {
        let mut v = Vec::new();
        for (n, k) in [
            (self.fail, Kind::Fail),
            (self.warn, Kind::Warn),
            (self.tip, Kind::Tip),
            (self.ok, Kind::Ok),
        ] {
            if n == 0 {
                continue;
            }
            if !v.is_empty() {
                v.push(Span::raw("  "));
            }
            let (m, st) = k.mark();
            v.push(Span::styled(format!("{m} {n}"), st));
        }
        v
    }
}

/// This computer's group: `This Mac` on a Mac.
pub const LOCAL_GROUP: &str = if cfg!(target_os = "macos") {
    "This Mac"
} else {
    "This computer"
};

/// The machine (or part of Aleph) a section belongs to, and which part of it
/// that is: `Host · b31` → (`b31`, `ssh`), `On b31` → (`b31`, `tools`),
/// `herdr · b31` → (`b31`, `herdr`). The daemon and the shortcuts live on this
/// computer: `Daemon` → (`This Mac`, `daemon`), `Shortcuts` → (`This Mac`,
/// `shortcuts`).
pub fn group_of(head: &str) -> (&str, &'static str) {
    match head {
        "Daemon" => return (LOCAL_GROUP, "daemon"),
        "Shortcuts" => return (LOCAL_GROUP, "shortcuts"),
        _ => {}
    }
    if let Some(h) = head.strip_prefix("Host · ") {
        return (h, "ssh");
    }
    if let Some(h) = head.strip_prefix("On ") {
        return (h, "tools");
    }
    if let Some(h) = head.strip_prefix("herdr · ") {
        return (h, "herdr");
    }
    (head, "")
}

/// A check's part of its machine: `ssh`, `agents`, `tools`, `herdr`, or ``.
pub fn area_of(head: &str, label: &str) -> &'static str {
    match group_of(head).1 {
        "tools" if agents::ALL.iter().any(|a| a.name == label) || label == "agents" => "agents",
        a => a,
    }
}

/// What a check says, in pieces a row and a detail can lay out: a short
/// title, the key fact, its version and path, the rest, and why.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Brief {
    /// `Claude Code`, `Key login`, `DNS`, `herdr machine`
    pub title: String,
    /// `no herdr integration`, `works`, `gpu2.lan doesn't resolve` (may be empty)
    pub fact: String,
    pub version: Option<String>,
    /// with `~` for a home folder, here or on the machine
    pub path: Option<String>,
    /// the rest of what it said: `default`, `1 agent session`
    pub extra: Vec<String>,
    /// what came after ` — `, or a long aside in parentheses
    pub why: Option<String>,
}

fn is_version(s: &str) -> bool {
    let s = s.strip_prefix('v').unwrap_or(s);
    let head: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    head.contains('.')
        && head.split('.').all(|p| !p.is_empty())
        && s[head.len()..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-+._".contains(c))
}

fn is_path(s: &str) -> bool {
    (s.starts_with('/') || s.starts_with("~/") || s == "~") && !s.contains(char::is_whitespace)
}

/// A label as a row's title: `key login` → `Key login`, `dns` → `DNS`;
/// tool names (`aleph`, `herdr`, `ssh …`) stay as they're written.
pub fn title_of(head: &str, label: &str) -> String {
    // a folded-in part keeps its name before each of its checks: `Daemon · Service`
    if let a @ ("daemon" | "shortcuts") = group_of(head).1 {
        let inner = title_of("", label);
        return format!("{}{} · {inner}", a[..1].to_uppercase(), &a[1..]);
    }
    match (group_of(head).1, label) {
        ("herdr", "machine") => return "herdr machine".into(),
        ("herdr", "session") => return "herdr session".into(),
        (_, "dns") => return "DNS".into(),
        (_, "http api") => return "HTTP API".into(),
        _ => {}
    }
    let tool = ["aleph", "herdr", "zsh", "ssh", "mosh", "moshi", "stat"]
        .iter()
        .any(|t| {
            label == *t
                || label.starts_with(&format!("{t} "))
                || label.starts_with(&format!("{t}-"))
        })
        || label.contains('.');
    if tool {
        return label.to_string();
    }
    let mut ch = label.chars();
    match ch.next() {
        Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
        None => String::new(),
    }
}

/// Take a check apart (`2.1.309  ·  /home/u/.local/bin/claude  ·  default  ·
/// no herdr integration` → version, `~/.local/bin/claude`, `default`, and
/// the integration as the key fact).
pub fn brief(head: &str, l: &Line) -> Brief {
    let text = l.text.replace('`', "");
    let (main, mut why) = match text.split_once(" — ") {
        Some((a, b)) => (a.trim().to_string(), Some(b.trim().to_string())),
        None => (text.trim().to_string(), None),
    };
    let mut b = Brief {
        title: title_of(head, &l.label),
        ..Default::default()
    };
    let mut rest: Vec<String> = Vec::new();
    for part in main.split('·').map(str::trim).filter(|p| !p.is_empty()) {
        if b.version.is_none() && is_version(part) {
            b.version = Some(part.to_string());
        } else if b.path.is_none() && is_path(part) {
            b.path = Some(ui::tilde_path(part));
        } else {
            rest.push(part.to_string());
        }
    }
    // the key fact: an agent's integration, a service's state, else the first thing said
    let key = rest
        .iter()
        .position(|p| p.contains("integration"))
        .or_else(|| {
            rest.iter()
                .position(|p| p.starts_with("running") || p.starts_with("stopped"))
        })
        .unwrap_or(0);
    if key < rest.len() {
        b.fact = rest.remove(key);
    }
    // a long aside in parentheses is the why, not the fact
    if b.fact.ends_with(')') {
        if let Some(i) = b.fact.rfind(" (") {
            if b.fact.len() - i > 26 {
                let aside = b.fact[i + 2..b.fact.len() - 1].trim().to_string();
                b.fact.truncate(i);
                why = Some(match why {
                    Some(w) => format!("{aside} — {w}"),
                    None => aside,
                });
            }
        }
    }
    // an agent's `default` is the agent new sessions start
    if area_of(head, &l.label) == "agents" {
        for x in &mut rest {
            if x == "default" {
                *x = "default agent".into();
            }
        }
    }
    b.extra = rest;
    b.why = why.filter(|w| !w.is_empty());
    b
}

/// A fix's command and what it's for: `aleph agents setup codex   — herdr
/// restores its sessions with it` → (`aleph agents setup codex`, why).
pub fn fix_parts(f: &str) -> (String, Option<String>) {
    match f.split_once(" — ") {
        Some((cmd, why)) => (cmd.trim().to_string(), Some(why.trim().to_string())),
        None => (f.trim().to_string(), None),
    }
}

/// Every check in its machine's group: (group, [(head, line)]), groups in
/// the order they came, checks with problems first (the rest in order).
pub fn groups<'a>(
    lines: impl IntoIterator<Item = &'a Line>,
) -> Vec<(String, Vec<(&'a str, &'a Line)>)> {
    let mut out: Vec<(String, Vec<(&'a str, &'a Line)>)> = Vec::new();
    let mut head: &str = "";
    for l in lines {
        if l.st == St::Head {
            head = &l.label;
            let g = group_of(head).0;
            if !out.iter().any(|(n, _)| n == g) {
                out.push((g.to_string(), Vec::new()));
            }
            continue;
        }
        let g = group_of(head).0;
        match out.iter_mut().find(|(n, _)| n == g) {
            Some((_, v)) => v.push((head, l)),
            None => out.push((g.to_string(), vec![(head, l)])),
        }
    }
    for (_, v) in &mut out {
        v.sort_by_key(|(_, l)| Kind::of(l).rank());
    }
    out
}

/// One agent's checks across the machines: (agent, [(machine, head, line)]).
pub type AgentGroup<'a> = (String, Vec<(String, &'a str, &'a Line)>);

/// The agents' checks gathered from every machine: (agent, [(machine, head,
/// line)]), the agents with problems first, each one's machines problems first.
pub fn agent_groups<'a>(lines: impl IntoIterator<Item = &'a Line>) -> Vec<AgentGroup<'a>> {
    let mut out: Vec<AgentGroup<'a>> = Vec::new();
    for (machine, items) in groups(lines) {
        for (h, l) in items {
            if area_of(h, &l.label) != "agents" || l.label == "agents" {
                continue;
            }
            match out.iter_mut().find(|(n, _)| *n == l.label) {
                Some((_, v)) => v.push((machine.clone(), h, l)),
                None => out.push((l.label.clone(), vec![(machine.clone(), h, l)])),
            }
        }
    }
    for (_, v) in &mut out {
        v.sort_by_key(|(_, _, l)| Kind::of(l).rank());
    }
    out.sort_by_key(|(_, v)| Counts::of(v.iter().map(|(_, _, l)| *l)).worst().rank());
    out
}

/// A machine's group in a few words: where ssh goes, its system, its herdr.
pub fn group_facts(items: &[(&str, &Line)]) -> String {
    let find = |area: &str, label: &str| {
        items
            .iter()
            .find(|(h, l)| group_of(h).1 == area && l.label == label)
            .map(|(h, l)| brief(h, l))
    };
    let mut v = Vec::new();
    if let Some(b) = find("ssh", "connects to") {
        v.push(b.fact);
    }
    if let Some((_, l)) = items.iter().find(|(_, l)| l.label == "system") {
        v.push(l.text.clone());
    }
    if let Some(ver) = find("tools", "herdr").and_then(|b| b.version) {
        v.push(format!("herdr {ver}"));
    }
    v.retain(|s| !s.is_empty());
    v.join(" · ")
}

// ------------------------------------------------------------------ rendering

/// A group's head: `⏺ gpu2   tester@gpu2.lan:2222 · herdr 0.8.0   ✗ 1  ! 1  ✓ 10`.
pub fn head_line(name: &str, facts: &str, n: &Counts) -> ratatui::text::Line<'static> {
    let mut v = vec![
        Span::styled("⏺ ", ui::fg(n.worst().color())),
        Span::styled(name.to_string(), ui::bold()),
    ];
    if !facts.is_empty() {
        v.push(Span::styled(format!("   {facts}"), ui::faint()));
    }
    v.push(Span::raw("   "));
    v.extend(n.spans());
    ratatui::text::Line::from(v)
}

/// Rows for one section, flat and aligned (brew/uv style).
pub fn section_rows(items: &[Line], inner: usize) -> Vec<ratatui::text::Line<'static>> {
    let v: Vec<(&str, &Line)> = items.iter().map(|l| ("", l)).collect();
    rows(&v, inner)
}

/// Checks as aligned rows (brew/uv style): mark, title, the key fact, its
/// version and path; under a problem, why and `hint:` its fix.
///
/// `  ! Claude Code     no herdr integration · default    2.1.309  ~/.local/bin/claude`
pub fn rows(items: &[(&str, &Line)], inner: usize) -> Vec<ratatui::text::Line<'static>> {
    use ratatui::text::Line as L;
    let briefs: Vec<Brief> = items.iter().map(|(h, l)| brief(h, l)).collect();
    // one title width for every section so columns line up across the report
    let tw = 14;
    let indent = 2 + 2 + tw + 2;
    let vw = briefs
        .iter()
        .filter_map(|b| b.version.as_deref().map(ui::width))
        .max()
        .unwrap_or(0)
        .min(12);
    let room = inner.saturating_sub(indent);
    // the path goes when there's no room for it
    let pw = if room >= 70 { 30 } else { 0 };
    let fw = room
        .saturating_sub(if vw > 0 { vw + 2 } else { 0 } + if pw > 0 { pw + 2 } else { 0 })
        .max(12);
    let mut out = Vec::new();
    for ((_, l), b) in items.iter().zip(&briefs) {
        let k = Kind::of(l);
        let (m, ms) = k.mark();
        let fact_st = match k {
            Kind::Fail | Kind::Warn => ui::fg(k.color()),
            Kind::Note => ui::muted(),
            _ => Style::new(),
        };
        let mut fact = vec![Span::styled(b.fact.clone(), fact_st)];
        if !b.extra.is_empty() {
            fact.push(Span::styled(
                format!(
                    "{}{}",
                    if b.fact.is_empty() { "" } else { " · " },
                    b.extra.join(" · ")
                ),
                ui::faint(),
            ));
        }
        let mut fact = clip_spans(fact, fw);
        let used: usize = fact.iter().map(|s| ui::width(&s.content)).sum();
        let mut row = vec![
            Span::raw("  "),
            Span::styled(format!("{m} "), ms),
            Span::styled(
                ui::pad(&ui::trunc(&b.title, tw), tw),
                match k {
                    Kind::Fail | Kind::Warn => ui::bold(),
                    Kind::Note => ui::muted(),
                    _ => Style::new(),
                },
            ),
            Span::raw("  "),
        ];
        row.append(&mut fact);
        if vw > 0 || pw > 0 {
            row.push(Span::raw(" ".repeat(fw.saturating_sub(used))));
        }
        if vw > 0 {
            let ver = b.version.as_deref().unwrap_or("");
            row.push(Span::raw("  "));
            row.push(Span::styled(
                format!("{:>vw$}", ui::trunc(ver, vw)),
                ui::muted(),
            ));
        }
        if pw > 0 {
            if let Some(p) = &b.path {
                row.push(Span::raw("  "));
                row.push(Span::styled(ui::trunc_left(p, pw), ui::faint()));
            }
        }
        out.push(L::from(row));
        // why, under anything that isn't a plain pass
        if k != Kind::Ok {
            if let Some(w) = &b.why {
                for chunk in ui::wrap(w, room.max(12)) {
                    out.push(L::from(vec![
                        Span::raw(" ".repeat(indent)),
                        Span::styled(
                            chunk,
                            if k == Kind::Note {
                                ui::faint()
                            } else {
                                ui::muted()
                            },
                        ),
                    ]));
                }
            }
        }
        for f in &l.fix {
            for (i, chunk) in ui::wrap(f, room.saturating_sub(6).max(12))
                .into_iter()
                .enumerate()
            {
                out.push(L::from(vec![
                    Span::raw(" ".repeat(indent)),
                    Span::styled(
                        if i == 0 { "hint: " } else { "      " },
                        ui::fg(c::BLUE()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(chunk, Style::new()),
                ]));
            }
        }
    }
    out
}

/// Spans cut to `w` columns (the last one that fits gets an ellipsis).
pub fn clip_spans(spans: Vec<Span<'static>>, w: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut left = w;
    for s in spans {
        let sw = ui::width(&s.content);
        if sw <= left {
            left -= sw;
            out.push(s);
        } else {
            if left > 0 {
                out.push(Span::styled(ui::trunc(&s.content, left), s.style));
            }
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_invocations_and_hosts() {
        let toml = "name = \"x\"\n[[panes]]\ncommands = [\"CC_DIR=/srv aleph -i\"]\n# commands = [\"aleph ls\"]\n";
        let n = ["claude"];
        assert_eq!(our_cmds(toml, &n), vec!["CC_DIR=/srv aleph -i"]);
        assert_eq!(
            our_cmds("commands = [\"aleph --host gpu2 new\", \"npm test\"]", &n),
            vec!["aleph --host gpu2 new"]
        );
        assert!(our_cmds("commands = [\"echo claude\"]", &n).is_empty());
        // another function name, or the binary itself
        assert_eq!(
            our_cmds("commands = [\"cc -i\"]", &["claude", "cc"]),
            vec!["cc -i"]
        );
        assert_eq!(
            our_cmds("x = \"/u/bin/aleph ls\"", &n),
            vec!["/u/bin/aleph ls"]
        );
        assert_eq!(host_in("aleph --host gpu2 -i").as_deref(), Some("gpu2"));
        assert_eq!(host_in("CC_MACHINE=b31 claude").as_deref(), Some("b31"));
        assert_eq!(host_in("aleph -i"), None);
    }

    #[test]
    fn local_is_checked_as_this_computer_not_over_ssh() {
        let kinds = |v: Vec<Part>| {
            v.iter()
                .map(|p| match p {
                    Part::Local => "local",
                    Part::Daemon => "daemon",
                    Part::Host(_) => "host",
                    Part::NoHost => "nohost",
                    Part::Shortcuts => "shortcuts",
                })
                .collect::<Vec<_>>()
        };
        // `fleet.default = "local"`: no DNS, ssh or `herdr machine` checks
        let cfg = Config::in_memory(&["ssh_host=local"]);
        assert_eq!(kinds(parts(&cfg, None)), ["local", "daemon", "shortcuts"]);
        assert_eq!(
            kinds(parts(&cfg, Some("local"))),
            ["local", "daemon", "shortcuts"]
        );
        // no machines at all still says so
        let none = Config::in_memory(&[]);
        assert_eq!(
            kinds(parts(&none, None)),
            ["local", "daemon", "nohost", "shortcuts"]
        );
    }

    #[test]
    fn wrapper_function_name() {
        let rc = "export X=1\n# >>> aleph >>>\ncc() { \"/u/.cargo/bin/aleph\" \"$@\"; }\n[[ -o interactive ]] && source <(x)\n# <<< aleph <<<\n";
        assert_eq!(wrapper_name(rc).as_deref(), Some("cc"));
        let rc = "# >>> aleph >>>\nclaude() { \"/b/aleph\" \"$@\"; }\n# <<< aleph <<<\n";
        assert_eq!(wrapper_name(rc).as_deref(), Some("claude"));
        assert_eq!(wrapper_name("claude() { x; }\n"), None);
    }

    fn check(st: St, label: &str, text: &str) -> Line {
        Line {
            st,
            label: label.into(),
            text: text.into(),
            fix: vec![],
            action: None,
        }
    }

    #[test]
    fn a_check_in_pieces() {
        // an agent: the integration is the key fact, the path loses its home
        let b = brief(
            "On dex42",
            &check(
                St::Warn,
                "Claude Code",
                "2.1.309  ·  /home/selene/.local/bin/claude  ·  default  ·  no herdr integration",
            ),
        );
        assert_eq!(b.title, "Claude Code");
        assert_eq!(b.fact, "no herdr integration");
        assert_eq!(b.version.as_deref(), Some("2.1.309"));
        assert_eq!(b.path.as_deref(), Some("~/.local/bin/claude"));
        assert_eq!(b.extra, vec!["default agent"]);
        // a long aside in parentheses is the why
        let b = brief(
            "Host · gpu2",
            &check(
                St::Bad,
                "dns",
                "gpu2.lan doesn't resolve (failed to lookup address information: nodename nor servname provided)",
            ),
        );
        assert_eq!(
            (b.title.as_str(), b.fact.as_str()),
            ("DNS", "gpu2.lan doesn't resolve")
        );
        assert!(b.why.unwrap().starts_with("failed to lookup"));
        // what follows ` — ` is the why; no backticks in a row
        let b = brief(
            "This Mac",
            &check(
                St::Info,
                "wrapper",
                "off — `claude` is Claude Code itself (opt in: ./install.sh --wrap-claude)",
            ),
        );
        assert_eq!((b.title.as_str(), b.fact.as_str()), ("Wrapper", "off"));
        assert_eq!(
            b.why.as_deref(),
            Some("claude is Claude Code itself (opt in: ./install.sh --wrap-claude)")
        );
        // a service's state is its key fact; a short aside stays
        let b = brief(
            "Daemon",
            &check(
                St::Ok,
                "service",
                "launchd dev.aleph.daemon · running (pid 24075)",
            ),
        );
        assert_eq!(b.fact, "running (pid 24075)");
        assert_eq!(b.extra, vec!["launchd dev.aleph.daemon"]);
        // a version and a path only: no fact
        let b = brief(
            "This Mac",
            &check(St::Ok, "aleph", "2.0.0  ·  ~/.cargo/bin/aleph"),
        );
        assert_eq!((b.fact.as_str(), b.version.as_deref()), ("", Some("2.0.0")));
        // titles: herdr's view says it's herdr's; tools keep their names
        assert_eq!(
            brief(
                "herdr · b31",
                &check(St::Ok, "machine", "'b31' is saved in herdr")
            )
            .title,
            "herdr machine"
        );
        assert_eq!(title_of("On b31", "moshi-hook"), "moshi-hook");
        assert_eq!(title_of("Host · b31", "ssh alias"), "ssh alias");
        // the daemon's checks are This Mac's, named for it
        assert_eq!(title_of("Daemon", "hub.machine"), "Daemon · hub.machine");
        assert_eq!(title_of("Daemon", "http api"), "Daemon · HTTP API");
        assert_eq!(group_of("Shortcuts"), (LOCAL_GROUP, "shortcuts"));
        assert_eq!(title_of("Host · b31", "key login"), "Key login");
        assert_eq!(
            fix_parts(
                "aleph --host b31 agents setup codex   — herdr restores its sessions with it"
            ),
            (
                "aleph --host b31 agents setup codex".to_string(),
                Some("herdr restores its sessions with it".to_string())
            )
        );
        assert!(!is_version("OpenSSH_9.8p1") && is_version("0.163.0") && is_version("v1.2"));
    }

    #[test]
    fn a_machines_sections_are_one_group() {
        let mut codex = check(
            St::Info,
            "Codex",
            "0.46.0  ·  /home/u/.local/bin/codex  ·  no herdr integration",
        );
        codex.fix = vec!["aleph --host b31 agents setup codex".into()];
        let lines = vec![
            check(St::Head, "This Mac", ""),
            check(St::Ok, "aleph", "2.0.0"),
            check(St::Head, "Host · b31", ""),
            check(St::Ok, "connects to", "u@10.0.0.49"),
            check(St::Head, "On b31", ""),
            check(St::Ok, "herdr", "0.9.5  ·  /home/u/.local/bin/herdr"),
            codex,
            check(St::Warn, "Claude Code", "2.1.0  ·  no herdr integration"),
            check(St::Info, "system", "Linux x86_64"),
            check(St::Head, "herdr · b31", ""),
            check(St::Ok, "machine", "'b31' is saved in herdr"),
        ];
        let g = groups(&lines);
        assert_eq!(
            g.iter()
                .map(|(n, v)| (n.as_str(), v.len()))
                .collect::<Vec<_>>(),
            vec![("This Mac", 1), ("b31", 6)]
        );
        // problems first, then tips; the rest as they came
        assert_eq!(
            g[1].1
                .iter()
                .map(|(_, l)| l.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Claude Code",
                "Codex",
                "connects to",
                "herdr",
                "system",
                "machine"
            ]
        );
        assert_eq!(area_of(g[1].1[0].0, "Claude Code"), "agents");
        assert_eq!(
            group_facts(&g[1].1),
            "u@10.0.0.49 · Linux x86_64 · herdr 0.9.5"
        );
        let n = Counts::of(g[1].1.iter().map(|(_, l)| *l));
        assert_eq!((n.fail, n.warn, n.tip, n.ok, n.note), (0, 1, 1, 3, 1));
        assert_eq!(n.worst(), Kind::Warn);
    }

    #[test]
    fn versions() {
        assert_eq!(version("2.1.299 (Claude Code)"), "2.1.299");
        assert_eq!(version("herdr 0.9.5\n"), "0.9.5");
    }
}
