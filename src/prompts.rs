//! The prompt library: saved prompts with variables, insertable wherever you
//! write one (the send box, the queue, a new session's first prompt) with
//! ctrl+p or as `@name`, and from the shell (`aleph send x @review`).
//!
//! They're settings, `prompt.<name>` (`[prompts]` in aleph.toml), so a
//! profile or a backup carries them. In the text, `{dir}`, `{folder}`,
//! `{host}`, `{session}`, `{agent}`, `{task}` (the session's title), `{date}`
//! and `{time}` fill themselves in from the session; any other `{word}` is
//! asked for when you use it (or `--var word=…` from the shell). The starters
//! (review, tests, debug, refactor, docs, commit, handoff, explain, plan,
//! security) stay until you save your own by those names.
//!
//! A prompt has scenarios (`prompt_tags.<name>`) and may be for certain agents
//! (`prompt_agents.<name>`). `<name>-<agent>` is that agent's way of doing
//! `<name>`: `@review` in a Codex session is `review-codex` when there is one.

use crate::cli::Args;
use crate::config::Config;
use crate::out::*;
use ratatui::prelude::*;
use serde_json::json;
use std::process::ExitCode;

#[derive(Clone, Debug, PartialEq)]
pub struct Prompt {
    pub name: String,
    pub text: String,
    pub starter: bool,
    /// the scenarios it's for: review, tests, your own
    pub tags: Vec<String>,
    /// the agents it's only for (none: any agent)
    pub agents: Vec<String>,
}

fn words(s: &str) -> Vec<String> {
    s.split(',')
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// (name, its scenario, the agent it's only for, its text). `<name>-<agent>` is
/// that agent's way of doing `<name>`, used in its place for that agent.
const STARTERS: &[(&str, &str, &str, &str)] = &[
    (
        "review",
        "review",
        "",
        "Review the changes in {folder} since the last commit: bugs first, then anything risky or unclear. Don't change any files yet.",
    ),
    (
        "review-claude",
        "review",
        "claude",
        "Review the changes in {folder} since the last commit, with a subagent for anything that needs a wide search so this conversation stays short. Bugs first, then anything risky or unclear. Don't change anything.",
    ),
    (
        "review-codex",
        "review",
        "codex",
        "Review the uncommitted changes in {folder} and report only: bugs first, then anything risky or unclear, each with its file and line. Read-only: don't edit files or run anything that writes.",
    ),
    (
        "tests",
        "tests",
        "",
        "Run the test suite in {folder}, fix what fails, and tell me what you changed and why.",
    ),
    (
        "debug",
        "debug",
        "",
        "Find out why {symptom} happens in {folder}. Reproduce it first, then narrow it down to the cause, and show me the evidence before you change anything.",
    ),
    (
        "refactor",
        "refactor",
        "",
        "Refactor {target} in {folder} without changing what it does: smaller pieces, clearer names, less repetition. Run the tests before and after, and keep each step small.",
    ),
    (
        "docs",
        "docs",
        "",
        "Bring the docs in {folder} up to date with the code: what's wrong or missing, in the words the docs already use. Don't touch the code.",
    ),
    (
        "commit",
        "commit",
        "",
        "Write a commit message for what you changed, in this repo's style, and commit it.",
    ),
    (
        "handoff",
        "handoff",
        "",
        "Summarise this session for whoever takes it over: the task, what's done, the decisions made and why, what's left, and anything half-done.",
    ),
    (
        "explain",
        "explain",
        "",
        "Explain how {topic} works in this codebase: where it lives, how the pieces connect, and anything surprising.",
    ),
    (
        "plan",
        "plan",
        "",
        "Plan how to {goal} in {folder}: read what you need, then give me the steps, the files they touch and the risks. Don't change anything yet.",
    ),
    (
        "plan-claude",
        "plan",
        "claude",
        "Work in plan mode: read what you need in {folder}, then propose how to {goal}, with the steps, the files they touch and the risks. Don't change anything until I approve it.",
    ),
    (
        "security",
        "security",
        "",
        "Look for security problems in {folder}: injection, secrets in the code or the logs, unsafe defaults, missing checks where input comes in. Most serious first, each with its file and line and how to fix it. Don't change anything.",
    ),
];

/// Every prompt: yours (by name), then the starters you haven't replaced.
pub fn all(cfg: &Config) -> Vec<Prompt> {
    let mut v: Vec<Prompt> = cfg
        .entries()
        .into_iter()
        .filter_map(|(k, text)| {
            let name = k.strip_prefix("prompt.")?;
            (!name.is_empty() && !name.contains('.')).then(|| Prompt {
                name: name.to_string(),
                text,
                starter: false,
                tags: Vec::new(),
                agents: Vec::new(),
            })
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    for (n, tag, agent, t) in STARTERS {
        match v.iter_mut().find(|p| p.name == *n) {
            // yours by that name still belongs to its scenario
            Some(p) => {
                p.tags = words(tag);
                p.agents = words(agent);
            }
            None => v.push(Prompt {
                name: n.to_string(),
                text: t.to_string(),
                starter: true,
                tags: words(tag),
                agents: words(agent),
            }),
        }
    }
    // what you've said about each, over what it came with
    for p in &mut v {
        if let Some(t) = cfg.get_raw(&format!("prompt_tags.{}", p.name)) {
            p.tags = words(&t);
        }
        if let Some(a) = cfg.get_raw(&format!("prompt_agents.{}", p.name)) {
            p.agents = words(&a);
        }
    }
    v
}

/// What suits this agent: those for any agent and those for it, where its own
/// way of doing something stands in for the general one.
pub fn for_agent(cfg: &Config, kind: &str) -> Vec<Prompt> {
    let all = all(cfg);
    let names: Vec<String> = all.iter().map(|p| p.name.clone()).collect();
    all.into_iter()
        .filter(|p| p.agents.is_empty() || p.agents.iter().any(|a| a == kind))
        .filter(|p| !names.contains(&format!("{}-{kind}", p.name)))
        .collect()
}

/// `@name` for this agent: its own way of doing it, if it has one.
pub fn resolve(cfg: &Config, name: &str, kind: &str) -> Option<Prompt> {
    let name = name.trim_start_matches('@');
    if !kind.is_empty() {
        if let Some(p) = get(cfg, &format!("{name}-{kind}")) {
            return Some(p);
        }
    }
    get(cfg, name)
}

/// The scenarios among these, in the order they first come.
pub fn scenarios(list: &[Prompt]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for t in list.iter().flat_map(|p| p.tags.iter()) {
        if !v.contains(t) {
            v.push(t.clone());
        }
    }
    v
}

/// Does what's typed match it? Every word, somewhere in it.
pub fn matches(p: &Prompt, q: &str) -> bool {
    let hay = format!(
        "{} {} {} {}",
        p.name,
        p.tags.join(" "),
        p.agents.join(" "),
        p.text
    )
    .to_lowercase();
    q.split_whitespace()
        .all(|w| hay.contains(&w.to_lowercase()))
}

/// Your prompt, and what you said about it, gone.
pub fn remove(cfg: &mut Config, name: &str) {
    for k in ["prompt", "prompt_tags", "prompt_agents"] {
        let _ = cfg.unset(&format!("{k}.{name}"));
    }
}

/// A variable that fills itself in, rather than being asked for.
pub fn is_automatic(var: &str) -> bool {
    matches!(
        var,
        "dir" | "folder" | "host" | "session" | "agent" | "task" | "date" | "time"
    )
}

pub fn get(cfg: &Config, name: &str) -> Option<Prompt> {
    let name = name.trim_start_matches('@');
    all(cfg).into_iter().find(|p| p.name == name)
}

/// A prompt's name from what you'd type: `Fix the ticket` → `fix-the-ticket`.
pub fn slug(s: &str) -> String {
    crate::workspace::slug(s)
}

/// The variables in a text, in order, each once: `{topic}`, `{ticket_id}`.
pub fn vars(text: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find('{') {
        let after = &rest[i + 1..];
        let n = after
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
            .count();
        if n > 0 && after.as_bytes().get(n) == Some(&b'}') && !after.as_bytes()[0].is_ascii_digit()
        {
            let name = &after[..n];
            if !v.iter().any(|x| x == name) {
                v.push(name.to_string());
            }
            rest = &after[n + 1..];
        } else {
            rest = after;
        }
    }
    v
}

/// What a prompt's session is: the values the automatic variables take.
#[derive(Clone, Debug, Default)]
pub struct Ctx {
    pub host: String,
    pub session: String,
    pub agent: String,
    /// the agent's id (`codex`), for its own way of doing a prompt
    pub kind: String,
    pub dir: String,
    pub task: String,
}

impl Ctx {
    pub fn of(s: &crate::herdr::Session) -> Ctx {
        Ctx {
            host: s.host.clone(),
            session: s.name.clone(),
            agent: crate::agents::find(&s.agent)
                .map(|a| a.name.to_string())
                .unwrap_or_else(|| s.agent.clone()),
            kind: s.agent.clone(),
            dir: s.cwd.clone(),
            task: s.title.clone(),
        }
    }

    /// An automatic variable's value (None: not one, or nothing to say).
    pub fn value(&self, var: &str) -> Option<String> {
        let now = crate::cache::now();
        let v = match var {
            "dir" => self.dir.clone(),
            "folder" => self
                .dir
                .trim_end_matches('/')
                .rsplit('/')
                .next()
                .unwrap_or("")
                .to_string(),
            "host" | "machine" => self.host.clone(),
            "session" => self.session.clone(),
            "agent" => self.agent.clone(),
            "task" => self.task.clone(),
            "date" => {
                let d = crate::ui::local_day(now);
                let (y, m, day) = crate::ui::civil(d);
                format!("{y:04}-{m:02}-{day:02}")
            }
            "time" => crate::ui::clock(now),
            _ => return None,
        };
        (!v.is_empty()).then_some(v)
    }
}

/// The variables a text still needs from you, given its session.
pub fn asks(text: &str, ctx: &Ctx) -> Vec<String> {
    vars(text)
        .into_iter()
        .filter(|v| ctx.value(v).is_none())
        .collect()
}

/// The text with every variable filled: the session's, then `given`.
pub fn fill(text: &str, ctx: &Ctx, given: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for v in vars(text) {
        let val = given
            .iter()
            .find(|(k, _)| *k == v)
            .map(|(_, x)| x.clone())
            .or_else(|| ctx.value(&v));
        if let Some(x) = val {
            out = out.replace(&format!("{{{v}}}"), &x);
        }
    }
    out
}

/// `@name` as the whole of a prompt, expanded (Err: what's missing).
pub fn expand(
    cfg: &Config,
    text: &str,
    ctx: &Ctx,
    given: &[(String, String)],
) -> Option<Result<String, String>> {
    let t = text.trim();
    let name = t.strip_prefix('@')?;
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let p = resolve(cfg, name, &ctx.kind)?;
    let missing: Vec<String> = asks(&p.text, ctx)
        .into_iter()
        .filter(|v| !given.iter().any(|(k, _)| k == v))
        .collect();
    Some(if missing.is_empty() {
        Ok(fill(&p.text, ctx, given))
    } else {
        Err(format!(
            "@{name} needs {}: --var {}=…",
            missing
                .iter()
                .map(|v| format!("{{{v}}}"))
                .collect::<Vec<_>>()
                .join(", "),
            missing[0]
        ))
    })
}

pub fn cmd(mut a: Args) -> ExitCode {
    let pos: Vec<String> = a.positional().iter().map(|s| s.to_string()).collect();
    let json_out = a.has("--json");
    match pos.first().map(String::as_str) {
        None | Some("ls" | "list") => {
            let wanted = |flag: &str| {
                a.rest
                    .iter()
                    .position(|x| x == flag)
                    .and_then(|i| a.rest.get(i + 1))
                    .cloned()
            };
            let (tag, agent) = (wanted("--tag"), wanted("--agent"));
            let all: Vec<Prompt> = match &agent {
                Some(k) => for_agent(&a.cfg, k),
                None => all(&a.cfg),
            }
            .into_iter()
            .filter(|p| tag.as_ref().is_none_or(|t| p.tags.contains(t)))
            .collect();
            if json_out {
                json(&json!(all
                    .iter()
                    .map(|p| json!({"name": p.name, "text": p.text, "vars": vars(&p.text), "starter": p.starter}))
                    .collect::<Vec<_>>()));
                return ExitCode::SUCCESS;
            }
            head("prompts  ·  ctrl+p or @name where you write one");
            for p in &all {
                out(Line::from(vec![
                    Span::styled(
                        format!("  @{}  ", crate::ui::pad(&p.name, 12)),
                        crate::ui::fg(crate::ui::c::ACCENT()),
                    ),
                    Span::raw(crate::ui::trunc(&p.text, 90)),
                    Span::styled(
                        format!(
                            "  {}{}{}",
                            p.tags.join(" "),
                            if p.agents.is_empty() {
                                String::new()
                            } else {
                                format!(" · for {}", p.agents.join(", "))
                            },
                            if p.starter { " · starter" } else { "" }
                        ),
                        crate::ui::faint(),
                    ),
                ]));
            }
            hint("aleph prompt add NAME \"text with {vars}\" · ls --tag review --agent codex · aleph send b31:hunt @review");
            ExitCode::SUCCESS
        }
        Some("show") => match pos.get(1).and_then(|n| get(&a.cfg, n)) {
            Some(p) => {
                raw(&p.text);
                ExitCode::SUCCESS
            }
            None => fail("no such prompt (aleph prompt ls)"),
        },
        Some("add" | "set") => {
            let (Some(n), true) = (pos.get(1), pos.len() > 2) else {
                return fail("usage: aleph prompt add NAME TEXT…");
            };
            let name = slug(n);
            if name.is_empty() {
                return fail("a prompt's name needs a letter or digit");
            }
            let text = pos[2..].join(" ");
            match a.cfg.set(&format!("prompt.{name}"), &text) {
                Ok(_) => {
                    ok(&format!("@{name} saved"));
                    let v = vars(&text);
                    if !v.is_empty() {
                        hint(&format!(
                            "its variables: {}",
                            v.iter()
                                .map(|x| format!("{{{x}}}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        ));
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&e.to_string()),
            }
        }
        Some("rm") => match pos.get(1) {
            Some(n) => {
                let k = format!("prompt.{}", n.trim_start_matches('@'));
                if a.cfg.get_raw(&k).is_none() {
                    return fail(&format!("no saved prompt {n}"));
                }
                let _ = a
                    .cfg
                    .unset(&format!("prompt_tags.{}", n.trim_start_matches('@')));
                let _ = a
                    .cfg
                    .unset(&format!("prompt_agents.{}", n.trim_start_matches('@')));
                match a.cfg.unset(&k) {
                    Ok(_) => {
                        ok(&format!("{n} deleted"));
                        ExitCode::SUCCESS
                    }
                    Err(e) => fail(&e.to_string()),
                }
            }
            None => fail("usage: aleph prompt rm NAME"),
        },
        Some(_) => fail(
            "usage: aleph prompt [ls [--tag T] [--agent A]] | show NAME | add NAME TEXT… | rm NAME",
        ),
    }
}

/// `--var k=v` pairs on a command line.
pub fn given(rest: &[String]) -> Vec<(String, String)> {
    let mut v = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--var" {
            if let Some((k, x)) = rest.get(i + 1).and_then(|s| s.split_once('=')) {
                v.push((k.to_string(), x.to_string()));
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_agent_gets_its_own_way_of_doing_a_prompt() {
        let cfg = crate::config::Config::in_memory(&[
            "prompt.mine=for everyone",
            "prompt.only=for codex",
            "prompt_agents.only=codex",
            "prompt_tags.mine=ops, Nightly",
        ]);
        let names = |kind: &str| -> Vec<String> {
            for_agent(&cfg, kind).into_iter().map(|p| p.name).collect()
        };
        // its variant in place of the general one; another agent's hidden
        let codex = names("codex");
        assert!(codex.contains(&"review-codex".to_string()));
        assert!(!codex.contains(&"review".to_string()));
        assert!(!codex.contains(&"review-claude".to_string()));
        assert!(codex.contains(&"only".to_string()));
        let other = names("gemini");
        assert!(other.contains(&"review".to_string()));
        assert!(!other.contains(&"only".to_string()));
        assert!(other.contains(&"mine".to_string()));
        // @review resolves the same way, and a named variant stays itself
        let of = |kind: &str| resolve(&cfg, "@review", kind).unwrap().name;
        assert_eq!(of("codex"), "review-codex");
        assert_eq!(of("gemini"), "review");
        assert_eq!(of(""), "review");
        assert_eq!(
            resolve(&cfg, "review-codex", "claude").unwrap().name,
            "review-codex"
        );
        // scenarios: lower-cased, from what you said and what they came with
        let mine = get(&cfg, "mine").unwrap();
        assert_eq!(mine.tags, vec!["ops".to_string(), "nightly".to_string()]);
        let tags = scenarios(&all(&cfg));
        for t in ["review", "tests", "debug", "refactor", "docs", "commit"] {
            assert!(tags.contains(&t.to_string()), "{t}");
        }
        for t in ["handoff", "explain", "plan", "security", "ops"] {
            assert!(tags.contains(&t.to_string()), "{t}");
        }
        // every word of a search, in anything it has
        assert!(matches(&mine, "NIGHTLY everyone"));
        assert!(!matches(&mine, "nightly gone"));
    }

    #[test]
    fn variables_fill_from_the_session_and_ask_for_the_rest() {
        assert_eq!(
            vars("fix {ticket} in {dir}, then {ticket} again; {} {9x} {Up}"),
            ["ticket", "dir"]
        );
        let ctx = Ctx {
            host: "b31".into(),
            session: "hunt".into(),
            agent: "Claude Code".into(),
            kind: "claude".into(),
            dir: "/srv/px/".into(),
            task: String::new(),
        };
        assert_eq!(
            asks("explain {topic} in {folder} on {host} ({task})", &ctx),
            ["topic", "task"]
        );
        assert_eq!(
            fill(
                "{folder} on {host}: {topic}",
                &ctx,
                &[("topic".into(), "LUTs".into())]
            ),
            "px on b31: LUTs"
        );
        let cfg = Config::in_memory(&["prompt.fix=Fix {ticket} in {folder}"]);
        assert_eq!(
            expand(&cfg, "@fix", &ctx, &[("ticket".into(), "VD-7".into())])
                .unwrap()
                .unwrap(),
            "Fix VD-7 in px"
        );
        assert!(expand(&cfg, "@fix", &ctx, &[])
            .unwrap()
            .unwrap_err()
            .contains("{ticket}"));
        // starters, and not every @ is a prompt
        assert!(expand(&cfg, "@review", &ctx, &[]).unwrap().is_ok());
        assert!(expand(&cfg, "@nope", &ctx, &[]).is_none());
        assert!(expand(&cfg, "@fix this please", &ctx, &[]).is_none());
        assert!(expand(&cfg, "plain text", &ctx, &[]).is_none());
        assert_eq!(
            given(&["--var".into(), "a=b c".into(), "x".into()]),
            [("a".to_string(), "b c".to_string())]
        );
    }
}
