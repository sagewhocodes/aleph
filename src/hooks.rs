//! Agent hooks (opt-in, per machine): the agents themselves tell Aleph when
//! they need you or finish, in their own words ("Claude needs your
//! permission to use Bash"). Each hook only appends one JSON line to
//! `~/.local/state/aleph/spool.jsonl` on its machine — plain `sh`, nothing
//! installed — and the hub reads new lines off the UI thread.
//!
//! * Claude Code: `Notification` and `Stop` entries in `~/.claude/settings.json`
//!   next to whatever is there (herdr's own hooks, moshi-hook's): appended,
//!   never replacing, each command ending in `# aleph-hook`.
//! * Codex: `notify` in `~/.codex/config.toml`; a `notify` you already have
//!   keeps running (ours hands it the same payload).
//! * OpenCode: a plugin of our own, `~/.config/opencode/plugins/aleph-hook.js`
//!   (OpenCode loads every file there; moshi-hook's sits next to it). It
//!   appends the same kind of line for a finished turn, a permission or
//!   question, and an error — from the main session, not its subagents.
//!
//! Every edit shows a diff first, keeps `<file>.aleph-bak-<time>`, and is
//! written only if the file didn't change meanwhile (`cksum` compare and
//! swap). `aleph hooks remove` takes out only Aleph's entries.

use crate::config::Config;
use crate::events::{Event, Kind};
use crate::out::*;
use crate::{proc, sshcfg};
use std::process::ExitCode;
use std::time::Duration;

/// The marker on every command we add.
pub const TAG: &str = "# aleph-hook";
const SPOOL: &str = "$HOME/.local/state/aleph/spool.jsonl";

// ------------------------------------------------------------------ JSON, in order

/// JSON that keeps its keys in the order they came (serde_json sorts them).
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    /// numbers as written
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(x, _)| x == k).map(|(_, j)| j),
            _ => None,
        }
    }
    fn get_mut(&mut self, k: &str) -> Option<&mut J> {
        match self {
            J::Obj(v) => v.iter_mut().find(|(x, _)| x == k).map(|(_, j)| j),
            _ => None,
        }
    }
    fn obj_entry(&mut self, k: &str, default: J) -> Option<&mut J> {
        let J::Obj(v) = self else { return None };
        if !v.iter().any(|(x, _)| x == k) {
            v.push((k.to_string(), default));
        }
        v.iter_mut().find(|(x, _)| x == k).map(|(_, j)| j)
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct P<'a> {
    s: &'a [u8],
    i: usize,
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn err(&self, what: &str) -> String {
        format!("not JSON at byte {} ({what})", self.i)
    }
    fn val(&mut self) -> Result<J, String> {
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.i) == Some(&b'}') {
                        self.i += 1;
                        return Ok(J::Obj(v));
                    }
                    let k = match self.val()? {
                        J::Str(k) => k,
                        _ => return Err(self.err("a key")),
                    };
                    self.ws();
                    if self.s.get(self.i) != Some(&b':') {
                        return Err(self.err("a colon"));
                    }
                    self.i += 1;
                    let x = self.val()?;
                    v.push((k, x));
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {}
                        _ => return Err(self.err("a comma")),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut v = Vec::new();
                loop {
                    self.ws();
                    if self.s.get(self.i) == Some(&b']') {
                        self.i += 1;
                        return Ok(J::Arr(v));
                    }
                    v.push(self.val()?);
                    self.ws();
                    match self.s.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {}
                        _ => return Err(self.err("a comma")),
                    }
                }
            }
            Some(b'"') => {
                self.i += 1;
                let mut out: Vec<u8> = Vec::new();
                while let Some(&c) = self.s.get(self.i) {
                    self.i += 1;
                    match c {
                        b'"' => {
                            return String::from_utf8(out)
                                .map(J::Str)
                                .map_err(|_| self.err("utf-8"))
                        }
                        b'\\' => {
                            let e = *self.s.get(self.i).ok_or_else(|| self.err("an escape"))?;
                            self.i += 1;
                            match e {
                                b'n' => out.push(b'\n'),
                                b't' => out.push(b'\t'),
                                b'r' => out.push(b'\r'),
                                b'b' => out.push(8),
                                b'f' => out.push(12),
                                b'u' => {
                                    let h = std::str::from_utf8(
                                        self.s
                                            .get(self.i..self.i + 4)
                                            .ok_or_else(|| self.err("\\u"))?,
                                    )
                                    .map_err(|_| self.err("\\u"))?;
                                    let mut cp =
                                        u32::from_str_radix(h, 16).map_err(|_| self.err("\\u"))?;
                                    self.i += 4;
                                    // a surrogate pair
                                    if (0xD800..0xDC00).contains(&cp)
                                        && self.s.get(self.i..self.i + 2) == Some(b"\\u")
                                    {
                                        if let Some(lo) = std::str::from_utf8(
                                            self.s.get(self.i + 2..self.i + 6).unwrap_or(b""),
                                        )
                                        .ok()
                                        .and_then(|x| u32::from_str_radix(x, 16).ok())
                                        {
                                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            self.i += 6;
                                        }
                                    }
                                    let ch = char::from_u32(cp).unwrap_or('\u{FFFD}');
                                    let mut b = [0u8; 4];
                                    out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                                }
                                x => out.push(x),
                            }
                        }
                        c => out.push(c),
                    }
                }
                Err(self.err("an end quote"))
            }
            Some(b't') if self.s[self.i..].starts_with(b"true") => {
                self.i += 4;
                Ok(J::Bool(true))
            }
            Some(b'f') if self.s[self.i..].starts_with(b"false") => {
                self.i += 5;
                Ok(J::Bool(false))
            }
            Some(b'n') if self.s[self.i..].starts_with(b"null") => {
                self.i += 4;
                Ok(J::Null)
            }
            Some(c) if *c == b'-' || c.is_ascii_digit() => {
                let st = self.i;
                while self.i < self.s.len()
                    && (self.s[self.i].is_ascii_digit() || b"+-.eE".contains(&self.s[self.i]))
                {
                    self.i += 1;
                }
                Ok(J::Num(
                    String::from_utf8_lossy(&self.s[st..self.i]).into_owned(),
                ))
            }
            _ => Err(self.err("a value")),
        }
    }
}

/// Parse JSON (comments refused: a file with them is left alone).
pub fn parse(text: &str) -> Result<J, String> {
    if text.trim().is_empty() {
        return Ok(J::Obj(vec![]));
    }
    let mut p = P {
        s: text.as_bytes(),
        i: 0,
    };
    let v = p.val()?;
    p.ws();
    if p.i < p.s.len() {
        return Err(p.err("more after the end (comments?)"));
    }
    Ok(v)
}

fn esc(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Pretty-print, two spaces (how Claude Code writes its settings).
pub fn print(j: &J) -> String {
    fn go(j: &J, ind: usize, out: &mut String) {
        let pad = |n: usize| "  ".repeat(n);
        match j {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(n),
            J::Str(s) => esc(s, out),
            J::Arr(v) if v.is_empty() => out.push_str("[]"),
            J::Arr(v) => {
                out.push_str("[\n");
                for (i, x) in v.iter().enumerate() {
                    out.push_str(&pad(ind + 1));
                    go(x, ind + 1, out);
                    if i + 1 < v.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(ind));
                out.push(']');
            }
            J::Obj(v) if v.is_empty() => out.push_str("{}"),
            J::Obj(v) => {
                out.push_str("{\n");
                for (i, (k, x)) in v.iter().enumerate() {
                    out.push_str(&pad(ind + 1));
                    esc(k, out);
                    out.push_str(": ");
                    go(x, ind + 1, out);
                    if i + 1 < v.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                out.push_str(&pad(ind));
                out.push('}');
            }
        }
    }
    let mut s = String::new();
    go(j, 0, &mut s);
    s.push('\n');
    s
}

// ------------------------------------------------------------------ the edits

/// Claude Code's hook command: the payload from stdin, on one line.
pub fn claude_command() -> String {
    format!(
        "d=$(tr -d '\\n'); mkdir -p \"$HOME/.local/state/aleph\" && printf '%s\\n' \"{{\\\"t\\\":$(date +%s),\\\"agent\\\":\\\"claude\\\",\\\"pane\\\":\\\"${{HERDR_PANE_ID:-}}\\\",\\\"hook\\\":${{d:-null}}}}\" >> \"{SPOOL}\" {TAG}"
    )
}

/// The events Claude Code tells us about.
const CLAUDE_EVENTS: &[&str] = &["Notification", "Stop"];

fn is_ours(entry: &J) -> bool {
    entry
        .get("hooks")
        .and_then(|h| match h {
            J::Arr(v) => Some(v),
            _ => None,
        })
        .is_some_and(|v| {
            v.iter().any(|x| {
                x.get("command")
                    .and_then(J::as_str)
                    .is_some_and(|c| c.contains(TAG))
            })
        })
}

/// settings.json with our entries (pure — tested). Unchanged text when
/// they're there already.
pub fn claude_install(text: &str) -> Result<String, String> {
    let mut j = parse(text)?;
    if !matches!(j, J::Obj(_)) {
        return Err("settings.json isn't an object".into());
    }
    let mut changed = false;
    let hooks = j
        .obj_entry("hooks", J::Obj(vec![]))
        .ok_or("hooks isn't an object")?;
    if !matches!(hooks, J::Obj(_)) {
        return Err("\"hooks\" isn't an object".into());
    }
    for ev in CLAUDE_EVENTS {
        let arr = hooks
            .obj_entry(ev, J::Arr(vec![]))
            .ok_or("hooks isn't an object")?;
        let J::Arr(v) = arr else {
            return Err(format!("hooks.{ev} isn't a list"));
        };
        if v.iter().any(is_ours) {
            continue;
        }
        v.push(J::Obj(vec![
            ("matcher".into(), J::Str(String::new())),
            (
                "hooks".into(),
                J::Arr(vec![J::Obj(vec![
                    ("type".into(), J::Str("command".into())),
                    ("command".into(), J::Str(claude_command())),
                    ("async".into(), J::Bool(true)),
                ])]),
            ),
        ]));
        changed = true;
    }
    Ok(if changed { print(&j) } else { text.to_string() })
}

/// settings.json without our entries (anything else untouched).
pub fn claude_remove(text: &str) -> Result<String, String> {
    let mut j = parse(text)?;
    let mut changed = false;
    if let Some(J::Obj(hooks)) = j.get_mut("hooks") {
        for (_, arr) in hooks.iter_mut() {
            if let J::Arr(v) = arr {
                let n = v.len();
                v.retain(|e| !is_ours(e));
                changed |= v.len() != n;
            }
        }
        hooks.retain(|(_, a)| !matches!(a, J::Arr(v) if v.is_empty()));
    }
    Ok(if changed { print(&j) } else { text.to_string() })
}

pub fn claude_installed(text: &str) -> bool {
    parse(text).ok().is_some_and(|j| {
        j.get("hooks").is_some_and(|h| {
            CLAUDE_EVENTS
                .iter()
                .all(|ev| matches!(h.get(ev), Some(J::Arr(v)) if v.iter().any(is_ours)))
        })
    })
}

/// Codex's notify script: append the payload (its last argument), then run
/// the notify you had, with the same payload.
fn codex_script() -> String {
    format!(
        "for j; do :; done; mkdir -p \"$HOME/.local/state/aleph\" && printf '%s\\n' \"{{\\\"t\\\":$(date +%s),\\\"agent\\\":\\\"codex\\\",\\\"pane\\\":\\\"${{HERDR_PANE_ID:-}}\\\",\\\"hook\\\":$j}}\" >> \"{SPOOL}\"; n=$#; if [ $n -gt 1 ]; then i=1; for a; do [ $i -lt $n ] && set -- \"$@\" \"$a\"; i=$((i+1)); done; shift $n; exec \"$@\" \"$j\"; fi {TAG}"
    )
}

/// config.toml with our notify (the one you had runs after it).
pub fn codex_install(text: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    let prev: Vec<String> = match doc.get("notify").and_then(|x| x.as_array()) {
        Some(a) => {
            let v: Vec<String> = a
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect();
            if v.iter().any(|x| x.contains(TAG)) {
                return Ok(text.to_string());
            }
            v
        }
        None => vec![],
    };
    let mut a = toml_edit::Array::new();
    a.push("sh");
    a.push("-c");
    a.push(codex_script());
    a.push("aleph-hook");
    for p in &prev {
        a.push(p.as_str());
    }
    doc.insert("notify", toml_edit::value(a));
    Ok(doc.to_string())
}

/// config.toml with the notify you had before ours (or none).
pub fn codex_remove(text: &str) -> Result<String, String> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| format!("config.toml: {e}"))?;
    let Some(a) = doc.get("notify").and_then(|x| x.as_array()) else {
        return Ok(text.to_string());
    };
    let v: Vec<String> = a
        .iter()
        .filter_map(|x| x.as_str().map(String::from))
        .collect();
    if !v.iter().any(|x| x.contains(TAG)) {
        return Ok(text.to_string());
    }
    // ["sh", "-c", script, "aleph-hook", prev…]
    let prev: Vec<String> = v.into_iter().skip(4).collect();
    if prev.is_empty() {
        doc.remove("notify");
    } else {
        let mut a = toml_edit::Array::new();
        for p in &prev {
            a.push(p.as_str());
        }
        doc.insert("notify", toml_edit::value(a));
    }
    Ok(doc.to_string())
}

/// OpenCode's plugin (the whole file is ours). `put` swallows every error: a
/// hook must never break the agent.
pub const OPENCODE_PLUGIN: &str = r#"// Tells Aleph when OpenCode needs you or finishes: one JSON line per event in
// ~/.local/state/aleph/spool.jsonl. Written by `aleph hooks install`;
// `aleph hooks remove` deletes it. # aleph-hook v1
import { appendFileSync, mkdirSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

const dir = join(homedir(), ".local", "state", "aleph")
const children = new Set()
const obj = (v) => (v && typeof v === "object" ? v : {})
const str = (o, ...ks) => {
  for (const k of ks) if (typeof o[k] === "string" && o[k]) return o[k]
  return ""
}
const put = (hook) => {
  try {
    mkdirSync(dir, { recursive: true })
    const line = { t: Math.floor(Date.now() / 1000), agent: "opencode", pane: process.env.HERDR_PANE_ID || "", hook }
    appendFileSync(join(dir, "spool.jsonl"), JSON.stringify(line) + "\n")
  } catch {}
}

export const AlephHook = async () => ({
  event: async ({ event }) => {
    const p = obj(event && event.properties)
    const info = obj(p.info)
    const id = str(p, "sessionID", "sessionId") || str(info, "id", "sessionID")
    if (id && str(info, "parentID", "parentId")) children.add(id)
    if (id && children.has(id)) return
    switch (event.type) {
      case "session.idle":
        put({ type: "idle" })
        break
      case "permission.asked":
      case "permission.updated": {
        const q = obj(p.permission).title ? obj(p.permission) : p
        put({ type: "permission", message: str(q, "title", "type", "pattern") })
        break
      }
      case "question.asked":
        put({ type: "question", message: str(p, "question", "prompt", "message", "title") || str(obj(p.question), "question", "prompt", "text", "title") })
        break
      case "session.error": {
        const e = obj(p.error)
        if (e.name === "MessageAbortedError") break
        put({ type: "error", message: str(obj(e.data), "message") || str(e, "message", "name") })
        break
      }
    }
  },
})
"#;

pub fn codex_installed(text: &str) -> bool {
    text.parse::<toml_edit::DocumentMut>()
        .ok()
        .is_some_and(|d| {
            d.get("notify").and_then(|x| x.as_array()).is_some_and(|a| {
                a.iter()
                    .any(|x| x.as_str().is_some_and(|s| s.contains(TAG)))
            })
        })
}

/// A minimal line diff: `- old` / `+ new` for what changed.
pub fn diff(old: &str, new: &str) -> Vec<String> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let pre = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..]
        .iter()
        .rev()
        .zip(b[pre..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mut out = Vec::new();
    for l in &a[pre..a.len() - suf] {
        out.push(format!("- {l}"));
    }
    for l in &b[pre..b.len() - suf] {
        out.push(format!("+ {l}"));
    }
    out
}

// ------------------------------------------------------------------ remote files

/// One agent's config file on a machine.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Claude,
    Codex,
    OpenCode,
}

/// Every agent Aleph can hook.
pub const TARGETS: [Target; 3] = [Target::Claude, Target::Codex, Target::OpenCode];

impl Target {
    fn path(self) -> &'static str {
        match self {
            Target::Claude => "$HOME/.claude/settings.json",
            Target::Codex => "$HOME/.codex/config.toml",
            Target::OpenCode => "$HOME/.config/opencode/plugins/aleph-hook.js",
        }
    }
    /// Is the agent set up there? (Claude Code: always worth a settings
    /// file; the others: their config or folder exists.)
    fn home(self) -> Option<&'static str> {
        match self {
            Target::Claude => None,
            Target::Codex => Some("$HOME/.codex/config.toml"),
            Target::OpenCode => Some("$HOME/.config/opencode"),
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Target::Claude => "claude",
            Target::Codex => "codex",
            Target::OpenCode => "opencode",
        }
    }
    fn parse(s: &str) -> Option<Target> {
        match s {
            "claude" | "claude-code" => Some(Target::Claude),
            "codex" => Some(Target::Codex),
            "opencode" => Some(Target::OpenCode),
            _ => None,
        }
    }
    /// The file with our hooks in or out ("" for OpenCode out: the file goes).
    fn edit(self, text: &str, install: bool) -> Result<String, String> {
        match (self, install) {
            (Target::Claude, true) => claude_install(text),
            (Target::Claude, false) => claude_remove(text),
            (Target::Codex, true) => codex_install(text),
            (Target::Codex, false) => codex_remove(text),
            (Target::OpenCode, true) => Ok(OPENCODE_PLUGIN.to_string()),
            // only a file that's ours
            (Target::OpenCode, false) if text.contains(TAG) => Ok(String::new()),
            (Target::OpenCode, false) => Ok(text.to_string()),
        }
    }
    fn installed(self, text: &str) -> bool {
        match self {
            Target::Claude => claude_installed(text),
            Target::Codex => codex_installed(text),
            Target::OpenCode => text == OPENCODE_PLUGIN,
        }
    }
}

/// Is `t` set up on `host` (so there's something to hook)?
fn agent_here(host: &str, t: Target) -> bool {
    let Some(p) = t.home() else { return true };
    proc::run(
        &mut sshcfg::shell(host, &format!("[ -e \"{p}\" ]")),
        Duration::from_secs(15),
        None,
    )
    .is_ok_and(|o| o.ok())
}

/// (exists, text, cksum) of a file on a machine.
fn read_remote(host: &str, path: &str) -> Result<(bool, String, String), String> {
    let cmd = format!(
        "f=\"{path}\"; if [ -f \"$f\" ]; then printf 'CK %s\\n' \"$(cksum < \"$f\")\"; cat \"$f\"; else echo NONE; fi"
    );
    let o = proc::run(
        &mut sshcfg::shell(host, &cmd),
        Duration::from_secs(20),
        None,
    )
    .map_err(|e| e.to_string())?;
    if !o.ok() {
        return Err(o.reason());
    }
    let t = o.text();
    if t.trim() == "NONE" {
        return Ok((false, String::new(), String::new()));
    }
    let (first, rest) = t.split_once('\n').unwrap_or((&t, ""));
    let ck = first.strip_prefix("CK ").unwrap_or("").trim().to_string();
    Ok((true, rest.to_string(), ck))
}

/// Write it only if it's still what we read (`ck`), keeping a backup. Empty
/// text removes the file (only ever one of ours).
fn write_remote(host: &str, path: &str, ck: &str, text: &str) -> Result<String, String> {
    if text.is_empty() {
        let cmd = format!(
            "f=\"{path}\"; [ \"$(cksum < \"$f\")\" = \"{ck}\" ] || {{ echo CHANGED; exit 4; }}; rm -f \"$f\" && echo OK none"
        );
        let o = proc::run(
            &mut sshcfg::shell(host, &cmd),
            Duration::from_secs(20),
            None,
        )
        .map_err(|e| e.to_string())?;
        let t = o.text();
        if t.contains("CHANGED") {
            return Err("the file changed while we looked — nothing removed; try again".into());
        }
        return if o.ok() && t.contains("OK") {
            Ok("none".into())
        } else {
            Err(o.reason())
        };
    }
    let stamp = crate::herdrtheme::stamp(crate::cache::now());
    let cmd = format!(
        "f=\"{path}\"; n=\"$f.aleph-new\"; mkdir -p \"$(dirname \"$f\")\" && cat > \"$n\" || exit 3; \
         if [ -f \"$f\" ]; then [ \"$(cksum < \"$f\")\" = \"{ck}\" ] || {{ rm -f \"$n\"; echo CHANGED; exit 4; }}; b=\"$f.aleph-bak-{stamp}-$$\"; cp -p \"$f\" \"$b\" || exit 5; else b=none; fi; \
         mv \"$n\" \"$f\" && echo OK \"$b\""
    );
    let o = proc::run(
        &mut sshcfg::shell(host, &cmd),
        Duration::from_secs(20),
        Some(text.as_bytes()),
    )
    .map_err(|e| e.to_string())?;
    let t = o.text();
    if t.contains("CHANGED") {
        return Err("the file changed while we looked — nothing written; try again".into());
    }
    if !o.ok() || !t.contains("OK") {
        return Err(o.reason());
    }
    Ok(t.trim().trim_start_matches("OK ").to_string())
}

/// Which agents on `host` have Aleph's hooks: (agent, file exists, installed).
/// (agent, set up there, hooks installed) — for every agent Aleph can hook.
pub fn status(host: &str) -> Vec<(Target, bool, bool)> {
    TARGETS
        .iter()
        .map(|t| match read_remote(host, t.path()) {
            Ok((ex, text, _)) => {
                let on = ex && t.installed(&text);
                (
                    *t,
                    on || (ex && *t != Target::OpenCode) || agent_here(host, *t),
                    on,
                )
            }
            Err(_) => (*t, false, false),
        })
        .collect()
}

// ------------------------------------------------------------------ the spool

/// One line of a machine's spool → an event (None: not one we understand).
pub fn spool_event(
    host: &str,
    line: &str,
    session_of: &dyn Fn(&str) -> Option<String>,
) -> Option<Event> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let agent = v.get("agent")?.as_str()?.to_string();
    let t = v
        .get("t")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_else(crate::cache::now);
    let pane = v
        .get("pane")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    // an agent outside herdr (a terminal of its own): nothing Aleph can
    // answer or open, and you're at it
    if pane.is_empty() {
        return None;
    }
    let name = session_of(pane).unwrap_or_default();
    let h = v.get("hook")?;
    let s = |k: &str| {
        h.get(k)
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    // (kind, the agent's own words — empty when it gave none, what else to say)
    let (kind, words, plain) = match agent.as_str() {
        "claude" => match s("hook_event_name").as_str() {
            // "waiting for your input" after a minute at rest isn't a
            // question, and a login isn't news: only what asks something
            "Notification"
                if matches!(
                    s("notification_type").as_str(),
                    "idle_prompt" | "auth_success"
                ) || s("message").contains("waiting for your input") =>
            {
                return None
            }
            "Notification" => (Kind::Blocked, s("message"), "needs you"),
            // (a subagent's own stop isn't the session's turn ending)
            "Stop" => (Kind::Done, s("last_assistant_message"), "finished its turn"),
            _ => return None,
        },
        "codex" => match s("type").as_str() {
            "agent-turn-complete" => (Kind::Done, s("last-assistant-message"), "finished its turn"),
            _ => return None,
        },
        "opencode" => match s("type").as_str() {
            "idle" => (Kind::Done, String::new(), "finished its turn"),
            "permission" => (
                Kind::Blocked,
                match s("message") {
                    m if m.is_empty() => String::new(),
                    m => format!("OpenCode needs your permission: {m}"),
                },
                "needs your permission",
            ),
            "question" => (Kind::Blocked, s("message"), "has a question for you"),
            "error" => (Kind::Error, s("message"), "stopped with an error"),
            _ => return None,
        },
        _ => return None,
    };
    let words = words.trim().to_string();
    Some(Event {
        t,
        host: host.to_string(),
        name,
        kind,
        title: match words.lines().next() {
            Some(l) if !l.trim().is_empty() => crate::ui::trunc(l.trim(), 200),
            _ => plain.to_string(),
        },
        agent,
        source: "agent".into(),
        body: crate::ui::trunc(&words, 1000),
        pane: pane.to_string(),
        // which conversation: Claude Code says `session_id`, Codex `thread-id`
        native: [s("session_id"), s("thread-id")]
            .into_iter()
            .find(|x| !x.trim().is_empty())
            .unwrap_or_default(),
        ..Default::default()
    })
}

/// New lines of a machine's spool after `offset`: (lines, new offset). A
/// spool over 256 KB is moved aside after reading (appends start a new one).
pub fn read_spool(host: &str, offset: u64) -> Result<(Vec<String>, u64), String> {
    let cmd = format!(
        "f=\"{SPOOL}\"; s=$(wc -c < \"$f\" 2>/dev/null | tr -d ' '); s=${{s:-0}}; echo \"SIZE $s\"; \
         if [ \"$s\" -gt {offset} ]; then tail -c +{} \"$f\"; fi; \
         if [ \"$s\" -gt 262144 ]; then mv \"$f\" \"$f.old\"; echo; echo ROTATED; fi",
        offset + 1
    );
    let o = proc::run(
        &mut sshcfg::shell(host, &cmd),
        Duration::from_secs(15),
        None,
    )
    .map_err(|e| e.to_string())?;
    if !o.ok() {
        return Err(o.reason());
    }
    let t = o.text();
    let mut lines = t.lines();
    let size: u64 = lines
        .next()
        .and_then(|l| l.strip_prefix("SIZE "))
        .and_then(|x| x.trim().parse().ok())
        .unwrap_or(0);
    let mut out: Vec<String> = Vec::new();
    let mut rotated = false;
    for l in lines {
        if l == "ROTATED" {
            rotated = true;
        } else if !l.trim().is_empty() {
            out.push(l.to_string());
        }
    }
    // a file smaller than what we'd read: it was replaced; read it whole next time
    let next = if rotated || size < offset { 0 } else { size };
    if size < offset && !rotated {
        return Ok((vec![], 0));
    }
    Ok((out, next))
}

// ------------------------------------------------------------------ the CLI

pub fn cmd(a: crate::cli::Args) -> ExitCode {
    let p = a.positional();
    let sub = p.first().copied().unwrap_or("status");
    let hosts: Vec<String> = match &a.flag {
        Some(h) => vec![h.clone()],
        None => a.cfg.fleet(),
    };
    let only = a
        .rest
        .iter()
        .position(|x| x == "--agent")
        .and_then(|i| a.rest.get(i + 1))
        .and_then(|x| Target::parse(x));
    let targets: Vec<Target> = match only {
        Some(t) => vec![t],
        None => TARGETS.to_vec(),
    };
    match sub {
        "status" | "ls" => {
            for h in &hosts {
                for (t, ex, on) in status(h) {
                    let what = match (ex, on) {
                        (_, true) => "installed".to_string(),
                        (true, false) => "not installed".to_string(),
                        (false, _) => "no config file (agent not set up there)".to_string(),
                    };
                    out(ratatui::text::Line::raw(format!(
                        "  {h:<10} {:<8} {what}",
                        t.id()
                    )));
                }
            }
            hint(&format!("{} hooks install [--host H] [--agent claude|codex|opencode] — the agents tell Aleph in their own words", crate::doctor::me()));
            ExitCode::SUCCESS
        }
        "install" | "remove" => {
            let install = sub == "install";
            let mut bad = 0;
            for h in &hosts {
                for t in &targets {
                    let (ex, text, ck) = match read_remote(h, t.path()) {
                        Ok(x) => x,
                        Err(e) => {
                            err(&format!("{h}: {e}"));
                            bad += 1;
                            continue;
                        }
                    };
                    if !ex && !install {
                        continue;
                    }
                    if !ex && !agent_here(h, *t) {
                        // the agent isn't set up there: nothing to hook
                        continue;
                    }
                    let new = match t.edit(&text, install) {
                        Ok(x) => x,
                        Err(e) => {
                            err(&format!("{h} {}: {e} — left as it is", t.path()));
                            bad += 1;
                            continue;
                        }
                    };
                    if new == text {
                        ok(&format!(
                            "{h} {}: {}",
                            t.id(),
                            if install {
                                "already installed"
                            } else {
                                "nothing of ours"
                            }
                        ));
                        continue;
                    }
                    head(&format!("{h}: {}", t.path().replace("$HOME", "~")));
                    for l in diff(&text, &new) {
                        say(ratatui::text::Line::raw(format!("    {l}")));
                    }
                    if !a.has("--yes") && !yes("Write it (a backup is kept)?") {
                        continue;
                    }
                    match write_remote(h, t.path(), &ck, &new) {
                        Ok(bak) => {
                            // the opt-in, remembered (integration installs re-check it)
                            let mut cfg = a.cfg.clone();
                            let _ = cfg.set(
                                &format!("host.{h}.hooks"),
                                if install { "on" } else { "off" },
                            );
                            ok(&format!(
                                "{h} {}: {} (backup {})",
                                t.id(),
                                if install { "installed" } else { "removed" },
                                bak.replace("$HOME", "~")
                            ))
                        }
                        Err(e) => {
                            err(&format!("{h} {}: {e}", t.id()));
                            bad += 1;
                        }
                    }
                }
            }
            if bad > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        // what the agents said lately, as Aleph reads it (troubleshooting)
        "tail" => {
            let mut bad = 0;
            for h in &hosts {
                match read_spool(h, 0) {
                    Ok((lines, _)) => {
                        let evs: Vec<Event> = lines
                            .iter()
                            .filter_map(|l| spool_event(h, l, &|_| None))
                            .collect();
                        if evs.is_empty() {
                            say(ratatui::text::Line::raw(format!("  {h:<10} nothing yet")));
                        }
                        for e in evs.iter().rev().take(10).rev() {
                            out(ratatui::text::Line::raw(format!(
                                "  {h:<10} {}  {:<8} {:<8} {}",
                                crate::ui::clock(e.t),
                                e.agent,
                                e.kind.as_str(),
                                e.title
                            )));
                        }
                    }
                    Err(e) => {
                        err(&format!("{h}: {e}"));
                        bad += 1;
                    }
                }
            }
            ExitCode::from(if bad > 0 { 1 } else { 0 })
        }
        _ => fail(&format!(
            "usage: {} hooks [status | install | remove | tail] [--host H] [--agent claude|codex|opencode] [--yes]",
            crate::doctor::me()
        )),
    }
}

/// What `hooks install` would change on `host`, per agent: (agent id, the
/// diff). An empty diff: already there. Agents with nothing to hook there
/// (no Codex config) are left out. Reads only (setup's preview).
pub fn preview(host: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut out = Vec::new();
    for t in TARGETS {
        let (ex, text, _) = read_remote(host, t.path())?;
        if !ex && !agent_here(host, t) {
            continue;
        }
        let new = t
            .edit(&text, true)
            .map_err(|e| format!("{}: {e}", t.path()))?;
        out.push((t.id().to_string(), diff(&text, &new)));
    }
    Ok(out)
}

/// `hooks install --yes` without printing (setup's apply): what it did.
/// The caller remembers the opt-in (`host.<h>.hooks = on`).
pub fn install_quiet(host: &str) -> Result<Vec<String>, String> {
    let mut done = Vec::new();
    for t in TARGETS {
        let (ex, text, ck) = read_remote(host, t.path())?;
        if !ex && !agent_here(host, t) {
            continue;
        }
        let new = t
            .edit(&text, true)
            .map_err(|e| format!("{}: {e}", t.path()))?;
        if new == text {
            done.push(format!("{} already had them", t.id()));
            continue;
        }
        let bak = write_remote(host, t.path(), &ck, &new)?;
        done.push(format!("{} (backup {})", t.id(), bak.replace("$HOME", "~")));
    }
    Ok(done)
}

/// After `herdr integration install` on an opted-in machine: our hooks back
/// where they were removed (no questions: the user opted in). What it did.
pub fn ensure(host: &str) -> Vec<String> {
    let mut out = Vec::new();
    for t in TARGETS {
        let Ok((ex, text, ck)) = read_remote(host, t.path()) else {
            continue;
        };
        if !ex || t.installed(&text) {
            continue;
        }
        if let Ok(new) = t.edit(&text, true) {
            if new != text && write_remote(host, t.path(), &ck, &new).is_ok() {
                out.push(format!("{host} {}: Aleph's hooks put back", t.id()));
            }
        }
    }
    out
}

/// A first look at a machine's spool (no saved place): lines this recent
/// are still news.
const FIRST_LOOK: u64 = 120;
/// Older than this, a line is never news (a spool read again from the start).
const STALE: u64 = 600;

fn offsets_file() -> std::path::PathBuf {
    crate::config::state_dir().join("spool_offsets.json")
}

/// Which spool lines become events: `now`, and whether it was a first look
/// (pure — tested).
pub fn news(evs: Vec<Event>, now: u64, first: bool) -> Vec<Event> {
    let keep = if first { FIRST_LOOK } else { STALE };
    evs.into_iter().filter(|e| e.t + keep >= now).collect()
}

/// One machine's read: (machine, new offset, lines, a first look).
type Read = (String, u64, Vec<String>, bool);

/// Reads every machine's spool now and then (the hub calls `tick`). Where
/// it got to is saved, so a restart (a deploy, `aleph update`) misses nothing.
#[derive(Default)]
pub struct Spool {
    offsets: std::collections::HashMap<String, u64>,
    loaded: bool,
    at: Option<std::time::Instant>,
    busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    rx: Option<std::sync::mpsc::Receiver<Read>>,
}

impl Spool {
    /// New agent events since last time (they go into the event log, where
    /// the router finds them). Off the UI thread: one `sh` per machine.
    pub fn tick(&mut self, cfg: &Config, fleet: &crate::fleet::Fleet) {
        use std::sync::atomic::Ordering;
        if cfg.get_or("notify.source") == "herdr" {
            return;
        }
        if !self.loaded {
            self.loaded = true;
            self.offsets = std::fs::read_to_string(offsets_file())
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
        }
        // results from the last read
        let mut moved = false;
        if let Some(rx) = &self.rx {
            while let Ok((host, off, lines, first)) = rx.try_recv() {
                moved |= self.offsets.insert(host.clone(), off) != Some(off);
                let sessions = fleet
                    .host(&host)
                    .map(|h| h.sessions.clone())
                    .unwrap_or_default();
                let of = |pane: &str| {
                    sessions
                        .iter()
                        .find(|s| s.pane_id == pane)
                        .map(|s| s.name.clone())
                };
                let evs: Vec<Event> = lines
                    .iter()
                    .filter_map(|l| spool_event(&host, l, &of))
                    .collect();
                let now = crate::cache::now();
                // logged as of now: the router reads the log forward in time,
                // and this line is a few seconds late
                let evs: Vec<Event> = news(evs, now, first)
                    .into_iter()
                    .map(|mut e| {
                        e.t = e.t.max(now);
                        e
                    })
                    .collect();
                if !evs.is_empty() {
                    crate::events::append_all(&evs);
                }
            }
        }
        if moved {
            if let Ok(j) = serde_json::to_vec(&self.offsets) {
                let _ = crate::config::write_atomic(&offsets_file(), &j);
            }
        }
        if self.busy.load(Ordering::SeqCst)
            || self
                .at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(5))
        {
            return;
        }
        self.at = Some(std::time::Instant::now());
        let hosts: Vec<(String, Option<u64>)> = fleet
            .hosts
            .iter()
            .filter(|h| {
                h.conn == crate::fleet::Conn::Up
                    && cfg.host_get(&h.alias, "hooks").as_deref() != Some("off")
            })
            .map(|h| (h.alias.clone(), self.offsets.get(&h.alias).copied()))
            .collect();
        if hosts.is_empty() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.rx = Some(rx);
        let busy = self.busy.clone();
        busy.store(true, Ordering::SeqCst);
        std::thread::spawn(move || {
            for (h, off) in hosts {
                // never looked at it: only its latest lines are news
                let first = off.is_none();
                if let Ok((lines, next)) = read_spool(&h, off.unwrap_or(0)) {
                    let _ = tx.send((h, next, lines, first));
                }
            }
            busy.store(false, Ordering::SeqCst);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTINGS: &str = r#"{
  "permissions": {
    "allow": [
      "Bash(ls:*)"
    ]
  },
  "model": "opus",
  "hooks": {
    "SessionStart": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "bash '/Users/u/.claude/hooks/herdr-agent-state.sh' session",
            "timeout": 10
          }
        ]
      }
    ],
    "Notification": [
      {
        "hooks": [
          {
            "async": true,
            "command": "'/opt/homebrew/bin/moshi-hook' claude-hook",
            "type": "command"
          }
        ],
        "matcher": "permission_prompt"
      }
    ]
  },
  "effortLevel": "high",
  "ratio": 1.5e3
}
"#;

    #[test]
    fn json_round_trips_in_order() {
        let j = parse(SETTINGS).unwrap();
        assert_eq!(print(&j), SETTINGS);
        assert!(parse("{\"a\": 1} // comment").is_err());
        assert_eq!(
            parse("\"\\u00e9\\ud83d\\ude00\"").unwrap(),
            J::Str("é😀".into())
        );
    }

    #[test]
    fn claude_hooks_coexist_and_come_out_cleanly() {
        let new = claude_install(SETTINGS).unwrap();
        assert!(claude_installed(&new));
        // herdr's and moshi-hook's entries are still there, in order
        let j = parse(&new).unwrap();
        let notes = match j.get("hooks").and_then(|h| h.get("Notification")) {
            Some(J::Arr(v)) => v.clone(),
            _ => panic!(),
        };
        assert_eq!(notes.len(), 2);
        assert!(new.contains("herdr-agent-state.sh") && new.contains("moshi-hook"));
        // the key order of the file is kept
        let keys: Vec<&str> = match &j {
            J::Obj(v) => v.iter().map(|(k, _)| k.as_str()).collect(),
            _ => vec![],
        };
        assert_eq!(
            keys,
            ["permissions", "model", "hooks", "effortLevel", "ratio"]
        );
        // twice is once
        assert_eq!(claude_install(&new).unwrap(), new);
        // removing gives back the original
        assert_eq!(claude_remove(&new).unwrap(), SETTINGS);
        // no file yet: a fresh one
        assert!(claude_installed(&claude_install("").unwrap()));
    }

    #[test]
    fn codex_notify_chains_yours() {
        let t = "model = \"gpt-5\"\nnotify = [\"notify-send\", \"Codex\"]\n";
        let new = codex_install(t).unwrap();
        assert!(codex_installed(&new));
        assert!(new.contains("\"notify-send\", \"Codex\"]"), "{new}");
        assert_eq!(codex_install(&new).unwrap(), new);
        let back = codex_remove(&new).unwrap();
        assert!(
            back.contains("notify = [\"notify-send\", \"Codex\"]"),
            "{back}"
        );
        assert!(!codex_installed(&back));
        // none before: none after
        let none = codex_remove(&codex_install("model = \"x\"\n").unwrap()).unwrap();
        assert!(!none.contains("notify"), "{none}");
    }

    #[test]
    fn the_hook_commands_run() {
        // Claude's: payload on stdin → one spool line
        let dir = std::env::temp_dir().join(format!("aleph-hook-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let o = std::process::Command::new("sh")
            .arg("-c")
            .arg(claude_command())
            .env("HOME", &dir)
            .env("HERDR_PANE_ID", "w1:p2")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                use std::io::Write;
                c.stdin.take().unwrap().write_all(b"{\"hook_event_name\":\"Notification\",\n\"message\":\"Claude needs your permission to use Bash\"}")?;
                c.wait()
            })
            .unwrap();
        assert!(o.success());
        let line = std::fs::read_to_string(dir.join(".local/state/aleph/spool.jsonl")).unwrap();
        let e = spool_event("b31", line.lines().next().unwrap(), &|p| {
            (p == "w1:p2").then(|| "temper".to_string())
        })
        .unwrap();
        assert_eq!(
            (e.kind, e.name.as_str(), e.source.as_str()),
            (Kind::Blocked, "temper", "agent")
        );
        assert!(e.title.contains("permission to use Bash"));
        // Codex's: payload as the last argument, then your notify with it
        let script = codex_script();
        let o = std::process::Command::new("sh")
            .args([
                "-c",
                &script,
                "aleph-hook",
                "sh",
                "-c",
                "printf '%s' \"$1\" > \"$HOME/chained\"",
                "x",
            ])
            .arg(
                "{\"type\":\"agent-turn-complete\",\"last-assistant-message\":\"All tests pass.\"}",
            )
            .env("HOME", &dir)
            .env("HERDR_PANE_ID", "w1:p3")
            .output()
            .unwrap();
        assert!(o.status.success(), "{o:?}");
        assert!(std::fs::read_to_string(dir.join("chained"))
            .unwrap()
            .contains("agent-turn-complete"));
        let all = std::fs::read_to_string(dir.join(".local/state/aleph/spool.jsonl")).unwrap();
        let e = spool_event("b31", all.lines().nth(1).unwrap(), &|_| None).unwrap();
        assert_eq!(e.kind, Kind::Done);
        assert_eq!(e.title, "All tests pass.");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_agent_in_its_own_words() {
        let ev = |l: &str| spool_event("b31", l, &|_| Some("heron".into()));
        // Claude's Stop: its last message when it gives one, else plain words
        let e = ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Stop","last_assistant_message":"Fixed the flaky test.\nAll 41 pass."}}"#).unwrap();
        assert_eq!(
            (e.kind, e.title.as_str()),
            (Kind::Done, "Fixed the flaky test.")
        );
        assert!(e.body.contains("All 41 pass") && e.pane == "w1:p2");
        // which conversation it was: Claude Code's session_id, Codex's thread-id
        let e = ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Stop","session_id":"5f0c-uuid"}}"#).unwrap();
        assert_eq!(e.native, "5f0c-uuid");
        let e = ev(r#"{"t":5,"agent":"codex","pane":"w1:p2","hook":{"type":"agent-turn-complete","thread-id":"019a-th"}}"#).unwrap();
        assert_eq!(e.native, "019a-th");
        let e = ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Stop"}}"#)
            .unwrap();
        assert_eq!(
            (e.title.as_str(), e.body.as_str()),
            ("finished its turn", "")
        );
        // at rest a minute, or outside herdr: not news
        assert!(ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Notification","notification_type":"idle_prompt","message":"Claude is waiting for your input"}}"#).is_none());
        assert!(ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Notification","message":"Claude is waiting for your input"}}"#).is_none());
        assert!(ev(r#"{"t":5,"agent":"claude","pane":"","hook":{"hook_event_name":"Stop","last_assistant_message":"hi"}}"#).is_none());
        let e = ev(r#"{"t":5,"agent":"claude","pane":"w1:p2","hook":{"hook_event_name":"Notification","notification_type":"permission_prompt","message":"Claude needs your permission to use Bash"}}"#).unwrap();
        assert_eq!(e.kind, Kind::Blocked);
        // OpenCode's plugin
        let e = ev(r#"{"t":5,"agent":"opencode","pane":"w1:p3","hook":{"type":"permission","message":"bash: rm -rf build"}}"#).unwrap();
        assert_eq!(e.kind, Kind::Blocked);
        assert_eq!(
            e.title,
            "OpenCode needs your permission: bash: rm -rf build"
        );
        let e = ev(r#"{"t":5,"agent":"opencode","pane":"w1:p3","hook":{"type":"idle"}}"#).unwrap();
        assert_eq!((e.kind, e.body.as_str()), (Kind::Done, ""));
        let e =
            ev(r#"{"t":5,"agent":"opencode","pane":"w1:p3","hook":{"type":"error","message":"rate limited"}}"#)
                .unwrap();
        assert_eq!((e.kind, e.title.as_str()), (Kind::Error, "rate limited"));
        assert!(ev(r#"{"t":5,"agent":"opencode","hook":{"type":"session.status"}}"#).is_none());
    }

    #[test]
    fn a_restart_misses_nothing_recent() {
        let at = |t: u64| Event {
            t,
            ..Default::default()
        };
        let evs = || vec![at(1_000), at(1_500), at(1_950)];
        // a first look: the last two minutes
        assert_eq!(news(evs(), 2_000, true).len(), 1);
        // a saved place: everything after it, unless it's long past
        assert_eq!(news(evs(), 2_000, false).len(), 2);
    }

    #[test]
    fn opencode_plugin_is_ours_alone() {
        let t = Target::OpenCode;
        assert_eq!(t.edit("", true).unwrap(), OPENCODE_PLUGIN);
        assert!(t.installed(OPENCODE_PLUGIN));
        // an older version of ours is replaced, then removed whole
        let old = OPENCODE_PLUGIN.replace("v1", "v0");
        assert!(!t.installed(&old));
        assert_eq!(t.edit(&old, true).unwrap(), OPENCODE_PLUGIN);
        assert_eq!(t.edit(OPENCODE_PLUGIN, false).unwrap(), "");
        // someone else's file of that name stays
        assert_eq!(
            t.edit("export const X = 1\n", false).unwrap(),
            "export const X = 1\n"
        );
        assert!(OPENCODE_PLUGIN.contains(TAG));
        // one JSON line per event: the newline is JavaScript's, not a raw break
        assert!(OPENCODE_PLUGIN.contains(r#"JSON.stringify(line) + "\n")"#));
    }

    #[test]
    #[ignore]
    fn real_settings_round_trip() {
        // ALEPH_RT=<file> cargo test real_settings_round_trip -- --ignored
        let Ok(f) = std::env::var("ALEPH_RT") else {
            return;
        };
        let t = std::fs::read_to_string(&f).unwrap();
        let back = print(&parse(&t).unwrap());
        if back != t {
            for (i, (a, b)) in t.lines().zip(back.lines()).enumerate() {
                if a != b {
                    panic!("line {}: {a:?} vs {b:?}", i + 1);
                }
            }
            panic!("lengths differ: {} vs {}", t.len(), back.len());
        }
    }

    #[test]
    fn diffs() {
        assert_eq!(diff("a\nb\nc\n", "a\nx\nc\n"), vec!["- b", "+ x"]);
    }
}
