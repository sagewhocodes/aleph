//! One-shot requests over herdr's socket API — newline-delimited JSON: one
//! `{"id","method","params"}` line out, one response line back (herdr serves
//! one request per connection) — for what herdr's CLI doesn't offer: exact
//! layouts (`layout.export` / `layout.apply`, `pane.move` with a ratio; see
//! `layouts.rs`), the sidebar's agent view (`agent.view.set` / `.clear`) and
//! herdr's own toasts (`notification.show`). `call` says how a request
//! failed, so a caller knows when the CLI may do it instead (`Fail`).
//!
//! This computer: herdr's socket itself (`herdr status server` says where).
//! A machine: the event listener's forwarded socket when it's alive, else a
//! short-lived ssh stream-local forward of its own (under the same watchdog
//! the listener uses), closed again after the request.

use crate::config::cache_dir;
use crate::{herdr, listen, sshcfg};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A forward we opened for one request (killed on drop).
struct Forward(Option<Child>);

impl Drop for Forward {
    fn drop(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = Command::new("kill")
                .args(["-TERM", &c.id().to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = c.wait();
        }
    }
}

fn endpoint(host: &str) -> Result<(PathBuf, Forward), String> {
    // showcase's demo world: no socket, no forward
    if let Some(e) = crate::showcase::not_here() {
        return Err(e);
    }
    if sshcfg::is_local(host) {
        let sock = listen::socket_from_status(&herdr::local_status())
            .ok_or("herdr isn't running here (no socket)")?;
        return Ok((PathBuf::from(sock), Forward(None)));
    }
    // the listener's forward, when it's up
    let shared = listen::local_forward_path(host);
    if shared.exists() && UnixStream::connect(&shared).is_ok() {
        return Ok((shared, Forward(None)));
    }
    let remote = listen::remote_socket(host)?;
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let local = cache_dir()
        .join("ev")
        .join(format!("req-{}-{n}.sock", std::process::id()));
    if let Some(d) = local.parent() {
        let _ = std::fs::create_dir_all(d);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
    }
    let _ = std::fs::remove_file(&local);
    let child = Command::new("sh")
        .arg("-c")
        .arg(listen::WATCHDOG)
        .arg("sh")
        .arg(sshcfg::ssh_bin())
        .args(sshcfg::BATCH)
        .args(["-o", "ControlMaster=no", "-o", "ControlPath=none"])
        .args([
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "StreamLocalBindUnlink=yes",
        ])
        .args(["-N", "-T", "-L"])
        .arg(format!("{}:{remote}", local.display()))
        .arg(host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("ssh: {e}"))?;
    let mut fwd = Forward(Some(child));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(12) {
        if local.exists() && UnixStream::connect(&local).is_ok() {
            return Ok((local, fwd));
        }
        if let Some(c) = fwd.0.as_mut() {
            if matches!(c.try_wait(), Ok(Some(_))) {
                return Err(format!("couldn't reach herdr's socket on {host}"));
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Err(format!("timed out reaching herdr's socket on {host}"))
}

/// The request line for `method` (pure — tested).
pub fn line(id: &str, method: &str, params: &Value) -> String {
    let mut s = json!({"id": id, "method": method, "params": params}).to_string();
    s.push('\n');
    s
}

/// Why a request didn't go through — and so whether doing it another way
/// (herdr's CLI) is safe.
#[derive(Clone, Debug, PartialEq)]
pub enum Fail {
    /// nothing reached herdr: no socket, no forward, no connection
    Unreachable(String),
    /// herdr said no (`invalid_request` is an older herdr that doesn't know
    /// the method); nothing changed
    Refused { code: String, message: String },
    /// sent, but no answer: it may have happened
    Lost(String),
    /// understood and answered, but it didn't happen for a reason the CLI
    /// would run into too (a zoomed tab, …)
    Declined(String),
}

impl Fail {
    /// Safe and worth doing another way (nothing happened)?
    pub fn retry_elsewhere(&self) -> bool {
        matches!(self, Fail::Unreachable(_) | Fail::Refused { .. })
    }

    /// The same, with any secret this process holds masked in its words (a
    /// request that carried one: herdr may quote what it got).
    pub fn masked(self) -> Fail {
        let m = |s: String| crate::log::mask_known(&s).into_owned();
        match self {
            Fail::Unreachable(s) => Fail::Unreachable(m(s)),
            Fail::Lost(s) => Fail::Lost(m(s)),
            Fail::Declined(s) => Fail::Declined(m(s)),
            Fail::Refused { code, message } => Fail::Refused {
                code,
                message: m(message),
            },
        }
    }
}

impl std::fmt::Display for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Fail::Unreachable(m) | Fail::Lost(m) | Fail::Declined(m) => f.write_str(m),
            Fail::Refused { message, .. } => f.write_str(message),
        }
    }
}

/// A response line → its `result`, or why not.
fn answer(id: &str, text: &str) -> Result<Value, Fail> {
    let v: Value =
        serde_json::from_str(text.trim()).map_err(|e| Fail::Lost(format!("herdr: {e}")))?;
    if v.get("id").and_then(Value::as_str).is_some_and(|x| x != id) {
        return Err(Fail::Lost("herdr answered another request".into()));
    }
    if let Some(e) = v.get("error") {
        let s = |k: &str| e.get(k).and_then(Value::as_str).map(String::from);
        return Err(Fail::Refused {
            code: s("code").unwrap_or_default(),
            message: s("message").unwrap_or_else(|| "herdr refused it".into()),
        });
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// One request to `host`'s herdr; its result.
pub fn request(host: &str, method: &str, params: Value) -> Result<Value, String> {
    call(host, method, params).map_err(|f| f.to_string())
}

/// `request`, saying how it failed.
pub fn call(host: &str, method: &str, params: Value) -> Result<Value, Fail> {
    open(host)?.call(method, params)
}

/// The way to `host`'s herdr socket, held for several requests (one
/// forward for all of them, closed when it's dropped).
pub struct Link {
    host: String,
    path: PathBuf,
    _fwd: Forward,
}

pub fn open(host: &str) -> Result<Link, Fail> {
    let (path, fwd) = endpoint(host).map_err(Fail::Unreachable)?;
    Ok(Link {
        host: host.to_string(),
        path,
        _fwd: fwd,
    })
}

impl Link {
    /// One request (a connection of its own: herdr answers one per connection).
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Fail> {
        let id = format!(
            "aleph-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let mut s = UnixStream::connect(&self.path)
            .map_err(|e| Fail::Unreachable(format!("herdr's socket: {e}")))?;
        let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
        // herdr acts on a whole line only: a failed write never half-happens
        s.write_all(line(&id, method, &params).as_bytes())
            .map_err(|e| Fail::Unreachable(format!("herdr's socket: {e}")))?;
        crate::proc::dlog(&format!("sock {} {method}", self.host));
        let mut r = BufReader::new(s);
        // skip anything that isn't our answer (events on a shared connection)
        for _ in 0..50 {
            let mut buf = String::new();
            match r.read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) if buf.contains(&id) => return answer(&id, &buf),
                Ok(_) => continue,
                Err(e) => return Err(Fail::Lost(format!("herdr didn't answer: {e}"))),
            }
        }
        Err(Fail::Lost(
            "herdr closed the connection without answering".into(),
        ))
    }
}

/// A toast in herdr's own UI on `host` (where someone is attached) — the
/// notification router's `herdr` sink.
#[allow(dead_code)] // the router's herdr sink (H2) calls it
pub fn toast(host: &str, title: &str, body: &str, needs_you: bool) -> Result<(), String> {
    let (title, body) = (crate::showcase::text(title), crate::showcase::text(body));
    let (title, body) = (title.as_ref(), body.as_ref());
    request(
        host,
        "notification.show",
        json!({
            "title": title,
            "body": body,
            "sound": if needs_you { "request" } else { "done" },
        }),
    )
    .map(|_| ())
}

/// herdr's sidebar sorted with what needs you first, while Aleph runs
/// (`herdr_view = needs-you-first`); `clear` puts herdr's own back.
pub fn view(host: &str, on: bool) -> Result<(), String> {
    if on {
        request(
            host,
            "agent.view.set",
            json!({
                "source": crate::brand::BIN,
                "label": "needs you first",
                "sort": [
                    {"field": "attention", "order": "desc"},
                    {"field": "state_change_seq", "order": "desc"},
                ],
            }),
        )
        .map(|_| ())
    } else {
        request(
            host,
            "agent.view.clear",
            json!({"source": crate::brand::BIN}),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `"a.b"` strings on a line, the way methods are written.
    fn method_names(line: &str) -> Vec<String> {
        line.split('"')
            .skip(1)
            .step_by(2)
            .filter(|s| {
                s.contains('.')
                    && s.split('.').all(|p| {
                        !p.is_empty() && p.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                    })
            })
            .map(str::to_string)
            .collect()
    }

    fn sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                sources(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }

    #[test]
    fn every_method_aleph_calls_is_one_herdr_has() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let known: Vec<String> =
            std::fs::read_to_string(root.join("tests/fixtures/herdr/methods.txt"))
                .unwrap()
                .lines()
                .filter(|l| !l.starts_with('#') && !l.is_empty())
                .map(str::to_string)
                .collect();
        let mut files = Vec::new();
        sources(&root.join("src"), &mut files);
        let mut used = Vec::new();
        for f in files {
            let text = std::fs::read_to_string(&f).unwrap();
            for l in text.lines() {
                // herdr's socket, not our own API (daemon::request…, `api.rs`)
                if l.contains("sock::call(")
                    || l.contains("sock::request(")
                    || l.contains("link.call(")
                {
                    used.extend(method_names(l));
                }
            }
        }
        assert!(!used.is_empty(), "the scan found no calls");
        let unknown: Vec<&String> = used.iter().filter(|m| !known.contains(m)).collect();
        assert!(unknown.is_empty(), "not in herdr's schema: {unknown:?}");
    }

    #[test]
    fn requests_and_answers() {
        let l = line("aleph-1", "layout.export", &json!({"tab_id": "w1:t2"}));
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["method"], "layout.export");
        assert!(l.ends_with('\n'));
        assert_eq!(
            answer("aleph-1", r#"{"id":"aleph-1","result":{"type":"ok"}}"#).unwrap()["type"],
            "ok"
        );
        let no = answer(
            "aleph-1",
            r#"{"id":"aleph-1","error":{"code":"x","message":"no such tab"}}"#,
        )
        .unwrap_err();
        assert_eq!(no.to_string(), "no such tab");
        // herdr said no: nothing happened, the CLI may try
        assert!(no.retry_elsewhere());
        // an older herdr doesn't know the method (what 0.9.5 says to one it doesn't know)
        let old = answer(
            "aleph-1",
            r#"{"id":"aleph-1","error":{"code":"invalid_request","message":"invalid request: unknown variant `layout.nope`, expected one of `ping`"}}"#,
        )
        .unwrap_err();
        assert!(matches!(&old, Fail::Refused { code, .. } if code == "invalid_request"));
        // an answer to someone else, or garbage: it may have happened — no retry
        let lost = answer("aleph-1", r#"{"id":"other","result":{}}"#).unwrap_err();
        assert!(!lost.retry_elsewhere());
        assert!(!answer("aleph-1", "{not json")
            .unwrap_err()
            .retry_elsewhere());
        assert!(!Fail::Declined("zoomed".into()).retry_elsewhere());
        assert!(Fail::Unreachable("no socket".into()).retry_elsewhere());
    }
}
