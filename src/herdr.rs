//! Talking to herdr through its CLI (`herdr --machine <m> …`, JSON responses).
//! Only herdr-managed agent sessions are ever listed or touched.
//!
//! herdr's JSON shapes are read defensively: we walk every object in a
//! response and pick fields by name, so wrapper/nesting changes don't break us.

use crate::agents::{Filter, Launch};
use crate::config::Target;
use crate::layouts::{self, Got, Leaf, Tree};
use crate::proc::{self, sh_quote};
use crate::{names, theme};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::process::Command;
use std::time::Duration;

const T_CALL: Duration = Duration::from_secs(30);
const T_LOCAL: Duration = Duration::from_secs(10);
const T_START: Duration = Duration::from_secs(45);

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Session {
    /// ssh alias of the machine (filled in by the fleet)
    pub host: String,
    pub pane_id: String,
    pub tab_id: String,
    pub workspace_id: String,
    pub terminal_id: String,
    /// tab label, e.g. `claude_ignite:ox_color_check` or `codex_tune`
    pub label: String,
    /// herdr agent name (`ignite`), or the name part of the label
    pub name: String,
    /// agent id (`agents::id_of`): claude, codex, opencode, …
    pub agent: String,
    pub status: String,
    pub cwd: String,
    /// the agent's terminal title — agents like Claude Code put their task summary here
    pub title: String,
    pub mark: String,
    pub hue: String,
    pub focused: bool,
    /// unix seconds when `status` last changed (filled in by the fleet)
    pub since: Option<u64>,
    /// stopped at a usage limit: when it resets ("" = it didn't say); filled
    /// in by the fleet (`limits.rs`)
    pub limit: Option<String>,
    /// herdr's `state_change_seq`: rises on every state change — a new
    /// question while still blocked too (0 = herdr didn't say)
    pub seq: u64,
    /// herdr's `completion_seq`: rises every time the agent finishes
    pub done_seq: u64,
    /// `interactive_ready`: the agent can take input right now
    pub ready: Option<bool>,
    /// `launch_pending`: herdr is still starting the agent in this pane
    pub launch_pending: bool,
    /// the agent's own conversation id (`agent_session.value`), for resuming it
    pub native_id: Option<String>,
    /// compacting its conversation, as its screen says (filled in by the fleet)
    pub compacting: bool,
    /// at rest because a compaction ended, not because a turn did
    pub compacted: bool,
    /// a pane with no agent in a tab Aleph didn't start for a shell (`shell.list`)
    pub plain: bool,
    /// herdr couldn't bring this pane back after a restart: why
    pub restore_error: Option<String>,
    /// what to call it when its only name is herdr's tab number (`name_the_bare`)
    pub shown: String,
}

impl Session {
    /// What we call it everywhere: the tab label without the folder suffix.
    /// What we call it everywhere: the tab label without the folder, and
    /// without the agent prefix for story names (`claude_smoldering-heron` →
    /// `smoldering-heron`: the creature says the agent already).
    pub fn display(&self) -> &str {
        if !self.shown.is_empty() {
            return &self.shown;
        }
        // (a pane whose tab has no label)
        if self.label.is_empty() {
            return &self.name;
        }
        let b = base_label(&self.label);
        match b.split_once('_') {
            Some((p, rest))
                if rest.contains('-')
                    && !rest.contains('_')
                    && (crate::agents::find(p).is_some() || p == crate::agents::SHELL) =>
            {
                rest
            }
            _ => b,
        }
    }
    pub fn color(&self) -> ratatui::style::Color {
        theme::session(&self.hue, &self.mark, &self.pane_id)
    }
    pub fn glyph(&self) -> &str {
        theme::glyph(&self.hue, &self.mark)
    }
    /// Stable id across the fleet.
    pub fn key(&self) -> String {
        format!("{}|{}", self.host, self.pane_id)
    }
    /// The state as the fleet tracks it: a blocked session's question counts
    /// separately (`blocked#<seq>`), so a second question is news too.
    pub fn state_key(&self) -> String {
        if self.status == "blocked" && self.seq > 0 {
            format!("blocked#{}", self.seq)
        } else {
            self.status.clone()
        }
    }
    pub fn blocked(&self) -> bool {
        self.status == "blocked"
    }
}

pub fn bin() -> String {
    // showcase's demo world: nothing real is ever reached
    if let Some(dead) = crate::showcase::dead_bin() {
        return dead;
    }
    crate::brand::env("HERDR_BIN").unwrap_or_else(|| "herdr".into())
}

/// `herdr status server` on this computer, for its named session when it
/// has one (`host.local.session`): the text, which says where the socket is.
pub fn local_status() -> String {
    let mut c = Command::new(bin());
    c.args(["status", "server"]);
    if let Some(s) = crate::chan::session(crate::sshcfg::LOCAL) {
        c.env("HERDR_SESSION", s);
    }
    proc::run(&mut c, Duration::from_secs(5), None)
        .map(|o| o.text())
        .unwrap_or_default()
}

/// Every JSON object in `v`, depth-first.
fn objects<'a>(v: &'a Value, out: &mut Vec<&'a Map<String, Value>>) {
    match v {
        Value::Object(m) => {
            out.push(m);
            m.values().for_each(|x| objects(x, out));
        }
        Value::Array(a) => a.iter().for_each(|x| objects(x, out)),
        _ => {}
    }
}

fn s(m: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| match m.get(*k) {
        Some(Value::String(v)) if !v.is_empty() => Some(v.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    })
}

/// herdr reports failures as JSON on stderr (or plain text); pull out the message.
/// What `herdr status server --json` says (fields we use; all optional).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ServerStatus {
    pub running: bool,
    /// the running server's version
    pub version: Option<String>,
    /// can this herdr client talk to that server (`None` when not running)
    pub compatible: Option<bool>,
    pub restart_needed: bool,
    /// the installed binary is newer than the running server
    pub binary_stale: bool,
}

impl ServerStatus {
    pub fn parse(text: &str) -> Option<ServerStatus> {
        let v: Value = serde_json::from_str(text.trim()).ok()?;
        v.get("status")?;
        Some(ServerStatus {
            running: v.get("running").and_then(Value::as_bool).unwrap_or(false),
            version: v.get("version").and_then(Value::as_str).map(String::from),
            compatible: v.get("compatible").and_then(Value::as_bool),
            restart_needed: v
                .get("restart_needed")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            binary_stale: v
                .get("server_binary_stale")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Separates the two JSON documents of a combined poll.
const SEP: &str = "@@aleph@@";

/// "herdr 0.9.5" → "0.9.5"
pub fn version_of(s: &str) -> String {
    s.lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_string()
}

fn err_text(o: &proc::Out) -> String {
    if o.timed_out {
        return "timed out talking to herdr".into();
    }
    for l in o.stderr.lines().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(l.trim()) {
            let mut objs = Vec::new();
            objects(&v, &mut objs);
            if let Some(m) = objs.iter().find_map(|m| s(m, &["message", "code"])) {
                return m;
            }
        }
    }
    o.reason()
}

/// The error code herdr put in its JSON error (`agent_not_ready`, …).
/// herdr refused an `agent …` call because no agent runs in that pane.
fn no_agent(o: &proc::Out) -> bool {
    err_code(o).as_deref() == Some("agent_not_found")
}

fn err_code(o: &proc::Out) -> Option<String> {
    o.stderr.lines().rev().find_map(|l| {
        let v: Value = serde_json::from_str(l.trim()).ok()?;
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        objs.iter().find_map(|m| s(m, &["code"]))
    })
}

/// A herdr workspace: a group of tabs on one machine.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub label: String,
    pub number: u64,
    pub tabs: u64,
    pub focused: bool,
    /// herdr's roll-up of its agents (working, blocked, idle, …)
    pub status: String,
}

/// A git worktree herdr knows about (`worktree list`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Worktree {
    pub branch: String,
    pub path: String,
    pub label: String,
    /// the workspace it's open in
    pub open_in: Option<String>,
    /// a linked worktree (not the main checkout)
    pub linked: bool,
}

/// `workspace list` → workspaces, in herdr's order.
pub fn parse_workspaces(v: &Value) -> Vec<Workspace> {
    let mut objs = Vec::new();
    objects(v, &mut objs);
    objs.iter()
        .filter_map(|m| {
            Some(Workspace {
                id: s(m, &["workspace_id"])?,
                label: s(m, &["label", "name"]).unwrap_or_default(),
                number: m.get("number").and_then(Value::as_u64).unwrap_or(0),
                tabs: m.get("tab_count").and_then(Value::as_u64).unwrap_or(0),
                focused: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
                status: s(m, &["agent_status"]).unwrap_or_default(),
            })
        })
        .collect()
}

/// `worktree list` → worktrees (the main checkout first, as herdr lists them).
pub fn parse_worktrees(v: &Value) -> Vec<Worktree> {
    let Some(a) = v.pointer("/result/worktrees").and_then(Value::as_array) else {
        return vec![];
    };
    a.iter()
        .filter_map(Value::as_object)
        .filter(|m| m.get("is_bare").and_then(Value::as_bool) != Some(true))
        .filter_map(|m| {
            Some(Worktree {
                path: s(m, &["path"])?,
                branch: s(m, &["branch"]).unwrap_or_default(),
                label: s(m, &["label"]).unwrap_or_default(),
                open_in: s(m, &["open_workspace_id"]),
                linked: m
                    .get("is_linked_worktree")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Where a start leaves its panes (`NewOpts::panes_out`).
pub type PanesOut = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// Half-second looks a typed command gets to show it has started.
const TYPED_CHECKS: usize = 8;

/// The line where a shell says it couldn't run `what`, if there is one.
fn couldnt_run<'a>(screen: &'a str, what: &str) -> Option<&'a str> {
    const SAID: [&str; 5] = [
        "not found",
        "no such file",
        "permission denied",
        "cannot execute",
        "unknown command",
    ];
    screen.lines().rev().map(str::trim).find(|l| {
        let low = l.to_lowercase();
        low.contains(&what.to_lowercase()) && SAID.iter().any(|s| low.contains(s))
    })
}

/// Where a new session goes and how it starts.
#[derive(Clone, Debug, Default)]
pub struct NewOpts {
    /// filled with the new tab's panes as soon as they exist, before the agent
    /// is up (the dashboard's starting row stands in for them till then)
    pub panes_out: Option<PanesOut>,
    pub dir: Option<Vec<u8>>,
    /// extra arguments for the agent
    pub args: Vec<String>,
    /// explicit name (validated by the caller)
    pub name: Option<String>,
    /// agent id; empty = the `agent` setting (`agents::default_agent`)
    pub agent: String,
    /// `Some(branch)` → new git worktree (empty branch = `<agent>/<name>`)
    pub worktree: Option<String>,
    /// first prompt
    pub prompt: Option<String>,
    /// number of agents tiled in the new tab (1 = just one)
    pub tile: usize,
    /// shell command run in each pane first (hook.pre_new)
    pub pre_hook: Option<String>,
    /// `story` | `verb` | `folder`
    pub naming: String,
    /// tab label prefix; empty = `<agent>_`
    pub prefix: String,
    /// names running elsewhere in the fleet + recently used ones
    pub live: Vec<String>,
    pub recent: Vec<String>,
    /// open the tab in this herdr workspace (else the active one)
    pub workspace: Option<String>,
    /// don't switch herdr's view to the new tab (a start from the dashboard)
    pub no_focus: bool,
    /// environment for the agent (a preset's `env`): `tab create --env`
    pub env: Vec<(String, String)>,
    /// the tab's exact shape (a preset's): agents start in its first `tile`
    /// panes, the rest stay shells (`new_tab` returns them); None = `tile`
    /// panes as an even grid
    pub layout: Option<Tree<Leaf>>,
}

/// Where `layout.apply` puts its tab.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// a new tab, in `workspace` (else the active one)
    New {
        label: String,
        workspace: Option<String>,
    },
    /// instead of this tab (herdr makes the new one, then closes it)
    Replace(String),
}

impl NewOpts {
    /// The agent to start: `agent`, or the configured default when it's empty.
    pub fn agent_id(&self) -> String {
        if self.agent.is_empty() {
            crate::agents::default_agent(&crate::config::Config::load())
                .id
                .to_string()
        } else {
            self.agent.clone()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Started {
    pub name: String,
    pub label: String,
    pub pane: String,
    pub hue: String,
    pub mark: String,
    pub dir: String,
    /// something the start couldn't do (the OpenRouter key left out: only
    /// herdr's CLI could open the tab), said on the first one
    pub note: Option<String>,
}

pub struct Herdr {
    pub machine: String,
    pub ssh: String,
}

impl Herdr {
    pub fn new(t: &Target) -> Herdr {
        Herdr {
            machine: t.machine.clone(),
            ssh: t.ssh.clone(),
        }
    }

    /// Run the machine's own herdr over our shared ssh connection (~0.1 s a
    /// call once the connection is up) instead of `herdr --machine` (a new
    /// ssh handshake every call). `herdr_via=machine` switches back.
    fn via_ssh(&self) -> bool {
        use std::sync::OnceLock;
        static VIA: OnceLock<String> = OnceLock::new();
        let via = VIA.get_or_init(|| {
            crate::brand::env("HERDR_VIA")
                .unwrap_or_else(|| crate::config::Config::load().get_or("herdr_via"))
        });
        via == "ssh" && !crate::sshcfg::is_local(&self.ssh)
    }

    /// `herdr <args…>` as one shell command line for the remote side.
    fn remote_line(&self, args: &[OsString]) -> Option<String> {
        let mut cmd = crate::chan::herdr_env(&self.ssh);
        cmd.push_str(&sh_quote(&crate::chan::herdr_path(&self.ssh)));
        for a in args {
            cmd.push(' ');
            cmd.push_str(&sh_quote(a.to_str()?));
        }
        Some(cmd)
    }

    fn raw(&self, args: &[OsString], timeout: Duration) -> Result<proc::Out, String> {
        // the demo world answers through its own seams, never here
        if let Some(e) = crate::showcase::not_here() {
            return Err(e);
        }
        if self.via_ssh() {
            if let Some(line) = self.remote_line(args) {
                let o = proc::run(&mut crate::sshcfg::shell(&self.ssh, &line), timeout, None)
                    .map_err(|e| format!("can't run ssh: {e}"))?;
                if o.code == Some(127) {
                    // the cached herdr path went away (reinstalled?): look again next time
                    crate::chan::forget(&self.ssh);
                }
                return Ok(o);
            }
        }
        let mut c = Command::new(bin());
        if !crate::sshcfg::is_local(&self.ssh) {
            c.arg("--machine").arg(&self.machine);
        } else if let Some(s) = crate::chan::session(&self.ssh) {
            // this computer's named session (herdr's own variable)
            c.env("HERDR_SESSION", s);
        }
        c.args(args);
        proc::run(&mut c, timeout, None).map_err(|e| format!("can't run herdr: {e}"))
    }

    /// `herdr status server --json` on the machine: the running server's
    /// version and whether this client can talk to it (`compatible`),
    /// whether it needs a restart, or runs an older binary than installed.
    pub fn server_status(&self) -> Option<ServerStatus> {
        if let Some(s) = crate::showcase::server_status(&self.ssh) {
            return s;
        }
        let o = self
            .raw(
                &["status".into(), "server".into(), "--json".into()],
                T_LOCAL,
            )
            .ok()?;
        ServerStatus::parse(&o.text())
    }

    /// The machine's herdr version (`0.9.5`), best effort.
    pub fn version(&self) -> Option<String> {
        if let Some(v) = crate::showcase::herdr_version(&self.ssh) {
            return v;
        }
        if !self.via_ssh() {
            return None;
        }
        let o = self.raw(&["--version".into()], T_LOCAL).ok()?;
        o.ok()
            .then(|| version_of(&o.text()))
            .filter(|v| !v.is_empty())
    }

    fn call(&self, args: &[OsString], timeout: Duration) -> Result<Value, String> {
        let o = self.raw(args, timeout)?;
        if !o.ok() {
            return Err(err_text(&o));
        }
        serde_json::from_slice(&o.stdout).or(Ok(Value::Null))
    }

    fn call_s(&self, args: &[&str]) -> Result<Value, String> {
        let a: Vec<OsString> = args.iter().map(|x| x.into()).collect();
        self.call(&a, T_CALL)
    }

    /// `agent …` on a pane; a pane with no agent in it (a plain shell, or
    /// one whose agent exited) is refused with `agent_not_found`, and gets
    /// `pane` instead (a pane-level call that does the same).
    fn agent_or_pane(
        &self,
        agent: &[&str],
        pane: &mut dyn FnMut() -> Result<Value, String>,
    ) -> Result<Value, String> {
        let a: Vec<OsString> = agent.iter().map(|x| x.into()).collect();
        let o = self.raw(&a, T_CALL)?;
        if o.ok() {
            return serde_json::from_slice(&o.stdout).or(Ok(Value::Null));
        }
        if no_agent(&o) {
            return pane();
        }
        Err(err_text(&o))
    }

    /// Why herdr sees a pane's agent the way it does (`agent explain --json`).
    pub fn explain(&self, pane: &str) -> Result<Value, String> {
        self.call_s(&["agent", "explain", pane, "--json"])
    }

    /// Is the machine a Mac (`uname -s` is Darwin)? Asked once per machine.
    pub fn remote_is_mac(&self) -> bool {
        static SEEN: std::sync::Mutex<Vec<(String, bool)>> = std::sync::Mutex::new(Vec::new());
        if let Some((_, m)) = SEEN.lock().unwrap().iter().find(|(h, _)| *h == self.ssh) {
            return *m;
        }
        if crate::showcase::demo() {
            return false;
        }
        let o = proc::run(
            &mut crate::sshcfg::shell(&self.ssh, "uname -s"),
            Duration::from_secs(10),
            None,
        );
        let Ok(o) = o else { return false };
        if !o.ok() {
            return false;
        }
        let m = o.text().trim() == "Darwin";
        SEEN.lock().unwrap().push((self.ssh.clone(), m));
        m
    }

    /// Split `root` and start `agent` (as `name`) in the new pane — a
    /// preset's pane beside one that already ran. Returns the new pane.
    #[allow(clippy::too_many_arguments)]
    pub fn start_in_split(
        &self,
        root: &str,
        dir: &str,
        cwd: Option<&[u8]>,
        env: &[(String, String)],
        name: &str,
        agent: &str,
        args: &[String],
    ) -> Result<String, String> {
        let pane = self.split_env(root, dir, 0.5, cwd, env)?;
        self.start_in_pane(&pane, name, agent, args)?;
        Ok(pane)
    }

    /// Start `agent` (as `name`) in an idle shell pane — the second and
    /// later panes of a preset's tab.
    pub fn start_in_pane(
        &self,
        pane: &str,
        name: &str,
        agent: &str,
        args: &[String],
    ) -> Result<(), String> {
        if agent != crate::agents::SHELL {
            self.start_agent(name, agent, pane, args)?;
        }
        let _ = self.report_tokens(pane, &[("name", name)]);
        Ok(())
    }

    /// The machine's herdr workspaces (groups of tabs).
    pub fn workspaces(&self) -> Result<Vec<Workspace>, String> {
        if let Some(r) = crate::showcase::workspaces(&self.ssh) {
            return r;
        }
        self.call_s(&["workspace", "list"])
            .map(|v| parse_workspaces(&v))
    }

    /// Git worktrees of the repository open in `workspace` (else the focused one).
    pub fn worktrees(&self, workspace: Option<&str>) -> Result<Vec<Worktree>, String> {
        if let Some(r) = crate::showcase::worktrees(&self.ssh) {
            return r;
        }
        let mut a = vec!["worktree", "list"];
        if let Some(w) = workspace {
            a.extend(["--workspace", w]);
        }
        self.call_s(&a).map(|v| parse_worktrees(&v))
    }

    pub fn server_up(&self) -> Result<(), String> {
        if let Some(r) = crate::showcase::herdr_ok(&self.ssh) {
            return r;
        }
        self.call_s(&["workspace", "list"]).map(|_| ())
    }

    pub fn tab_labels(&self) -> Vec<String> {
        if let Some(v) = crate::showcase::tab_labels(&self.ssh) {
            return v;
        }
        let Ok(v) = self.call_s(&["tab", "list"]) else {
            return vec![];
        };
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        objs.iter()
            .filter(|m| !m.contains_key("pane_id"))
            .filter_map(|m| s(m, &["label", "name"]))
            .collect()
    }

    /// Agent sessions on this machine, for the agents `f` lists.
    /// Err = herdr unreachable / not running.
    pub fn sessions_matching(&self, f: &Filter) -> Result<Vec<Session>, String> {
        if let Some(r) = crate::showcase::sessions(&self.ssh, f) {
            return r;
        }
        let (agents, tabs) = self.snapshot()?;
        let mut v = parse_sessions(&agents, &tabs, f);
        for x in v.iter_mut() {
            x.host = self.ssh.clone();
        }
        Ok(v)
    }

    /// Agents and tabs in one call: `herdr api snapshot` (0.9+), else
    /// `agent list` + `tab list` (one ssh round trip either way). The
    /// snapshot's `panes` go with the tabs (they'd look like agents to the
    /// defensive parser): a plain shell's tab has a pane and no agent.
    fn snapshot(&self) -> Result<(Value, Value), String> {
        let split = |v: &Value| -> Option<(Value, Value)> {
            let s = v.pointer("/result/snapshot")?;
            Some((
                serde_json::json!({ "agents": s.get("agents")?.clone() }),
                serde_json::json!({
                    "tabs": s.get("tabs").cloned().unwrap_or(Value::Null),
                    "panes": s.get("panes").cloned().unwrap_or(Value::Null),
                }),
            ))
        };
        if let Some(e) = crate::showcase::not_here() {
            return Err(e);
        }
        if self.via_ssh() {
            let h = sh_quote(&crate::chan::herdr_path(&self.ssh));
            let line = format!(
                "{}{h} api snapshot 2>/dev/null || {{ {h} agent list && printf '\\n{SEP}\\n' && {h} tab list; }}",
                crate::chan::herdr_env(&self.ssh)
            );
            let o = proc::run(&mut crate::sshcfg::shell(&self.ssh, &line), T_CALL, None)
                .map_err(|e| format!("can't run ssh: {e}"))?;
            if !o.ok() {
                if o.code == Some(127) {
                    crate::chan::forget(&self.ssh);
                }
                return Err(err_text(&o));
            }
            let text = o.text();
            if let Some(x) = serde_json::from_str::<Value>(text.trim())
                .ok()
                .as_ref()
                .and_then(split)
            {
                return Ok(x);
            }
            if let Some((a, t)) = text.split_once(SEP) {
                return Ok((
                    serde_json::from_str(a.trim()).unwrap_or(Value::Null),
                    serde_json::from_str(t.trim()).unwrap_or(Value::Null),
                ));
            }
            // a snapshot without agents (a stub server): ask the old way
        } else if let Some(x) = self
            .call_s(&["api", "snapshot"])
            .ok()
            .as_ref()
            .and_then(split)
        {
            return Ok(x);
        }
        let agents = self.call_s(&["agent", "list"])?;
        let tabs = self.call_s(&["tab", "list"]).unwrap_or(Value::Null);
        Ok((agents, tabs))
    }

    /// Every agent session on this machine, whatever the agent.
    pub fn sessions(&self) -> Result<Vec<Session>, String> {
        self.sessions_matching(&Filter::All)
    }

    /// Terminal text of a pane. `source`: visible | recent | detection.
    pub fn read(
        &self,
        pane: &str,
        source: &str,
        lines: usize,
        ansi: bool,
    ) -> Result<String, String> {
        if let Some(r) = crate::showcase::read(&self.ssh, pane, source, lines, ansi) {
            return r;
        }
        let mut a: Vec<OsString> = ["pane", "read", pane, "--source", source]
            .iter()
            .map(OsString::from)
            .collect();
        if lines > 0 {
            a.push("--lines".into());
            a.push(lines.to_string().into());
        }
        if ansi {
            a.push("--ansi".into());
        }
        let o = self.raw(&a, T_CALL)?;
        if !o.ok() {
            return Err(err_text(&o));
        }
        Ok(o.text())
    }

    /// The last `lines` of several panes' text in one round trip (one ssh
    /// call running a short sh loop); panes that can't be read are left out.
    pub fn read_many(&self, panes: &[String], lines: usize) -> HashMap<String, String> {
        const MARK: &str = "@@CCH-PANE ";
        let mut out = HashMap::new();
        if panes.is_empty() {
            return out;
        }
        if crate::showcase::demo() {
            for p in panes {
                if let Some(Ok(t)) = crate::showcase::read(&self.ssh, p, "recent", lines, false) {
                    out.insert(p.clone(), t);
                }
            }
            return out;
        }
        let one = |p: &str| -> Vec<OsString> {
            ["pane", "read", p, "--source", "recent", "--lines"]
                .iter()
                .map(OsString::from)
                .chain([OsString::from(lines.to_string())])
                .collect()
        };
        if self.via_ssh() || crate::sshcfg::is_local(&self.ssh) {
            let herdr = if crate::sshcfg::is_local(&self.ssh) {
                sh_quote(&bin())
            } else {
                sh_quote(&crate::chan::herdr_path(&self.ssh))
            };
            let list: Vec<String> = panes.iter().map(|p| sh_quote(p)).collect();
            let line = format!(
                "{}for p in {}; do printf '\\n{MARK}%s\\n' \"$p\"; {herdr} pane read \"$p\" --source recent --lines {lines} 2>/dev/null; done",
                crate::chan::herdr_env(&self.ssh),
                list.join(" ")
            );
            let Ok(o) = proc::run(&mut crate::sshcfg::shell(&self.ssh, &line), T_CALL, None) else {
                return out;
            };
            let text = o.text();
            for chunk in text.split(MARK).skip(1) {
                let (pane, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
                out.insert(pane.trim().to_string(), body.to_string());
            }
            return out;
        }
        for p in panes {
            if let Ok(o) = self.raw(&one(p), T_CALL) {
                if o.ok() {
                    out.insert(p.clone(), o.text());
                }
            }
        }
        out
    }

    /// Recent terminal text of a pane (ANSI stripped) — the session preview.
    pub fn read_pane(&self, pane: &str, lines: usize) -> Result<String, String> {
        self.read(pane, "recent", lines, false)
    }

    pub fn focus(&self, pane: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::herdr_ok(&self.ssh) {
            return r;
        }
        self.agent_or_pane(&["agent", "focus", pane], &mut || self.focus_pane(pane))
            .map(|_| ())
    }

    /// How far a pane is scrolled back into its history (`pane get`'s
    /// `scroll.offset_from_bottom`; 0 at the bottom). None: herdr didn't say.
    pub fn scroll_offset(&self, pane: &str) -> Option<u64> {
        self.scroll_metrics(pane).map(|m| m.0)
    }

    /// Where a pane's history view is: (rows up from the bottom, the most
    /// it can go up). The most is 0 on an agent's alternate screen, or one
    /// with no history.
    pub fn scroll_metrics(&self, pane: &str) -> Option<(u64, u64)> {
        let v = self.call_s(&["pane", "get", pane]).ok()?;
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        objs.iter().find_map(|m| {
            let off = m.get("offset_from_bottom").and_then(Value::as_u64)?;
            let max = m
                .get("max_offset_from_bottom")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            Some((off, max))
        })
    }

    /// Focus a pane that holds no agent: herdr's socket focuses the pane
    /// itself; without one, its tab (`pane get` says which).
    fn focus_pane(&self, pane: &str) -> Result<Value, String> {
        if crate::sock::call(
            &self.ssh,
            "pane.focus",
            serde_json::json!({ "pane_id": pane }),
        )
        .is_ok()
        {
            return Ok(Value::Null);
        }
        let v = self.call_s(&["pane", "get", pane])?;
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        let tab = objs
            .iter()
            .find_map(|m| s(m, &["tab_id"]))
            .ok_or_else(|| format!("herdr didn't say which tab {pane} is in"))?;
        self.call_s(&["tab", "focus", &tab])
    }

    /// Submit a prompt to an agent (text + Enter, bracketed-paste aware).
    /// Wait until the agent is in one of `until` (herdr's default: idle, done
    /// or blocked); its state then. `timeout` None: as long as it takes.
    pub fn wait(
        &self,
        target: &str,
        until: &[String],
        timeout: Option<Duration>,
    ) -> Result<String, String> {
        if let Some(r) = crate::showcase::wait(&self.ssh, target, until, timeout) {
            return r;
        }
        let mut a: Vec<OsString> = vec!["agent".into(), "wait".into(), target.into()];
        for u in until {
            a.push("--until".into());
            a.push(u.into());
        }
        if let Some(t) = timeout {
            a.push("--timeout".into());
            a.push(t.as_millis().to_string().into());
        }
        let deadline = timeout.unwrap_or(Duration::from_secs(24 * 3600)) + Duration::from_secs(10);
        let v = self.call(&a, deadline)?;
        Ok(state_of(&v))
    }

    /// `prompt`, then wait for what it does (herdr's `--wait`).
    pub fn prompt_wait(
        &self,
        target: &str,
        text: &str,
        until: &[String],
        timeout: Option<Duration>,
    ) -> Result<String, String> {
        if let Some(r) = crate::showcase::send(&self.ssh, target, text, true) {
            r?;
            crate::sentlog::record(&self.ssh, target, "", text);
            return self.wait(target, until, timeout);
        }
        let sent_at = crate::cache::now();
        let mut a: Vec<OsString> = vec![
            "agent".into(),
            "prompt".into(),
            target.into(),
            text.into(),
            "--wait".into(),
        ];
        for u in until {
            a.push("--until".into());
            a.push(u.into());
        }
        if let Some(t) = timeout {
            a.push("--timeout".into());
            a.push(t.as_millis().to_string().into());
        }
        let deadline = timeout.unwrap_or(Duration::from_secs(24 * 3600)) + Duration::from_secs(10);
        let o = self.raw(&a, deadline)?;
        // a plain shell: typed in, then enter — nothing to wait for
        if no_agent(&o) {
            self.prompt(target, text)?;
            return Ok("idle".into());
        }
        if !o.ok() {
            if err_code(&o).as_deref() == Some("agent_blocked") {
                return Err(
                    "it's waiting on a question — answer with keys (1, 2, enter, esc)".into(),
                );
            }
            return Err(err_text(&o));
        }
        // the journal's record of what was asked (`sentlog.rs`)
        crate::sentlog::record_at(sent_at, &self.ssh, target, "", text);
        Ok(serde_json::from_slice::<Value>(&o.stdout)
            .map(|v| state_of(&v))
            .unwrap_or_default())
    }

    pub fn prompt(&self, target: &str, text: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::send(&self.ssh, target, text, true) {
            if r.is_ok() {
                crate::sentlog::record(&self.ssh, target, "", text);
            }
            return r;
        }
        let a: Vec<OsString> = ["agent", "prompt", target, text]
            .iter()
            .map(OsString::from)
            .collect();
        let o = self.raw(&a, T_CALL)?;
        // a plain shell (no agent there): typed in, then enter
        let o = if no_agent(&o) {
            self.raw(&["pane", "run", target, text].map(OsString::from), T_CALL)?
        } else {
            o
        };
        if o.ok() {
            // the journal's record of what was asked (`sentlog.rs`)
            crate::sentlog::record(&self.ssh, target, "", text);
            return Ok(());
        }
        // herdr refuses prompts while the agent shows an approval UI
        if err_code(&o).as_deref() == Some("agent_blocked") {
            return Err("it's waiting on a question — answer with keys (1, 2, enter, esc)".into());
        }
        Err(err_text(&o))
    }

    /// Logical keys (`1`, `enter`, `esc`, `ctrl+c`, `up`, …).
    pub fn send_keys(&self, target: &str, keys: &[&str]) -> Result<(), String> {
        if let Some(r) = crate::showcase::keys(&self.ssh, target, keys) {
            return r;
        }
        let mut a = vec!["agent", "send-keys", target];
        a.extend_from_slice(keys);
        let mut p = vec!["pane", "send-keys", target];
        p.extend_from_slice(keys);
        self.agent_or_pane(&a, &mut || self.call_s(&p)).map(|_| ())
    }

    pub fn send_text(&self, pane: &str, text: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::send(&self.ssh, pane, text, false) {
            return r;
        }
        self.call_s(&["pane", "send-text", pane, text]).map(|_| ())
    }

    /// Answer the agent's question with its "yes" option (see `approve_keys`).
    /// Returns the keys sent.
    pub fn approve(&self, pane: &str) -> Result<Vec<String>, String> {
        let q = self.read(pane, "detection", 0, false)?;
        let keys = approve_keys(&q)
            .ok_or("no clear yes option in the question — answer it with keys (K) or attach")?;
        let k: Vec<&str> = keys.iter().map(String::as_str).collect();
        self.send_keys(pane, &k)?;
        Ok(keys)
    }

    pub fn rename_tab(&self, tab: &str, label: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::rename(&self.ssh, tab, label, true) {
            return r;
        }
        self.call_s(&["tab", "rename", tab, label]).map(|_| ())
    }

    pub fn rename_agent(&self, target: &str, name: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::rename(&self.ssh, target, name, false) {
            return r;
        }
        self.call_s(&["agent", "rename", target, name]).map(|_| ())
    }

    /// Remove the git worktree checkout behind `workspace` (never forced:
    /// herdr refuses when it has changes).
    pub fn remove_worktree(&self, workspace: &str) -> Result<(), String> {
        if workspace.is_empty() {
            return Err("herdr didn't say which workspace it's in".into());
        }
        self.call_s(&["worktree", "remove", "--workspace", workspace])
            .map(|_| ())
    }

    /// Is this pane back at its shell's prompt, nothing else running in it?
    /// (herdr's `pane.process_info`: the foreground process group is the shell's.)
    pub fn at_prompt(&self, pane: &str) -> Result<bool, String> {
        let v = self.call_s(&["pane", "process-info", "--pane", pane])?;
        Ok(prompt_in(&v))
    }

    /// What each pane has in the foreground, in one round trip: a command, or
    /// `None` at its shell's prompt. A pane herdr didn't answer for is left out.
    pub fn foreground_many(&self, panes: &[String]) -> HashMap<String, Option<String>> {
        const MARK: &str = "@@CCH-PANE ";
        let mut out = HashMap::new();
        if panes.is_empty() || crate::showcase::demo() {
            return out;
        }
        if self.via_ssh() || crate::sshcfg::is_local(&self.ssh) {
            let herdr = if crate::sshcfg::is_local(&self.ssh) {
                sh_quote(&bin())
            } else {
                sh_quote(&crate::chan::herdr_path(&self.ssh))
            };
            let list: Vec<String> = panes.iter().map(|p| sh_quote(p)).collect();
            let line = format!(
                "{}for p in {}; do printf '\\n{MARK}%s\\n' \"$p\"; {herdr} pane process-info --pane \"$p\" 2>/dev/null; done",
                crate::chan::herdr_env(&self.ssh),
                list.join(" ")
            );
            let Ok(o) = proc::run(&mut crate::sshcfg::shell(&self.ssh, &line), T_CALL, None) else {
                return out;
            };
            let text = o.text();
            for chunk in text.split(MARK).skip(1) {
                let (pane, body) = chunk.split_once('\n').unwrap_or((chunk, ""));
                let v: Value = serde_json::from_str(body.trim()).unwrap_or(Value::Null);
                if let Some(fg) = foreground_of(&v) {
                    out.insert(pane.trim().to_string(), fg);
                }
            }
            return out;
        }
        for p in panes {
            if let Ok(v) = self.call_s(&["pane", "process-info", "--pane", p]) {
                if let Some(fg) = foreground_of(&v) {
                    out.insert(p.clone(), fg);
                }
            }
        }
        out
    }

    pub fn close_pane(&self, pane: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::close(&self.ssh, pane) {
            return r;
        }
        self.call_s(&["pane", "close", pane]).map(|_| ())
    }

    pub fn close_tab(&self, tab: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::close(&self.ssh, tab) {
            return r;
        }
        self.call_s(&["tab", "close", tab]).map(|_| ())
    }

    pub fn report_tokens(&self, pane: &str, tokens: &[(&str, &str)]) -> Result<(), String> {
        if let Some(r) = crate::showcase::herdr_ok(&self.ssh) {
            return r;
        }
        let mut a: Vec<String> = vec![
            "pane".into(),
            "report-metadata".into(),
            pane.into(),
            "--source".into(),
            crate::brand::HERDR_SOURCE.into(),
        ];
        for (k, v) in tokens {
            a.push("--token".into());
            a.push(format!("{k}={v}"));
        }
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        self.call_s(&a).map(|_| ())
    }

    /// Split `pane`; returns the new pane id. `ratio` is the share kept by `pane`.
    pub fn split(
        &self,
        pane: &str,
        dir: &str,
        ratio: f32,
        cwd: Option<&[u8]>,
    ) -> Result<String, String> {
        self.split_env(pane, dir, ratio, cwd, &[])
    }

    /// `split`, with environment variables for what starts in the new pane.
    pub fn split_env(
        &self,
        pane: &str,
        dir: &str,
        ratio: f32,
        cwd: Option<&[u8]>,
        env: &[(String, String)],
    ) -> Result<String, String> {
        let a = split_args(pane, dir, ratio, cwd, env);
        let v = self.call(&a, T_CALL)?;
        v.pointer("/result/pane/pane_id")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| new_pane_of(&v, Some(pane)))
            .ok_or_else(|| "herdr didn't return the new pane".into())
    }

    /// Move `pane` next to `target` in `tab` (native tiling), keeping
    /// `ratio` of the space for `target` (an older herdr without `--ratio`
    /// gets an even split). Returns the pane's id now.
    pub fn move_pane_ratio(
        &self,
        pane: &str,
        tab: &str,
        dir: &str,
        target: Option<&str>,
        ratio: Option<f32>,
    ) -> Result<String, String> {
        let mut a: Vec<String> = ["pane", "move", pane, "--tab", tab, "--split", dir]
            .iter()
            .map(|x| x.to_string())
            .collect();
        if let Some(t) = target {
            a.extend(["--target-pane".into(), t.to_string()]);
        }
        let plain = a.clone();
        if let Some(r) = ratio {
            a.extend(["--ratio".into(), format!("{r:.3}")]);
        }
        let run = |a: &[String]| self.call_s(&a.iter().map(String::as_str).collect::<Vec<_>>());
        let v = match run(&a) {
            Err(e) if ratio.is_some() && e.to_lowercase().contains("ratio") => run(&plain)?,
            r => r?,
        };
        Ok(v.pointer("/result/move_result/pane/pane_id")
            .and_then(Value::as_str)
            .unwrap_or(pane)
            .to_string())
    }

    /// Move `pane` into a fresh tab; returns (tab id, pane id).
    pub fn move_to_new_tab(&self, pane: &str, label: &str) -> Result<(String, String), String> {
        let v = self.call_s(&[
            "pane",
            "move",
            pane,
            "--new-tab",
            "--label",
            label,
            "--focus",
        ])?;
        let p = v
            .pointer("/result/move_result/pane/pane_id")
            .and_then(Value::as_str)
            .unwrap_or(pane)
            .to_string();
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        let t = objs
            .iter()
            .find_map(|m| s(m, &["tab_id"]))
            .ok_or("herdr didn't return the new tab")?;
        Ok((t, p))
    }

    /// A new tab in `cwd` running `cmd` (dashboard custom actions). Returns the pane.
    pub fn run_in_new_tab(&self, cwd: &str, label: &str, cmd: &str) -> Result<String, String> {
        let mut a = vec!["tab", "create", "--label", label];
        if !cwd.is_empty() {
            a.extend(["--cwd", cwd]);
        }
        let v = self.call_s(&a)?;
        let pane = new_pane_of(&v, None).ok_or("herdr didn't return a pane for the new tab")?;
        self.call_s(&["pane", "run", &pane, cmd])?;
        Ok(pane)
    }

    /// Start the herdr server on the remote (via ssh) and wait until it answers.
    pub fn start_server(&self) -> Result<(), String> {
        // showcase's demo world: its own herdr, started where its story stopped it
        if let Some(r) = crate::showcase::world_heal(&self.ssh) {
            return r.map(|_| ());
        }
        if let Some(e) = crate::showcase::refused() {
            return Err(e);
        }
        // herdr on a Mac keeps its keys in the login Keychain, which an ssh
        // session can't unlock: starting it from here would fail oddly
        if !crate::sshcfg::is_local(&self.ssh) && self.remote_is_mac() {
            return Err(format!(
                "herdr isn't running on {} — it's a Mac: start herdr there (open a terminal on it and run herdr), then try again",
                self.ssh
            ));
        }
        let start = if crate::sshcfg::is_local(&self.ssh) {
            format!(
                "nohup {} server >/dev/null 2>&1 </dev/null &",
                sh_quote(&bin())
            )
        } else {
            r#"sh -c 'nohup "${SHELL:-sh}" -lc "herdr server" >/dev/null 2>&1 </dev/null &'"#.into()
        };
        let o = proc::run(
            &mut crate::sshcfg::shell(&self.ssh, &start),
            Duration::from_secs(25),
            None,
        )
        .map_err(|e| e.to_string())?;
        if !o.ok() {
            return Err(format!("ssh {}: {}", self.ssh, o.reason()));
        }
        for _ in 0..30 {
            if self.server_up().is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        Err(format!("herdr didn't come up on {}", self.ssh))
    }

    /// Start an agent in an (idle) shell pane the way the registry says
    /// (`Agent::launch`): `agent start --kind` for herdr's own kinds (names
    /// the agent, returns once it's ready), else typed into the shell.
    fn start_agent(
        &self,
        name: &str,
        agent: &str,
        pane: &str,
        args: &[String],
    ) -> Result<(), String> {
        let sdk_args;
        let args = if agent == crate::agentsdk::ID {
            sdk_args = crate::agentsdk::default_args(args);
            &sdk_args[..]
        } else {
            args
        };
        let reg = crate::agents::find(agent);
        let exe = reg.map_or(agent, |r| r.exe());
        let hkind = match reg.map(|r| r.launch()) {
            Some(Launch::Kind(k)) => k,
            // Crush, Command Code, … are typed into the shell
            Some(Launch::Command(cmd)) => return self.run_agent(name, cmd, pane, args),
            // not in the registry: its name is the command
            None => return self.run_agent(name, agent, pane, args),
        };
        let mut a: Vec<OsString> = vec![
            "agent".into(),
            "start".into(),
            name.into(),
            "--kind".into(),
            hkind.into(),
            "--pane".into(),
            pane.into(),
            "--timeout".into(),
            "40000".into(),
        ];
        if !args.is_empty() {
            a.push("--".into());
            a.extend(args.iter().map(OsString::from));
        }
        for attempt in 0..20 {
            let o = self.raw(&a, T_START)?;
            if o.ok() {
                return Ok(());
            }
            match err_code(&o).as_deref() {
                // started, but showing a trust/approval screen: fine — it's running
                Some("agent_not_ready") => return Ok(()),
                // the shell isn't idle yet (hook still running, or the tab is fresh)
                Some(c)
                    if (c.contains("busy")
                        || c.contains("unavailable")
                        || c.contains("foreground"))
                        && attempt < 19 =>
                {
                    std::thread::sleep(Duration::from_millis(750));
                }
                _ => break,
            }
        }
        // older herdr / unsupported kind: type the command into the shell
        self.run_agent(name, exe, pane, args)
    }

    /// A command typed into a shell can fail where nobody sees it. It gets a
    /// few seconds: something other than the shell in the foreground means it
    /// runs. It only counts as failed when the shell itself says so, naming it:
    /// a slow shell start looks just the same, and that tab mustn't be closed.
    /// herdr not saying (an older one) counts as started.
    fn typed_started(&self, pane: &str, what: &str) -> Result<(), String> {
        let id = [pane.to_string()];
        for _ in 0..TYPED_CHECKS {
            std::thread::sleep(Duration::from_millis(500));
            match self.foreground_many(&id).get(pane) {
                Some(None) => {}
                _ => return Ok(()),
            }
            let screen = self.read_pane(pane, 8).unwrap_or_default();
            if let Some(l) = couldnt_run(&screen, what) {
                return Err(format!("{what} didn't start: {}", crate::sentlog::scrub(l)));
            }
        }
        Ok(())
    }

    /// Type `exe args…` into the pane's shell, then name the agent once herdr sees it.
    fn run_agent(&self, name: &str, exe: &str, pane: &str, args: &[String]) -> Result<(), String> {
        let cmd = crate::agents::command_line(exe, args);
        self.call_s(&["pane", "run", pane, &cmd])?;
        self.typed_started(pane, exe)?;
        // herdr may not know this agent by its process: give the sidebar its name
        if let Some(a) = crate::agents::find(exe) {
            let _ = self.display_agent(pane, a.name);
        }
        // herdr never sees this one as an agent: its tab's label names it
        if exe != crate::agentsdk::EXE {
            let _ = self.rename_agent_later(pane, name);
        }
        Ok(())
    }

    /// Started under a wrapper (a sandbox: `agent_wrapper`): typed into the
    /// shell as `HERDR_AGENT=<kind> <wrapper> <exe> <args…>`, so herdr still
    /// knows which agent runs inside, and its sidebar shows the agent's name.
    fn run_wrapped(
        &self,
        name: &str,
        agent: &str,
        wrapper: &str,
        pane: &str,
        args: &[String],
    ) -> Result<(), String> {
        // the sandbox must be there: an agent is never started without it
        // (nor typed into a shell that can't find it)
        let prog = wrapper.split_whitespace().next().unwrap_or("");
        if !self.has_program(prog) {
            return Err(format!(
                "agent_wrapper: `{prog}` isn't installed on {} (install it there, or aleph settings unset agent_wrapper)",
                self.ssh
            ));
        }
        let reg = crate::agents::find(agent);
        let exe = reg.map_or(agent, |r| r.exe());
        let kind = reg.and_then(|r| r.kind).unwrap_or(agent);
        let cmd = Self::wrapped_command(kind, wrapper, &crate::agents::command_line(exe, args));
        self.call_s(&["pane", "run", pane, &cmd])?;
        self.typed_started(pane, prog)?;
        let _ = self.display_agent(pane, reg.map_or(agent, |r| r.name));
        let _ = self.rename_agent_later(pane, name);
        Ok(())
    }

    /// Is `prog` on this machine's PATH (the user's login PATH)?
    fn has_program(&self, prog: &str) -> bool {
        if crate::showcase::demo() {
            return false;
        }
        if prog.is_empty()
            || !prog
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
        {
            return false;
        }
        let script = format!(
            "{}\ncommand -v {prog} >/dev/null 2>&1 && echo yes\n",
            crate::chan::PATH_PRELUDE
        );
        let mut c = crate::sshcfg::shell(&self.ssh, "sh -s");
        crate::proc::run(&mut c, Duration::from_secs(10), Some(script.as_bytes()))
            .map(|o| o.text().contains("yes"))
            .unwrap_or(false)
    }

    /// The agent's name in herdr's sidebar (`report-metadata --display-agent`).
    pub fn display_agent(&self, pane: &str, label: &str) -> Result<(), String> {
        if let Some(r) = crate::showcase::herdr_ok(&self.ssh) {
            return r;
        }
        self.call_s(&[
            "pane",
            "report-metadata",
            pane,
            "--source",
            crate::brand::HERDR_SOURCE,
            "--display-agent",
            label,
        ])
        .map(|_| ())
    }

    /// Best effort: name a manually started agent once herdr has detected it.
    fn rename_agent_later(&self, pane: &str, name: &str) -> Result<(), String> {
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(500));
            if self.rename_agent(pane, name).is_ok() {
                return Ok(());
            }
        }
        Err("agent not detected".into())
    }

    /// New agent session(s) in a new tab (new workspace if the session is
    /// empty, or a git worktree workspace). `step(i, label)` reports progress:
    /// 0 herdr up · 1 new tab · 2 agent started.
    pub fn new_session(
        &self,
        o: &NewOpts,
        step: &mut dyn FnMut(usize, String),
    ) -> Result<Vec<Started>, String> {
        self.new_tab(o, step).map(|(v, _)| v)
    }

    /// `new_session`, also returning every pane of the new tab in its
    /// layout's order (`o.layout`: a preset starts its other sessions there).
    pub fn new_tab(
        &self,
        o: &NewOpts,
        step: &mut dyn FnMut(usize, String),
    ) -> Result<(Vec<Started>, Vec<String>), String> {
        let kind = o.agent_id();
        let kind = kind.as_str();
        let shown = crate::agents::label(kind);
        step(0, format!("Connecting to herdr on {}", self.ssh));
        if let Some(r) = crate::showcase::new_tab(&self.ssh, o) {
            step(2, format!("Starting {shown}"));
            return r;
        }
        if self.server_up().is_err() {
            step(0, format!("Starting herdr on {}", self.ssh));
            self.start_server()?;
        }
        step(1, "Opening a new tab".into());
        let existing = self.sessions().unwrap_or_default();
        let mut live: Vec<String> = o.live.clone();
        live.extend(existing.iter().map(|s| s.name.clone()));
        live.extend(self.tab_labels().iter().map(|l| names::from_label(l)));
        let hues_in_use: Vec<String> = existing.iter().map(|s| s.hue.clone()).collect();
        let folder = o.dir.as_deref().map(|d| {
            let b = crate::path::basename(d);
            if b.is_empty() {
                "root".to_string()
            } else {
                crate::path::show(b)
            }
        });
        // the tab's shape: a preset's, else `--tile` as an even grid
        let shape: Tree<Leaf> = o.layout.clone().unwrap_or_else(|| {
            let leaf = Leaf {
                cwd: o.dir.as_deref().map(crate::path::show),
                env: o.env.clone(),
                openrouter: false,
            };
            layouts::grid(o.tile.clamp(1, 9))
                .unwrap_or(Tree::Pane(0))
                .map(&mut |_| leaf.clone())
        });
        // agents start in the first `n` panes (a preset's tab: just its first)
        let n = o.tile.clamp(1, 9).min(shape.len());
        // the OpenRouter key, for the agents that read it (`openrouter.agents`;
        // a preset marks its other panes itself): only ever inside the one
        // `layout.apply` request over herdr's socket, never on a command line
        let cfg = crate::config::Config::load();
        let mine = crate::openrouter::wants(&cfg, kind);
        // OpenCode with OpenRouter: only the free (or only the paid) models, as
        // config of its own for that session (`openrouter.models`; not a secret)
        let oc = (mine && kind == "opencode" && crate::openrouter::configured(&cfg))
            .then(|| crate::openrouter::opencode_config(&cfg))
            .flatten();
        let mut k = 0;
        let shape = shape.map(&mut |l| {
            k += 1;
            let mut env = l.env.clone();
            if let (true, Some(c)) = (mine && k <= n, &oc) {
                if !env.iter().any(|(k, _)| k == "OPENCODE_CONFIG_CONTENT") {
                    env.push(("OPENCODE_CONFIG_CONTENT".into(), c.clone()));
                }
            }
            Leaf {
                openrouter: l.openrouter || (mine && k <= n),
                env,
                ..l.clone()
            }
        });
        let key =
            if shape.leaves().iter().any(|l| l.openrouter) && crate::openrouter::configured(&cfg) {
                crate::openrouter::key(&cfg)
            } else {
                None
            };
        let key = key.as_deref();
        // why it couldn't go along, when it couldn't (said, never leaked)
        let mut note: Option<String> = None;
        // story names take their mood from the machine they start on
        let story_color = (o.naming == "story")
            .then(|| crate::config::Config::load().host_get(&self.ssh, "color"))
            .flatten();
        let mut picked: Vec<String> = Vec::new();
        for i in 0..n {
            let name = match (&o.name, i) {
                (Some(nm), 0) => names::numbered(nm, &live),
                (Some(nm), _) => names::numbered(&format!("{nm}-{}", i + 1), &live),
                (None, _) if o.naming == "folder" && folder.is_some() => {
                    names::from_folder(folder.as_deref().unwrap_or("root"), &live)
                }
                (None, _) if o.naming == "story" => names::story(
                    kind,
                    names::land_words(story_color.as_deref(), &self.ssh),
                    &live,
                    &o.recent,
                    names::seed() ^ (i as u64 * 0x9e37),
                ),
                (None, _) => names::pick(&live, &o.recent, names::seed() ^ (i as u64 * 0x9e37)),
            };
            live.push(name.clone());
            picked.push(name);
        }
        let prefix = if o.prefix.is_empty() {
            format!("{kind}_")
        } else {
            o.prefix.clone()
        };
        let mut label = format!("{prefix}{}", picked[0]);
        if shape.len() > 1 {
            label.push_str(&format!("+{}", shape.len() - 1));
        }
        if let Some(f) = &folder {
            label = format!("{label}:{f}");
        }

        // the tab (or worktree workspace) and its panes, laid out as `shape`
        let (panes, dir) = if let Some(branch) = &o.worktree {
            let Some(d) = &o.dir else {
                return Err("a worktree needs a folder inside a git repository".into());
            };
            let branch = if branch.is_empty() {
                format!("{kind}/{}", picked[0])
            } else {
                branch.clone()
            };
            step(1, format!("Creating worktree {branch}"));
            let a: Vec<OsString> = vec![
                "worktree".into(),
                "create".into(),
                "--cwd".into(),
                OsString::from_vec(d.clone()),
                "--branch".into(),
                branch.into(),
                "--label".into(),
                label.clone().into(),
            ];
            let mut a = a;
            if !o.no_focus {
                a.push("--focus".into());
            }
            let v = self.call(&a, T_CALL)?;
            let pane = v
                .pointer("/result/root_pane/pane_id")
                .and_then(Value::as_str)
                .map(String::from)
                .or_else(|| new_pane_of(&v, None))
                .ok_or("herdr didn't return a pane for the worktree")?;
            let path = v
                .pointer("/result/worktree/path")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| crate::path::show(d));
            // laid out by herdr's socket: more than one pane, or a key to pass
            let panes = if shape.len() > 1 || key.is_some() {
                if shape.len() > 1 {
                    step(1, format!("Laying out {} panes", shape.len()));
                }
                // the first pane is the worktree's; with `--tile` the others
                // are in it too (a preset's other panes keep their folders)
                let repo = crate::path::show(d);
                let mut k = 0;
                let shape = shape.map(&mut |l| {
                    k += 1;
                    let here = k == 1 || (o.layout.is_none() && l.cwd.as_deref() == Some(&repo));
                    Leaf {
                        cwd: if here {
                            Some(path.clone())
                        } else {
                            l.cwd.clone()
                        },
                        ..l.clone()
                    }
                });
                // herdr replaces the worktree's fresh tab (an idle shell) in one go
                let tab = v
                    .pointer("/result/tab/tab_id")
                    .or_else(|| v.pointer("/result/root_pane/tab_id"))
                    .and_then(Value::as_str)
                    .map(String::from);
                match tab.map(|t| self.apply_layout(&shape, Place::Replace(t), !o.no_focus, key)) {
                    Some(Ok(p)) => p,
                    Some(Err(f)) if !f.retry_elsewhere() => return Err(f.to_string()),
                    other => {
                        if key.is_some() {
                            let why = match &other {
                                Some(Err(f)) => sock_why(f),
                                _ => "didn't say which tab the worktree is in".into(),
                            };
                            note = Some(crate::openrouter::not_passed(&self.ssh, &why));
                        }
                        self.split_tree(&pane, &shape)?
                    }
                }
            } else {
                vec![pane]
            };
            (panes, path)
        } else {
            let panes = if shape.len() > 1 || key.is_some() {
                if shape.len() > 1 {
                    step(1, format!("Laying out {} panes", shape.len()));
                }
                let place = Place::New {
                    label: label.clone(),
                    workspace: o.workspace.clone(),
                };
                match self.apply_layout(&shape, place, !o.no_focus, key) {
                    Ok(p) => p,
                    Err(f) if !f.retry_elsewhere() => return Err(f.to_string()),
                    // a fresh server with no workspace yet: herdr's CLI makes
                    // one (its tab an idle shell), the key's tab replaces it
                    Err(crate::sock::Fail::Refused { code, .. })
                        if code == "workspace_not_found" && key.is_some() =>
                    {
                        let root = self.open_tab(o, &label)?;
                        let tab = self.export_layout(&root).ok().and_then(|(t, _)| t);
                        match tab
                            .map(|t| self.apply_layout(&shape, Place::Replace(t), !o.no_focus, key))
                        {
                            Some(Ok(p)) => p,
                            Some(Err(f)) if !f.retry_elsewhere() => return Err(f.to_string()),
                            other => {
                                let why = match &other {
                                    Some(Err(f)) => sock_why(f),
                                    _ => "didn't say which tab it made".into(),
                                };
                                note = Some(crate::openrouter::not_passed(&self.ssh, &why));
                                self.split_tree(&root, &shape)?
                            }
                        }
                    }
                    // no socket, or an older herdr: a tab, then the same shape by
                    // splits (the key stays out: it would be on their command lines)
                    Err(f) => {
                        if key.is_some() {
                            note = Some(crate::openrouter::not_passed(&self.ssh, &sock_why(&f)));
                        }
                        let root = self.open_tab(o, &label)?;
                        self.split_tree(&root, &shape)?
                    }
                }
            } else {
                vec![self.open_tab(o, &label)?]
            };
            (
                panes,
                o.dir.as_deref().map(crate::path::show).unwrap_or_default(),
            )
        };

        if let Some(out) = &o.panes_out {
            *out.lock().unwrap_or_else(|e| e.into_inner()) = panes.clone();
        }
        if let Some(n) = &note {
            // said three ways: the log, the start's progress, the Inbox
            crate::log::warn("openrouter", n);
            step(1, n.clone());
            crate::events::append(crate::events::Event {
                t: crate::cache::now(),
                host: self.ssh.clone(),
                name: picked[0].clone(),
                kind: crate::events::Kind::Error,
                title: n.clone(),
                agent: kind.to_string(),
                source: "aleph".into(),
                ..Default::default()
            });
        }
        // a sandbox every agent starts under (`agent_wrapper`, per machine too)
        let wrapper = cfg
            .host_get(&self.ssh, "agent_wrapper")
            .or_else(|| cfg.get("agent_wrapper"))
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty());
        let mut out = Vec::new();
        let mut hues = hues_in_use;
        let mut start_all = || -> Result<(), String> {
            for (i, (pane, name)) in panes.iter().zip(picked.iter()).enumerate() {
                step(
                    2,
                    if n > 1 {
                        format!("Starting {shown} {}/{n} · {name}", i + 1)
                    } else {
                        format!("Starting {shown} as {name}")
                    },
                );
                let (hue, mark) = theme::pick_hue(&hues, names::seed() ^ i as u64);
                hues.push(hue.to_string());
                let _ = self.report_tokens(pane, &[("mark", mark), ("hue", hue), ("name", name)]);
                if let Some(h) = &o.pre_hook {
                    self.call_s(&["pane", "run", pane, h])?;
                }
                let mut args = o.args.clone();
                if let Some(p) = &o.prompt {
                    args.push(p.clone());
                }
                // `shell`: just the tab, no agent in it
                if kind != crate::agents::SHELL {
                    match &wrapper {
                        Some(w) => self.run_wrapped(name, kind, w, pane, &args)?,
                        None => self.start_agent(name, kind, pane, &args)?,
                    }
                    // its first prompt, for the journal (`sentlog.rs`)
                    if let Some(p) = &o.prompt {
                        crate::sentlog::record(&self.ssh, pane, name, p);
                    }
                }
                out.push(Started {
                    name: name.clone(),
                    label: label.clone(),
                    pane: pane.clone(),
                    hue: hue.to_string(),
                    mark: mark.to_string(),
                    dir: dir.clone(),
                    note: if i == 0 { note.clone() } else { None },
                });
            }
            Ok(())
        };
        // it didn't start: its tab doesn't stay behind, an empty shell nobody asked for
        if let Err(e) = start_all() {
            for p in &panes {
                let _ = self.close_pane(p);
            }
            return Err(e);
        }
        Ok((out, panes))
    }

    /// A new tab (a new workspace when the server has none yet): its pane.
    fn open_tab(&self, o: &NewOpts, label: &str) -> Result<String, String> {
        let mut opts = tab_opts(o, label);
        let mut a = vec![OsString::from("tab"), "create".into()];
        if let Some(w) = &o.workspace {
            a.push("--workspace".into());
            a.push(w.into());
        }
        a.extend(opts.iter().cloned());
        let mut tab = self.call(&a, T_CALL);
        if tab.as_ref().is_err_and(|e| e.contains("no-focus")) {
            a.retain(|x| x != "--no-focus");
            opts.retain(|x| x != "--no-focus");
            tab = self.call(&a, T_CALL);
        }
        if let Some(p) = tab.as_ref().ok().and_then(|v| new_pane_of(v, None)) {
            return Ok(p);
        }
        // no active workspace yet (fresh server) → create one instead;
        // if that fails too, report the original tab error
        let mut a = vec![OsString::from("workspace"), "create".into()];
        a.extend(opts.iter().cloned());
        let tab_err = tab.err();
        match self.call(&a, T_CALL) {
            Ok(v) => new_pane_of(&v, None).ok_or_else(|| {
                tab_err.unwrap_or_else(|| "herdr didn't return a pane for the new tab".into())
            }),
            Err(e) => Err(tab_err.unwrap_or(e)),
        }
    }

    /// A tab laid out as `tree` in one request (herdr's `layout.apply`, over
    /// its socket): every pane starts at its final size. `openrouter`: the
    /// key, put into the environment of the panes that ask for it — it
    /// lives only in this one JSON line (`sock.rs` logs the method alone).
    /// The panes, in the tree's order.
    pub fn apply_layout(
        &self,
        tree: &Tree<Leaf>,
        place: Place,
        focus: bool,
        openrouter: Option<&str>,
    ) -> Result<Vec<String>, crate::sock::Fail> {
        use crate::sock::Fail;
        let secret = openrouter.map(|k| (crate::openrouter::ENV, k));
        let mut p =
            serde_json::json!({"root": layouts::to_herdr_with(tree, secret), "focus": focus});
        match place {
            Place::New { label, workspace } => {
                p["tab_label"] = label.into();
                if let Some(w) = workspace {
                    p["workspace_id"] = w.into();
                }
            }
            Place::Replace(tab) => p["tab_id"] = tab.into(),
        }
        // (an answer that echoes the request back never shows a secret)
        let v = crate::sock::call(&self.ssh, "layout.apply", p).map_err(Fail::masked)?;
        let (_, got) = layouts::layout_in(&v)
            .ok_or_else(|| Fail::Lost("herdr's answer has no layout in it".into()))?;
        let panes: Vec<String> = got
            .leaves()
            .into_iter()
            .filter_map(|g| g.pane_id.clone())
            .collect();
        if panes.len() != tree.len() {
            return Err(Fail::Lost(format!(
                "herdr made {} panes, not {}",
                panes.len(),
                tree.len()
            )));
        }
        Ok(panes)
    }

    /// `root` (a tab's only pane) split into `tree`'s shape the way herdr's
    /// `layout.apply` does it — each split keeps `ratio` for its first side —
    /// for when the socket isn't there. The panes, in the tree's order.
    pub fn split_tree(&self, root: &str, tree: &Tree<Leaf>) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        self.split_into(root, tree, &mut out)?;
        Ok(out)
    }

    fn split_into(&self, pane: &str, t: &Tree<Leaf>, out: &mut Vec<String>) -> Result<(), String> {
        match t {
            Tree::Pane(_) => {
                out.push(pane.to_string());
                Ok(())
            }
            Tree::Split {
                dir,
                ratio,
                first,
                second,
            } => {
                let l = second.first();
                let cwd = l.cwd.as_deref().map(str::as_bytes);
                let new = self.split_env(pane, dir.name(), *ratio, cwd, &l.env)?;
                self.split_into(pane, first, out)?;
                self.split_into(&new, second, out)
            }
        }
    }

    /// The layout of the tab `pane` is in (`layout.export`, over the socket).
    pub fn export_layout(&self, pane: &str) -> Result<(Option<String>, Tree<Got>), String> {
        let v = crate::sock::request(
            &self.ssh,
            "layout.export",
            serde_json::json!({ "pane_id": pane }),
        )?;
        layouts::layout_in(&v).ok_or_else(|| "herdr's answer has no layout in it".into())
    }

    /// Running panes moved into one new tab in `tree`'s shape (`tree` holds
    /// their pane ids; herdr tiles within one machine). Over the socket
    /// (`pane.move`, ratios exact), else herdr's `pane move`. The new tab.
    pub fn push_tree(&self, tree: &Tree<String>, label: &str) -> Result<String, String> {
        let first = tree.first().clone();
        let to_new = serde_json::json!({"type": "new_tab", "label": label});
        // one way to the socket for every move
        let link = crate::sock::open(&self.ssh);
        let moved = match &link {
            Ok(l) => Self::sock_move(l, &first, to_new, true),
            Err(f) => Err(f.clone()),
        };
        let (tab, root, link) = match moved {
            Ok((p, Some(t))) => (t, p, link.ok()),
            Ok((_, None)) => return Err("herdr didn't say which tab it made".into()),
            Err(f) if !f.retry_elsewhere() => return Err(f.to_string()),
            // no socket, or an older herdr: its CLI
            Err(_) => {
                let (t, p) = self.move_to_new_tab(&first, label)?;
                (t, p, None)
            }
        };
        self.move_into(&tab, &root, tree, link.as_ref())?;
        let _ = self.focus(&root);
        Ok(tab)
    }

    fn move_into(
        &self,
        tab: &str,
        pane: &str,
        t: &Tree<String>,
        link: Option<&crate::sock::Link>,
    ) -> Result<(), String> {
        let Tree::Split {
            dir,
            ratio,
            first,
            second,
        } = t
        else {
            return Ok(());
        };
        let mover = second.first();
        let moved = match link {
            Some(l) => {
                let to = serde_json::json!({"type": "tab", "tab_id": tab, "target_pane_id": pane,
                    "split": dir.name(), "ratio": (*ratio as f64 * 1000.0).round() / 1000.0});
                Self::sock_move(l, mover, to, false)
                    .map_err(|f| f.to_string())?
                    .0
            }
            None => self.move_pane_ratio(mover, tab, dir.name(), Some(pane), Some(*ratio))?,
        };
        self.move_into(tab, pane, first, link)?;
        self.move_into(tab, &moved, second, link)
    }

    /// One `pane.move` over the socket: (the pane's id now, the tab it made).
    fn sock_move(
        link: &crate::sock::Link,
        pane: &str,
        destination: Value,
        focus: bool,
    ) -> Result<(String, Option<String>), crate::sock::Fail> {
        use crate::sock::Fail;
        let v = link.call(
            "pane.move",
            serde_json::json!({"pane_id": pane, "destination": destination, "focus": focus}),
        )?;
        // the move's record, wherever it's nested (`move_result`)
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        let Some(m) = objs
            .iter()
            .find(|m| m.contains_key("previous_pane_id") || m.contains_key("changed"))
            .copied()
        else {
            return Err(Fail::Lost(
                "herdr's answer doesn't say where it moved".into(),
            ));
        };
        if m.get("changed").and_then(Value::as_bool) == Some(false) {
            // understood and declined (a zoomed tab, …): the CLI would too
            let why = s(m, &["reason"]).unwrap_or_else(|| "nothing changed".into());
            return Err(Fail::Declined(format!(
                "herdr didn't move {pane} ({})",
                why.replace('_', " ")
            )));
        }
        let field = |obj: &str, k: &str| {
            m.get(obj)
                .and_then(Value::as_object)
                .and_then(|o| s(o, &[k]))
        };
        let now = field("pane", "pane_id").unwrap_or_else(|| pane.to_string());
        let tab = field("created_tab", "tab_id")
            .or_else(|| field("pane", "tab_id"))
            .or_else(|| field("target_layout", "tab_id"));
        Ok((now, tab))
    }

    /// `HERDR_AGENT=<kind> <wrapper> <command>` (pure — tested).
    pub fn wrapped_command(kind: &str, wrapper: &str, cmd: &str) -> String {
        format!("HERDR_AGENT={kind} {} {cmd}", wrapper.trim())
    }

    /// Replace this process with the herdr client attached to the remote session.
    pub fn attach(&self) -> std::io::Error {
        let a = self.attach_args();
        if let Some(m) = self.mosh_argv() {
            return proc::exec(&m[0], &m[1..]);
        }
        proc::exec(bin(), &a)
    }

    /// herdr's own attach: `--remote <ssh>` for a machine, nothing here; with
    /// the machine's named session (`--session <name>`) when it has one.
    fn attach_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if !crate::sshcfg::is_local(&self.ssh) {
            a.extend(["--remote".to_string(), self.ssh.clone()]);
        }
        if let Some(s) = crate::chan::session(&self.ssh) {
            a.extend(["--session".to_string(), s]);
        }
        a
    }

    /// Attach as a child process (returns when the user detaches).
    pub fn attach_wait(&self) -> std::io::Result<bool> {
        let a = self.attach_args();
        if let Some(m) = self.mosh_argv() {
            return proc::interactive(&m[0], &m[1..]);
        }
        proc::interactive(&bin(), &a)
    }

    /// `attach_via = mosh` (and mosh is installed here): mosh runs the herdr
    /// client on the machine itself, so the attach survives roaming and
    /// sleep. None: plain `herdr --remote` over ssh.
    fn mosh_argv(&self) -> Option<Vec<String>> {
        if crate::config::Config::load().get_or("attach_via") != "mosh" {
            return None;
        }
        if crate::sshcfg::is_local(&self.ssh) {
            return None;
        }
        let mosh = proc::which("mosh")?;
        let mut v = vec![
            mosh.to_string_lossy().into_owned(),
            self.ssh.clone(),
            "--".into(),
        ];
        // mosh runs the command itself: the named session goes through env
        if let Some(s) = crate::chan::session(&self.ssh) {
            v.extend(["env".into(), format!("HERDR_SESSION={s}")]);
        }
        v.push(crate::chan::herdr_path(&self.ssh));
        Some(v)
    }
}

/// The agent state a wait / prompt-wait answered with.
/// `pane split`'s arguments (pure — tested): the plain environment only
/// (a preset's `env`); a secret never goes on a command line.
fn split_args(
    pane: &str,
    dir: &str,
    ratio: f32,
    cwd: Option<&[u8]>,
    env: &[(String, String)],
) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "pane".into(),
        "split".into(),
        pane.into(),
        "--direction".into(),
        dir.into(),
        "--ratio".into(),
        format!("{ratio:.3}").into(),
    ];
    if let Some(d) = cwd {
        a.push("--cwd".into());
        a.push(OsString::from_vec(d.to_vec()));
    }
    for (k, v) in env {
        a.push("--env".into());
        a.push(format!("{k}={v}").into());
    }
    a
}

/// `pane process-info`'s answer: the shell itself in the foreground? Anything
/// unclear is no.
/// What `pane process-info` says is in the foreground: `Some(None)` at the
/// shell's prompt, `Some(Some(command))` otherwise (its command line, secrets
/// masked), `None` when it doesn't say.
fn foreground_of(v: &Value) -> Option<Option<String>> {
    let info = &v["result"]["process_info"];
    let fg = info["foreground_process_group_id"].as_u64()?;
    if Some(fg) == info["shell_pid"].as_u64() {
        return Some(None);
    }
    // the group's leader, else the first of it
    let procs = info["foreground_processes"].as_array();
    let p = procs.and_then(|v| {
        v.iter()
            .find(|p| p["pid"].as_u64() == Some(fg))
            .or_else(|| v.first())
    });
    let text = p
        .and_then(|p| {
            p["cmdline"]
                .as_str()
                .filter(|c| !c.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    let argv: Vec<&str> = p["argv"]
                        .as_array()?
                        .iter()
                        .filter_map(Value::as_str)
                        .collect();
                    (!argv.is_empty()).then(|| argv.join(" "))
                })
                .or_else(|| p["name"].as_str().map(str::to_string))
        })
        .unwrap_or_else(|| "a command".to_string());
    let text = crate::sentlog::scrub(crate::log::mask_known(text.trim()).as_ref());
    Some(Some(text.chars().take(240).collect()))
}

fn prompt_in(v: &Value) -> bool {
    let info = &v["result"]["process_info"];
    let (fg, shell) = (
        info["foreground_process_group_id"].as_u64(),
        info["shell_pid"].as_u64(),
    );
    fg.is_some() && fg == shell
}

/// `tab create` / `workspace create`'s options (pure — tested): the plain
/// environment only; the OpenRouter key never goes this way.
fn tab_opts(o: &NewOpts, label: &str) -> Vec<OsString> {
    let mut opts: Vec<OsString> = vec!["--label".into(), label.into()];
    // detached starts say so (herdr ≥ 0.9 has --no-focus; an older one
    // that doesn't know it is asked again without it, in `open_tab`)
    opts.push(if o.no_focus { "--no-focus" } else { "--focus" }.into());
    if let Some(d) = &o.dir {
        opts.push("--cwd".into());
        opts.push(OsString::from_vec(d.to_vec()));
    }
    for (k, v) in &o.env {
        opts.push("--env".into());
        opts.push(format!("{k}={v}").into());
    }
    opts
}

/// Why herdr's socket didn't make a tab, in a few words (for the note a start
/// leaves when the OpenRouter key couldn't go along).
fn sock_why(f: &crate::sock::Fail) -> String {
    use crate::sock::Fail;
    match f {
        Fail::Unreachable(_) => "isn't reachable".into(),
        Fail::Refused { code, .. } if code == "invalid_request" => {
            "belongs to an older herdr (no layout.apply)".into()
        }
        Fail::Refused { message, .. } => format!("refused the tab ({message})"),
        Fail::Lost(m) | Fail::Declined(m) => format!("didn't make the tab ({m})"),
    }
}

fn state_of(v: &Value) -> String {
    let mut objs = Vec::new();
    objects(v, &mut objs);
    objs.iter()
        .find_map(|m| s(m, &["agent_status", "status", "state"]))
        .unwrap_or_default()
}

/// The pane a create/split response is about (root_pane / pane), skipping `not`.
fn new_pane_of(v: &Value, not: Option<&str>) -> Option<String> {
    if let Some(p) = v
        .pointer("/result/root_pane/pane_id")
        .and_then(Value::as_str)
    {
        return Some(p.to_string());
    }
    let mut objs = Vec::new();
    objects(v, &mut objs);
    objs.iter()
        .filter_map(|m| s(m, &["pane_id"]))
        .find(|p| Some(p.as_str()) != not)
}

/// Pure parser (unit-tested): agent list + tab list → sessions, sorted by label.
pub fn parse_sessions(agents: &Value, tabs: &Value, f: &Filter) -> Vec<Session> {
    let mut tobjs = Vec::new();
    objects(tabs, &mut tobjs);
    let labels: Vec<(String, String)> = tobjs
        .iter()
        .filter(|m| !m.contains_key("pane_id"))
        .filter_map(|m| Some((s(m, &["tab_id"])?, s(m, &["label", "name"])?)))
        .collect();
    let mut aobjs = Vec::new();
    objects(agents, &mut aobjs);
    // every agent's pane, listed or not: none of them is a plain shell
    let agent_panes: Vec<String> = aobjs.iter().filter_map(|m| s(m, &["pane_id"])).collect();
    let mut out: Vec<Session> = Vec::new();
    for m in aobjs {
        let Some(pane) = s(m, &["pane_id"]) else {
            continue;
        };
        let raw = s(m, &["agent", "kind", "display_agent"]).unwrap_or_default();
        if raw.is_empty() {
            continue;
        }
        let agent = crate::agents::id_of(&raw);
        if !f.matches(&agent) || out.iter().any(|x| x.pane_id == pane) {
            continue;
        }
        let tab = s(m, &["tab_id"]).unwrap_or_default();
        let tok = |k: &str| {
            m.get("tokens")
                .and_then(Value::as_object)
                .and_then(|t| s(t, &[k]))
        };
        let label = labels
            .iter()
            .find(|(t, _)| *t == tab)
            .map(|(_, l)| l.clone())
            .unwrap_or_else(|| pane.clone());
        let name = s(m, &["name"])
            .or_else(|| tok("name"))
            .unwrap_or_else(|| names::from_label(&label));
        let title =
            s(m, &["terminal_title_stripped", "title", "terminal_title"]).unwrap_or_default();
        out.push(Session {
            host: String::new(),
            status: s(m, &["agent_status", "status", "state"]).unwrap_or_else(|| "?".into()),
            cwd: s(m, &["foreground_cwd", "cwd"]).unwrap_or_else(|| "-".into()),
            workspace_id: s(m, &["workspace_id"]).unwrap_or_default(),
            terminal_id: s(m, &["terminal_id"]).unwrap_or_default(),
            focused: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
            mark: tok("mark").unwrap_or_else(|| "·".into()),
            hue: tok("hue").unwrap_or_default(),
            pane_id: pane,
            tab_id: tab,
            label,
            name,
            agent,
            title,
            since: None,
            limit: None,
            seq: m
                .get("state_change_seq")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            done_seq: m.get("completion_seq").and_then(Value::as_u64).unwrap_or(0),
            ready: m.get("interactive_ready").and_then(Value::as_bool),
            launch_pending: m
                .get("launch_pending")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            native_id: m
                .get("agent_session")
                .and_then(|a| a.get("value").or(Some(a)))
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(String::from),
            compacting: false,
            compacted: false,
            plain: false,
            restore_error: None,
            shown: String::new(),
        });
    }
    // plain shells: panes with no agent (only `api snapshot` lists panes). Those
    // Aleph started have `shell_<name>` tabs; any other is `plain`
    for m in tobjs.iter().filter(|m| m.contains_key("pane_id")) {
        let (Some(pane), Some(tab)) = (s(m, &["pane_id"]), s(m, &["tab_id"])) else {
            continue;
        };
        if agent_panes.contains(&pane) || s(m, &["agent"]).is_some_and(|a| !a.is_empty()) {
            continue;
        }
        let label = labels
            .iter()
            .find(|(t, _)| *t == tab)
            .map_or(String::new(), |x| x.1.clone());
        let prefix = label.split_once('_').map(|(p, _)| p);
        let sdk = prefix == Some(crate::agentsdk::ID);
        let ours = sdk || prefix == Some(crate::agents::SHELL);
        let name = names::from_label(&label);
        out.push(Session {
            host: String::new(),
            status: "idle".into(),
            cwd: s(m, &["foreground_cwd", "cwd"]).unwrap_or_else(|| "-".into()),
            workspace_id: s(m, &["workspace_id"]).unwrap_or_default(),
            terminal_id: s(m, &["terminal_id"]).unwrap_or_default(),
            focused: m.get("focused").and_then(Value::as_bool).unwrap_or(false),
            mark: "·".into(),
            name: if name.is_empty() { pane.clone() } else { name },
            pane_id: pane,
            tab_id: tab,
            label,
            agent: if sdk {
                crate::agentsdk::ID.into()
            } else {
                crate::agents::SHELL.into()
            },
            plain: !ours,
            ..Default::default()
        });
    }
    // what herdr couldn't bring back after a restart
    for m in tobjs.iter().filter(|m| m.contains_key("pane_id")) {
        if let (Some(pane), Some(e)) = (s(m, &["pane_id"]), s(m, &["restore_error"])) {
            if let Some(x) = out.iter_mut().find(|x| x.pane_id == pane) {
                x.restore_error = Some(e);
            }
        }
    }
    out.sort_by(|a, b| a.label.cmp(&b.label));
    name_the_bare(&mut out);
    out
}

/// A session whose only name is herdr's tab number ("1"), or its pane id, goes by
/// its folder: `train`, `train 2`. In a home folder or none, by its agent. Only what
/// it's called on screen: herdr's own name stays, and still finds it.
fn name_the_bare(v: &mut [Session]) {
    let is_bare = |s: &Session| {
        let n = s.display();
        n.is_empty() || n == s.pane_id || n.chars().all(|c| c.is_ascii_digit())
    };
    let mut taken: Vec<String> = v
        .iter()
        .filter(|s| !is_bare(s))
        .map(|s| s.display().to_string())
        .collect();
    for s in v.iter_mut().filter(|s| is_bare(s)) {
        let base = folder_name(&s.cwd).unwrap_or_else(|| crate::agents::short_label(&s.agent));
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&name) {
            name = format!("{base} {n}");
            n += 1;
        }
        taken.push(name.clone());
        s.shown = name;
    }
}

/// The last part of a folder, unless it's a home or the root (that says nothing).
fn folder_name(cwd: &str) -> Option<String> {
    let parts: Vec<&str> = cwd.split('/').filter(|p| !p.is_empty()).collect();
    if !cwd.starts_with('/') {
        return None;
    }
    match parts.as_slice() {
        [] | ["root"] | ["home", _] | ["Users", _] => None,
        [.., last] => Some(last.to_string()),
    }
}

/// Keys that choose the affirmative answer to a question herdr detected
/// (`pane read --source detection`). Numbered menus get that number; arrow
/// menus (`❯` marks the cursor) get ups/downs, then enter. None when there's
/// no clear "yes" option. Never assumes option 1 is yes: e.g. Claude Code's folder-trust
/// prompt lists "No, exit" first.
pub fn approve_keys(detection: &str) -> Option<Vec<String>> {
    // a form of questions is for you to answer: no rule or button says yes to it
    if crate::ask::is_form(detection) {
        return None;
    }
    fn yes(t: &str) -> bool {
        let t = t.trim().to_lowercase();
        [
            "yes", "allow", "approve", "trust", "continue", "proceed", "accept", "ok",
        ]
        .iter()
        .any(|w| t.starts_with(w))
    }
    fn strip_cursor(l: &str) -> (bool, &str) {
        let t = l.trim_start();
        for c in ['❯', '›', '>', '→', '▸'] {
            if let Some(r) = t.strip_prefix(c) {
                return (true, r.trim_start());
            }
        }
        (false, t)
    }
    fn numbered(t: &str) -> Option<(u32, &str)> {
        let (n, rest) = t.split_once(['.', ')'])?;
        let n: u32 = n.trim().parse().ok()?;
        Some((n, rest.trim_start()))
    }
    let lines: Vec<&str> = detection.lines().collect();
    // numbered menu: the first "yes" option's number
    let nums: Vec<(u32, &str)> = lines
        .iter()
        .filter_map(|l| numbered(strip_cursor(l).1))
        .collect();
    if nums.len() >= 2 {
        return nums
            .iter()
            .find(|(_, t)| yes(t))
            .map(|(n, _)| vec![n.to_string()]);
    }
    // arrow menu: the block of non-blank lines around the cursor line
    let ci = lines.iter().position(|l| strip_cursor(l).0)?;
    let mut a = ci;
    while a > 0 && !lines[a - 1].trim().is_empty() {
        a -= 1;
    }
    let mut b = ci;
    while b + 1 < lines.len() && !lines[b + 1].trim().is_empty() {
        b += 1;
    }
    let items: Vec<&str> = lines[a..=b].iter().map(|l| strip_cursor(l).1).collect();
    let cur = ci - a;
    let yi = items.iter().position(|t| yes(t))?;
    let mut keys: Vec<String> = Vec::new();
    let (dir, n) = if yi >= cur {
        ("down", yi - cur)
    } else {
        ("up", cur - yi)
    };
    keys.extend((0..n).map(|_| dir.to_string()));
    keys.push("enter".into());
    Some(keys)
}

/// Pick by 1-based number, full label, agent name, or short verb
/// (`temper` → `claude_temper[:dir]`, `codex_temper`, …). Optional `host:`
/// prefix narrows the machine.
pub fn pick<'a>(sess: &'a [Session], q: &str) -> Option<&'a Session> {
    // showcase mode: a masked `host:name` (what the screen showed) is the real one
    let back;
    let q = if crate::showcase::on() {
        let all: Vec<String> = sess
            .iter()
            .flat_map(|s| {
                [
                    format!("{}:{}", s.host, s.name),
                    format!("{}:{}", s.host, s.display()),
                    s.name.clone(),
                    s.display().to_string(),
                ]
            })
            .collect();
        back = crate::showcase::unmask(q, &all).into_owned();
        back.as_str()
    } else {
        q
    };
    if let Ok(n) = q.parse::<usize>() {
        return n.checked_sub(1).and_then(|i| sess.get(i));
    }
    let (host, q) = match q.split_once(':') {
        Some((h, r)) if sess.iter().any(|s| s.host == h) => (Some(h), r),
        _ => (None, q),
    };
    sess.iter()
        .filter(|x| host.is_none_or(|h| x.host == h))
        .find(|x| {
            let base = base_label(&x.label);
            x.label == q
                || base == q
                || x.name == q
                || x.pane_id == q
                || base.split_once('_').map(|(_, r)| r) == Some(q)
        })
}

// ------------------------------------------------------------------ saved machines

/// (ssh target, selector) for each machine saved in herdr. Best effort.
pub fn profiles() -> Vec<(String, String)> {
    let o = proc::cap(&bin(), &["machine", "list", "--json"], T_LOCAL);
    let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) else {
        return vec![];
    };
    let mut objs = Vec::new();
    objects(&v, &mut objs);
    objs.iter()
        .filter_map(|m| {
            let sel = s(m, &["label", "id", "profile_id"])?;
            let tgt = s(m, &["ssh_target", "target", "ssh", "destination", "host"])?;
            Some((tgt, sel))
        })
        .collect()
}

pub fn machine_for(alias: &str) -> String {
    if crate::sshcfg::is_local(alias) {
        return alias.to_string();
    }
    profiles()
        .into_iter()
        .find(|(t, _)| t == alias)
        .map(|(_, s)| s)
        .unwrap_or_else(|| alias.to_string())
}

/// Every name herdr knows its saved machines by (label, ssh target, id) —
/// read defensively: herdr 0.9.5 prints a bare array, older ones
/// `{result:{machines:[…]}}`. None when herdr didn't answer.
pub fn saved_names() -> Option<Vec<String>> {
    let o = proc::cap(&bin(), &["machine", "list", "--json"], T_LOCAL);
    let v = serde_json::from_slice::<Value>(&o.stdout).ok()?;
    Some(saved_names_in(&v))
}

/// [`saved_names`] over the JSON (pure).
pub fn saved_names_in(v: &Value) -> Vec<String> {
    let mut objs = Vec::new();
    objects(v, &mut objs);
    let mut out = Vec::new();
    for m in objs {
        for k in ["label", "ssh_target", "target", "id", "profile_id"] {
            if let Some(x) = s(m, &[k]) {
                if !out.contains(&x) {
                    out.push(x);
                }
            }
        }
    }
    out
}

/// Is `sel` a machine herdr knows (label or id)?
pub fn has_machine(sel: &str) -> bool {
    let o = proc::cap(&bin(), &["machine", "list", "--json"], T_LOCAL);
    if let Ok(v) = serde_json::from_slice::<Value>(&o.stdout) {
        let mut objs = Vec::new();
        objects(&v, &mut objs);
        if objs.iter().any(|m| {
            s(m, &["label"]).as_deref() == Some(sel)
                || s(m, &["id", "profile_id"]).as_deref() == Some(sel)
        }) {
            return true;
        }
    }
    let o = proc::cap(&bin(), &["machine", "list"], T_LOCAL);
    o.text()
        .split(|c: char| c.is_whitespace() || c == ',' || c == '"')
        .any(|w| w == sel)
}

/// `/home/<user>/x` (or `/Users/<user>/x`, `/root/x`) → `~/x`, for remote paths.
pub fn short_path(p: &str, user: &str) -> String {
    for home in [
        format!("/home/{user}"),
        format!("/Users/{user}"),
        "/root".to_string(),
    ] {
        if user.is_empty() && home != "/root" {
            continue;
        }
        if p == home {
            return "~".into();
        }
        if let Some(r) = p.strip_prefix(&format!("{home}/")) {
            return format!("~/{r}");
        }
    }
    p.to_string()
}

/// `claude_scan:ox_2nd_copy` → `claude_scan` (the folder is shown separately).
pub fn base_label(l: &str) -> &str {
    l.split(':').next().unwrap_or(l)
}

#[cfg(test)]
mod tests {
    use super::{couldnt_run, folder_name, name_the_bare, Session};

    #[test]
    fn a_session_with_only_a_number_goes_by_its_folder() {
        let s = |label: &str, cwd: &str, agent: &str| Session {
            label: label.into(),
            name: label.into(),
            cwd: cwd.into(),
            agent: agent.into(),
            pane_id: format!("w1:{label}{cwd}"),
            ..Default::default()
        };
        let mut v = vec![
            s("1", "/srv/train", "claude"),
            s("2", "/data/train/", "codex"),
            s("3", "/home/u", "claude"),
            s("4", "/", "shell"),
            s("claude_bobbing-wren:train", "/srv/train", "claude"),
            s("hunt", "/srv/hunt", "claude"),
            s("5", "/srv/hunt", "codex"),
        ];
        name_the_bare(&mut v);
        let names: Vec<&str> = v.iter().map(Session::display).collect();
        assert_eq!(
            names,
            vec![
                "train",
                "train 2",
                "Claude",
                "Terminal",
                "bobbing-wren",
                "hunt",
                "hunt 2"
            ]
        );
        // herdr's own name stays: it's how a session is addressed
        assert_eq!(v[0].name, "1");
        assert_eq!(folder_name("/Users/u"), None);
        assert_eq!(folder_name("-"), None);
        assert_eq!(folder_name("/root"), None);
        assert_eq!(folder_name("/home/u/x").as_deref(), Some("x"));
    }

    #[test]
    fn a_typed_command_failed_only_if_the_shell_names_it() {
        for line in [
            "zsh: command not found: crush",
            "bash: crush: command not found",
            "sh: 1: crush: not found",
            "fish: Unknown command: crush",
            "bash: /opt/crush: Permission denied",
        ] {
            let screen = format!("\n$ crush\n{line}\n$ \n");
            assert_eq!(couldnt_run(&screen, "crush"), Some(line), "{line}");
        }
        // someone else's noise from the shell's start, or just a slow start
        assert_eq!(couldnt_run("nvm: command not found\n$ ", "crush"), None);
        assert_eq!(couldnt_run("", "crush"), None);
        assert_eq!(couldnt_run("$ crush --help\n", "crush"), None);
    }

    #[test]
    fn a_pane_is_at_its_prompt_only_when_the_shell_has_the_foreground() {
        let info = |fg: Value, shell: Value| {
            json!({"result": {"process_info": {
                "foreground_process_group_id": fg, "shell_pid": shell
            }}})
        };
        assert!(prompt_in(&info(json!(46456), json!(46456))));
        // herdr's answer for a pane running Claude Code, as read on a real one
        assert!(!prompt_in(&info(json!(47186), json!(46456))));
        assert!(!prompt_in(&info(Value::Null, json!(46456))));
        assert!(!prompt_in(&info(Value::Null, Value::Null)));
        assert!(!prompt_in(&json!({"error": {"code": "pane_not_found"}})));
    }

    #[test]
    fn saved_machines_are_read_whatever_the_shape() {
        // herdr 0.9.5
        let v: Value = serde_json::from_str(
            r#"[{"id":"7b76","label":"b31","target":"b31","session":"default","enabled":true},
               {"id":"3124","label":"dex42","target":"dex42.lan"}]"#,
        )
        .unwrap();
        let n = saved_names_in(&v);
        assert!(
            ["b31", "dex42", "dex42.lan", "7b76"]
                .iter()
                .all(|x| n.iter().any(|y| y == x)),
            "{n:?}"
        );
        // older
        let v: Value = serde_json::from_str(
            r#"{"result":{"machines":[{"id":"9","label":"gpu2","ssh_target":"gpu2"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            saved_names_in(&v),
            vec!["gpu2".to_string(), "9".to_string()]
        );
    }
    #[test]
    fn server_status_json() {
        let up = r#"{"status":"running","running":true,"version":"0.9.5","protocol":22,"compatible":true,"restart_needed":false,"server_binary_stale":true}"#;
        let s = ServerStatus::parse(up).unwrap();
        assert!(s.running && s.binary_stale && !s.restart_needed);
        assert_eq!(s.compatible, Some(true));
        assert_eq!(s.version.as_deref(), Some("0.9.5"));
        let down = r#"{"status":"not_running","running":false,"version":null,"compatible":null,"restart_needed":false}"#;
        let s = ServerStatus::parse(down).unwrap();
        assert!(!s.running && s.compatible.is_none());
        assert_eq!(ServerStatus::parse("status: running"), None);
    }

    #[test]
    fn approve_picks_yes_not_option_one() {
        let numbered = " Do you want to run this command?\n   cargo bench -p parser\n\n ❯ 1. Yes\n   2. Yes, and don't ask again\n   3. No, and tell Claude what to do differently (esc)\n";
        assert_eq!(approve_keys(numbered), Some(vec!["1".to_string()]));
        let no_first = " ❯ 1. No\n   2. Yes, proceed\n";
        assert_eq!(approve_keys(no_first), Some(vec!["2".to_string()]));
        // Claude Code's folder-trust prompt: an arrow menu with "No, exit" first
        let trust = " Accessing workspace:\n\n /private/tmp\n\n Quick safety check: Is this a project you trust?\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm · Esc to cancel\n";
        assert_eq!(
            approve_keys(trust),
            Some(vec!["down".to_string(), "enter".to_string()])
        );
        let on_yes = " ❯ Yes, allow\n   No\n";
        assert_eq!(approve_keys(on_yes), Some(vec!["enter".to_string()]));
        let up = "   Yes\n   Maybe\n ❯ No\n";
        assert_eq!(
            approve_keys(up),
            Some(vec![
                "up".to_string(),
                "up".to_string(),
                "enter".to_string()
            ])
        );
        assert_eq!(approve_keys("What should I name the file?"), None);
        assert_eq!(approve_keys(" ❯ 1. Red\n   2. Blue\n"), None);
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn parses_sessions_and_picks() {
        let agents = json!({"result":{"agents":[
            {"pane_id":"w1:p3","tab_id":"w1:t3","agent":"claude","agent_status":"working","cwd":"/h","foreground_cwd":"/srv/px","tokens":{"mark":"◆","hue":"amber"},
             "terminal_title_stripped":"Fix flaky tests","name":"map3"},
            {"pane_id":"w1:p2","tab_id":"w1:t2","agent":"Claude Code","status":"blocked","cwd":"/h",
             "agent_session":{"agent":"claude","kind":"id"}},
            {"pane_id":"w1:p9","tab_id":"w1:t1","agent":"codex","agent_status":"idle"}]}});
        let tabs = json!({"result":{"tabs":[{"tab_id":"w1:t2","label":"claude_temper"},{"tab_id":"w1:t3","label":"claude_map3:px"},{"tab_id":"w1:t1","label":"codex_x"}]}});
        let v = parse_sessions(&agents, &tabs, &Filter::Only(vec!["claude"]));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].label, "claude_map3:px");
        assert_eq!(
            (v[0].cwd.as_str(), v[0].mark.as_str(), v[0].hue.as_str()),
            ("/srv/px", "◆", "amber")
        );
        assert_eq!(
            (v[0].title.as_str(), v[0].name.as_str()),
            ("Fix flaky tests", "map3")
        );
        assert_eq!(
            (v[1].status.as_str(), v[1].mark.as_str(), v[1].name.as_str()),
            ("blocked", "·", "temper")
        );
        assert_eq!(pick(&v, "2").unwrap().pane_id, "w1:p2");
        assert_eq!(pick(&v, "temper").unwrap().pane_id, "w1:p2");
        assert_eq!(pick(&v, "map3").unwrap().pane_id, "w1:p3");
        assert_eq!(pick(&v, "claude_map3:px").unwrap().pane_id, "w1:p3");
        assert!(pick(&v, "3").is_none() && pick(&v, "0").is_none() && pick(&v, "zzz").is_none());
        let all = parse_sessions(&agents, &tabs, &Filter::All);
        assert_eq!(all.len(), 3);
        assert!(all.iter().any(|s| s.agent == "codex"));
        // "Claude Code" is the same agent as "claude"
        assert!(all.iter().filter(|s| s.agent == "claude").count() == 2);
        let codex = parse_sessions(&agents, &tabs, &Filter::Only(vec!["codex"]));
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].label, "codex_x");
    }

    #[test]
    fn plain_shells_are_sessions_too() {
        // `api snapshot`: an agent's tab, a shell Aleph started, a pane of someone else's
        let agents = serde_json::json!({"agents": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "agent_status": "idle"}
        ]});
        let tabs = serde_json::json!({
            "tabs": [
                {"tab_id": "w1:t1", "label": "claude_ignite"},
                {"tab_id": "w1:t2", "label": "shell_frosting-crane:px"},
                {"tab_id": "w1:t3", "label": "scratch"}
            ],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1", "cwd": "/srv/a"},
                {"pane_id": "w1:p2", "tab_id": "w1:t2", "cwd": "/srv/px", "foreground_cwd": "/srv/px/src"},
                {"pane_id": "w1:p3", "tab_id": "w1:t3", "cwd": "/tmp"}
            ]
        });
        let v = parse_sessions(&agents, &tabs, &Filter::Only(vec!["claude"]));
        assert_eq!(v.len(), 3, "{v:?}");
        // a pane in a tab that isn't Aleph's is a shell too, marked as such
        let other = v.iter().find(|s| s.pane_id == "w1:p3").unwrap();
        assert!(other.plain && other.agent == crate::agents::SHELL);
        assert_eq!(other.name, "scratch");
        let sh = v.iter().find(|s| s.pane_id == "w1:p2").unwrap();
        assert!(!sh.plain);
        assert_eq!((sh.pane_id.as_str(), sh.status.as_str()), ("w1:p2", "idle"));
        assert_eq!(
            (sh.name.as_str(), sh.display()),
            ("frosting-crane", "frosting-crane")
        );
        assert_eq!(sh.cwd, "/srv/px/src");
    }

    #[test]
    fn a_panes_foreground_is_its_command_or_nothing() {
        let info = |fg: u64, procs: Value| {
            json!({"result": {"process_info": {
                "foreground_process_group_id": fg, "shell_pid": 10, "foreground_processes": procs
            }}})
        };
        assert_eq!(
            foreground_of(&info(10, json!([]))),
            Some(None),
            "its prompt"
        );
        assert_eq!(foreground_of(&json!({"result": {}})), None, "says nothing");
        // the group's leader, by its command line
        let v = info(
            20,
            json!([
                {"pid": 21, "cmdline": "rustc --edition"},
                {"pid": 20, "cmdline": "cargo build --release"}
            ]),
        );
        assert_eq!(
            foreground_of(&v),
            Some(Some("cargo build --release".into()))
        );
        // no command line: its argv, then its name, then that something runs
        let v = info(20, json!([{"pid": 20, "argv": ["sleep", "60"]}]));
        assert_eq!(foreground_of(&v), Some(Some("sleep 60".into())));
        let v = info(20, json!([{"pid": 20, "name": "node"}]));
        assert_eq!(foreground_of(&v), Some(Some("node".into())));
        assert_eq!(
            foreground_of(&info(20, json!([]))),
            Some(Some("a command".into()))
        );
        // long ones stop, and a key on the command line never gets further
        let v = info(20, json!([{"pid": 20, "cmdline": "x".repeat(500)}]));
        assert_eq!(foreground_of(&v).flatten().unwrap().chars().count(), 240);
        crate::log::mask("not-a-real-secret-value");
        let v = info(
            20,
            json!([{"pid": 20, "cmdline": "deploy --token not-a-real-secret-value"}]),
        );
        assert!(!foreground_of(&v)
            .flatten()
            .unwrap()
            .contains("not-a-real-secret-value"));
    }

    #[test]
    fn an_agent_left_out_of_the_list_is_never_taken_for_a_shell() {
        let agents = serde_json::json!({"agents": [
            {"pane_id": "w1:p1", "tab_id": "w1:t1", "agent": "claude", "agent_status": "idle"},
            {"pane_id": "w1:p2", "tab_id": "w1:t1", "agent": "codex", "agent_status": "idle"}
        ]});
        let tabs = serde_json::json!({
            "tabs": [{"tab_id": "w1:t1", "label": "work"}],
            "panes": [
                {"pane_id": "w1:p1", "tab_id": "w1:t1"},
                {"pane_id": "w1:p2", "tab_id": "w1:t1", "agent": "codex"},
                {"pane_id": "w1:p3", "tab_id": "w1:t1", "restore_error": "cwd gone"},
                {"pane_id": "w1:p4", "tab_id": "w1:t9"}
            ]
        });
        let v = parse_sessions(&agents, &tabs, &Filter::Only(vec!["claude"]));
        let panes: Vec<&str> = v.iter().map(|s| s.pane_id.as_str()).collect();
        assert!(!panes.contains(&"w1:p2"), "{panes:?}");
        // beside an agent in its tab: a shell, with why it didn't come back
        let sh = v.iter().find(|s| s.pane_id == "w1:p3").unwrap();
        assert_eq!(sh.restore_error.as_deref(), Some("cwd gone"));
        // its tab unnamed: it's known by its id, and goes by what it is
        let bare = v.iter().find(|s| s.pane_id == "w1:p4").unwrap();
        assert_eq!((bare.name.as_str(), bare.display()), ("w1:p4", "Terminal"));
        // a machine's standing terminal is known by its name
        let standing = Session {
            label: "shell_ready:home".into(),
            ..Default::default()
        };
        assert_eq!(
            names::from_label(&standing.label),
            crate::agents::READY_SHELL
        );
    }

    #[test]
    fn host_prefixed_pick() {
        let mut a = Session {
            host: "b31".into(),
            pane_id: "w1:p1".into(),
            label: "claude_x".into(),
            name: "x".into(),
            ..Default::default()
        };
        let mut b = a.clone();
        b.host = "gpu2".into();
        b.pane_id = "w1:p9".into();
        a.status = "idle".into();
        let v = vec![a, b];
        assert_eq!(pick(&v, "gpu2:x").unwrap().pane_id, "w1:p9");
        assert_eq!(pick(&v, "x").unwrap().pane_id, "w1:p1");
    }

    #[test]
    fn the_openrouter_key_rides_only_the_socket_request() {
        let key = "sk-or-v1-feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface00";
        let leaf = Leaf {
            cwd: Some("/srv/px".into()),
            env: vec![("RUST_LOG".into(), "info".into())],
            openrouter: true,
        };
        let tree = layouts::grid(2).unwrap().map(&mut |_| leaf.clone());
        // the one JSON line herdr's socket gets: there, beside the plain env
        let j = layouts::to_herdr_with(&tree, Some((crate::openrouter::ENV, key)));
        assert_eq!(j["first"]["env"]["OPENROUTER_API_KEY"], key);
        assert_eq!(j["second"]["env"]["RUST_LOG"], "info");
        // what sock.rs writes down about it: the method, nothing else
        let line = crate::sock::line("aleph-1", "layout.apply", &json!({"root": j}));
        assert!(line.contains(key)); // (the wire itself)
                                     // every command line the CLI fallback would run: never
        let o = NewOpts {
            dir: Some(b"/srv/px".to_vec()),
            env: leaf.env.clone(),
            ..Default::default()
        };
        let mut argv: Vec<OsString> = tab_opts(&o, "opencode_ignite");
        for l in tree.leaves() {
            argv.extend(split_args("w1:p1", "right", 0.5, Some(b"/srv/px"), &l.env));
        }
        let joined: Vec<String> = argv
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(joined.iter().any(|a| a == "RUST_LOG=info"));
        assert!(joined
            .iter()
            .all(|a| !a.contains(key) && !a.contains("OPENROUTER")));
        // and a failure that quotes it back is masked once the key is known
        crate::log::mask(key);
        let f = crate::sock::Fail::Refused {
            code: "invalid_env".into(),
            message: format!("bad env {key}"),
        }
        .masked();
        assert!(!f.to_string().contains(key), "{f}");
    }

    #[test]
    fn wrapped_agents_keep_their_kind() {
        assert_eq!(
            Herdr::wrapped_command("claude", " nono run -- ", "claude --resume x"),
            "HERDR_AGENT=claude nono run -- claude --resume x"
        );
    }

    #[test]
    fn herdr_counters_and_native_ids() {
        let agents = serde_json::json!({"result": {"agents": [
            {"pane_id": "w1:p2", "tab_id": "w1:t2", "agent": "claude", "agent_status": "blocked",
             "state_change_seq": 41, "completion_seq": 7, "interactive_ready": true,
             "launch_pending": true,
             "agent_session": {"agent": "claude", "kind": "id", "source": "x", "value": "5f0c-uuid"}}
        ]}});
        let v = parse_sessions(
            &agents,
            &serde_json::json!({}),
            &crate::agents::Filter::parse("all").unwrap(),
        );
        assert_eq!((v[0].seq, v[0].done_seq, v[0].ready), (41, 7, Some(true)));
        assert!(v[0].launch_pending);
        assert_eq!(v[0].native_id.as_deref(), Some("5f0c-uuid"));
        assert_eq!(v[0].state_key(), "blocked#41");
        let mut w = v[0].clone();
        w.status = "working".into();
        assert_eq!(w.state_key(), "working");
        w.status = "blocked".into();
        w.seq = 0;
        assert_eq!(w.state_key(), "blocked");
    }
}
