//! What an alert says, wherever it goes (ntfy, Moshi, Slack, a webhook, Web
//! Push): a short title in plain words, then what a phone needs to decide
//! without opening a laptop — which agent, on which machine, in which
//! folder, for how long, and what it actually asks, with its own options.
//!
//! Pure (tested): the caller gathers the context ([`Ctx::of`], one look at
//! the session's question when it waits) and each sink formats [`Msg`].
//! `notify.include_text` decides how much of the agent's words leave the
//! machine: off (none), summary (140 characters), full.

use crate::config::Config;
use crate::events::{Event, Kind};
use crate::herdr::Session;

/// What's known around an event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ctx {
    /// the agent's name (`Claude Code`)
    pub agent: String,
    /// where it works, `~`-short
    pub folder: String,
    /// what it's doing (the session's title)
    pub task: String,
    /// what it asks, and its numbered options
    pub question: String,
    pub options: Vec<(u32, String)>,
    /// how long it has waited / run, in seconds
    pub since: Option<u64>,
    /// a machine event: how many sessions are on it
    pub sessions_there: Option<usize>,
    /// a limit: when it resets
    pub resets: String,
    /// the session in the web console (when `api.url` is set)
    pub open_url: Option<String>,
    /// Approve / Deny buttons come with it
    pub buttons: bool,
    /// remote control covers this machine and agent (`remote.allow`)
    pub remote_ok: bool,
    /// a question remote control may not answer: why
    pub terminal_only: Option<String>,
    /// a finished turn: `this turn: 412k tok · $1.20 · 6 min` (`turncost.rs`),
    /// once its usage is read
    pub turn: Option<String>,
}

/// An alert, ready for a sink.
#[derive(Clone, Debug, PartialEq)]
pub struct Msg {
    pub title: String,
    pub body: String,
    /// ntfy's emoji tag first, then the machine and the agent
    pub tags: Vec<String>,
    /// 1 (min) … 5 (max), ntfy's scale
    pub priority: u8,
    pub click: Option<String>,
}

impl Ctx {
    /// From the session (when it's still there), the question on its screen
    /// (`detection`, read only for questions), and the config.
    pub fn of(cfg: &Config, e: &Event, s: Option<&Session>, detection: &str) -> Ctx {
        let mode = cfg.get_or("notify.include_text");
        let cut = |t: &str, n: usize| match mode.as_str() {
            "off" => String::new(),
            "full" => crate::ui::trunc(t.trim(), n.max(600)),
            _ => crate::ui::trunc(t.trim(), n),
        };
        let agent_id = if e.agent.is_empty() {
            s.map(|s| s.agent.clone()).unwrap_or_default()
        } else {
            e.agent.clone()
        };
        let mut c = Ctx {
            agent: if agent_id.is_empty() {
                String::new()
            } else {
                crate::agents::label(&agent_id)
            },
            ..Default::default()
        };
        if let Some(s) = s {
            c.folder = crate::ui::tilde_path(&s.cwd);
            c.task = cut(&s.title, 140);
            c.since = s.since.map(|t| crate::cache::now().saturating_sub(t));
            if let Some(r) = &s.limit {
                c.resets = r.clone();
            }
        } else if !e.title.is_empty() && e.kind != Kind::Error {
            c.task = cut(&e.title, 140);
        }
        if !detection.trim().is_empty() && mode != "off" {
            c.question = cut(&crate::moshi::question_text(detection), 300);
            c.options = crate::moshi::options(detection)
                .into_iter()
                .map(|(n, l)| (n, crate::ui::trunc(&l, 60)))
                .collect();
            c.terminal_only = crate::permit::not_from_phone(cfg, detection);
        }
        if !e.body.is_empty() && c.question.is_empty() && mode != "off" {
            // the agent's own words (its hook) when there's no question to read
            c.question = cut(&e.body, 300);
        }
        // showcase: what it says on its screen, redacted or hidden like the
        // screen (`showcase.screens`; its options stay)
        c.question = crate::showcase::screen_text(&c.question).into_owned();
        if e.name.is_empty() {
            c.sessions_there = None;
        }
        c.remote_ok = !agent_id.is_empty() && crate::permit::allowed(cfg, &e.host, &agent_id);
        c.open_url = open_url(cfg, e);
        c
    }
}

/// The session (or machine) in the web console, when it's reachable from a
/// phone (`api.url`).
pub fn open_url(cfg: &Config, e: &Event) -> Option<String> {
    if !cfg.flag("api.http") {
        return None;
    }
    let base = cfg.get("api.url")?;
    let base = base.trim().trim_end_matches('/');
    if !base.starts_with("https://") && !base.starts_with("http://") {
        return None;
    }
    Some(if e.name.is_empty() {
        format!("{base}/#/fleet")
    } else {
        format!("{base}/#/s/{}:{}", e.host, e.name)
    })
}

/// The option an Approve / a Deny button picks (their labels).
pub fn yes_no(options: &[(u32, String)]) -> (Option<String>, Option<String>) {
    let low = |l: &str| l.trim().to_lowercase();
    let yes = options
        .iter()
        .find(|(_, l)| low(l).starts_with("yes"))
        .or(options.first())
        .map(|(n, l)| format!("{n}. {l}"));
    let no = options
        .iter()
        .find(|(_, l)| low(l).starts_with("no"))
        .map(|(n, l)| format!("{n}. {l}"));
    (yes, no)
}

/// ntfy's emoji for a kind (its tag list shows the emoji first).
pub fn emoji(k: Kind) -> &'static str {
    match k {
        Kind::Blocked => "raising_hand",
        Kind::Done => "white_check_mark",
        Kind::New => "rocket",
        Kind::Gone => "checkered_flag",
        Kind::Error => "rotating_light",
        Kind::HostUp => "green_circle",
        Kind::Limit => "hourglass_flowing_sand",
        Kind::Auto => "zap",
        Kind::Task => "inbox_tray",
        Kind::Heal => "arrows_counterclockwise",
        Kind::Remote => "iphone",
        Kind::Match => "mag",
    }
}

/// ntfy's 1–5 from the event's severity.
fn priority(e: &Event) -> u8 {
    match e.severity() {
        crate::events::Severity::High => 4,
        crate::events::Severity::Normal => 3,
        crate::events::Severity::Low => 2,
    }
}

/// `Claude Code · b31 · ~/ivy · waiting 2m`
fn where_line(e: &Event, c: &Ctx, verb: &str) -> String {
    let mut v: Vec<String> = Vec::new();
    if !c.agent.is_empty() {
        v.push(c.agent.clone());
    }
    v.push(e.host.clone());
    if !c.folder.is_empty() {
        v.push(c.folder.clone());
    }
    // a minute or more: under that, it just happened
    if let (Some(s), false) = (c.since, verb.is_empty()) {
        if s >= 60 {
            v.push(format!("{verb} {}", crate::ui::dur(s)));
        }
    }
    v.join(" · ")
}

/// The words for a session's own moment (Moshi's cards), as if it were an
/// event of `kind` about `s`.
pub fn about(cfg: &Config, s: &Session, kind: Kind, detection: &str) -> Msg {
    let e = Event {
        t: crate::cache::now(),
        host: s.host.clone(),
        name: s.name.clone(),
        kind,
        title: s.title.clone(),
        agent: s.agent.clone(),
        ..Default::default()
    };
    compose(&e, &Ctx::of(cfg, &e, Some(s), detection))
}

/// A finished turn of `s`: how long it worked (0: unknown), what the agent
/// said about it (its hook; may be empty), and what it took (`this turn:
/// 412k tok · …`, once its usage is read).
pub fn finished_turn(
    cfg: &Config,
    s: &Session,
    took: u64,
    words: &str,
    turn: Option<String>,
) -> Msg {
    let e = Event {
        t: crate::cache::now(),
        host: s.host.clone(),
        name: s.name.clone(),
        kind: Kind::Done,
        title: s.title.clone(),
        agent: s.agent.clone(),
        body: words.to_string(),
        took,
        ..Default::default()
    };
    let mut c = Ctx::of(cfg, &e, Some(s), "");
    c.turn = turn;
    compose(&e, &c)
}

/// `Claude Code · b31 · ~/ivy · waiting 2m` for a session (a question's
/// first line on the phone).
pub fn where_of(cfg: &Config, s: &Session) -> String {
    let e = Event {
        host: s.host.clone(),
        name: s.name.clone(),
        agent: s.agent.clone(),
        kind: Kind::Blocked,
        ..Default::default()
    };
    where_line(&e, &Ctx::of(cfg, &e, Some(s), ""), "waiting")
}

/// The alert for `e`, in words.
pub fn compose(e: &Event, c: &Ctx) -> Msg {
    let who = if e.name.is_empty() {
        e.host.clone()
    } else if e.name.chars().all(|ch| ch.is_ascii_digit()) && !c.agent.is_empty() {
        // an unnamed herdr tab ("1"): the agent and its folder say more
        match c.folder.trim_end_matches('/').rsplit('/').next() {
            Some(d) if !d.is_empty() && d != "~" => format!("{} in {d}", c.agent),
            _ => c.agent.clone(),
        }
    } else {
        e.name.clone()
    };
    let mut body: Vec<String> = Vec::new();
    let title = match e.kind {
        Kind::Blocked => {
            body.push(where_line(e, c, "waiting"));
            if !c.question.is_empty() {
                body.push(c.question.clone());
            }
            // one option a line: easier to read on a phone
            for (n, l) in &c.options {
                body.push(format!("{n}. {l}"));
            }
            if !c.task.is_empty() && c.task != c.question {
                body.push(format!("Task: {}", c.task));
            }
            match (&c.terminal_only, c.buttons) {
                // a test says so below, nothing about answering
                _ if e.source == "test" => {}
                (Some(rule), _) => body.push(format!(
                    "This one waits for you at a terminal ({rule}): it can't be answered from the phone."
                )),
                // the buttons name the options they pick; say it only without
                (None, true) if c.options.is_empty() => {
                    body.push("Approve answers yes · Deny says no.".into())
                }
                (None, true) => {}
                // no buttons although it's allowed: the question was gone, or
                // its screen couldn't be read in time
                (None, false) if c.remote_ok => {
                    body.push("Answer it in Aleph or at its terminal.".into())
                }
                (None, false) => body.push(format!(
                    "Answer it in Aleph (remote control is off for {}:{}).",
                    e.host,
                    agent_id_hint(e)
                )),
            }
            format!("{who} needs you")
        }
        Kind::Done => {
            body.push(where_line(e, c, ""));
            if let Some(t) = &c.turn {
                body.push(t.clone());
            }
            if !c.task.is_empty() {
                body.push(c.task.clone());
            }
            if !c.question.is_empty() {
                body.push(c.question.clone());
            }
            if e.took >= 60 {
                format!("{who} finished after {}", crate::ui::dur(e.took))
            } else {
                format!("{who} finished its turn")
            }
        }
        Kind::New => {
            body.push(where_line(e, c, ""));
            if !c.task.is_empty() && !c.task.starts_with('/') && !c.task.starts_with('~') {
                body.push(c.task.clone());
            }
            format!("{who} started")
        }
        Kind::Gone => {
            body.push(where_line(e, c, ""));
            format!("{who} ended")
        }
        Kind::Limit => {
            body.push(where_line(e, c, ""));
            let r = if c.resets.is_empty() {
                e.title.trim_start_matches("resets ").to_string()
            } else {
                c.resets.clone()
            };
            body.push(if r.is_empty() {
                "Out of quota until its plan resets. Queue a message and it goes the moment it can work again.".into()
            } else {
                format!("Back {}. Queue a message and it goes then: aleph send {}:{} \"…\" --after idle", crate::limits::label(&r), e.host, e.name)
            });
            format!("{who} hit its usage limit")
        }
        Kind::Error => {
            let (head, detail) = crate::tui::inbox::explain_error(&e.title);
            body.push(if detail.is_empty() {
                head
            } else {
                format!("{head} ({detail})")
            });
            body.push(match c.sessions_there {
                Some(n) if n > 0 => format!(
                    "{n} session{} there {} out of reach. Aleph keeps trying and tells you when it's back.",
                    if n == 1 { "" } else { "s" },
                    if n == 1 { "is" } else { "are" }
                ),
                _ => "Aleph keeps trying and tells you when it's back.".into(),
            });
            format!("{} is unreachable", e.host)
        }
        Kind::HostUp => {
            if let Some(n) = c.sessions_there {
                body.push(format!(
                    "{n} session{} reachable again.",
                    if n == 1 { "" } else { "s" }
                ));
            }
            format!("{} is back", e.host)
        }
        Kind::Auto => {
            body.push(where_line(e, c, ""));
            if !e.title.is_empty() {
                body.push(e.title.clone());
            }
            format!("{who}: auto-approved")
        }
        Kind::Task => {
            if !e.title.is_empty() {
                body.push(e.title.clone());
            }
            format!("queued message sent to {who}")
        }
        Kind::Heal => {
            body.push("herdr had stopped there; Aleph started it again.".into());
            format!("herdr restarted on {}", e.host)
        }
        Kind::Remote => {
            body.push(where_line(e, c, ""));
            format!("{who}: {}", e.title)
        }
        Kind::Match => {
            body.push(where_line(e, c, ""));
            if !e.body.is_empty() {
                body.push(e.body.clone());
            }
            format!("{who}: {}", e.title)
        }
    };
    let mut tags = vec![emoji(e.kind).to_string(), e.host.clone()];
    if let Some(a) = agent_tag(e) {
        tags.push(a);
    }
    let mut title = title;
    if e.source == "test" {
        title = format!("Test · {title}");
        body.push("This is a test from Aleph: nothing is really waiting.".into());
        tags.push("test".into());
    }
    Msg {
        title,
        body: body
            .into_iter()
            .filter(|l| !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        tags,
        priority: priority(e),
        click: c.open_url.clone(),
    }
}

fn agent_tag(e: &Event) -> Option<String> {
    (!e.agent.is_empty()).then(|| {
        crate::agents::find(&e.agent)
            .map(|a| a.id.to_string())
            .unwrap_or_else(|| e.agent.clone())
    })
}

fn agent_id_hint(e: &Event) -> String {
    agent_tag(e).unwrap_or_else(|| "*".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: Kind) -> Event {
        Event {
            t: 1,
            host: "b31".into(),
            name: "temper".into(),
            kind,
            title: "Run the cargo benches".into(),
            agent: "claude".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_finished_turn_says_how_long_and_what() {
        let cfg = Config::in_memory(&[]);
        let mut e = ev(Kind::Done);
        e.took = 340;
        e.source = "agent".into();
        e.body = "Fixed the flaky test. All 41 pass.".into();
        let m = compose(&e, &Ctx::of(&cfg, &e, None, ""));
        assert_eq!(m.title, "temper finished after 5m");
        assert!(m.body.contains("All 41 pass"), "{}", m.body);
        e.took = 20;
        assert_eq!(
            compose(&e, &Ctx::of(&cfg, &e, None, "")).title,
            "temper finished its turn"
        );
        // its numbers, once its usage is read: right under where it ran
        e.took = 360;
        let mut c = Ctx::of(&cfg, &e, None, "");
        c.turn = Some("this turn: 412k tok · $1.20 · 6 min".into());
        let m = compose(&e, &c);
        let lines: Vec<&str> = m.body.lines().collect();
        assert_eq!(lines[0], "Claude Code · b31");
        assert_eq!(lines[1], "this turn: 412k tok · $1.20 · 6 min");
        assert!(m.body.contains("All 41 pass"), "{}", m.body);
        // not known in time: the alert goes without it
        assert!(!compose(&e, &Ctx::of(&cfg, &e, None, ""))
            .body
            .contains("this turn"));
    }

    const DET: &str = "Do you want to run this command?\n  cargo bench --bench parse_layout\n\n❯ 1. Yes\n  2. Yes, and don't ask again for cargo\n  3. No, and tell Claude what to do differently";

    #[test]
    fn a_question_says_what_it_asks_and_what_the_buttons_do() {
        let cfg = Config::in_memory(&[]);
        let mut c = Ctx::of(&cfg, &ev(Kind::Blocked), None, DET);
        c.folder = "~/ivy".into();
        c.since = Some(125);
        c.buttons = true;
        let m = compose(&ev(Kind::Blocked), &c);
        assert_eq!(m.title, "temper needs you");
        // a tab herdr only numbered is named by its agent and folder
        let mut tab = ev(Kind::Blocked);
        tab.name = "1".into();
        assert_eq!(compose(&tab, &c).title, "Claude Code in ivy needs you");
        let lines: Vec<&str> = m.body.lines().collect();
        assert_eq!(lines[0], "Claude Code · b31 · ~/ivy · waiting 2m");
        assert!(
            m.body.contains("Do you want to run this command?"),
            "{}",
            m.body
        );
        assert!(
            m.body.contains("1. Yes\n2. Yes, and don't ask again"),
            "{}",
            m.body
        );
        assert!(m.body.contains("Task: Run the cargo benches"), "{}", m.body);
        // the buttons say what they pick (router::ntfy_labels)
        assert!(!m.body.contains("Approve picks"), "{}", m.body);
        assert_eq!(m.tags, vec!["raising_hand", "b31", "claude"]);
        assert_eq!(m.priority, 4);
        // without remote control it says where to answer
        c.buttons = false;
        assert!(compose(&ev(Kind::Blocked), &c)
            .body
            .contains("remote control is off for b31:claude"));
    }

    #[test]
    fn what_leaves_the_machine_follows_include_text() {
        let off = Config::in_memory(&["notify.include_text=off"]);
        let c = Ctx::of(&off, &ev(Kind::Blocked), None, DET);
        assert!(c.question.is_empty() && c.options.is_empty() && c.task.is_empty());
        let m = compose(&ev(Kind::Blocked), &c);
        assert!(!m.body.contains("cargo bench"), "{}", m.body);
    }

    #[test]
    fn risky_questions_wait_for_a_terminal() {
        let cfg = Config::in_memory(&[]);
        let det = "Do you want to run this command?\n  rm -rf build\n\n❯ 1. Yes\n  2. No";
        let c = Ctx::of(&cfg, &ev(Kind::Blocked), None, det);
        assert!(c.terminal_only.is_some());
        assert!(compose(&ev(Kind::Blocked), &c)
            .body
            .contains("can't be answered from the phone"));
    }

    #[test]
    fn machines_and_limits_read_like_sentences() {
        let mut e = ev(Kind::Error);
        e.name.clear();
        e.title = "ssh: connect to host 10.0.0.9 port 22: Operation timed out".into();
        let c = Ctx {
            sessions_there: Some(3),
            ..Default::default()
        };
        let m = compose(&e, &c);
        assert_eq!(m.title, "b31 is unreachable");
        assert!(
            m.body.contains("3 sessions there are out of reach"),
            "{}",
            m.body
        );
        assert_eq!(m.tags[0], "rotating_light");
        let m = compose(&ev(Kind::Limit), &Ctx::default());
        assert_eq!(m.title, "temper hit its usage limit");
        assert!(m.body.contains("Queue a message"), "{}", m.body);
    }

    #[test]
    fn the_console_link_needs_a_public_url() {
        let none = Config::in_memory(&["api.http=on"]);
        assert_eq!(open_url(&none, &ev(Kind::Blocked)), None);
        let yes = Config::in_memory(&["api.http=on", "api.url=https://box.tail.ts.net/"]);
        assert_eq!(
            open_url(&yes, &ev(Kind::Blocked)).as_deref(),
            Some("https://box.tail.ts.net/#/s/b31:temper")
        );
    }

    #[test]
    fn yes_and_no_buttons_pick_the_agents_own_options() {
        let o = crate::moshi::options(DET);
        let (y, n) = yes_no(&o);
        assert_eq!(y.as_deref(), Some("1. Yes"));
        assert!(n.unwrap().starts_with("3. No"));
    }
}
