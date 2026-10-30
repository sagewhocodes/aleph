//! One ssh connection to the remote, owned by a manager thread.
//!
//! Nothing is installed remotely: ssh runs a single-line POSIX `sh` loop
//! (HELPER) that answers three requests over stdin/stdout:
//!   `<id>\tS\t<path>`  → `<id>\tM\t<mtime>\t<realpath>`            (exists + mtime)
//!   `<id>\tL\t<path>`  → `<id>\tP\t<mtime>\t<realpath>`, then one
//!                        `<id>\tD\t<flags>\t<name>` per sub-folder, then `<id>\t.`
//!   `<id>\tI\t<path>`  → preview facts: `N` mtime/dirs/files, `F` marker file,
//!                        `B` git branch, `C` last commit (hash, age, subject), then `<id>\t.`
//!   `<id>\tF\t<depth>\t<globs>\t<path>` → deep index: `P` like a listing, then one
//!                        `<id>\tD\t-\t<relative/path>` per folder up to `depth` levels
//!                        below (dot-folders and `/`-separated `globs` pruned on the
//!                        server, at most DEEP_CAP + 1 lines), then `<id>\t.`
//!   `<id>\tK\t<path>`  → make the folder (`mkdir -p`), then answer like `S`
//!   errors             → `<id>\tE\t<reason>`
//!
//! Weak links: requests time out on *lack of progress*, the connection is
//! dropped and re-established with backoff, and unanswered requests are
//! re-sent. A request that times out is failed (not retried) so one hung
//! mount cannot wedge the session. `shutdown()` kills ssh immediately.

use crate::cache::Dir;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// Remote helper. One line, no single quotes, no '!' — so it survives being
/// wrapped in `sh -c '…'` by sh/bash/zsh/fish login shells.
pub const HELPER: &str = concat!(
    r#"unset CDPATH; LC_ALL=C; export LC_ALL; T=$(printf "\t"); N=$(printf "\nx"); N=${N%x}; "#,
    r#"mt() { stat -c %Y "$1" 2>/dev/null || stat -f %m "$1" 2>/dev/null || echo 0; }; "#,
    r#"printf "CCH\t%s\n" "$HOME"; "#,
    r#"while IFS= read -r l; do "#,
    r#"i=${l%%"$T"*}; r=${l#*"$T"}; o=${r%%"$T"*}; p=${r#*"$T"}; "#,
    r#"if test "$o" = F; then dp=${p%%"$T"*}; p=${p#*"$T"}; px=${p%%"$T"*}; p=${p#*"$T"}; fi; "#,
    r#"if test "$o" = K; then if mkdir -p "$p" 2>/dev/null && cd "$p" 2>/dev/null; then printf "%s\tM\t%s\t%s\n" "$i" "$(mt .)" "$(pwd -P)"; else printf "%s\tE\tcan not create it there\n" "$i"; fi; continue; fi; "#,
    r#"if test -e "$p"; then :; else printf "%s\tE\tmissing\n" "$i"; continue; fi; "#,
    r#"if test -d "$p"; then :; else printf "%s\tE\tnot a folder\n" "$i"; continue; fi; "#,
    r#"if cd "$p" 2>/dev/null; then :; else printf "%s\tE\tpermission denied\n" "$i"; continue; fi; "#,
    r#"a=$(pwd -P); m=$(mt .); "#,
    r#"if test "$o" = S; then printf "%s\tM\t%s\t%s\n" "$i" "$m" "$a"; continue; fi; "#,
    r#"if test -r .; then :; else printf "%s\tE\tpermission denied\n" "$i"; continue; fi; "#,
    r#"if test "$o" = F; then printf "%s\tP\t%s\t%s\n" "$i" "$m" "$a"; "#,
    r#"set -f; s=$IFS; IFS=/; set -- -name ".*" -o -name "*$N*"; for n in $px; do set -- "$@" -o -name "$n"; done; IFS=$s; set +f; "#,
    r#"find . -mindepth 1 -maxdepth "$dp" -type d "(" "$@" ")" -prune -o -type d -print 2>/dev/null | head -n 5001 | "#,
    r#"while IFS= read -r e; do printf "%s\tD\t-\t%s\n" "$i" "${e#./}"; done; printf "%s\t.\n" "$i"; continue; fi; "#,
    r#"if test "$o" = I; then d=0; n=0; for e in * .*; do case $e in .|..) continue;; esac; "#,
    r#"if test -d "$e"; then d=$((d+1)); elif test -e "$e"; then n=$((n+1)); fi; done; "#,
    r#"printf "%s\tN\t%s\t%s\t%s\n" "$i" "$m" "$d" "$n"; "#,
    r#"for k in CLAUDE.md AGENTS.md README.md Cargo.toml package.json pyproject.toml requirements.txt setup.py go.mod Makefile CMakeLists.txt Dockerfile docker-compose.yml flake.nix .venv bot/instructions.md bot/agent.ts; do "#,
    r#"test -e "$k" && printf "%s\tF\t%s\n" "$i" "$k"; done; "#,
    r#"if test -f .git/HEAD; then read -r h < .git/HEAD; h=${h#ref: refs/heads/}; printf "%s\tB\t%s\n" "$i" "$h"; "#,
    r#"if command -v git >/dev/null 2>&1; then c=$(git log -1 --format=%h%x09%cr%x09%s 2>/dev/null | head -n 1); "#,
    r#"test -n "$c" && printf "%s\tC\t%s\n" "$i" "$c"; fi; "#,
    r#"elif test -e .git; then printf "%s\tB\tworktree\n" "$i"; fi; printf "%s\t.\n" "$i"; continue; fi; "#,
    r#"printf "%s\tP\t%s\t%s\n" "$i" "$m" "$a"; "#,
    r#"for e in * .*; do case $e in .|..) continue;; *"$N"*) continue;; esac; "#,
    r#"test -d "$e" || continue; f=; test -h "$e" && f=l; "#,
    r#"if test -r "$e" && test -x "$e"; then test -e "$e/.git" && f="${f}g"; else f="${f}d"; fi; "#,
    r#"printf "%s\tD\t%s\t%s\n" "$i" "${f:--}" "$e"; done; "#,
    r#"printf "%s\t.\n" "$i"; done"#
);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(25);
const STAT_TIMEOUT: Duration = Duration::from_secs(8);
/// For listings this is "no new line for N seconds", so big folders on slow
/// links are fine as long as data keeps arriving.
const LIST_STALL_TIMEOUT: Duration = Duration::from_secs(15);
const BACKOFF: [u64; 5] = [1, 2, 4, 8, 15];
/// Deep-index line cap (the helper sends one more so truncation is visible).
pub const DEEP_CAP: usize = 5000;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Stat,
    List,
    Info,
    /// deep index (see `Remote::send_find`)
    Find,
    /// make a folder (answered like `Stat`)
    Mkdir,
}

/// Facts for the preview pane.
#[derive(Clone, Debug, Default)]
pub struct Info {
    pub mtime: i64,
    pub dirs: usize,
    pub files: usize,
    pub markers: Vec<String>,
    pub branch: Option<String>,
    /// (short hash, relative age, subject)
    pub commit: Option<(String, String, String)>,
}

#[derive(Debug)]
pub struct Req {
    pub id: u64,
    pub op: Op,
    pub path: Vec<u8>,
}

#[derive(Debug)]
pub struct Listing {
    pub mtime: i64,
    pub dirs: Vec<Dir>,
}

#[derive(Debug)]
pub enum Ev {
    Online {
        home: Vec<u8>,
    },
    Offline {
        reason: String,
        retry_in: u64,
    },
    Stat {
        id: u64,
        res: Result<(i64, Vec<u8>), String>,
    },
    List {
        id: u64,
        res: Result<Listing, String>,
    },
    Info {
        id: u64,
        res: Result<Info, String>,
    },
    /// deep index: `dirs` hold relative paths; `bool` = cut at DEEP_CAP
    Find {
        id: u64,
        res: Result<(Listing, bool), String>,
    },
    Closed,
}

pub(crate) enum Msg {
    Req(Req),
    Line(u64, Vec<u8>),
    Eof(u64),
    Shutdown,
}

pub struct Remote {
    tx: Sender<Msg>,
    pub rx: Receiver<Ev>,
    next: u64,
}

impl Remote {
    /// `ssh` can be overridden (CC_BROWSE_SSH) for testing or custom wrappers.
    pub fn start_with(host: String, ssh: String) -> Remote {
        let (tx, mrx) = mpsc::channel();
        let (etx, erx) = mpsc::channel();
        let me = tx.clone();
        if crate::showcase::demo() {
            // showcase's demo world: its machines' folders, no ssh
            crate::showcase::folders(host, mrx, etx);
        } else {
            thread::spawn(move || Mgr::new(host, ssh, me, etx).run(mrx));
        }
        Remote {
            tx,
            rx: erx,
            next: 1,
        }
    }

    pub fn send(&mut self, op: Op, path: &[u8]) -> u64 {
        let id = self.next;
        self.next += 1;
        let _ = self.tx.send(Msg::Req(Req {
            id,
            op,
            path: path.to_vec(),
        }));
        id
    }

    /// Index sub-folders up to `depth` levels below `path`, pruning dot-folders
    /// and `globs` (find `-name` patterns) on the server.
    pub fn send_find(&mut self, path: &[u8], depth: u64, globs: &[Vec<u8>]) -> u64 {
        self.send(Op::Find, &find_arg(path, depth, globs))
    }

    /// Kill the ssh connection now and wait (bounded) for the manager to confirm.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Msg::Shutdown);
        let end = Instant::now() + Duration::from_secs(2);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(Ev::Closed) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
}

struct Conn {
    child: Child,
    stdin: Option<ChildStdin>,
    gen: u64,
    ready: bool,
    started: Instant,
}

struct Pending {
    op: Op,
    path: Vec<u8>,
    last: Instant,
    acc: Option<Listing>,
    info: Option<Info>,
}

struct Mgr {
    host: String,
    ssh: String,
    me: Sender<Msg>,
    ev: Sender<Ev>,
    stderr: Arc<Mutex<String>>,
    gen: u64,
    conn: Option<Conn>,
    retry_at: Instant,
    attempt: usize,
    queue: VecDeque<Req>,
    pending: HashMap<u64, Pending>,
}

impl Mgr {
    fn new(host: String, ssh: String, me: Sender<Msg>, ev: Sender<Ev>) -> Mgr {
        Mgr {
            host,
            ssh,
            me,
            ev,
            stderr: Arc::new(Mutex::new(String::new())),
            gen: 0,
            conn: None,
            retry_at: Instant::now(),
            attempt: 0,
            queue: VecDeque::new(),
            pending: HashMap::new(),
        }
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            if self.conn.is_none() && Instant::now() >= self.retry_at {
                self.connect();
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Msg::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    let _ = self.ev.send(Ev::Closed);
                    return;
                }
                Ok(Msg::Req(r)) => self.submit(r),
                Ok(Msg::Line(g, l)) if self.cur_gen() == Some(g) => self.line(l),
                Ok(Msg::Eof(g)) if self.cur_gen() == Some(g) => self.lost("connection closed"),
                _ => {}
            }
            self.check_timeouts();
        }
    }

    fn cur_gen(&self) -> Option<u64> {
        self.conn.as_ref().map(|c| c.gen)
    }

    fn connect(&mut self) {
        self.gen += 1;
        *self.stderr.lock().unwrap() = String::new();
        // BatchMode + timeouts + optional connection sharing (ssh_reuse)
        let mut cmd = if crate::sshcfg::is_local(&self.host) {
            crate::sshcfg::shell(&self.host, &format!("sh -c '{HELPER}'"))
        } else {
            let mut c = Command::new(&self.ssh);
            c.arg("-T")
                .args(crate::sshcfg::batch())
                .args(["-o", "Compression=yes", "-o", "LogLevel=ERROR"])
                .arg(&self.host)
                .arg(format!("sh -c '{HELPER}'"));
            c
        };
        let spawned = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return self.schedule_retry(format!("cannot run {}: {e}", self.ssh)),
        };
        let (out, err, stdin) = (child.stdout.take(), child.stderr.take(), child.stdin.take());
        let (me, g) = (self.me.clone(), self.gen);
        if let Some(out) = out {
            thread::spawn(move || {
                let mut r = BufReader::new(out);
                loop {
                    let mut b = Vec::new();
                    match r.read_until(b'\n', &mut b) {
                        Ok(0) | Err(_) => {
                            let _ = me.send(Msg::Eof(g));
                            break;
                        }
                        Ok(_) => {
                            while matches!(b.last(), Some(b'\n') | Some(b'\r')) {
                                b.pop();
                            }
                            if me.send(Msg::Line(g, b)).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        }
        if let Some(err) = err {
            let se = self.stderr.clone();
            thread::spawn(move || {
                for l in BufReader::new(err).split(b'\n').map_while(Result::ok) {
                    let t = String::from_utf8_lossy(&l).trim().to_string();
                    if !t.is_empty() {
                        *se.lock().unwrap() = t;
                    }
                }
            });
        }
        self.conn = Some(Conn {
            child,
            stdin,
            gen: self.gen,
            ready: false,
            started: Instant::now(),
        });
    }

    fn submit(&mut self, r: Req) {
        if self.conn.as_ref().is_some_and(|c| c.ready) {
            self.write(r);
        } else {
            self.queue.push_back(r);
        }
    }

    fn write(&mut self, r: Req) {
        if r.path.contains(&b'\n') {
            return self.fail(r.id, r.op, "unsupported folder name".into());
        }
        let mut line = format!(
            "{}\t{}\t",
            r.id,
            match r.op {
                Op::Stat => "S",
                Op::List => "L",
                Op::Info => "I",
                Op::Find => "F",
                Op::Mkdir => "K",
            }
        )
        .into_bytes();
        line.extend_from_slice(&r.path);
        line.push(b'\n');
        let ok = self
            .conn
            .as_mut()
            .and_then(|c| c.stdin.as_mut())
            .is_some_and(|s| s.write_all(&line).and_then(|_| s.flush()).is_ok());
        if ok {
            self.pending.insert(
                r.id,
                Pending {
                    op: r.op,
                    path: r.path,
                    last: Instant::now(),
                    acc: None,
                    info: None,
                },
            );
        } else {
            self.queue.push_front(r);
            self.lost("write failed");
        }
    }

    fn fail(&self, id: u64, op: Op, reason: String) {
        let _ = self.ev.send(match op {
            Op::Stat | Op::Mkdir => Ev::Stat {
                id,
                res: Err(reason),
            },
            Op::List => Ev::List {
                id,
                res: Err(reason),
            },
            Op::Info => Ev::Info {
                id,
                res: Err(reason),
            },
            Op::Find => Ev::Find {
                id,
                res: Err(reason),
            },
        });
    }

    fn line(&mut self, l: Vec<u8>) {
        let ready = self.conn.as_ref().is_some_and(|c| c.ready);
        if !ready {
            // Anything before the handshake is login-shell noise (rc files echoing etc).
            if let Some(home) = l.strip_prefix(b"CCH\t") {
                if let Some(c) = self.conn.as_mut() {
                    c.ready = true;
                }
                self.attempt = 0;
                let _ = self.ev.send(Ev::Online {
                    home: home.to_vec(),
                });
                while let Some(r) = self.queue.pop_front() {
                    self.write(r);
                    if self.conn.is_none() {
                        break;
                    }
                }
            }
            return;
        }
        let Some((id, kind, rest)) = parse_line(&l) else {
            return;
        };
        let Some(p) = self.pending.get_mut(&id) else {
            return;
        }; // late reply to a failed request
        p.last = Instant::now();
        match kind {
            b"M" => {
                let (m, real) = split2(rest);
                self.pending.remove(&id);
                let _ = self.ev.send(Ev::Stat {
                    id,
                    res: Ok((parse_i64(m), real.to_vec())),
                });
            }
            b"E" => {
                let op = p.op;
                self.pending.remove(&id);
                self.fail(id, op, String::from_utf8_lossy(rest).into_owned());
            }
            b"P" => {
                let (m, _real) = split2(rest);
                p.acc = Some(Listing {
                    mtime: parse_i64(m),
                    dirs: Vec::new(),
                });
            }
            b"D" => {
                if let Some(acc) = p.acc.as_mut() {
                    let (f, name) = split2(rest);
                    acc.dirs.push(Dir {
                        name: name.to_vec(),
                        link: f.contains(&b'l'),
                        denied: f.contains(&b'd'),
                        git: f.contains(&b'g'),
                    });
                }
            }
            b"N" => {
                let f: Vec<&[u8]> = rest.split(|&b| b == b'\t').collect();
                let num = |i: usize| f.get(i).map_or(0, |x| parse_i64(x).max(0) as usize);
                p.info = Some(Info {
                    mtime: f.first().map_or(0, |x| parse_i64(x)),
                    dirs: num(1),
                    files: num(2),
                    ..Default::default()
                });
            }
            b"F" | b"B" | b"C" => {
                if let Some(inf) = p.info.as_mut() {
                    let t = String::from_utf8_lossy(rest).into_owned();
                    match kind {
                        b"F" => inf.markers.push(t),
                        b"B" => inf.branch = Some(t),
                        _ => {
                            let mut it = t.splitn(3, '\t');
                            let (h, a, s) = (it.next(), it.next(), it.next());
                            inf.commit = Some((
                                h.unwrap_or("").into(),
                                a.unwrap_or("").into(),
                                s.unwrap_or("").into(),
                            ));
                        }
                    }
                }
            }
            b"." => {
                if let Some(mut done) = self.pending.remove(&id) {
                    let _ = match done.op {
                        Op::Info => self.ev.send(Ev::Info {
                            id,
                            res: done.info.take().ok_or_else(|| "malformed info".to_string()),
                        }),
                        Op::Find => self.ev.send(Ev::Find {
                            id,
                            res: done
                                .acc
                                .take()
                                .map(|mut l| {
                                    let cut = l.dirs.len() > DEEP_CAP;
                                    l.dirs.truncate(DEEP_CAP);
                                    (l, cut)
                                })
                                .ok_or_else(|| "malformed index".to_string()),
                        }),
                        _ => self.ev.send(Ev::List {
                            id,
                            res: done
                                .acc
                                .take()
                                .ok_or_else(|| "malformed listing".to_string()),
                        }),
                    };
                }
            }
            _ => {}
        }
    }

    fn check_timeouts(&mut self) {
        if let Some(c) = &self.conn {
            if !c.ready && c.started.elapsed() > CONNECT_TIMEOUT {
                return self.lost("ssh connect timed out");
            }
        }
        let now = Instant::now();
        let stuck: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, p)| {
                now.duration_since(p.last)
                    > if matches!(p.op, Op::Stat | Op::Mkdir) {
                        STAT_TIMEOUT
                    } else {
                        LIST_STALL_TIMEOUT
                    }
            })
            .map(|(id, _)| *id)
            .collect();
        if stuck.is_empty() {
            return;
        }
        for id in stuck {
            if let Some(p) = self.pending.remove(&id) {
                self.fail(id, p.op, "remote not responding".into());
            }
        }
        self.lost("remote not responding");
    }

    /// Drop the connection, requeue in-flight requests, schedule a reconnect.
    fn lost(&mut self, why: &str) {
        self.kill();
        let mut ids: Vec<u64> = self.pending.keys().copied().collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        for id in ids {
            if let Some(p) = self.pending.remove(&id) {
                self.queue.push_front(Req {
                    id,
                    op: p.op,
                    path: p.path,
                });
            }
        }
        thread::sleep(Duration::from_millis(30)); // let the stderr reader catch ssh's last words
        let detail = self.stderr.lock().unwrap().clone();
        let reason = if detail.is_empty() {
            why.to_string()
        } else {
            format!("{why}: {detail}")
        };
        self.schedule_retry(reason);
    }

    fn schedule_retry(&mut self, reason: String) {
        let d = BACKOFF[self.attempt.min(BACKOFF.len() - 1)];
        self.attempt += 1;
        self.retry_at = Instant::now() + Duration::from_secs(d);
        let _ = self.ev.send(Ev::Offline {
            reason,
            retry_in: d,
        });
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.conn.take() {
            drop(c.stdin.take()); // EOF → remote helper loop ends
            let _ = c.child.kill();
            let _ = c.child.wait();
        }
    }
}

/// The `F` request's path field: `<depth>\t<glob/glob/…>\t<path>`.
fn find_arg(path: &[u8], depth: u64, globs: &[Vec<u8>]) -> Vec<u8> {
    let mut arg = format!("{}\t", depth.clamp(1, 12)).into_bytes();
    let ok = globs
        .iter()
        .filter(|g| !g.is_empty() && !g.iter().any(|b| matches!(b, b'/' | b'\t' | b'\n')));
    for (i, g) in ok.enumerate() {
        if i > 0 {
            arg.push(b'/');
        }
        arg.extend_from_slice(g);
    }
    arg.push(b'\t');
    arg.extend_from_slice(path);
    arg
}

fn parse_line(l: &[u8]) -> Option<(u64, &[u8], &[u8])> {
    let mut it = l.splitn(3, |&b| b == b'\t');
    let id = std::str::from_utf8(it.next()?).ok()?.parse().ok()?;
    let kind = it.next()?;
    Some((id, kind, it.next().unwrap_or(b"")))
}

fn split2(b: &[u8]) -> (&[u8], &[u8]) {
    match b.iter().position(|&c| c == b'\t') {
        Some(i) => (&b[..i], &b[i + 1..]),
        None => (b, b""),
    }
}

fn parse_i64(b: &[u8]) -> i64 {
    std::str::from_utf8(b)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_is_quote_safe() {
        assert!(
            !HELPER.contains('\''),
            "helper must not contain single quotes"
        );
        assert!(
            !HELPER.contains('!'),
            "helper must not contain '!' (csh history)"
        );
        assert!(!HELPER.contains('\n'), "helper must be one line");
    }

    /// Run the helper locally against a scratch tree, exactly as ssh would.
    #[test]
    fn helper_runs_locally() {
        let root = std::env::temp_dir().join(format!("chr-helper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in [
            "a/b/c/d",
            "node_modules/x",
            ".git/y",
            "a/z.egg-info/q",
            "a/data/deep",
            "proj/src",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("proj/Cargo.toml"), "").unwrap();
        let r = root.to_string_lossy().into_owned();
        let globs: Vec<Vec<u8>> = vec![
            b"node_modules".to_vec(),
            b"*.egg-info".to_vec(),
            b"data".to_vec(),
        ];
        let mut input = Vec::new();
        input.extend_from_slice(format!("1\tS\t{r}\n2\tL\t{r}\n").as_bytes());
        input.extend_from_slice(b"3\tF\t");
        input.extend_from_slice(&find_arg(r.as_bytes(), 3, &globs));
        input.extend_from_slice(format!("\n4\tI\t{r}/proj\n5\tF\t2\t\t{r}/nope\n").as_bytes());
        // K makes a folder; a file in the way fails
        input.extend_from_slice(
            format!("6\tK\t{r}/made/deeper\n7\tK\t{r}/proj/Cargo.toml/nope\n").as_bytes(),
        );
        let mut ch = Command::new("sh")
            .arg("-c")
            .arg(HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin.take().unwrap().write_all(&input).unwrap();
        let out = ch.wait_with_output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("CCH\t"), "{text}");
        let of = |id: &str, k: &str| -> Vec<String> {
            lines
                .iter()
                .filter_map(|l| l.strip_prefix(&format!("{id}\t{k}\t")))
                .map(String::from)
                .collect()
        };
        assert_eq!(of("1", "M").len(), 1);
        let mut top = of("2", "D");
        top.sort();
        assert_eq!(top, vec!["-\t.git", "-\ta", "-\tnode_modules", "-\tproj"]);
        let mut deep: Vec<String> = of("3", "D")
            .into_iter()
            .map(|x| x.trim_start_matches("-\t").to_string())
            .collect();
        deep.sort();
        // depth 3, dot-folders + globs pruned (a/b/c/d is level 4)
        assert_eq!(deep, vec!["a", "a/b", "a/b/c", "proj", "proj/src"]);
        assert!(lines.contains(&"3\t."));
        assert_eq!(of("4", "F"), vec!["Cargo.toml"]);
        assert_eq!(of("5", "E"), vec!["missing"]);
        assert_eq!(of("6", "M").len(), 1);
        assert!(root.join("made/deeper").is_dir());
        assert_eq!(of("7", "E"), vec!["can not create it there"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn parses_protocol_lines() {
        assert_eq!(
            parse_line(b"12\tD\tlg\tna\tme"),
            Some((12, &b"D"[..], &b"lg\tna\tme"[..]))
        );
        assert_eq!(parse_line(b"7\t."), Some((7, &b"."[..], &b""[..])));
        assert_eq!(parse_line(b"junk from bashrc"), None);
        assert_eq!(split2(b"lg\tna\tme"), (&b"lg"[..], &b"na\tme"[..]));
        assert_eq!(parse_i64(b"1700000000"), 1_700_000_000);
        assert_eq!(parse_i64(b"?"), 0);
    }
}
