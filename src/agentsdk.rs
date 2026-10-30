//! Cursor's Agent SDK: agents written as code, a folder of Markdown and TypeScript
//! (`bot/instructions.md`, `bot/agent.ts`, `bot/tools/…`; cursor.com/docs/agent-sdk).
//! Run and watched here as Fleet sessions: `aleph sdk dev | validate | run`, Agent SDK
//! in the `n` form and the new-session form, `S` on a project in the folder browser.
//!
//! herdr doesn't recognise `agent-sdk`. It's typed into a pane of a tab labelled
//! `agentsdk_<name>`, and the row's state is whatever runs in that pane's foreground
//! (`Herdr::foreground_many`), as for a shell's command: working while it runs, and a
//! notification when it finishes (`notify.commands`).
//!
//! Signing in stays yours: `agent-sdk login` opens a browser and stores a key that
//! Aleph never touches. `status` asks `agent-sdk whoami` and keeps its exit code only.
//! There are no tokens or cost here yet, and nothing deployed is watched.

use crate::cli::{self, Args};
use crate::out::{fail, head, hint, json, ok, out, warn, Spinner};
use crate::ui;
use ratatui::prelude::*;
use serde_json::json;
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::process::ExitCode;
use std::time::Duration;

/// Its id in the agent registry, and its tabs' label prefix.
pub const ID: &str = "agentsdk";
/// Its command.
pub const EXE: &str = "agent-sdk";
/// What the docs say it installs from (it was `@cursor/bdk` before).
pub const INSTALL: &str = "npm i -g @grok/agent-sdk";
/// The oldest Node it runs on.
const MIN_NODE: (u64, u64) = (22, 13);
/// How deep `projects` looks under a folder.
const FIND_DEPTH: u32 = 5;

const USAGE: &str = "usage: aleph sdk [status | projects [DIR] | dev [DIR] | validate [DIR] | run [DIR] -m TEXT] [--host H] [--name N] [--json]";

/// What a pane starts with when nothing else was asked for.
pub fn default_args(args: &[String]) -> Vec<String> {
    if args.is_empty() {
        vec!["dev".to_string()]
    } else {
        args.to_vec()
    }
}

/// A folder's markers (`remote::Info`) say it's a project: `bot/` with its instructions
/// or agent file, beside a package.json. `agent-sdk validate` has the last word.
pub fn is_project(markers: &[String]) -> bool {
    let has = |m: &str| markers.iter().any(|x| x == m);
    has("package.json") && (has("bot/instructions.md") || has("bot/agent.ts"))
}

/// `agent-sdk <verb>`'s words, for a project in the pane's own folder.
fn verb_args(verb: &str, messages: &[String]) -> Result<Vec<String>, String> {
    match verb {
        "dev" => Ok(vec!["dev".into()]),
        "validate" => Ok(vec!["validate".into(), "--dir".into(), ".".into()]),
        "run" => {
            if messages.is_empty() {
                return Err("aleph sdk run needs -m TEXT: what to send the agent".into());
            }
            let mut v: Vec<String> = vec!["run".into(), "--dir".into(), ".".into()];
            for m in messages {
                v.push("--message".into());
                v.push(m.clone());
            }
            Ok(v)
        }
        other => Err(format!("{other}? (dev, validate or run)")),
    }
}

/// What a machine has.
#[derive(Clone, Debug, Default, PartialEq)]
struct Status {
    version: Option<String>,
    node: Option<String>,
    /// `whoami`'s answer; None when it didn't give one
    signed_in: Option<bool>,
}

impl Status {
    fn node_ok(&self) -> Option<bool> {
        self.node.as_deref().map(node_new_enough)
    }
}

/// `v22.14.0` → is it at least the oldest it runs on?
fn node_new_enough(v: &str) -> bool {
    let mut it = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|x| x.parse::<u64>().unwrap_or(0));
    let (a, b) = (it.next().unwrap_or(0), it.next().unwrap_or(0));
    (a, b) >= MIN_NODE
}

/// One `sh -s`: the versions, and whoami's exit code. Its output never leaves the
/// machine: whoami names a credential.
const STATUS_SH: &str = r#"LC_ALL=C; export LC_ALL
command -v node >/dev/null 2>&1 && printf 'N\t%s\n' "$(node --version 2>/dev/null)"
if command -v agent-sdk >/dev/null 2>&1; then
  printf 'V\t%s\n' "$(agent-sdk --version 2>/dev/null | head -n 1)"
  AGENT_SERVE_NO_UPDATE_CHECK=1 agent-sdk whoami >/dev/null 2>&1 </dev/null
  printf 'W\t%s\n' "$?"
fi
"#;

fn parse_status(text: &str) -> Status {
    let mut st = Status::default();
    for l in text.lines() {
        match l.split_once('\t') {
            Some(("N", v)) if !v.trim().is_empty() => st.node = Some(v.trim().to_string()),
            Some(("V", v)) => {
                st.version = Some(v.trim().to_string());
            }
            // the documented exit codes: 0 ok, 5 unauthenticated or denied
            Some(("W", v)) => {
                st.signed_in = match v.trim() {
                    "0" => Some(true),
                    "5" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    st
}

/// One line of sh that lists the projects under `dir` (a project: `bot/` with its
/// instructions or agent file, beside a package.json).
fn find_script(dir: &str) -> String {
    format!(
        r#"LC_ALL=C; export LC_ALL
D={}
case "$D" in "~") D=$HOME ;; "~/"*) D=$HOME/${{D#\~/}} ;; esac
find "$D" -maxdepth {FIND_DEPTH} \( -name node_modules -o -name .git -o -name target -o -name .cache -o -name Library \) -prune -o -type f \( -path '*/bot/instructions.md' -o -path '*/bot/agent.ts' \) -print 2>/dev/null | while IFS= read -r f; do
  r=${{f%/bot/*}}
  [ -f "$r/package.json" ] && printf 'P\t%s\n' "$r"
done
exit 0
"#,
        crate::proc::sh_quote(dir)
    )
}

fn parse_projects(text: &str) -> Vec<String> {
    let set: BTreeSet<String> = text
        .lines()
        .filter_map(|l| l.strip_prefix("P\t"))
        .map(String::from)
        .collect();
    set.into_iter().collect()
}

pub fn cmd(a: Args) -> ExitCode {
    let mut host = a.flag.clone();
    let mut name: Option<String> = None;
    let mut messages: Vec<String> = Vec::new();
    let mut words: Vec<&str> = Vec::new();
    let want_json = a.has("--json");
    let mut i = 0;
    while i < a.rest.len() {
        let w = a.rest[i].as_str();
        let next = a.rest.get(i + 1);
        match w {
            "--host" | "-H" => match next {
                Some(h) => {
                    host = Some(h.clone());
                    i += 1;
                }
                None => return fail("--host needs a machine"),
            },
            "--name" | "-n" => match next {
                Some(n) => {
                    name = Some(n.clone());
                    i += 1;
                }
                None => return fail("--name needs a name"),
            },
            "-m" | "--message" => match next {
                Some(m) => {
                    messages.push(m.clone());
                    i += 1;
                }
                None => return fail("-m needs some text"),
            },
            "--json" => {}
            "-h" | "--help" => {
                println!("{USAGE}");
                println!("  Cursor's Agent SDK, run and watched as sessions — aleph help sdk");
                return ExitCode::SUCCESS;
            }
            _ if w.starts_with('-') => return fail(&format!("unknown argument '{w}' ({USAGE})")),
            _ => words.push(w),
        }
        i += 1;
    }
    match words.first().copied() {
        None | Some("status") => status(&a, host, want_json),
        Some("projects") => projects(&a, host, words.get(1).copied(), want_json),
        Some(verb @ ("dev" | "validate" | "run")) => {
            start(&a, host, verb, words.get(1).copied(), name, &messages)
        }
        Some(other) => fail(&format!("{other}? ({USAGE})")),
    }
}

fn hosts_of(a: &Args, host: Option<String>) -> Vec<String> {
    match host {
        Some(h) => vec![h],
        None => a.cfg.fleet(),
    }
}

fn status(a: &Args, host: Option<String>, want_json: bool) -> ExitCode {
    let hosts = hosts_of(a, host);
    if hosts.is_empty() {
        return cli::no_host();
    }
    let sp = Spinner::start("Looking for agent-sdk".into());
    let got: Vec<(String, Result<Status, String>)> = std::thread::scope(|s| {
        let v: Vec<_> = hosts
            .iter()
            .map(|h| {
                s.spawn(move || {
                    let r = crate::probe::sh(h, STATUS_SH, Duration::from_secs(25))
                        .map(|o| parse_status(&o.text()));
                    (h.clone(), r)
                })
            })
            .collect();
        v.into_iter().filter_map(|t| t.join().ok()).collect()
    });
    sp.stop();
    if want_json {
        json(&json!(got
            .iter()
            .map(|(h, r)| match r {
                Ok(st) => json!({"host": h, "version": st.version, "node": st.node,
                    "node_ok": st.node_ok(), "signed_in": st.signed_in}),
                Err(e) => json!({"host": h, "error": e}),
            })
            .collect::<Vec<_>>()));
        return ExitCode::SUCCESS;
    }
    if !std::io::stdout().is_terminal() {
        println!("host\tversion\tnode\tsigned_in");
        for (h, r) in &got {
            match r {
                Ok(st) => println!(
                    "{h}\t{}\t{}\t{}",
                    st.version.as_deref().unwrap_or(""),
                    st.node.as_deref().unwrap_or(""),
                    st.signed_in.map_or("", |x| if x { "yes" } else { "no" })
                ),
                Err(_) => println!("{h}\t\t\t"),
            }
        }
        return ExitCode::SUCCESS;
    }
    head("Agent SDK · Cursor's agents as code");
    let width = got.iter().map(|(h, _)| ui::width(h)).max().unwrap_or(5);
    let mut any = false;
    for (h, r) in &got {
        let label = Span::styled(ui::pad(h, width), ui::bold());
        let text = match r {
            Err(e) => Span::styled(format!("  can't tell: {e}"), ui::muted()),
            Ok(st) => match &st.version {
                None => Span::styled("  not installed".to_string(), ui::faint()),
                Some(v) => {
                    any = true;
                    let who = match st.signed_in {
                        Some(true) => "signed in",
                        Some(false) => "not signed in",
                        None => "sign-in unknown",
                    };
                    let node = match (&st.node, st.node_ok()) {
                        (Some(n), Some(false)) => format!(" · node {n} is too old"),
                        (None, _) => " · no node".to_string(),
                        _ => String::new(),
                    };
                    Span::raw(format!("  {v} · {who}{node}"))
                }
            },
        };
        out(Line::from(vec![label, text]));
    }
    println!();
    if !any {
        hint(&format!(
            "install it where you want it (Node {}.{}+): {INSTALL}, then agent-sdk login",
            MIN_NODE.0, MIN_NODE.1
        ));
    }
    if got
        .iter()
        .any(|(_, r)| matches!(r, Ok(st) if st.signed_in == Some(false)))
    {
        hint("signing in is yours: agent-sdk login on that machine");
    }
    hint("aleph sdk projects · aleph sdk dev DIR · aleph sdk run DIR -m \"…\"");
    ExitCode::SUCCESS
}

fn projects(a: &Args, host: Option<String>, dir: Option<&str>, want_json: bool) -> ExitCode {
    let hosts = hosts_of(a, host);
    if hosts.is_empty() {
        return cli::no_host();
    }
    let script = find_script(dir.unwrap_or("~"));
    let mut all: Vec<(String, String)> = Vec::new();
    let sp = Spinner::start("Looking for Agent SDK projects".into());
    let mut errors = Vec::new();
    for h in &hosts {
        match crate::probe::sh(h, &script, Duration::from_secs(30)) {
            Ok(o) => all.extend(
                parse_projects(&o.text())
                    .into_iter()
                    .map(|p| (h.clone(), p)),
            ),
            Err(e) => errors.push((h.clone(), e)),
        }
    }
    sp.stop();
    if want_json {
        json(&json!(all
            .iter()
            .map(|(h, p)| json!({"host": h, "dir": p}))
            .collect::<Vec<_>>()));
        return ExitCode::SUCCESS;
    }
    for (h, e) in &errors {
        warn(&format!("{h}: {e}"));
    }
    if !std::io::stdout().is_terminal() {
        for (h, p) in &all {
            println!("{h}\t{p}");
        }
        return ExitCode::SUCCESS;
    }
    if all.is_empty() {
        ok("no Agent SDK projects found");
        hint("a project has bot/instructions.md beside its package.json: agent-sdk init ./my-bot");
        return ExitCode::SUCCESS;
    }
    for (h, p) in &all {
        out(Line::from(vec![
            Span::styled(format!("{h}  "), ui::bold()),
            Span::raw(p.clone()),
        ]));
    }
    println!();
    hint("start one: aleph sdk dev DIR --host MACHINE");
    ExitCode::SUCCESS
}

/// A session running `agent-sdk <verb>` in the folder, through `aleph new`.
fn start(
    a: &Args,
    host: Option<String>,
    verb: &str,
    dir: Option<&str>,
    name: Option<String>,
    messages: &[String],
) -> ExitCode {
    let args = match verb_args(verb, messages) {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    let mut rest: Vec<String> = vec!["--agent".into(), ID.into(), "--detach".into()];
    if let Some(h) = host {
        rest.extend(["--host".into(), h]);
    }
    if let Some(d) = dir {
        rest.extend(["--dir".into(), d.into()]);
    }
    if let Some(n) = name {
        rest.extend(["--name".into(), n]);
    }
    rest.push("--".into());
    rest.extend(args);
    cli::new(Args {
        cfg: a.cfg.clone(),
        flag: a.flag.clone(),
        rest: &rest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_starts_dev_unless_told_otherwise() {
        assert_eq!(default_args(&[]), vec!["dev"]);
        assert_eq!(default_args(&["validate".to_string()]), vec!["validate"]);
    }

    #[test]
    fn the_verbs_become_the_documented_commands() {
        assert_eq!(verb_args("dev", &[]).unwrap(), vec!["dev"]);
        assert_eq!(
            verb_args("validate", &[]).unwrap(),
            vec!["validate", "--dir", "."]
        );
        assert_eq!(
            verb_args("run", &["hi".into(), "again".into()]).unwrap(),
            vec!["run", "--dir", ".", "--message", "hi", "--message", "again"]
        );
        assert!(verb_args("run", &[]).is_err(), "run needs a message");
        assert!(verb_args("deploy", &[]).is_err(), "deploying stays yours");
    }

    #[test]
    fn node_has_a_floor() {
        assert!(node_new_enough("v22.13.0"));
        assert!(node_new_enough("v24.1.0"));
        assert!(!node_new_enough("v22.12.9"));
        assert!(!node_new_enough("v20.19.0"));
        assert!(!node_new_enough("nonsense"));
    }

    #[test]
    fn status_reads_versions_and_only_whoamis_exit_code() {
        let st = parse_status("N\tv22.14.0\nV\t0.5.0\nW\t0\n");
        assert_eq!(st.version.as_deref(), Some("0.5.0"));
        assert_eq!(st.signed_in, Some(true));
        assert_eq!(st.node_ok(), Some(true));
        assert_eq!(parse_status("W\t5\n").signed_in, Some(false));
        assert_eq!(parse_status("W\t1\n").signed_in, None);
        let none = parse_status("N\tv20.0.0\n");
        assert_eq!(none.version, None);
        assert_eq!(none.node_ok(), Some(false));
        assert!(
            STATUS_SH.contains("whoami >/dev/null 2>&1"),
            "what whoami says stays there"
        );
    }

    #[test]
    fn a_project_is_a_package_beside_bot() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(is_project(&m(&["package.json", "bot/instructions.md"])));
        assert!(is_project(&m(&["package.json", "bot/agent.ts"])));
        assert!(!is_project(&m(&["package.json"])));
        assert!(!is_project(&m(&["bot/instructions.md"])));
    }

    #[test]
    fn projects_are_found_by_their_files_and_listed_once() {
        let t = std::env::temp_dir().join(format!("aleph-sdk-{}", std::process::id()));
        let root = t.join("work/my-bot");
        std::fs::create_dir_all(root.join("bot")).unwrap();
        std::fs::write(root.join("package.json"), "{}").unwrap();
        std::fs::write(root.join("bot/instructions.md"), "x").unwrap();
        std::fs::write(root.join("bot/agent.ts"), "x").unwrap();
        // no package.json: not a project
        let other = t.join("work/not-one/bot");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("instructions.md"), "x").unwrap();
        // inside dependencies: never looked at
        let dep = t.join("work/my-bot/node_modules/dep/bot");
        std::fs::create_dir_all(&dep).unwrap();
        std::fs::write(dep.join("instructions.md"), "x").unwrap();
        std::fs::write(t.join("work/my-bot/node_modules/dep/package.json"), "{}").unwrap();
        let o = std::process::Command::new("sh")
            .args(["-c", &find_script(&t.to_string_lossy())])
            .output()
            .unwrap();
        let v = parse_projects(&String::from_utf8_lossy(&o.stdout));
        assert_eq!(v, vec![root.to_string_lossy().to_string()]);
        let _ = std::fs::remove_dir_all(&t);
    }
}
