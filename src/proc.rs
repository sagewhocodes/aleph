//! Running external tools (herdr, ssh, ssh-keygen, …) with hard timeouts, so
//! a stalled network never wedges the CLI or the dashboard.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Out {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub timed_out: bool,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    /// Last meaningful stderr line (ssh/herdr put the reason last).
    pub fn reason(&self) -> String {
        if self.timed_out {
            return "timed out".into();
        }
        self.stderr
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("Warning: Permanently added"))
            .map(str::to_string)
            .unwrap_or_else(|| match self.code {
                Some(c) => format!("exited with status {c}"),
                None => "killed".into(),
            })
    }
}

fn drain<R: Read + Send + 'static>(mut r: R) -> Arc<Mutex<Vec<u8>>> {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let b = buf.clone();
    thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => b.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    });
    buf
}

// ------------------------------------------------------------------ debug log

static DEBUG: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();

/// `--debug` / ALEPH_DEBUG=1 (or =<path>): log every external command
/// with its duration, exit code and stderr to ~/.cache/aleph/debug.log.
pub fn debug_init(flag: bool) {
    let env = crate::brand::env("DEBUG").filter(|v| v != "0");
    let path = match (flag, env) {
        (_, Some(p)) if p.contains('/') => Some(PathBuf::from(p)),
        (true, _) | (_, Some(_)) => Some(crate::config::cache_dir().join("debug.log")),
        _ => None,
    };
    let _ = DEBUG.set(path);
}

pub fn debug_path() -> Option<PathBuf> {
    DEBUG.get().cloned().flatten()
}

pub fn dlog(line: &str) {
    crate::log::debug("proc", line);
    let Some(p) = debug_path() else { return };
    // a secret value read from the keystore never lands here either
    let line = crate::log::mask_known(line);
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&p)
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let _ = writeln!(f, "{t:.3} [{}] {line}", std::process::id());
    }
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        let a = a.to_string_lossy();
        s.push(' ');
        if a.len() > 120 {
            s.push_str(&a[..a.char_indices().nth(117).map_or(a.len(), |(i, _)| i)]);
            s.push('…');
        } else {
            s.push_str(&sh_quote(&a));
        }
    }
    s
}

/// Run with captured output and a deadline. Never blocks past `timeout`
/// (+1s grace for output), even if a grandchild keeps the pipes open.
pub fn run(cmd: &mut Command, timeout: Duration, input: Option<&[u8]>) -> io::Result<Out> {
    run_cancel(cmd, timeout, input, None)
}

/// [`run`] that also gives up early, killing the child, once `cancel` is set
/// (a background watcher whose view went away).
pub fn run_cancel(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
    cancel: Option<&AtomicBool>,
) -> io::Result<Out> {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let started = Instant::now();
    detach_under_view(cmd);
    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut si)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let so = drain(child.stdout.take().expect("piped"));
    let se = drain(child.stderr.take().expect("piped"));
    let end = Instant::now() + timeout;
    let (code, timed_out) = loop {
        match child.try_wait()? {
            Some(st) => break (st.code(), false),
            None if Instant::now() >= end || cancel.is_some_and(|c| c.load(Ordering::Relaxed)) => {
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            None => thread::sleep(Duration::from_millis(15)),
        }
    };
    // let the readers catch the tail; don't wait forever on inherited pipes
    let grace = Instant::now() + Duration::from_millis(if timed_out { 100 } else { 1000 });
    while Instant::now() < grace && (Arc::strong_count(&so) > 1 || Arc::strong_count(&se) > 1) {
        thread::sleep(Duration::from_millis(5));
    }
    let stdout = so.lock().unwrap().clone();
    let stderr = String::from_utf8_lossy(&se.lock().unwrap()).into_owned();
    if debug_path().is_some() || crate::log::enabled(crate::log::Level::Debug) {
        dlog(&format!(
            "{:>6}ms {} → {}{}",
            started.elapsed().as_millis(),
            describe(cmd),
            if timed_out {
                "TIMEOUT".to_string()
            } else {
                format!("{code:?}")
            },
            stderr
                .lines()
                .last()
                .map(|l| format!(" · {l}"))
                .unwrap_or_default()
        ));
    }
    Ok(Out {
        code,
        stdout,
        stderr,
        timed_out,
    })
}

/// [`run`] for input too big to hold at once (a video's frames): `write`
/// feeds the child's stdin, on this thread, while its output drains. The
/// deadline kills the child, which ends the writing too. The writer's own
/// result comes back beside the child's.
#[cfg(feature = "showcase")]
pub fn feed(
    cmd: &mut Command,
    timeout: Duration,
    write: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<(Out, io::Result<()>)> {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let so = drain(child.stdout.take().expect("piped"));
    let se = drain(child.stderr.take().expect("piped"));
    let child = Arc::new(Mutex::new(child));
    let done = Arc::new(AtomicBool::new(false));
    let killed = Arc::new(AtomicBool::new(false));
    {
        let (child, done, killed) = (child.clone(), done.clone(), killed.clone());
        thread::spawn(move || {
            let end = Instant::now() + timeout;
            while Instant::now() < end {
                if done.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            if !done.load(Ordering::Relaxed) {
                killed.store(true, Ordering::Relaxed);
                let _ = child.lock().unwrap_or_else(|e| e.into_inner()).kill();
            }
        });
    }
    let wrote = write(&mut stdin);
    drop(stdin);
    let code = loop {
        let st = child.lock().unwrap_or_else(|e| e.into_inner()).try_wait()?;
        if let Some(st) = st {
            break st.code();
        }
        thread::sleep(Duration::from_millis(15));
    };
    done.store(true, Ordering::Relaxed);
    let grace = Instant::now() + Duration::from_millis(1000);
    while Instant::now() < grace && (Arc::strong_count(&so) > 1 || Arc::strong_count(&se) > 1) {
        thread::sleep(Duration::from_millis(5));
    }
    let stdout = so.lock().unwrap().clone();
    let stderr = String::from_utf8_lossy(&se.lock().unwrap()).into_owned();
    let timed_out = killed.load(Ordering::Relaxed);
    if debug_path().is_some() || crate::log::enabled(crate::log::Level::Debug) {
        dlog(&format!(
            "{:>6}ms {} → {} (fed)",
            started.elapsed().as_millis(),
            describe(cmd),
            if timed_out {
                "TIMEOUT".to_string()
            } else {
                format!("{code:?}")
            },
        ));
    }
    Ok((
        Out {
            code,
            stdout,
            stderr,
            timed_out,
        },
        wrote,
    ))
}

/// Run with a deadline, handing every output line (stdout and stderr, in
/// arrival order, `\r` progress redraws split too) to `on_line` as it comes —
/// for long installs whose progress the user should see. Also returns
/// everything, like [`run`].
pub fn stream(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
    on_line: impl FnMut(&str),
) -> io::Result<Out> {
    stream_until(cmd, timeout, input, on_line, || false)
}

/// [`stream`] for a child that never ends by itself (a dev server): once
/// `stop` says so it gets SIGINT, as ctrl+c would, then 3s to tidy up
/// before it's killed. A stop isn't a timeout.
pub fn stream_until(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
    on_line: impl FnMut(&str),
    stop: impl Fn() -> bool,
) -> io::Result<Out> {
    stream_watch(cmd, timeout, input, Watch::default(), on_line, stop).map(|(o, _)| o)
}

/// How [`stream_watch`] keeps an eye on its child.
#[derive(Clone, Copy, Debug, Default)]
pub struct Watch {
    /// its own session, with no terminal (an installer's prompt can't grab
    /// one and wait): it and everything it starts are stopped together
    pub detach: bool,
    /// stopped when it says nothing for this long (waiting for input)
    pub quiet: Option<Duration>,
    /// stopped when it says nothing for this long after what reads as a
    /// question (`?`, `[y/N]`, a menu) — a silent clone or build goes on
    pub asked: Option<Duration>,
}

/// Do these last lines read as a question waiting for an answer — a line
/// ending in `?`, `[y/N]`, "press enter", a menu's `❯ 1` (pure)?
pub fn looks_asked(lines: &[String]) -> bool {
    lines.iter().rev().take(8).any(|l| {
        let t = l.trim().to_lowercase();
        let t = t.trim_end_matches([' ', '\u{1b}']);
        t.ends_with('?')
            || t.ends_with("? ")
            || t.contains("[y/n]")
            || t.contains("(y/n)")
            || t.contains("[y/n]:")
            || t.contains("press enter")
            || t.contains("press any key")
            || t.starts_with('❯')
            || t.starts_with("› ")
            || (t.ends_with(':')
                && [
                    "password",
                    "passphrase",
                    "token",
                    "enter ",
                    "choose",
                    "select",
                    "username",
                ]
                .iter()
                .any(|w| t.contains(w)))
    })
}

/// While a full-screen view is up, a child gets its own session with no
/// terminal: one that grabs it (herdr starting its server did) can't take it
/// from the view, and the shell never stops Aleph for it.
fn detach_under_view(cmd: &mut Command) {
    if crate::term::active() {
        // SAFETY: setsid is async-signal-safe; run between fork and exec
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

/// Stop a child — and, when it has its own session, everything it started
/// (a script's curl and bash): SIGTERM, a moment, then SIGKILL.
fn kill_tree(child: &mut std::process::Child, group: bool) {
    if group {
        let pg = child.id() as libc::pid_t;
        // SAFETY: signals to the process group our own child leads
        unsafe {
            libc::kill(-pg, libc::SIGTERM);
        }
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1500) {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        // SAFETY: as above
        unsafe {
            libc::kill(-pg, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// [`stream_until`] with a [`Watch`]: the second value says whether it was
/// stopped for going quiet.
pub fn stream_watch(
    cmd: &mut Command,
    timeout: Duration,
    input: Option<&[u8]>,
    watch: Watch,
    mut on_line: impl FnMut(&str),
    stop: impl Fn() -> bool,
) -> io::Result<(Out, bool)> {
    if watch.detach {
        // SAFETY: setsid is async-signal-safe; run between fork and exec
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    } else {
        detach_under_view(cmd);
    }
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    let started = Instant::now();
    let mut child = cmd.spawn()?;
    if let (Some(data), Some(mut si)) = (input, child.stdin.take()) {
        let data = data.to_vec();
        thread::spawn(move || {
            let _ = si.write_all(&data);
        });
    }
    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    let reader = |mut r: Box<dyn Read + Send>, is_out: bool, tx: mpsc::Sender<(bool, Vec<u8>)>| {
        thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((is_out, chunk[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    };
    reader(
        Box::new(child.stdout.take().expect("piped")),
        true,
        tx.clone(),
    );
    reader(Box::new(child.stderr.take().expect("piped")), false, tx);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    // partial lines per stream
    let mut pend: [Vec<u8>; 2] = [Vec::new(), Vec::new()];
    let emit = |buf: &mut Vec<u8>, flush: bool, on_line: &mut dyn FnMut(&str)| {
        while let Some(i) = buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let t = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if !t.trim().is_empty() {
                on_line(&t);
            }
        }
        if flush && !buf.is_empty() {
            let t = String::from_utf8_lossy(buf).into_owned();
            buf.clear();
            if !t.trim().is_empty() {
                on_line(&t);
            }
        }
    };
    let end = Instant::now() + timeout;
    let mut code = None;
    let mut timed_out = false;
    let mut stalled = false;
    let mut heard = Instant::now();
    // the last few lines, to tell a question from a long silent step
    let mut last: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    let mut exited_at: Option<Instant> = None;
    let mut stopping: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok((is_out, data)) => {
                heard = Instant::now();
                let k = usize::from(!is_out);
                if is_out {
                    stdout.extend_from_slice(&data);
                } else {
                    stderr.extend_from_slice(&data);
                }
                pend[k].extend_from_slice(&data);
                if watch.asked.is_some() {
                    let mut tap = |l: &str| {
                        last.push_back(l.to_string());
                        if last.len() > 8 {
                            last.pop_front();
                        }
                        on_line(l);
                    };
                    emit(&mut pend[k], false, &mut tap);
                } else {
                    emit(&mut pend[k], false, &mut on_line);
                }
            }
            // both readers done: every byte is in
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                if exited_at.is_none() {
                    code = child.wait().ok().and_then(|s| s.code());
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if exited_at.is_none() {
            if let Some(st) = child.try_wait()? {
                code = st.code();
                exited_at = Some(Instant::now());
            } else if Instant::now() >= end {
                kill_tree(&mut child, watch.detach);
                timed_out = true;
                exited_at = Some(Instant::now());
            } else if watch.quiet.is_some_and(|q| heard.elapsed() > q)
                || watch.asked.is_some_and(|q| {
                    heard.elapsed() > q && looks_asked(&last.iter().cloned().collect::<Vec<_>>())
                })
            {
                kill_tree(&mut child, watch.detach);
                stalled = true;
                exited_at = Some(Instant::now());
            } else if stopping.is_none() && stop() {
                // SAFETY: a plain signal to our own child
                unsafe {
                    libc::kill(child.id() as libc::pid_t, libc::SIGINT);
                }
                stopping = Some(Instant::now());
            } else if stopping.is_some_and(|t| t.elapsed() > Duration::from_secs(3)) {
                let _ = child.kill();
                code = child.wait().ok().and_then(|s| s.code());
                exited_at = Some(Instant::now());
            }
        }
        // a grandchild holding the pipes open must not keep us here
        if exited_at.is_some_and(|t| t.elapsed() > Duration::from_millis(1000)) {
            break;
        }
    }
    for p in pend.iter_mut() {
        let mut b = std::mem::take(p);
        emit(&mut b, true, &mut on_line);
    }
    if debug_path().is_some() || crate::log::enabled(crate::log::Level::Debug) {
        dlog(&format!(
            "{:>6}ms {} → {} (streamed)",
            started.elapsed().as_millis(),
            describe(cmd),
            if timed_out {
                "TIMEOUT".to_string()
            } else {
                format!("{code:?}")
            },
        ));
    }
    Ok((
        Out {
            code,
            stdout,
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
            timed_out,
        },
        stalled,
    ))
}

/// Convenience: run `prog args…` and capture.
pub fn cap<S: AsRef<OsStr>>(prog: &str, args: &[S], timeout: Duration) -> Out {
    run(Command::new(prog).args(args), timeout, None).unwrap_or_else(|e| Out {
        code: None,
        stderr: format!("{prog}: {e}"),
        ..Default::default()
    })
}

/// Run attached to the user's terminal (for prompts: passwords, ssh-keygen, herdr setup).
pub fn interactive<S: AsRef<OsStr>>(prog: &str, args: &[S]) -> io::Result<bool> {
    Ok(Command::new(prog).args(args).status()?.success())
}

/// Replace this process (attach to herdr, hand off to the wrapped agent).
pub fn exec<S: AsRef<OsStr>>(prog: impl AsRef<OsStr>, args: &[S]) -> io::Error {
    Command::new(prog).args(args).exec()
}

/// Open a web address on this computer: the browser, left running on its own
/// (`ALEPH_OPEN_CMD` stands in, for tests). Nothing but http and https: what a
/// screen shows can be anything.
pub fn open_url(url: &str) -> Result<(), String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("not opening {url}: only web addresses"));
    }
    let prog = crate::brand::env("OPEN_CMD").unwrap_or_else(|| {
        if cfg!(target_os = "macos") {
            "open".to_string()
        } else {
            "xdg-open".to_string()
        }
    });
    Command::new(prog)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("couldn't open {url}: {e}"))
}

pub fn which(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_exe(&p).then_some(p);
    }
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| is_exe(p))
    })
}

/// The real program `name` (the agent a shell function shadows): the first
/// `name` on PATH that isn't this binary.
pub fn real_program(name: &str) -> Option<PathBuf> {
    let me = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    if name.contains('/') {
        let p = PathBuf::from(name);
        return (is_exe(&p) && p.canonicalize().ok() != me).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(name))
        .find(|p| is_exe(p) && p.canonicalize().ok().as_ref() != me.as_ref())
}

fn is_exe(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// POSIX single-quote a word (also valid in zsh/bash/fish).
pub fn sh_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,+@%".contains(&b))
    {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_web_addresses_open() {
        for bad in [
            "file:///etc/passwd",
            "ssh://x",
            "javascript:alert(1)",
            "/etc/hosts",
            "",
        ] {
            assert!(open_url(bad).is_err(), "{bad}");
        }
    }

    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(sh_quote("--model"), "--model");
        assert_eq!(sh_quote("fix the bug"), "'fix the bug'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn timeout_is_enforced() {
        let t = Instant::now();
        let o = cap("sh", &["-c", "sleep 5"], Duration::from_millis(300));
        assert!(o.timed_out && t.elapsed() < Duration::from_secs(2));
        // a background grandchild holding stdout open must not block us
        let t = Instant::now();
        let o = cap(
            "sh",
            &["-c", "(sleep 5 &) ; echo hi"],
            Duration::from_secs(3),
        );
        assert!(o.ok() && o.text().contains("hi") && t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn cancel_kills_early() {
        let flag = AtomicBool::new(false);
        let t = Instant::now();
        std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(200));
                flag.store(true, Ordering::Relaxed);
            });
            let o = run_cancel(
                Command::new("sh").args(["-c", "sleep 5"]),
                Duration::from_secs(10),
                None,
                Some(&flag),
            )
            .unwrap();
            assert!(o.timed_out);
        });
        assert!(t.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn stream_until_stops_a_child_that_never_ends_as_ctrl_c_would() {
        use std::cell::Cell;
        let seen = Cell::new(false);
        let mut got: Vec<String> = Vec::new();
        let t = Instant::now();
        // a dev server: says it's ready, then waits forever; ctrl+c ends it
        let o = stream_until(
            Command::new("sh").args([
                "-c",
                "trap 'echo bye; exit 0' INT; echo ready; while :; do sleep 0.05; done",
            ]),
            Duration::from_secs(20),
            None,
            |l| {
                seen.set(seen.get() || l == "ready");
                got.push(l.to_string());
            },
            || seen.get(),
        )
        .unwrap();
        assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
        assert!(!o.timed_out);
        assert!(
            got.iter().any(|l| l == "bye"),
            "SIGINT, not a kill: {got:?}"
        );
        // one that ignores it is killed after the grace
        let seen = Cell::new(false);
        let t = Instant::now();
        let o = stream_until(
            Command::new("sh").args([
                "-c",
                "trap '' INT; echo ready; while :; do sleep 0.05; done",
            ]),
            Duration::from_secs(20),
            None,
            |l| seen.set(seen.get() || l == "ready"),
            || seen.get(),
        )
        .unwrap();
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
        assert!(!o.timed_out && !o.ok());
    }

    #[test]
    fn a_detached_child_that_waits_is_stopped_with_everything_it_started() {
        let t = Instant::now();
        let mut got = Vec::new();
        // it prints, then waits on a grandchild that sleeps (a login prompt)
        let (o, stalled) = stream_watch(
            Command::new("sh").args(["-c", "echo installed; sleep 60 & wait"]),
            Duration::from_secs(30),
            None,
            Watch {
                detach: true,
                quiet: Some(Duration::from_millis(600)),
                asked: None,
            },
            |l| got.push(l.to_string()),
            || false,
        )
        .unwrap();
        assert!(stalled && !o.timed_out, "{o:?}");
        assert!(got.iter().any(|l| l == "installed"));
        assert!(t.elapsed() < Duration::from_secs(8), "{:?}", t.elapsed());
        // and no terminal to grab: /dev/tty can't be opened in its session
        let (o, _) = stream_watch(
            Command::new("sh").args([
                "-c",
                // (a subshell: dash exits on a special builtin's failed redirection)
                "if (: >/dev/tty) 2>/dev/null; then echo tty; else echo none; fi",
            ]),
            Duration::from_secs(10),
            None,
            Watch {
                detach: true,
                quiet: None,
                asked: None,
            },
            |_| {},
            || false,
        )
        .unwrap();
        assert_eq!(o.text().trim(), "none");
    }

    #[test]
    fn a_question_reads_as_one_and_a_long_step_does_not() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Devin's login menu
        assert!(looks_asked(&l(&[
            "✓ Installed devin v3000.11.205 to ~/.local/bin/devin.",
            "How would you like to log in?",
            "❯ 1 Log in with browser",
            "  Recommended for most users",
            "  3 Log in with Windsurf for Enterprise",
            "  For enterprise customers only",
        ])));
        assert!(looks_asked(&l(&["Continue? [y/N]"])));
        assert!(looks_asked(&l(&["Enter your API token:"])));
        // Hermes cloning: silent, but not asking
        assert!(!looks_asked(&l(&[
            "→ Cloning https://github.com/NousResearch/hermes-agent.git (main)",
            "Cloning into '/home/x/.hermes/.hermes-clone-a3Gvl7/tree'...",
        ])));
        assert!(!looks_asked(&l(&["Downloading cpython-3.13.17 (28.1MiB)"])));
    }

    #[test]
    fn stream_hands_over_lines() {
        let mut got: Vec<String> = Vec::new();
        let o = stream(
            Command::new("sh").args([
                "-c",
                "echo one; echo two >&2; printf 'a\\rb\\n'; printf tail",
            ]),
            Duration::from_secs(5),
            None,
            |l| got.push(l.to_string()),
        )
        .unwrap();
        assert!(o.ok());
        for want in ["one", "two", "a", "b", "tail"] {
            assert!(got.iter().any(|g| g == want), "{want} in {got:?}");
        }
        assert!(o.text().contains("one") && o.stderr.contains("two"));
        // a deadline still holds
        let t = Instant::now();
        let o = stream(
            Command::new("sh").args(["-c", "echo x; sleep 5"]),
            Duration::from_millis(300),
            None,
            |_| {},
        )
        .unwrap();
        assert!(o.timed_out && t.elapsed() < Duration::from_secs(3));
    }
}
