//! A fake herdr socket server for the e2e fake world (`tests/e2e-fake`),
//! never part of the product: one listener per machine at `<dir>/<host>.sock`
//! (the fake `herdr status server` prints it; the fake `ssh -N -L` links a
//! forward to it), answering herdr 0.9.5's JSON-lines API for what Aleph
//! uses — `layout.export`, `layout.apply`, `pane.move`, `agent.view.set` /
//! `.clear`, `notification.show`, `events.subscribe` (started, then held
//! open; event lines a test writes to `$ST/sockevent_<host>` are streamed to
//! a subscriber), `ping` — one request per connection, like herdr. Every request is
//! logged to `$ST/log` as `sock <host> <method> <params>`, beside the fake
//! CLI's `herdr …` lines — a pane's `env` values as `…<length>` only, never
//! the value (it may be a secret: Aleph's OpenRouter key).
//!
//! State, shared with `tests/e2e-fake/bin/herdr`: `$ST/tabs_<host>.json`,
//! `$ST/agents_<host>.json`, `$ST/seq` (ids); its own: `$ST/layout_<host>_<tab>.json`
//! (each tab's tree, for `layout.export`), `$ST/panetab_<host>` (pane → tab,
//! for `herdr agent start`). `$ST/sockold_<host>` makes that machine answer
//! like an older herdr (layout and move methods unknown).
//!
//!   cargo build --example fake_herdr_sock
//!   fake_herdr_sock <socket dir> <host>…     (tests/e2e-fake/fakesock.sh starts it)
//!
//! It writes `$ST/fakesock.pid` and exits when `$ST` goes away or that file
//! stops naming it (removed, or a newer one started).

use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

static STATE: Mutex<()> = Mutex::new(());
static TAKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn st() -> PathBuf {
    PathBuf::from(std::env::var("ST").expect("ST must point at the fake state dir"))
}

fn log(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(st().join("log"))
    {
        let _ = writeln!(f, "{line}");
    }
}

fn load(p: &Path, empty: Value) -> Value {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(empty)
}

fn store(p: &Path, v: &Value) {
    let tmp = p.with_extension("tmp-sock");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

/// The next id number (`$ST/seq`, which the fake CLI counts with too).
fn next() -> u64 {
    let p = st().join("seq");
    let n = std::fs::read_to_string(&p)
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .unwrap_or(10)
        + 1;
    let _ = std::fs::write(&p, format!("{n}\n"));
    n
}

fn tabs_file(host: &str) -> PathBuf {
    st().join(format!("tabs_{host}.json"))
}
fn agents_file(host: &str) -> PathBuf {
    st().join(format!("agents_{host}.json"))
}
fn layout_file(host: &str, tab: &str) -> PathBuf {
    st().join(format!("layout_{host}_{}.json", tab.replace(':', "_")))
}

fn add_tab(host: &str, tab: &str, label: &str) {
    let p = tabs_file(host);
    let mut v = load(&p, json!({"result": {"tabs": []}}));
    if let Some(a) = v.pointer_mut("/result/tabs").and_then(Value::as_array_mut) {
        a.push(json!({"tab_id": tab, "label": label}));
    }
    store(&p, &v);
}

fn drop_tab(host: &str, tab: &str) {
    let p = tabs_file(host);
    let mut v = load(&p, json!({"result": {"tabs": []}}));
    if let Some(a) = v.pointer_mut("/result/tabs").and_then(Value::as_array_mut) {
        a.retain(|t| t.get("tab_id").and_then(Value::as_str) != Some(tab));
    }
    store(&p, &v);
}

/// Where a pane is: what we placed, else its agent's tab, else herdr's
/// numbering the fake CLI uses (`w1:pN` in `w1:tN`).
fn tab_of(host: &str, pane: &str) -> String {
    if let Ok(t) = std::fs::read_to_string(st().join(format!("panetab_{host}"))) {
        if let Some(tab) = t
            .lines()
            .rev()
            .find_map(|l| l.strip_prefix(&format!("{pane} ")))
        {
            return tab.trim().to_string();
        }
    }
    let agents = load(&agents_file(host), json!({}));
    if let Some(a) = agents.pointer("/result/agents").and_then(Value::as_array) {
        if let Some(t) = a
            .iter()
            .find(|x| x.get("pane_id").and_then(Value::as_str) == Some(pane))
            .and_then(|x| x.get("tab_id").and_then(Value::as_str))
        {
            return t.to_string();
        }
    }
    pane.replacen(":p", ":t", 1)
}

fn place(host: &str, pane: &str, tab: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(st().join(format!("panetab_{host}")))
    {
        let _ = writeln!(f, "{pane} {tab}");
    }
    // the agent in it (if any) is in that tab now
    let p = agents_file(host);
    let mut v = load(&p, json!({"result": {"agents": []}}));
    if let Some(a) = v
        .pointer_mut("/result/agents")
        .and_then(Value::as_array_mut)
    {
        for x in a.iter_mut() {
            if x.get("pane_id").and_then(Value::as_str) == Some(pane) {
                x["tab_id"] = json!(tab);
            }
        }
    }
    store(&p, &v);
}

/// `v` with every pane's environment value replaced by `…<length>` — like
/// herdr, the fake never writes a pane's launch environment down (it may
/// carry a secret: Aleph's OpenRouter key); the length shows it arrived.
fn env_masked(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| {
                    let x = match (k.as_str(), x) {
                        ("env", Value::Object(e)) => Value::Object(
                            e.iter()
                                .map(|(n, val)| {
                                    let len = val.as_str().map_or(0, |s| s.chars().count());
                                    (n.clone(), json!(format!("…{len}")))
                                })
                                .collect(),
                        ),
                        _ => env_masked(x),
                    };
                    (k.clone(), x)
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(env_masked).collect()),
        x => x.clone(),
    }
}

fn leaves(n: &Value, out: &mut Vec<Value>) {
    if let (Some(a), Some(b)) = (n.get("first"), n.get("second")) {
        leaves(a, out);
        leaves(b, out);
    } else {
        out.push(n.clone());
    }
}

/// Give every pane of `n` a new id (and put it in `tab`).
fn number(host: &str, tab: &str, n: &mut Value) -> Result<(), String> {
    if n.get("first").is_some() || n.get("second").is_some() {
        let d = n.get("direction").and_then(Value::as_str).unwrap_or("");
        if d != "right" && d != "down" {
            return Err(format!(
                "invalid request: unknown variant `{d}`, expected `right` or `down`"
            ));
        }
        for k in ["first", "second"] {
            let c = n
                .get_mut(k)
                .ok_or("invalid request: a split needs first and second")?;
            number(host, tab, c)?;
        }
        return Ok(());
    }
    if n.get("type").and_then(Value::as_str) != Some("pane") {
        return Err("invalid request: a node is a pane or a split".into());
    }
    let id = format!("w1:p{}", next());
    place(host, &id, tab);
    n["pane_id"] = json!(id);
    Ok(())
}

fn description(tab: &str, root: &Value) -> Value {
    let mut l = Vec::new();
    leaves(root, &mut l);
    let focused = l
        .first()
        .and_then(|p| p.get("pane_id"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({"workspace_id": "w1", "tab_id": tab, "zoomed": false, "focused_pane_id": focused, "root": root})
}

/// The tree of `tab`: what we laid out, else its agents side by side.
fn tree_of(host: &str, tab: &str) -> Value {
    let saved = load(&layout_file(host, tab), Value::Null);
    if !saved.is_null() {
        return saved;
    }
    let agents = load(&agents_file(host), json!({}));
    let panes: Vec<String> = agents
        .pointer("/result/agents")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter(|x| x.get("tab_id").and_then(Value::as_str) == Some(tab))
                .filter_map(|x| x.get("pane_id").and_then(Value::as_str).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let mut nodes: Vec<Value> = panes
        .iter()
        .map(|p| json!({"type": "pane", "pane_id": p}))
        .collect();
    if nodes.is_empty() {
        nodes.push(json!({"type": "pane", "pane_id": tab.replacen(":t", ":p", 1)}));
    }
    let mut t = nodes.pop().unwrap_or(Value::Null);
    while let Some(n) = nodes.pop() {
        t = json!({"type": "split", "direction": "right", "ratio": 0.5, "first": n, "second": t});
    }
    t
}

/// `target` in `t` becomes a split: `target` first, `moved` second.
fn graft(t: &mut Value, target: &str, moved: &str, dir: &str, ratio: f64) -> bool {
    if let (Some(_), Some(_)) = (t.get("first"), t.get("second")) {
        for k in ["first", "second"] {
            if let Some(c) = t.get_mut(k) {
                if graft(c, target, moved, dir, ratio) {
                    return true;
                }
            }
        }
        return false;
    }
    if t.get("pane_id").and_then(Value::as_str) == Some(target) {
        let old = t.clone();
        *t = json!({"type": "split", "direction": dir, "ratio": ratio, "first": old,
            "second": {"type": "pane", "pane_id": moved}});
        return true;
    }
    false
}

/// `pane` out of `t`: its sibling takes the split's place. True when found.
fn cut(t: &mut Value, pane: &str) -> bool {
    let (Some(a), Some(b)) = (t.get("first").cloned(), t.get("second").cloned()) else {
        return false;
    };
    let is = |n: &Value| n.get("pane_id").and_then(Value::as_str) == Some(pane);
    if is(&a) {
        *t = b;
        return true;
    }
    if is(&b) {
        *t = a;
        return true;
    }
    ["first", "second"]
        .iter()
        .any(|k| t.get_mut(*k).is_some_and(|c| cut(c, pane)))
}

fn unknown(method: &str) -> Result<Value, (String, String)> {
    Err((
        "invalid_request".into(),
        format!("invalid request: unknown variant `{method}`, expected one of `ping`, `notification.show`, `agent.view.set`, `events.subscribe`"),
    ))
}

fn handle(host: &str, method: &str, p: &Value) -> Result<Value, (String, String)> {
    let old = st().join(format!("sockold_{host}")).exists();
    let s = |k: &str| p.get(k).and_then(Value::as_str).map(String::from);
    let bad = |m: String| ("invalid_request".to_string(), m);
    let _g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    match method {
        "ping" => Ok(json!({"type": "pong", "version": "0.9.5", "protocol": 22})),
        // the same world `api snapshot` shows, unless this machine is an older herdr
        "session.snapshot" if !old => {
            let read = |f: String| -> Value {
                std::fs::read_to_string(st().join(f))
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(Value::Null)
            };
            let panes = match read(format!("panes_{host}.json")) {
                Value::Null => json!([]),
                v => v,
            };
            Ok(json!({"type": "session_snapshot", "snapshot": {
                "version": "0.9.5", "protocol": 22,
                "agents": read(format!("agents_{host}.json")).pointer("/result/agents").cloned().unwrap_or(json!([])),
                "tabs": read(format!("tabs_{host}.json")).pointer("/result/tabs").cloned().unwrap_or(json!([])),
                "panes": panes,
            }}))
        }
        // $ST/link_<host>: `row start end` of the link wherever you click
        "pane.link.resolve" => {
            let regions: Vec<Value> = std::fs::read_to_string(st().join(format!("link_{host}")))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| {
                    let n: Vec<u64> = l
                        .split_whitespace()
                        .filter_map(|x| x.parse().ok())
                        .collect();
                    (n.len() == 3).then(|| json!({"row": n[0], "start_col": n[1], "end_col": n[2]}))
                })
                .collect();
            Ok(json!({"type": "pane_link_resolved", "regions": regions}))
        }
        "notification.show" => {
            if s("title").is_none() {
                return Err(bad("invalid request: missing field `title`".into()));
            }
            Ok(json!({"type": "notification_show", "shown": true, "reason": "shown"}))
        }
        "agent.view.set" => Ok(json!({"type": "agent_view", "active": true,
            "label": p.get("label").cloned().unwrap_or(Value::Null), "source": s("source")})),
        "agent.view.clear" => {
            Ok(json!({"type": "agent_view", "active": false, "label": null, "source": s("source")}))
        }
        m if old && (m.starts_with("layout.") || m == "pane.move") => unknown(m),
        "layout.export" => {
            let tab = match (s("tab_id"), s("pane_id")) {
                (Some(_), Some(_)) => {
                    return Err(("layout_not_found".into(), "layout target not found".into()))
                }
                (Some(t), None) => t,
                (None, Some(pn)) => tab_of(host, &pn),
                (None, None) => "w1:t1".into(),
            };
            let root = tree_of(host, &tab);
            Ok(json!({"type": "layout_export", "layout": description(&tab, &root)}))
        }
        "layout.apply" => {
            let mut root = p
                .get("root")
                .cloned()
                .ok_or_else(|| bad("invalid request: missing field `root`".into()))?;
            let replace = s("tab_id");
            if replace.is_some() && s("workspace_id").is_some() {
                return Err((
                    "invalid_target".into(),
                    "use either tab_id or workspace_id, not both".into(),
                ));
            }
            let mut l = Vec::new();
            leaves(&root, &mut l);
            if l.len() > 24 {
                return Err((
                    "invalid_layout".into(),
                    format!("layout has {} panes; maximum is 24", l.len()),
                ));
            }
            let tab = format!("w1:t{}", next());
            number(host, &tab, &mut root).map_err(bad)?;
            if let Some(old) = &replace {
                drop_tab(host, old);
            }
            add_tab(host, &tab, &s("tab_label").unwrap_or_default());
            store(&layout_file(host, &tab), &env_masked(&root));
            Ok(json!({"type": "layout_apply", "layout": description(&tab, &root)}))
        }
        "pane.move" => {
            let pane = s("pane_id")
                .ok_or_else(|| bad("invalid request: missing field `pane_id`".into()))?;
            let d = p
                .get("destination")
                .ok_or_else(|| bad("invalid request: missing field `destination`".into()))?;
            let from = tab_of(host, &pane);
            let ds = |k: &str| d.get(k).and_then(Value::as_str).map(String::from);
            let (to, created) = match ds("type").as_deref() {
                Some("new_tab") | Some("new_workspace") => {
                    let t = format!("w1:t{}", next());
                    add_tab(host, &t, &ds("label").unwrap_or_default());
                    store(
                        &layout_file(host, &t),
                        &json!({"type": "pane", "pane_id": pane}),
                    );
                    (t, true)
                }
                Some("tab") => {
                    let t = ds("tab_id")
                        .ok_or_else(|| bad("invalid request: missing field `tab_id`".into()))?;
                    let dir = ds("split")
                        .ok_or_else(|| bad("invalid request: missing field `split`".into()))?;
                    if t == from {
                        return Ok(
                            json!({"type": "pane_move", "move_result": {"changed": false,
                            "reason": "same_tab", "previous_pane_id": pane, "previous_tab_id": from,
                            "previous_workspace_id": "w1", "pane": {"pane_id": pane, "tab_id": from},
                            "focused_pane_id": pane, "target_layout": {"tab_id": t}}}),
                        );
                    }
                    let target = ds("target_pane_id").unwrap_or_else(|| {
                        let mut l = Vec::new();
                        leaves(&tree_of(host, &t), &mut l);
                        l.first()
                            .and_then(|x| x.get("pane_id").and_then(Value::as_str))
                            .unwrap_or("")
                            .to_string()
                    });
                    let ratio = d.get("ratio").and_then(Value::as_f64).unwrap_or(0.5);
                    let mut tree = tree_of(host, &t);
                    if !graft(&mut tree, &target, &pane, &dir, ratio) {
                        return Err(("pane_not_found".into(), format!("pane {target} not found")));
                    }
                    store(&layout_file(host, &t), &tree);
                    (t, false)
                }
                _ => return Err(bad("invalid request: unknown destination".into())),
            };
            // it leaves its old tab (which closes when that was its last pane)
            let mut old = tree_of(host, &from);
            if cut(&mut old, &pane) {
                store(&layout_file(host, &from), &old);
            } else {
                let _ = std::fs::remove_file(layout_file(host, &from));
                drop_tab(host, &from);
            }
            place(host, &pane, &to);
            let mut r = json!({"changed": true, "previous_pane_id": pane, "previous_tab_id": from,
                "previous_workspace_id": "w1", "pane": {"pane_id": pane, "tab_id": to, "workspace_id": "w1"},
                "focused_pane_id": pane, "target_layout": {"tab_id": to, "workspace_id": "w1"}});
            if created {
                r["created_tab"] = json!({"tab_id": to, "workspace_id": "w1",
                    "label": ds("label").unwrap_or_default()});
            }
            Ok(json!({"type": "pane_move", "move_result": r}))
        }
        m => unknown(m),
    }
}

fn reply(s: &mut UnixStream, id: &Value, r: Result<Value, (String, String)>) {
    let v = match r {
        Ok(x) => json!({"id": id, "result": x}),
        Err((code, message)) => json!({"id": id, "error": {"code": code, "message": message}}),
    };
    let _ = s.write_all(format!("{v}\n").as_bytes());
}

fn serve(host: String, mut s: UnixStream) {
    let mut line = String::new();
    let mut r = BufReader::new(match s.try_clone() {
        Ok(x) => x,
        Err(_) => return,
    });
    if r.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let req: Value = match serde_json::from_str(line.trim()) {
        Ok(v) => v,
        Err(e) => {
            return reply(
                &mut s,
                &json!(""),
                Err(("invalid_request".into(), format!("invalid request: {e}"))),
            )
        }
    };
    let id = req.get("id").cloned().unwrap_or(json!(""));
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req
        .get("params")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    log(&format!("sock {host} {method} {}", env_masked(&params)));
    if method == "events.subscribe" {
        reply(&mut s, &id, Ok(json!({"type": "subscription_started"})));
        // held open until the client goes; event lines a test drops in
        // $ST/sockevent_<host> are streamed to a subscriber (one takes them)
        let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
        let drop_in = st().join(format!("sockevent_{host}"));
        let mut sink = [0u8; 256];
        loop {
            match r.get_mut().read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return,
            }
            let mine = st().join(format!(
                "sockevent_{host}.{}",
                TAKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            if std::fs::rename(&drop_in, &mine).is_ok() {
                let text = std::fs::read_to_string(&mine).unwrap_or_default();
                let _ = std::fs::remove_file(&mine);
                for l in text.lines().filter(|l| !l.trim().is_empty()) {
                    log(&format!("sock {host} event {l}"));
                    if s.write_all(format!("{}\n", l.trim()).as_bytes()).is_err() {
                        return;
                    }
                }
            }
        }
    }
    let out = handle(&host, method, &params);
    reply(&mut s, &id, out);
}

fn main() {
    let mut a = std::env::args().skip(1);
    let dir = PathBuf::from(
        a.next()
            .expect("usage: fake_herdr_sock <socket dir> <host>…"),
    );
    let hosts: Vec<String> = a.collect();
    let _ = std::fs::create_dir_all(&dir);
    let me = std::process::id().to_string();
    // one at a time: a newer one takes over (this one sees and goes)
    let _ = std::fs::write(st().join("fakesock.pid"), &me);
    let mut paths = Vec::new();
    for h in hosts {
        let path = dir.join(format!("{h}.sock"));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path)
            .unwrap_or_else(|e| panic!("can't listen on {}: {e}", path.display()));
        paths.push(path);
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                let h = h.clone();
                std::thread::spawn(move || serve(h, c));
            }
        });
    }
    // gone with the fake world (reset.sh), or when fakesock.sh stops it
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let pid = std::fs::read_to_string(st().join("fakesock.pid")).unwrap_or_default();
        if !st().is_dir() || pid.trim() != me {
            for p in &paths {
                let _ = std::fs::remove_file(p);
            }
            std::process::exit(0);
        }
    }
}
