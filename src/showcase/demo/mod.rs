//! The demo world (`aleph showcase on --demo`): a made-up fleet that stands
//! in for the real one behind typed seams (`showcase.rs`). The world is a
//! pure function of (seed, time, actions) — `world::World::at` — so the
//! daemon, the dashboard and each CLI run agree without sharing memory;
//! what people do to it (approve, send, new, close, rename) is appended to
//! `<state>/showcase/actions.jsonl`, which every process reads.
//!
//! Nothing real is ever reached: ssh and herdr point at a path that can't
//! exist (`DEAD`), herdr calls the world doesn't answer are refused, and the
//! state the world writes goes to `<state|cache>/showcase/`.

pub mod browse;
pub mod clock;
pub mod machines;
pub mod screens;
pub mod stats;
pub mod story;
pub mod usage;
pub mod world;

#[cfg(test)]
mod tests;

use super::state;
use crate::herdr::{NewOpts, ServerStatus, Session, Started, Workspace, Worktree};
use serde_json::{json, Value};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use world::{Act, Action, World};

/// What ssh and herdr are in the demo world: nothing can run from here.
pub const DEAD: &str = "/dev/null/aleph-demo-world";
/// A herdr call the world has no answer for.
pub const NOT_HERE: &str = "not in this world";
/// Something the demo world won't do.
pub const REFUSED: &str =
    "not in the demo world — nothing real is reached there (aleph showcase off goes back)";
const FIXED: &str = "the demo world's machines are fixed (aleph showcase off goes back to yours)";
const BLOCKED: &str = "it's waiting on a question — answer with keys (1, 2, enter, esc)";
const STORY_DECIDES: &str =
    "the demo world's story decides it while it plays (showcase.story off hands it back)";
/// What herdr says where the story stopped it (`fleet::not_running` knows it).
const STOPPED: &str = "no herdr server is running (server_not_running)";

// ------------------------------------------------------------------ settings, files

/// The demo world's machines, in order.
pub fn aliases() -> Vec<String> {
    world::MACHINES
        .iter()
        .map(|m| m.alias.to_string())
        .collect()
}

/// Is `key` one the demo world decides? (Asked first: settings are read
/// often, and this is cheaper than looking at the mode.)
pub fn decides(key: &str) -> bool {
    matches!(
        key,
        "hosts"
            | "ssh_host"
            | "machine"
            | "hub.machine"
            | "local"
            | "listen"
            | "auto_heal"
            | "auto_approve"
            | "analytics.enabled"
            | "herdr_via"
    ) || key.starts_with("host.")
}

/// The settings the demo world decides (None: the files do).
pub fn setting(key: &str) -> Option<Option<String>> {
    let on = |v: &str| Some(Some(v.to_string()));
    match key {
        "hosts" => Some(Some(aliases().join(","))),
        "ssh_host" => Some(world::MACHINES.first().map(|m| m.alias.to_string())),
        "machine" | "hub.machine" => Some(None),
        // no `local`: this computer's herdr is real
        "local" | "listen" => on("false"),
        // the story's drop is healed by the hub (simulated: `heal`), and its
        // question answered by the world's own rule; without it, no self-heal
        // and your own auto-approve setting
        "auto_heal" => on(if story_has(story::DROP) {
            "true"
        } else {
            "false"
        }),
        "auto_approve" => story_has(story::ASK).then(|| Some("true".to_string())),
        "analytics.enabled" => on("true"),
        "herdr_via" => on("ssh"),
        _ => {
            let rest = key.strip_prefix("host.")?;
            let (alias, field) = rest.rsplit_once('.')?;
            let m = world::machine(alias);
            // a real machine's settings never reach the world's
            Some(match (m, field) {
                (Some(m), "color") => Some(m.color.to_string()),
                _ => None,
            })
        }
    }
}

/// Settings that would change the fleet can't change in the demo world.
pub fn refuse_setting(key: &str) -> Option<String> {
    if (key == "auto_heal" && story_has(story::DROP))
        || (key == "auto_approve" && story_has(story::ASK))
    {
        return Some(STORY_DECIDES.to_string());
    }
    (matches!(key, "hosts" | "ssh_host" | "machine") || key.starts_with("host."))
        .then(|| FIXED.to_string())
}

/// Does the story play this beat now?
fn story_has(b: u8) -> bool {
    story::current().is_some_and(|s| s.has(b))
}

/// The world's own auto-approve rules: the story's, which answers its soak
/// test (and nothing else the world asks).
pub fn rules() -> Vec<(String, String)> {
    if !story_has(story::ASK) {
        return Vec::new();
    }
    vec![(story::RULE.0.to_string(), story::RULE.1.to_string())]
}

/// Does the hub's self-heal run here? (Only for the story's drop: what it
/// restarts is the world's own herdr, `heal`.)
pub fn heals() -> bool {
    story_has(story::DROP)
}

/// The world's clock and its story now (`aleph showcase status`): its time
/// and speed, and — while the story plays — its cycle, where it is in it,
/// the beat on now and the plan (seconds of the world's time).
pub fn flow_json() -> Value {
    let t = clock();
    let mut v = json!({"time": t, "warp": clock::warp()});
    if let Some(s) = story::current() {
        let (k, p) = s.place(t);
        v["story"] = json!({
            "every": s.every(),
            "every_real": clock::real_secs(s.every()),
            "cycle": k,
            "at": p,
            "beats": story::BEATS.iter().filter(|(_, b)| s.has(*b)).map(|(n, _)| *n).collect::<Vec<_>>(),
            "now": s.beat_now(t).map(|(n, left)| json!({"beat": n, "left": left})),
            "plan": s.plan().iter().map(|(n, a, z)| json!({"beat": n, "from": a, "until": z})).collect::<Vec<_>>(),
        });
    }
    v
}

/// How long the hub counts a usage limit hit as told (real seconds, `secs`
/// as ever): the story hits its own every cycle, each one news (half a
/// cycle: a hit seen again — the daemon started again — is the same one).
pub fn told_for(secs: u64) -> u64 {
    match story::current() {
        Some(s) if s.has(story::LIMIT) => clock::real_secs(s.every() / 2).max(10),
        _ => secs,
    }
}

/// `heal_now` in the demo world: herdr "restarts" where the story stopped
/// it (the action log says when, so every process sees it come back).
pub fn heal(host: &str) -> Result<String, String> {
    let m = world::machine(host).ok_or_else(|| NOT_HERE.to_string())?;
    let w = world();
    if !w.link_up(host) {
        return Err(m.down_text());
    }
    if w.herdr_up(host) {
        return Err(format!("herdr is already running on {host}"));
    }
    act(host, "", Act::Heal)?;
    let n = w.on(host).count();
    Ok(format!(
        "herdr restarted; {n} session{} restored",
        if n == 1 { "" } else { "s" }
    ))
}

/// `<dir>/showcase`: the world's own copy of what it writes.
pub fn sandbox(dir: PathBuf) -> PathBuf {
    dir.join("showcase")
}

/// Is `w` one of the demo world's own words — a machine, a user, a folder,
/// or a part of one? The lens leaves those as they are: they're made up.
pub fn owns(w: &str) -> bool {
    use std::collections::HashSet;
    static WORDS: OnceLock<HashSet<String>> = OnceLock::new();
    let words = WORDS.get_or_init(|| {
        let mut s = HashSet::new();
        let mut put = |x: &str| {
            s.insert(x.to_lowercase());
            for p in x.split(['-', '_', '.', '/']) {
                if p.len() >= 3 {
                    s.insert(p.to_lowercase());
                }
            }
        };
        for m in world::MACHINES {
            put(m.alias);
            put(m.user);
        }
        for b in world::BASE {
            put(b.dir);
        }
        s
    });
    words.contains(&w.to_lowercase())
}

/// The demo world went on or off since this process first asked.
pub fn world_changed() -> bool {
    static FIRST: OnceLock<bool> = OnceLock::new();
    let now = state::mode() == state::Mode::Demo;
    *FIRST.get_or_init(|| now) != now
}

/// exec this program again with the same arguments (the other world).
pub fn restart() {
    use std::os::unix::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    crate::log::info("showcase", "the demo world switched — starting again");
    // the same screen, a moment later: no splash this time
    let e = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env("ALEPH_NO_SPLASH", "1")
        .exec();
    crate::log::warn("showcase", &format!("couldn't start again: {e}"));
}

// ------------------------------------------------------------------ the action log

thread_local! {
    /// tests: an action log of their own
    static LOG: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn set_log(p: Option<PathBuf>) {
    LOG.with(|l| *l.borrow_mut() = p);
}

pub fn log_path() -> PathBuf {
    LOG.with(|l| l.borrow().clone())
        .unwrap_or_else(|| sandbox(crate::config::state_dir()).join("actions.jsonl"))
}

fn to_json(a: &Action) -> Value {
    let mut v = json!({"t": a.t, "host": a.host, "pane": a.pane});
    match &a.act {
        Act::Approve => v["do"] = json!("approve"),
        Act::Heal => v["do"] = json!("heal"),
        Act::Close => v["do"] = json!("close"),
        Act::Send(t) => {
            v["do"] = json!("send");
            v["text"] = json!(t);
        }
        Act::Rename { name, label } => {
            v["do"] = json!("rename");
            v["name"] = json!(name);
            v["label"] = json!(label);
        }
        Act::New {
            tab,
            agent,
            name,
            label,
            dir,
            prompt,
        } => {
            v["do"] = json!("new");
            v["tab"] = json!(tab);
            v["agent"] = json!(agent);
            v["name"] = json!(name);
            v["label"] = json!(label);
            v["dir"] = json!(dir);
            v["prompt"] = json!(prompt);
        }
    }
    v
}

pub fn parse(line: &str) -> Option<Action> {
    let v: Value = serde_json::from_str(line).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
    let act = match s("do")?.as_str() {
        "approve" => Act::Approve,
        "heal" => Act::Heal,
        "close" => Act::Close,
        "send" => Act::Send(s("text")?),
        "rename" => Act::Rename {
            name: s("name"),
            label: s("label"),
        },
        "new" => Act::New {
            tab: s("tab")?,
            agent: s("agent")?,
            name: s("name")?,
            label: s("label")?,
            dir: s("dir").unwrap_or_default(),
            prompt: s("prompt"),
        },
        _ => return None,
    };
    Some(Action {
        t: v.get("t")?.as_u64()?,
        host: s("host")?,
        pane: s("pane")?,
        act,
    })
}

/// Append one action: a single short line in one write (O_APPEND keeps it whole).
pub fn log(a: &Action) -> Result<(), String> {
    let p = log_path();
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let mut line = to_json(a).to_string();
    line.push('\n');
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
        .and_then(|mut f| f.write_all(line.as_bytes()))
        .map_err(|e| format!("couldn't keep it: {e}"))
}

/// Everything done to the world so far, oldest first (re-read when the
/// file changed).
pub fn actions() -> Arc<Vec<Action>> {
    type Cache = Option<(PathBuf, Option<SystemTime>, u64, Arc<Vec<Action>>)>;
    static C: Mutex<Cache> = Mutex::new(None);
    let p = log_path();
    let meta = std::fs::metadata(&p).ok();
    let (mtime, len) = (
        meta.as_ref().and_then(|m| m.modified().ok()),
        meta.as_ref().map_or(0, |m| m.len()),
    );
    let mut c = C.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((cp, ct, cl, v)) = c.as_ref() {
        if *cp == p && *ct == mtime && *cl == len {
            return v.clone();
        }
    }
    let v: Vec<Action> = std::fs::read_to_string(&p)
        .unwrap_or_default()
        .lines()
        .filter_map(parse)
        .collect();
    let v = Arc::new(v);
    *c = Some((p, mtime, len, v.clone()));
    v
}

// ------------------------------------------------------------------ the world now

pub fn seed() -> u64 {
    state::seed()
}

/// Real time: what the world keeps (usage records, history) is stamped with it.
pub fn now() -> u64 {
    crate::cache::now()
}

/// The world's own time (`showcase.warp` runs it faster): what it does — its
/// sessions' scripts, the story's beats, the action log — runs on it.
pub fn clock() -> u64 {
    clock::now()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The world this second of its clock (worked out once per second, action
/// log and story).
pub fn world() -> Arc<World> {
    type Cache = Option<(u64, u64, usize, Option<story::Story>, Arc<World>)>;
    static C: Mutex<Cache> = Mutex::new(None);
    let (seed, t, acts, st) = (seed(), clock(), actions(), story::current());
    let key = Arc::as_ptr(&acts) as usize;
    let mut c = C.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((s, ct, k, cs, w)) = c.as_ref() {
        if *s == seed && *ct == t && *k == key && *cs == st {
            return w.clone();
        }
    }
    let w = Arc::new(World::script(seed, t, &acts, st));
    *c = Some((seed, t, key, st, w.clone()));
    w
}

/// Does `m` answer ssh now (its link)?
pub fn link_up(m: &world::Machine) -> bool {
    world().link_up(m.alias)
}

/// The machine, when its herdr answers now.
fn up_now(host: &str) -> Result<&'static world::Machine, String> {
    let m = world::machine(host).ok_or_else(|| NOT_HERE.to_string())?;
    let w = world();
    if !w.link_up(host) {
        return Err(m.down_text());
    }
    if !w.herdr_up(host) {
        return Err(STOPPED.to_string());
    }
    Ok(m)
}

pub fn up(host: &str) -> Result<(), String> {
    up_now(host).map(|_| ())
}

fn act(host: &str, pane: &str, a: Act) -> Result<(), String> {
    log(&Action {
        t: clock(),
        host: host.to_string(),
        pane: pane.to_string(),
        act: a,
    })
}

// ------------------------------------------------------------------ herdr, simulated

fn to_session(s: &world::Sess) -> Session {
    let shell = s.agent == crate::agents::SHELL;
    // at its limit: what the fleet's limit scan reads on its screen, at once
    // (the scan looks every 30 s; the story's limit may be shorter)
    let limit = (s.at.phase == world::Phase::Limit)
        .then(|| world::machine(&s.host))
        .flatten()
        .and_then(|m| {
            crate::limits::detect(&screens::plain(&screens::lines(s, m, 0, 120).join("\n")))
        });
    Session {
        host: s.host.clone(),
        pane_id: s.pane.clone(),
        tab_id: s.tab.clone(),
        workspace_id: s.ws.clone(),
        terminal_id: format!("term-{}", s.pane.replace(':', "-")),
        label: s.label.clone(),
        name: s.name.clone(),
        agent: s.agent.clone(),
        status: s.status().to_string(),
        cwd: s.cwd.clone(),
        title: s.title(),
        mark: s.mark.clone(),
        hue: s.hue.clone(),
        focused: false,
        since: None,
        limit,
        seq: s.at.seq,
        done_seq: s.at.done_seq,
        compacting: false,
        compacted: false,
        plain: false,
        restore_error: None,
        ready: (!shell).then(|| s.status() != "working"),
        launch_pending: false,
        shown: String::new(),
        native_id: (!shell).then(|| world::conversation(&s.host, &s.pane)),
    }
}

pub fn sessions(host: &str, f: &crate::agents::Filter) -> Result<Vec<Session>, String> {
    let m = up_now(host)?;
    // a round trip like a real link's (polls run off the UI thread)
    if !cfg!(test) {
        std::thread::sleep(Duration::from_millis(m.rtt));
    }
    let w = world();
    let mut v: Vec<Session> = w
        .on(host)
        .filter(|s| s.agent == crate::agents::SHELL || f.matches(&s.agent))
        .map(to_session)
        .collect();
    v.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(v)
}

fn screen_lines(host: &str, pane: &str, cols: u16) -> Result<Vec<String>, String> {
    let m = up_now(host)?;
    let w = world();
    let s = w
        .find(host, pane)
        .ok_or_else(|| format!("pane {pane} not found"))?;
    Ok(screens::lines(s, m, now_ms(), cols))
}

pub fn read(
    host: &str,
    pane: &str,
    source: &str,
    lines: usize,
    ansi: bool,
) -> Result<String, String> {
    if source == "detection" {
        up_now(host)?;
        let w = world();
        let s = w
            .find(host, pane)
            .ok_or_else(|| format!("pane {pane} not found"))?;
        return Ok(screens::detection(s));
    }
    let mut v = screen_lines(host, pane, 100)?;
    let keep = match (source, lines) {
        (_, n) if n > 0 => n,
        ("visible", _) => 40,
        _ => v.len(),
    };
    v.drain(..v.len().saturating_sub(keep));
    let t = v.join("\n");
    Ok(if ansi { t } else { screens::plain(&t) })
}

/// A live tile's frame at `size` (cols, rows).
pub fn screen(host: &str, pane: &str, size: Option<(u16, u16)>) -> Result<String, String> {
    let (cols, rows) = size.unwrap_or((100, 40));
    let mut v = screen_lines(host, pane, cols)?;
    v.drain(..v.len().saturating_sub(rows.max(1) as usize));
    Ok(v.join("\n"))
}

fn find_now(host: &str, target: &str) -> Result<world::Sess, String> {
    up_now(host)?;
    world()
        .find(host, target)
        .cloned()
        .ok_or_else(|| format!("no agent '{target}' on {host}"))
}

pub fn keys(host: &str, target: &str, keys: &[&str]) -> Result<(), String> {
    let s = find_now(host, target)?;
    // any answer to the question moves it on
    if s.status() == "blocked" && !keys.is_empty() {
        return act(host, &s.pane, Act::Approve);
    }
    Ok(())
}

pub fn send(host: &str, target: &str, text: &str, submit: bool) -> Result<(), String> {
    let s = find_now(host, target)?;
    let blocked = s.status() == "blocked";
    if submit {
        if blocked {
            return Err(BLOCKED.into());
        }
        if s.agent == crate::agents::SHELL {
            return Ok(());
        }
        return act(host, &s.pane, Act::Send(text.trim().to_string()));
    }
    // typed: a number or y answers the question
    let t = text.trim().to_lowercase();
    if blocked && (t.chars().all(|c| c.is_ascii_digit()) && !t.is_empty() || t == "y") {
        return act(host, &s.pane, Act::Approve);
    }
    Ok(())
}

pub fn close(host: &str, id: &str) -> Result<(), String> {
    let s = find_now(host, id)?;
    let what = if s.tab == id { &s.tab } else { &s.pane };
    act(host, what, Act::Close)
}

pub fn rename(host: &str, target: &str, to: &str, tab: bool) -> Result<(), String> {
    let s = find_now(host, target)?;
    let a = if tab {
        Act::Rename {
            name: None,
            label: Some(to.to_string()),
        }
    } else {
        Act::Rename {
            name: Some(to.to_string()),
            label: None,
        }
    };
    act(host, &s.pane, a)
}

pub fn new_tab(host: &str, o: &NewOpts) -> Result<(Vec<Started>, Vec<String>), String> {
    let m = up_now(host)?;
    let kind = o.agent_id();
    let w = world();
    let mut live: Vec<String> = o.live.clone();
    live.extend(w.sessions.iter().map(|s| s.name.clone()));
    let folder = o.dir.as_deref().map(|d| {
        let b = crate::path::basename(d);
        if b.is_empty() {
            "root".to_string()
        } else {
            crate::path::show(b)
        }
    });
    let t = now();
    let name = match &o.name {
        Some(n) => crate::names::numbered(n, &live),
        None if o.naming == "folder" && folder.is_some() => {
            crate::names::from_folder(folder.as_deref().unwrap_or("root"), &live)
        }
        None if o.naming == "story" => crate::names::story(
            &kind,
            crate::names::land_words(Some(m.color), host),
            &live,
            &o.recent,
            world::hash(&[host, &t.to_string(), "new"]),
        ),
        None => crate::names::pick(&live, &o.recent, world::hash(&[host, &t.to_string()])),
    };
    let prefix = if o.prefix.is_empty() {
        format!("{kind}_")
    } else {
        o.prefix.clone()
    };
    let mut label = format!("{prefix}{name}");
    if let Some(f) = &folder {
        label = format!("{label}:{f}");
    }
    let ws = o
        .workspace
        .clone()
        .filter(|x| x.starts_with('w'))
        .unwrap_or_else(|| "w1".into());
    let mut n = 100 + world::hash(&[host, &name, &t.to_string()]) % 800;
    while w.on(host).any(|s| s.pane == format!("{ws}:p{n}")) {
        n += 1;
    }
    let (pane, tab) = (format!("{ws}:p{n}"), format!("{ws}:t{n}"));
    let dir = o
        .dir
        .as_deref()
        .map(crate::path::show)
        .unwrap_or_else(|| m.home());
    act(
        host,
        &pane,
        Act::New {
            tab,
            agent: kind.clone(),
            name: name.clone(),
            label: label.clone(),
            dir: dir.clone(),
            prompt: o.prompt.clone(),
        },
    )?;
    let (hue, mark) = crate::theme::pick_hue(&[], world::hash(&[&format!("{host}|{pane}"), "hue"]));
    Ok((
        vec![Started {
            name,
            label,
            pane: pane.clone(),
            hue: hue.to_string(),
            mark: mark.to_string(),
            dir,
            note: None,
        }],
        vec![pane],
    ))
}

pub fn server_status(host: &str) -> Option<ServerStatus> {
    let m = up_now(host).ok()?;
    Some(ServerStatus {
        running: true,
        version: Some(m.herdr.to_string()),
        compatible: Some(true),
        restart_needed: false,
        binary_stale: false,
    })
}

pub fn herdr_version(host: &str) -> Option<String> {
    up_now(host).ok().map(|m| m.herdr.to_string())
}

pub fn herdr_info(host: &str) -> Option<(String, String)> {
    world::machine(host).map(|m| (m.herdr.to_string(), m.os_line()))
}

pub fn workspaces(host: &str) -> Result<Vec<Workspace>, String> {
    up_now(host)?;
    let w = world();
    let mut out: Vec<Workspace> = Vec::new();
    for s in w.on(host) {
        if let Some(x) = out.iter_mut().find(|x| x.id == s.ws) {
            x.tabs += 1;
            continue;
        }
        out.push(Workspace {
            id: s.ws.clone(),
            label: s.cwd.rsplit('/').next().unwrap_or("").to_string(),
            number: s.ws.trim_start_matches('w').parse().unwrap_or(1),
            tabs: 1,
            focused: out.is_empty(),
            status: s.status().to_string(),
        });
    }
    out.sort_by_key(|x| x.number);
    Ok(out)
}

pub fn worktrees(host: &str) -> Result<Vec<Worktree>, String> {
    up_now(host)?;
    let w = world();
    Ok(w.on(host)
        .filter(|s| s.agent != crate::agents::SHELL)
        .take(1)
        .map(|s| Worktree {
            branch: "main".into(),
            path: s.cwd.clone(),
            label: s.cwd.rsplit('/').next().unwrap_or("").to_string(),
            open_in: Some(s.ws.clone()),
            linked: false,
        })
        .collect())
}

pub fn tab_labels(host: &str) -> Vec<String> {
    if up_now(host).is_err() {
        return vec![];
    }
    world().on(host).map(|s| s.label.clone()).collect()
}

pub fn wait(
    host: &str,
    target: &str,
    until: &[String],
    timeout: Option<Duration>,
) -> Result<String, String> {
    let want: Vec<String> = if until.is_empty() {
        vec!["idle".into(), "done".into(), "blocked".into()]
    } else {
        until.to_vec()
    };
    let end = timeout.map(|t| Instant::now() + t);
    loop {
        let s = find_now(host, target)?;
        if want.iter().any(|w| w == s.status()) {
            return Ok(s.status().to_string());
        }
        if end.is_some_and(|e| Instant::now() >= e) {
            return Err(format!("timed out waiting for {target}"));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}
