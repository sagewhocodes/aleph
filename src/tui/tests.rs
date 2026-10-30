//! Dashboard tests: key map, layout maths, and TestBackend renders.

use super::*;
use crate::config::Config;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn k(c: KeyCode) -> KeyEvent {
    KeyEvent::new(c, KeyModifiers::NONE)
}

#[test]
fn key_strings() {
    assert_eq!(key_str(&k(KeyCode::Char('j'))), "j");
    assert_eq!(key_str(&k(KeyCode::Char(' '))), "space");
    assert_eq!(
        key_str(&KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        "ctrl+k"
    );
    assert_eq!(key_str(&k(KeyCode::BackTab)), "shift+tab");
    assert_eq!(key_str(&k(KeyCode::Enter)), "enter");
}

#[test]
fn keymap_overrides() {
    let cfg = Config::in_memory(&["key.down=n", "action.g=Git|lazygit"]);
    let keys = Keys::from_config(&cfg);
    assert_eq!(keys.act(&k(KeyCode::Char('n'))), Some(Act::Down));
    // the default for down is replaced, arrows still work
    assert_eq!(keys.act(&k(KeyCode::Char('j'))), None);
    assert_eq!(keys.act(&k(KeyCode::Down)), Some(Act::Down));
    assert_eq!(
        keys.custom,
        vec![("g".into(), "Git".into(), "lazygit".into())]
    );
}

#[test]
fn b64() {
    assert_eq!(b64_encode(b""), "");
    assert_eq!(b64_encode(b"f"), "Zg==");
    assert_eq!(b64_encode(b"fo"), "Zm8=");
    assert_eq!(b64_encode(b"foo"), "Zm9v");
    assert_eq!(b64_encode("✻ hi".as_bytes()), "4py7IGhp");
}

fn sess(host: &str, pane: &str, name: &str, status: &str) -> Session {
    Session {
        host: host.into(),
        pane_id: pane.into(),
        tab_id: "w1:t1".into(),
        label: format!("claude_{name}"),
        name: name.into(),
        agent: "claude".into(),
        status: status.into(),
        cwd: format!("/home/u/{name}"),
        title: format!("task of {name}"),
        mark: "◆".into(),
        hue: "amber".into(),
        ..Default::default()
    }
}

/// ctrl+]: keys back from the agent to Aleph (Focus stays).
fn leave_typing(d: &mut Dash) {
    d.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
    assert!(!d.wall.typing);
}

/// A dashboard whose settings file is its own: for tests that change a
/// setting (another test's dashboard would read the shared one back).
fn dash_own() -> Dash {
    let mut d = dash();
    d.cfg = Config::in_memory_own(&["hosts=b31,gpu2", "stats=off", "local=false"]);
    d
}

fn dash() -> Dash {
    // an isolated config so Fleet::new doesn't read the user's
    let dir = std::env::temp_dir().join(format!("cch-tui-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    std::env::set_var("ALEPH_CONFIG", dir.join("config"));
    std::env::set_var("XDG_CACHE_HOME", dir.join("cache"));
    // never touch the real queue, mux layout or inbox state
    std::env::set_var("XDG_STATE_HOME", dir.join("state"));
    std::env::set_var("CC_SSH_BIN", "/usr/bin/false");
    let cfg = Config::in_memory(&["hosts=b31,gpu2", "stats=off", "local=false"]);
    let (tx, rx) = channel();
    let mut fleet = crate::fleet::Fleet::new(&cfg, None);
    for (h, v) in [
        (
            "b31",
            vec![
                sess("b31", "w1:p1", "ignite", "working"),
                sess("b31", "w1:p2", "temper", "blocked"),
            ],
        ),
        ("gpu2", vec![sess("gpu2", "w1:p4", "weave", "done")]),
    ] {
        let x = fleet.host_mut(h).unwrap();
        x.sessions = v;
        x.conn = crate::fleet::Conn::Up;
        x.addr = format!("u@{h}");
    }
    Dash {
        fv: fleet::State::new(None),
        fb: super::board::State::default(),
        layout_mode: None,
        wall: wall::State::new(&[]),
        hv: hosts::State::default(),
        set: settings::State::default(),
        doc: doctor::State::default(),
        inbox: inbox::State::default(),
        mon: monitor::State::default(),
        usage: Default::default(),
        outside: Default::default(),
        appr: Default::default(),
        keys: Keys::from_config(&cfg),
        cfg,
        flag: None,
        view: View::Fleet,
        mode: Mode::Normal,
        tx,
        rx,
        t0: Instant::now(),
        fleet,
        tele: HashMap::new(),
        tokens_at: HashMap::new(),
        toasts: VecDeque::new(),
        hits: Hits::default(),
        last_click: None,
        rung: HashSet::new(),
        drift_told: HashSet::new(),
        hub: crate::auto::Hub::new(),
        owner: (false, Instant::now()),
        agents: crate::agentwatch::AgentWatch::fixed(vec![], &[]),
        services: vec![],
        hints: super::MoshiSession::new(false),
        hub_lock: None,
        api: None,
        api_seen: None,
        pending_focus: None,
        upgrade: crate::watch::Upgrade::start(),
        net: crate::net::Net::quiet(),
        net_was: 0,
        new_build: false,
        focused: true,
        onscreen_at: None,
        changed: crate::refresh::Watch::new(),
        ungated: false,
        tl: Default::default(),
        tv: Default::default(),
        motion: Default::default(),
        alerts: Default::default(),
    }
}

fn render(d: &mut Dash, w: u16, h: u16) -> String {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| d.draw(f)).unwrap();
    let b = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            (0..w)
                .map(|x| b[(x, y)].symbol().to_string())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn fleet_rows_and_selection() {
    let mut d = dash();
    let r = fleet::rows(&d);
    assert_eq!(r.len(), 5); // 2 headers + 3 sessions
                            // first session is selected by default
    assert_eq!(
        fleet::selected(&d),
        Some(fleet::RowId::Sess("b31|w1:p1".into()))
    );
    d.on_key(k(KeyCode::Char('j')));
    assert_eq!(
        fleet::selected(&d),
        Some(fleet::RowId::Sess("b31|w1:p2".into()))
    );
    d.on_key(k(KeyCode::Char(']')));
    assert_eq!(fleet::selected(&d), Some(fleet::RowId::Host("gpu2".into())));
    // fold gpu2
    d.on_key(k(KeyCode::Char(' ')));
    assert_eq!(fleet::rows(&d).len(), 4);
    // filter
    d.fv.filter = "weave".into();
    let r = fleet::rows(&d);
    assert_eq!(
        r,
        vec![
            fleet::RowId::Host("gpu2".into()),
            fleet::RowId::Sess("gpu2|w1:p4".into())
        ]
    );
}

#[test]
fn marking_and_targets() {
    let mut d = dash();
    d.on_key(k(KeyCode::Char(' '))); // marks ignite, moves down
    d.on_key(k(KeyCode::Char(' '))); // marks temper
    assert_eq!(d.fv.marked.len(), 2);
    assert_eq!(fleet::targets(&d).len(), 2);
    d.on_key(k(KeyCode::Esc)); // clears marks first
    assert!(d.fv.marked.is_empty());
}

#[test]
fn renders_fleet_wide_and_narrow() {
    let mut d = dash();
    let s = render(&mut d, 140, 36);
    assert!(s.contains("aleph"), "{s}");
    assert!(s.contains("b31") && s.contains("gpu2"));
    assert!(s.contains("ignite") && s.contains("temper") && s.contains("weave"));
    assert!(s.contains("needs you"));
    assert!(s.contains("Fleet"));
    let s = render(&mut d, 80, 30);
    assert!(s.contains("ignite"), "{s}");
    let s = render(&mut d, 50, 24);
    assert!(s.contains("ignite"), "{s}");
}

#[test]
fn blocked_session_shows_question_box() {
    let mut d = dash();
    d.on_key(k(KeyCode::Char('j')));
    d.fv.on_preview(
        "b31|w1:p2".into(),
        Ok("hello".into()),
        Some(" Do you want to run this?\n ❯ 1. Yes\n   2. No".into()),
    );
    let s = render(&mut d, 140, 36);
    assert!(s.contains("is asking"), "{s}");
    assert!(s.contains("1. Yes"));
}

/// Showcase mode from inside the app: ctrl+k offers it (a build without the
/// feature offers nothing).
#[test]
fn the_palette_switches_showcase_mode() {
    let mut d = dash();
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    for ch in "showcase".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 140, 40);
    if cfg!(feature = "showcase") {
        assert!(
            s.contains("Showcase: masked") && s.contains("Showcase: the demo world"),
            "{s}"
        );
        assert!(
            !s.contains("Showcase: off"),
            "it's off now: not offered: {s}"
        );
    } else {
        assert!(!s.contains("Showcase"), "{s}");
    }
}

#[test]
fn what_a_key_did_shows_briefly_alerts_longer() {
    let mut d = dash();
    d.cfg = Config::in_memory(&["hosts=b31,gpu2", "ui.note_for=0s", "ui.alert_for=1m"]);
    d.note("processes hidden");
    assert!(
        d.current_toast().is_none(),
        "a note past ui.note_for is gone"
    );
    d.error("couldn't reach b31");
    assert!(
        d.current_toast().is_some_and(|t| t.text.contains("b31")),
        "an error stays"
    );
    d.cfg = Config::in_memory(&["hosts=b31,gpu2", "ui.note_for=500ms"]);
    d.note("marked read");
    assert!(d.current_toast().is_some_and(|t| t.text == "marked read"));
}

#[test]
fn help_and_palette_render() {
    let mut d = dash();
    d.on_key(k(KeyCode::Char('?')));
    let s = render(&mut d, 120, 40);
    assert!(s.contains("Keys") && s.contains("command palette"), "{s}");
    d.on_key(k(KeyCode::Esc));
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Palette(_)));
    for ch in "weav".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 120, 40);
    assert!(s.contains("weave"), "{s}");
    d.on_key(k(KeyCode::Enter));
    assert_eq!(
        fleet::selected(&d),
        Some(fleet::RowId::Sess("gpu2|w1:p4".into()))
    );
}

#[test]
fn views_render() {
    let mut d = dash();
    for v in [View::Wall, View::Monitor, View::Settings, View::Inbox] {
        d.view = v;
        let s = render(&mut d, 120, 30);
        assert!(s.contains("aleph"), "{v:?}: {s}");
    }
}

#[test]
fn a_key_pasted_into_settings_is_kept_out_of_the_file() {
    use super::settings::keystore_name;
    let key = "sk-or-v1-c0ffee00settings0000000000000000000000000000000000000000beef";
    // a pasted key goes to the keystore under the name the setting points at
    assert_eq!(
        keystore_name("openrouter.key", key).as_deref(),
        Some("openrouter")
    );
    assert_eq!(
        keystore_name("notify.ntfy", "https://ntfy.sh/x").as_deref(),
        Some("notify.ntfy")
    );
    // a reference, off, nothing, or a setting that isn't secret: saved as it is
    for (k, v) in [
        ("openrouter.key", "secret:openrouter"),
        ("openrouter.key", "off"),
        ("openrouter.key", ""),
        ("openrouter.agents", "opencode,pi"),
        ("theme", "nord"),
    ] {
        assert_eq!(keystore_name(k, v), None, "{k}={v}");
    }
}

#[test]
fn monitor_usage_renders() {
    use super::monitor_draw::Sub;
    use crate::usage::dash::{Data, Live};
    use crate::usage::Row;
    let mut d = dash();
    d.view = View::Monitor;
    d.mon.sub = Sub::Usage;
    // still loading: says so
    let s = render(&mut d, 120, 30);
    assert!(s.contains("reading the usage store"), "{s}");
    let row = |k: &str, t: u64, cost: f64| Row {
        key: k.into(),
        input: t,
        requests: 3,
        cost_estimated: cost,
        ..Default::default()
    };
    let mut data = Data {
        total: row("total", 3000, 1.5),
        kpis: vec![
            ("today", row("t", 1000, 0.5)),
            ("7 days", row("t", 3000, 1.5)),
            ("30 days", row("t", 3000, 1.5)),
        ],
        columns: vec![
            ("Mon 19".into(), vec![1000, 0]),
            ("Tue 20".into(), vec![500, 1500]),
        ],
        series: vec!["claude".into(), "codex".into()],
        breakdown: vec![row("claude", 1500, 1.0), row("codex", 1500, 0.5)],
        insights: vec![("busiest day".into(), "2026-10-20 · 2.0k".into())],
        ..Default::default()
    };
    data.heat[1][9] = 2000;
    data.live.insert(
        "s1".into(),
        Live {
            tokens: 5000,
            ..Default::default()
        },
    );
    let today = crate::ui::local_day(crate::cache::now());
    data.more = crate::usage::explore::More {
        breakup: vec![
            ("claude".into(), "Claude Code".into(), 1.0, 1500),
            ("codex".into(), "Codex".into(), 0.5, 1500),
        ],
        series: vec![
            ("claude".into(), "Claude Code".into()),
            ("codex".into(), "Codex".into()),
        ],
        daily: vec![
            (today - 1, "10/25".into(), vec![0.5, 0.0]),
            (today, "10/26".into(), vec![0.5, 0.5]),
        ],
        cum: vec![0.5, 1.5],
        cum_prev: vec![0.2, 0.9],
        tree: vec![(row("claude", 1500, 1.0), vec![row("aaaa", 1500, 1.0)])],
        insights: vec![("busiest day".into(), "2026-10-20 · $1.00".into())],
        tools: vec![("Bash".into(), 12, 2)],
        ..Default::default()
    };
    d.usage.data = Some(data);
    for (w, h) in [(80, 24), (120, 32), (200, 50)] {
        let s = render(&mut d, w, h);
        assert!(
            s.contains("Overview") && s.contains("Insights"),
            "{w}x{h}: {s}"
        );
        assert!(s.contains("cost by day"), "{w}x{h}: {s}");
        assert!(s.contains("Claude Code"), "{w}x{h}: {s}");
    }
    // when, at width; the Insights tab (v four times)
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("hour × weekday") && s.contains("the last 12 weeks"),
        "{s}"
    );
    for _ in 0..4 {
        d.on_key(k(KeyCode::Char('v')));
    }
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("busiest day") && s.contains("tools the agents used") && s.contains("Bash"),
        "{s}"
    );
    // the Breakdown tab: each row and its split under it
    d.on_key(k(KeyCode::Char('V')));
    d.on_key(k(KeyCode::Char('V')));
    d.on_key(k(KeyCode::Char('V')));
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("by agent → account") && s.contains("Claude Code"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char('V')));
    assert!(!s.contains("OpenRouter"), "no key, no card: {s}");
    // an OpenRouter key: its own card, wide (heading the side column) and narrow
    d.usage.or.has_key = Some(true);
    d.usage.or.spend = Some(crate::openrouter::Spend {
        usage: Some(40.1),
        today: Some(0.42),
        month: Some(12.75),
        credits: Some(120.0),
        credits_used: Some(40.1),
        limit: Some(50.0),
        limit_left: Some(37.25),
        limit_reset: Some("monthly".into()),
        at: crate::cache::now() - 120,
        ..Default::default()
    });
    for (w, h) in [(200, 50), (100, 40)] {
        let s = render(&mut d, w, h);
        assert!(s.contains("OpenRouter"), "{w}x{h}: {s}");
        assert!(
            s.contains("today $0.42") && s.contains("this month $12.75"),
            "{w}x{h}: {s}"
        );
        assert!(s.contains("$79.90 left of $120"), "{w}x{h}: {s}");
        assert!(
            s.contains("$37.25 left of $50.00 · resets monthly"),
            "{w}x{h}: {s}"
        );
        assert!(s.contains("read 2m ago"), "{w}x{h}: {s}");
        // the rest of the view is still there
        assert!(s.contains("Overview"), "{w}x{h}: {s}");
    }
    // the header: OpenRouter's today beside the fleet's, and what's left
    let s = render(&mut d, 200, 50);
    let head = s.lines().next().unwrap_or("");
    assert!(head.contains("OpenRouter $0.42 · $37.25 left"), "{head}");
    // a read that failed says why, in the card
    if let Some(sp) = d.usage.or.spend.as_mut() {
        sp.error = Some("OpenRouter refused the key (401) — aleph secret set openrouter".into());
    }
    let s = render(&mut d, 200, 50);
    assert!(s.contains("refused the key (401)"), "{s}");
    // no numbers at all yet (the key's first read failed): the card, no header chip
    d.usage.or.spend = Some(crate::openrouter::Spend {
        at: crate::cache::now(),
        error: Some("no answer from OpenRouter".into()),
        ..Default::default()
    });
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("nothing read yet") && s.contains("no answer from OpenRouter"),
        "{s}"
    );
    assert!(
        !s.lines().next().unwrap_or("").contains("OpenRouter"),
        "{s}"
    );
    // the key gone: nothing
    d.usage.or.has_key = Some(false);
    let s = render(&mut d, 200, 50);
    assert!(!s.contains("OpenRouter"), "{s}");
}

#[test]
fn quick_start_enter_chooses_and_only_i_starts() {
    use super::quick::{QuickRes, QuickStart};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let d = dash();
    let mut q = QuickStart::new(&d, "b31");
    q.loaded(Ok(vec![]), vec![]);
    let k = |c| KeyEvent::new(c, KeyModifiers::NONE);
    // a folder (or the first place), then an agent: enter never starts
    q.choose_folder("/home/u/proj");
    assert!(matches!(q.key(k(KeyCode::Enter)), QuickRes::Keep));
    assert!(matches!(q.key(k(KeyCode::Left)), QuickRes::Keep));
    assert!(matches!(q.key(k(KeyCode::Enter)), QuickRes::Keep));
    // i starts, o starts in Focus
    assert!(matches!(
        q.key(k(KeyCode::Char('i'))),
        QuickRes::Start { focus: false }
    ));
    assert!(matches!(
        q.key(k(KeyCode::Char('o'))),
        QuickRes::Start { focus: true }
    ));
    // enter on "Browse folders" opens the browser (its folder comes back here)
    let browse = q
        .places()
        .iter()
        .position(|p| *p == super::quick::Place::Browse)
        .unwrap();
    q.key(k(KeyCode::Left));
    for _ in 0..20 {
        q.key(k(KeyCode::Up));
    }
    for _ in 0..browse {
        q.key(k(KeyCode::Down));
    }
    assert!(matches!(q.key(k(KeyCode::Enter)), QuickRes::Browse));
    let mut d = d;
    super::quick::open(&mut d, "b31");
    let s = render(&mut d, 120, 30);
    assert!(s.contains("choose") && s.contains("start + Focus"), "{s}");
}

/// `/` in Quick start searches the agents: letters are the search's (not
/// the launcher's keys), the machine's own agents first, enter chooses.
#[test]
fn quick_start_searches_the_agents() {
    use super::quick::{QuickRes, QuickStart};
    let d = dash();
    let mut q = QuickStart::new(&d, "b31");
    q.loaded(Ok(vec![]), vec![]);
    q.choose_folder("/home/u/proj");
    let ch = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
    q.key(ch('/'));
    // "pi" is a search, not i (start)
    assert!(matches!(q.key(ch('p')), QuickRes::Keep));
    assert!(matches!(q.key(ch('i')), QuickRes::Keep));
    assert_eq!(q.agent_id(), Some("pi"));
    for _ in 0..2 {
        q.key(k(KeyCode::Backspace));
    }
    for c in "gem".chars() {
        q.key(ch(c));
    }
    assert_eq!(q.agent_id(), Some("gemini"));
    // enter chooses it and closes the search; i then starts (installs) it
    assert!(matches!(q.key(k(KeyCode::Enter)), QuickRes::Keep));
    assert_eq!(q.agent_id(), Some("gemini"));
    assert!(matches!(q.key(ch('i')), QuickRes::Start { focus: false }));
    // what the machine has (here: its default, Claude Code) comes first in
    // a search, ahead of better-matching names; esc gives the column back
    q.key(ch('/'));
    q.key(ch('c'));
    assert_eq!(q.agent_id(), Some("claude"));
    q.key(k(KeyCode::Esc));
    assert!(matches!(q.key(k(KeyCode::Esc)), QuickRes::Cancel));
    let mut d = d;
    super::quick::open(&mut d, "b31");
    let s = render(&mut d, 120, 30);
    assert!(s.contains("/ search"), "{s}");
}

/// A big fleet: 20 machines × 20 sessions.
fn big_dash() -> Dash {
    let mut d = dash();
    let hosts: Vec<String> = (0..20).map(|i| format!("m{i:02}")).collect();
    let cfg = Config::in_memory(&[
        &format!("hosts={}", hosts.join(",")),
        "stats=off",
        "local=false",
    ]);
    let mut fleet = crate::fleet::Fleet::new(&cfg, None);
    let states = ["working", "blocked", "done", "idle"];
    for h in &hosts {
        let x = fleet.host_mut(h).unwrap();
        x.sessions = (0..20)
            .map(|j| {
                sess(
                    h,
                    &format!("w1:p{j}"),
                    &format!("s{j:02}x{h}"),
                    states[j % 4],
                )
            })
            .collect();
        x.conn = crate::fleet::Conn::Up;
        x.addr = format!("u@{h}");
    }
    d.fleet = fleet;
    d.cfg = cfg;
    d
}

/// Frame budgets on a big fleet (debug build: generous; it catches a view
/// that turns quadratic). `PERF=1 cargo test perf -- --nocapture` prints them.
#[test]
fn perf_big_fleet_frames() {
    let mut d = big_dash();
    let views = [View::Fleet, View::Monitor, View::Inbox, View::Settings];
    for v in views {
        d.view = v;
        let _ = render(&mut d, 200, 50); // warm caches
        let t0 = std::time::Instant::now();
        let n = 10;
        for _ in 0..n {
            let _ = render(&mut d, 200, 50);
        }
        let per = t0.elapsed() / n;
        if std::env::var_os("PERF").is_some() {
            eprintln!("{v:?}: {per:?} per frame");
        }
        assert!(
            per < std::time::Duration::from_millis(60),
            "{v:?}: {per:?} per frame"
        );
    }
    // moving through 400 rows stays cheap
    d.view = View::Fleet;
    let t0 = std::time::Instant::now();
    for _ in 0..50 {
        super::fleet::rows(&d);
    }
    let per = t0.elapsed() / 50;
    if std::env::var_os("PERF").is_some() {
        eprintln!("fleet rows: {per:?}");
    }
    assert!(per < std::time::Duration::from_millis(5), "rows: {per:?}");
}

#[test]
fn every_view_survives_tiny_and_huge_terminals() {
    let mut d = dash();
    for v in View::ALL {
        d.view = v;
        for (w, h) in [
            (1, 1),
            (10, 3),
            (20, 5),
            (40, 10),
            (80, 24),
            (150, 6),
            (300, 100),
        ] {
            let _ = render(&mut d, w, h);
        }
    }
}

#[test]
fn inbox_rows_read_well() {
    let mut d = dash_own();
    // finished turns listed, as they were before inbox.finished
    let _ = d.cfg.set("inbox.finished", "all");
    let now = crate::cache::now();
    let ev =
        |dt: u64, host: &str, name: &str, kind, title: &str, agent: &str| crate::events::Event {
            t: now - dt,
            host: host.into(),
            name: name.into(),
            kind,
            title: title.into(),
            agent: agent.into(),
            ..Default::default()
        };
    use crate::events::Kind;
    inbox::set_rows(
        &mut d,
        vec![
            ev(60, "b31", "temper", Kind::Blocked, "Run tests?", "claude"),
            ev(120, "b31", "ignite", Kind::Done, "training", "codex"),
            ev(
                3 * 86_400,
                "gpu2",
                "",
                Kind::Error,
                "ssh: connect to host 10.0.0.9 port 22: Operation timed out",
                "",
            ),
        ],
    );
    d.view = View::Inbox;
    let s = render(&mut d, 160, 40);
    // the overview: tiles, then a panel per machine with its column titles
    assert!(s.contains("group machine") && s.contains("SESSION"), "{s}");
    assert!(s.contains("╭ ▲ Needs you") && s.contains("▌b31"), "{s}");
    assert!(s.contains("needs you") && s.contains("temper"), "{s}");
    assert!(s.contains("unreachable — ssh timed out"), "{s}");
    // G: by day
    d.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Today") && s.contains("by day"), "{s}");
    for _ in 0..4 {
        d.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::SHIFT));
    }
    // the details pane shows the selected event's session state now
    assert!(s.contains("now ▲") || s.contains("now"), "{s}");
    // f cycles to Problems: only the error is left
    d.on_key(k(KeyCode::Char('f')));
    d.on_key(k(KeyCode::Char('f')));
    d.on_key(k(KeyCode::Char('f')));
    let s = render(&mut d, 100, 30);
    assert!(s.contains("gpu2") && !s.contains("temper"), "{s}");
    // search narrows; esc clears (f three more times: back to All)
    for _ in 0..3 {
        d.on_key(k(KeyCode::Char('f')));
    }
    d.on_key(k(KeyCode::Char('/')));
    for ch in "ign".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 100, 30);
    assert!(s.contains("ignite") && !s.contains("temper"), "{s}");
    d.on_key(k(KeyCode::Esc));
    let s = render(&mut d, 60, 12);
    assert!(s.contains("temper"), "{s}");
}

#[test]
fn inbox_says_whats_alive_and_archives() {
    let mut d = dash_own();
    // finished turns listed, as they were before inbox.finished
    let _ = d.cfg.set("inbox.finished", "all");
    // sessions starting and ending listed too
    let _ = d.cfg.set("inbox.lifecycle", "true");
    inbox::apply_settings(&mut d);
    inbox::apply_settings(&mut d);
    let now = crate::cache::now();
    use crate::events::{Event, Kind};
    let ev = |dt: u64, name: &str, kind, title: &str| Event {
        t: now - dt,
        host: "b31".into(),
        name: name.into(),
        kind,
        title: title.into(),
        agent: "claude".into(),
        ..Default::default()
    };
    inbox::set_rows(
        &mut d,
        vec![
            ev(600, "ignite", Kind::New, "/srv/ignite"),
            ev(7200, "gone1", Kind::Done, "it finished"),
            ev(3 * 86_400, "oldie", Kind::New, "/srv/oldie"),
        ],
    );
    d.view = View::Inbox;
    let s = render(&mut d, 170, 40);
    // ignite runs: its row says so; gone1 ended; oldie's start ended days
    // ago, so it's in the archive by itself
    assert!(s.contains("ignite") && s.contains("working"), "{s}");
    assert!(s.contains("gone1") && s.contains("○ ended"), "{s}");
    assert!(!s.contains("oldie"), "{s}");
    // the folder where it runs now (the live session's), over where it began
    assert!(s.contains("/home/u/ignite"), "the folder column: {s}");
    // e hides what ended
    d.on_key(k(KeyCode::Char('e')));
    let s = render(&mut d, 170, 40);
    assert!(!s.contains("gone1") && s.contains("ignite"), "{s}");
    d.on_key(k(KeyCode::Char('e')));
    // A: the archive holds oldie; x brings it back for good
    d.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
    let s = render(&mut d, 170, 40);
    assert!(s.contains("▣ archive") && s.contains("oldie"), "{s}");
    assert!(!s.contains("gone1"), "{s}");
    d.on_key(k(KeyCode::Char('x')));
    d.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT));
    let s = render(&mut d, 170, 40);
    assert!(s.contains("oldie"), "back from the archive: {s}");
    // D asks before deleting for good; X asks before closing a live one
    let at = |d: &mut Dash, name: &str| {
        for _ in 0..20 {
            let s = render(d, 170, 40);
            let sel = s.lines().find(|l| l.contains('┃')).unwrap_or("");
            if sel.contains(name) {
                return;
            }
            d.on_key(k(KeyCode::Down));
        }
        panic!("{name} never selected");
    };
    at(&mut d, "gone1");
    d.on_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT));
    assert!(
        matches!(&d.mode, Mode::Confirm { title, .. } if title.contains("Delete for good")),
        "D asks first"
    );
    d.mode = Mode::Normal;
    d.on_key(k(KeyCode::Home));
    at(&mut d, "ignite");
    d.on_key(KeyEvent::new(KeyCode::Char('X'), KeyModifiers::SHIFT));
    assert!(
        matches!(&d.mode, Mode::Confirm { title, .. } if title.contains("Close session")),
        "X asks first"
    );
    d.mode = Mode::Normal;
    // x archives a row (gone1) and it leaves the inbox
    at(&mut d, "gone1");
    d.on_key(k(KeyCode::Char('x')));
    let s = render(&mut d, 170, 40);
    assert!(
        !s.lines()
            .any(|l| l.contains("gone1") && l.contains("finished")),
        "{s}"
    );
    assert!(s.contains("gone1 archived"), "and it says so: {s}");
}

#[test]
fn inbox_dismisses_by_kind_ended_first() {
    let mut d = dash_own();
    // finished turns listed, as they were before inbox.finished
    let _ = d.cfg.set("inbox.finished", "all");
    inbox::apply_settings(&mut d);
    let now = crate::cache::now();
    use crate::events::{Event, Kind};
    let ev = |dt: u64, name: &str, kind| Event {
        t: now - dt,
        host: "b31".into(),
        name: name.into(),
        kind,
        title: "t".into(),
        agent: "claude".into(),
        ..Default::default()
    };
    inbox::set_rows(
        &mut d,
        vec![
            ev(600, "ignite", Kind::Done),  // alive
            ev(700, "gone1", Kind::Done),   // ended
            ev(800, "gone2", Kind::Error),  // ended
            ev(900, "ignite", Kind::Limit), // alive
        ],
    );
    d.view = View::Inbox;
    d.on_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT));
    let s = render(&mut d, 170, 40);
    assert!(s.contains("Dismiss alerts") && s.contains("All"), "{s}");
    assert!(
        s.lines()
            .any(|l| l.contains("[✓]") && l.contains("from ended sessions")),
        "ended ones ticked to begin with: {s}"
    );
    assert!(s.contains("Archive 2 alerts"), "{s}");
    // enter → one confirm → they're archived, the live ones stay
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(&d.mode, Mode::Confirm { title, .. } if title == "Archive 2 alerts?"));
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 170, 40);
    assert!(s.contains("2 alerts archived"), "{s}");
    assert!(
        !s.contains("gone1") && !s.contains("gone2") && s.contains("ignite"),
        "{s}"
    );
    // All, and the switch to delete for good
    d.on_key(KeyEvent::new(KeyCode::Char('U'), KeyModifiers::SHIFT));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    d.on_key(k(KeyCode::Right));
    let s = render(&mut d, 170, 40);
    assert!(s.contains("Delete 2 for good"), "{s}");
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(&d.mode, Mode::Confirm { title, .. } if title.contains("for good")));
}

#[test]
fn inbox_joins_launches_to_their_events() {
    let mut d = dash_own();
    // sessions starting and ending listed too
    let _ = d.cfg.set("inbox.lifecycle", "true");
    inbox::apply_settings(&mut d);
    let now = crate::cache::now();
    let en = |dt: u64, host: &str, name: &str, agent: &str, dir: &str| crate::history::Entry {
        t: now - dt,
        host: host.into(),
        dir: dir.into(),
        name: name.into(),
        agent: agent.into(),
        prompt: Some("fix the flaky tests".into()),
        ..Default::default()
    };
    let ev = |dt: u64, host: &str, name: &str, kind: crate::events::Kind, agent: &str| {
        crate::events::Event {
            t: now - dt,
            host: host.into(),
            name: name.into(),
            kind,
            title: "a title".into(),
            agent: agent.into(),
            ..Default::default()
        }
    };
    use crate::events::Kind;
    // ignite's launch and its `started` event (seen 3 s later) are one row;
    // spark's launch has no event; old's is days ago
    inbox::set_rows(
        &mut d,
        vec![
            ev(27, "b31", "ignite", Kind::New, "claude"),
            ev(10, "b31", "ignite", Kind::Done, "claude"),
        ],
    );
    inbox::set_launches(
        &mut d,
        vec![
            en(30, "b31", "ignite", "claude", "/home/u/ignite"),
            en(400, "gpu2", "spark", "codex", "/home/u/spark"),
            en(4 * 86_400, "b31", "old", "claude", "/home/u/old"),
        ],
    );
    d.view = View::Inbox;
    let s = render(&mut d, 180, 50);
    assert!(
        s.contains("ignite") && s.contains("spark") && s.contains("old"),
        "{s}"
    );
    assert!(s.contains("Launches"), "{s}");
    // the Launches chip: the three launches, ignite's joined to its event
    for _ in 0..5 {
        d.on_key(k(KeyCode::Char('f')));
    }
    let s = render(&mut d, 180, 50);
    assert!(!s.contains("finished its turn"), "{s}");
    assert!(
        s.contains("Started with") && s.contains("new --dir /home/u/ignite"),
        "{s}"
    );
    // g: one agent (claude is the most frequent), then the next (codex)
    d.on_key(k(KeyCode::Char('g')));
    let s = render(&mut d, 180, 50);
    assert!(s.contains("ignite") && !s.contains("spark"), "{s}");
    d.on_key(k(KeyCode::Char('g')));
    let s = render(&mut d, 180, 50);
    assert!(s.contains("spark") && !s.contains("ignite"), "{s}");
    // r: start it again with the form filled in; P: save it as a preset
    d.on_key(k(KeyCode::Char('r')));
    assert!(matches!(d.mode, Mode::NewForm(_)));
    d.mode = Mode::Normal;
    d.on_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT));
    let Mode::Input(i) = &d.mode else {
        panic!("no preset name box");
    };
    assert!(matches!(i.purpose, forms::For::SavePreset(_)));
}

#[test]
fn the_first_esc_into_an_agent_is_held_back() {
    let mut d = dash_own();
    let _ = render(&mut d, 160, 30);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Enter));
    assert!(d.wall.in_focus() && d.wall.typing);
    // the first: held, with a toast and the status bar's warning
    d.on_key(k(KeyCode::Esc));
    assert!(wall::esc_waits(&d));
    assert!(d.toasts.iter().any(|t| t.text.contains("esc again stops")));
    let s = render(&mut d, 160, 30);
    assert!(s.contains("esc again sends it to the agent"), "{s}");
    // the second goes; native input stays on
    d.on_key(k(KeyCode::Esc));
    assert!(!wall::esc_waits(&d) && d.wall.typing);
    let s = render(&mut d, 160, 30);
    assert!(!s.contains("esc again sends it"), "{s}");
    // another key starts over: the next esc is held again
    d.on_key(k(KeyCode::Char('x')));
    d.on_key(k(KeyCode::Esc));
    assert!(wall::esc_waits(&d));
    // leaving native input drops the warning
    leave_typing(&mut d);
    assert!(!wall::esc_waits(&d));
    // focus.esc_guard = off: every esc goes
    let _ = d.cfg.set("focus.esc_guard", "off");
    d.wall.typing = true;
    d.on_key(k(KeyCode::Esc));
    assert!(!wall::esc_waits(&d) && d.wall.typing);
}

#[test]
fn settings_search_finds_keys_file_names_and_words() {
    let mut d = dash();
    d.view = View::Settings;
    let find = |d: &mut Dash, q: &str| {
        d.set.search = q.into();
        render(d, 160, 40)
    };
    // its key, with or without the dots and underscores, words in any order
    for q in ["ui.splash_ms", "splash ms", "ms splash", "UI.SPLASH"] {
        let s = find(&mut d, q);
        assert!(s.contains("ui.splash_ms"), "{q}: {s}");
    }
    // its name in aleph.toml: [notify] desktop_done is notify_done
    let s = find(&mut d, "desktop_done");
    assert!(s.contains("notify_done"), "{s}");
    let s = find(&mut d, "fleet.default");
    assert!(s.contains("ssh_host"), "{s}");
}

#[test]
fn the_palette_focuses_the_selected_session_and_leaves_focus() {
    let mut d = dash();
    let _ = render(&mut d, 160, 30);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    let ctrl_k = || KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL);
    // Fleet: Focus on the selected session comes first
    d.on_key(ctrl_k());
    let s = render(&mut d, 160, 30);
    assert!(s.contains("┃ Focus claude_ignite"), "{s}");
    d.on_key(k(KeyCode::Enter));
    assert!(d.view == View::Wall && d.wall.in_focus() && d.wall.typing);
    // in Focus, typing: the ways out come first
    d.on_key(ctrl_k());
    let s = render(&mut d, 160, 30);
    assert!(
        s.contains("Leave Focus") && s.contains("Keys back to Aleph"),
        "{s}"
    );
    d.on_key(k(KeyCode::Down));
    d.on_key(k(KeyCode::Enter));
    assert!(d.view == View::Wall && !d.wall.typing);
    d.on_key(ctrl_k());
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Fleet);
}

#[test]
fn ctrl_k_opens_the_palette_while_typing_into_an_agent() {
    let mut d = dash();
    let _ = render(&mut d, 160, 30);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Enter));
    assert!(d.wall.in_focus() && d.wall.typing);
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Palette(_)));
    // esc closes the palette (not held, never the agent's), typing goes on
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    assert!(d.wall.typing && !wall::esc_waits(&d));
    // and with Aleph's keys, as before
    leave_typing(&mut d);
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Palette(_)));
}

#[test]
fn ctrl_a_opens_the_approvals_while_typing_when_something_waits() {
    let mut d = dash();
    let _ = render(&mut d, 160, 30);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Enter));
    assert!(d.wall.in_focus() && d.wall.typing);
    // temper needs you: ctrl+a is the board's
    assert!(d.fleet.sessions().iter().any(|s| s.blocked()));
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Approvals));
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal) && d.wall.typing);
    // nothing waits: ctrl+a is the agent's (the start of its line)
    for h in d.fleet.hosts.iter_mut() {
        for s in h.sessions.iter_mut().filter(|s| s.blocked()) {
            s.status = "working".into();
        }
    }
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Normal) && d.wall.typing);
}

#[test]
fn enter_opens_focus_and_w_tiles() {
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    let cur = |d: &Dash| match d.wall.content_of_focus() {
        Some(wall::Content::Session(k)) => k.clone(),
        _ => String::new(),
    };
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    assert!(d.on_key(k(KeyCode::Enter)).is_none());
    assert_eq!(d.view, View::Wall);
    assert!(d.wall.in_focus());
    assert_eq!(cur(&d), "b31|w1:p1");
    // keys go to the agent first; ctrl+] gives them back to Focus
    assert!(d.wall.typing);
    leave_typing(&mut d);
    let s = render(&mut d, 120, 30);
    assert!(s.contains("FOCUS") && s.contains("ignite"), "{s}");
    assert!(s.contains("needs you") && s.contains("new tab"), "{s}");
    // ] steps to the next session in the same pane; } to the next that
    // needs you (temper is the only one, and already shown)
    d.on_key(k(KeyCode::Char(']')));
    assert_eq!(cur(&d), "b31|w1:p2");
    d.on_key(k(KeyCode::Char('}')));
    assert_eq!(cur(&d), "b31|w1:p2");
    d.on_key(k(KeyCode::Char('[')));
    assert_eq!(cur(&d), "b31|w1:p1");
    // esc goes back to where Focus was opened from
    d.on_key(k(KeyCode::Esc));
    assert_eq!(d.view, View::Fleet);
    // a attaches in herdr, A goes straight to the agent (same keys as Focus)
    assert!(matches!(
        d.on_key(k(KeyCode::Char('a'))),
        Some(Handoff::Attach { .. })
    ));
    assert!(matches!(
        d.on_key(k(KeyCode::Char('A'))),
        Some(Handoff::Direct { .. })
    ));
    // w: the selected machine's sessions side by side
    d.on_key(k(KeyCode::Char('w')));
    assert_eq!(d.view, View::Wall);
    assert_eq!(
        d.wall
            .win()
            .map(|w| (w.name.clone(), w.root.leaves().len(), w.zoom)),
        Some((wall::TILE.to_string(), 2, false))
    );
    let s = render(&mut d, 120, 30);
    assert!(s.contains("TILE"), "{s}");
    d.on_key(k(KeyCode::Esc));
    assert_eq!(d.view, View::Fleet);
    // marked sessions win over the machine's
    d.fv.marked.insert("gpu2|w1:p4".into());
    d.fv.marked.insert("b31|w1:p2".into());
    d.on_key(k(KeyCode::Char('w')));
    assert_eq!(d.wall.win().map(|w| w.root.leaves().len()), Some(2));
    d.on_key(k(KeyCode::Esc));
    // enter_opens=attach brings back attach-on-enter
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "enter_opens=attach",
    ]);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    assert!(matches!(
        d.on_key(k(KeyCode::Enter)),
        Some(Handoff::Attach { .. })
    ));
}

#[test]
fn fleet_prompt_tab_queues_it() {
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    d.fv.sel = Some(fleet::RowId::Sess("gpu2|w1:p4".into()));
    d.on_key(k(KeyCode::Char('s')));
    assert!(matches!(d.mode, Mode::Input(_)));
    for ch in "tidy up".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Tab));
    assert!(matches!(d.mode, Mode::Normal));
    let q = crate::queue::for_session("gpu2", "weave");
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].prompt, "tidy up");
    crate::queue::remove(q[0].id);
}

#[test]
fn machine_buttons_quick_start_and_close_list() {
    let mut d = dash();
    d.fv.sel = Some(fleet::RowId::Host("b31".into()));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("New session") && s.contains("Close…"), "{s}");
    // the buttons are clickable: New session opens Quick start
    let (r, _) = *d
        .hits
        .buttons
        .iter()
        .find(|(_, a)| *a == Act::New)
        .expect("a New session button");
    let click = crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: r.x + 1,
        row: r.y,
        modifiers: KeyModifiers::NONE,
    };
    assert!(d.on_mouse(click).is_none());
    let Mode::Quick(q) = &mut d.mode else {
        panic!("Quick start didn't open");
    };
    let gen = q.gen;
    // the machine's workspaces arrive: the cursor goes to the open one
    let ws = vec![
        herdr::Workspace {
            id: "w1".into(),
            label: "train".into(),
            number: 1,
            tabs: 3,
            focused: false,
            status: "working".into(),
        },
        herdr::Workspace {
            id: "w2".into(),
            label: "eval".into(),
            number: 2,
            tabs: 1,
            focused: true,
            status: "idle".into(),
        },
    ];
    d.on_bg(Bg::Quick {
        host: "b31".into(),
        gen,
        ws: Ok(ws),
        wt: vec![],
    });
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("New session on b31") && s.contains("train") && s.contains("eval"),
        "{s}"
    );
    assert!(
        s.contains("Where") && s.contains("Agent") && s.contains("Browse folders"),
        "{s}"
    );
    // a stale answer (another Quick start) is dropped
    d.on_bg(Bg::Quick {
        host: "b31".into(),
        gen: gen + 99,
        ws: Ok(vec![]),
        wt: vec![],
    });
    assert!(render(&mut d, 160, 40).contains("eval"));
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    // x on a machine: tick sessions, then one confirm
    d.on_key(k(KeyCode::Char('x')));
    assert!(matches!(d.mode, Mode::CloseList(_)));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Close sessions on b31") && s.contains("ignite"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Enter));
    match &d.mode {
        Mode::Confirm {
            on_yes: Pending::Close(v),
            ..
        } => assert_eq!(v.len(), 2),
        _ => panic!("no confirm"),
    }
}

#[test]
fn settings_grids_keys_and_checks() {
    let mut d = dash();
    d.view = View::Settings;
    // remote control as a grid: tick b31 × Claude Code, save
    settings::go_section(&mut d, "Remote control");
    d.on_key(k(KeyCode::Enter));
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Remote control · machines × agents") && s.contains("☐"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.cfg.get("remote.allow").as_deref(), Some("b31:claude"));
    // analytics: everything is read by default; a whole row off
    settings::go_section(&mut d, "Analytics");
    d.on_key(k(KeyCode::Left));
    d.on_key(k(KeyCode::Enter));
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("☑") && !s.contains("☐"), "{s}");
    d.on_key(k(KeyCode::Char('a')));
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.cfg.get("analytics.include").as_deref(), Some("gpu2:*"));
    // Moshi per machine: a radio row
    settings::go_section(&mut d, "Notifications");
    d.on_key(k(KeyCode::Left));
    d.on_key(k(KeyCode::Enter));
    for _ in 0..60 {
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    d.on_key(k(KeyCode::Right));
    d.on_key(k(KeyCode::Right));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Enter));
    assert_eq!(
        d.cfg.host_get("b31", "moshi").as_deref(),
        Some("machine-only")
    );
    // a key is bound by pressing it; the dashboard's own keys are refused
    settings::go_section(&mut d, "Keys");
    d.on_key(k(KeyCode::Left));
    d.on_key(k(KeyCode::Enter));
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Bind a key"), "{s}");
    d.on_key(k(KeyCode::Char('q')));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("the dashboard's own"), "{s}");
    d.on_key(k(KeyCode::F(9)));
    let first = super::ACTS[0].1;
    assert_eq!(d.cfg.get(&format!("key.{first}")).as_deref(), Some("f9"));
    // a bad value is refused before it's saved
    d.mode = Mode::Input(forms::Input::new(
        "Set refresh".into(),
        "",
        String::new(),
        forms::For::Setting("refresh".into()),
    ));
    for ch in "soon".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Enter));
    match &d.mode {
        Mode::Input(i) => assert!(i.err.is_some(), "an error is shown"),
        _ => panic!("the input stays open"),
    }
    d.mode = Mode::Normal;
    assert_ne!(d.cfg.get("refresh").as_deref(), Some("soon"));
    // one already in the file is marked
    let _ = d.cfg.set("theme", "purple");
    settings::go_section(&mut d, "Appearance");
    let s = render(&mut d, 160, 40);
    assert!(s.contains("✗ purple"), "{s}");
    let _ = d.cfg.unset("theme");
}

#[test]
fn settings_sections_health_and_search() {
    use crate::doctor::{Fix, Line as DLine, St};
    let mut d = dash();
    let l = |st, label: &str, text: &str, action| DLine {
        st,
        label: label.into(),
        text: text.into(),
        fix: vec![],
        action,
    };
    doctor::set_lines(
        &mut d,
        vec![
            l(St::Head, "Host · b31", "", None),
            l(St::Ok, "connects to", "u@b31", None),
            l(
                St::Bad,
                "key login",
                "no key accepted",
                Some(Fix::KeySetup("b31".into())),
            ),
        ],
    );
    d.view = View::Settings;
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("General") && s.contains("Doctor") && s.contains("Health · General"),
        "{s}"
    );
    assert!(s.contains("✗1"), "{s}");
    // ] twice: Machines, with its check in Health
    d.on_key(k(KeyCode::Char(']')));
    d.on_key(k(KeyCode::Char(']')));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Health · Machines") && s.contains("key login"),
        "{s}"
    );
    assert!(s.contains("set up key login to b31"), "{s}");
    // f asks before fixing
    d.on_key(k(KeyCode::Char('f')));
    match &d.mode {
        Mode::Confirm {
            on_yes: Pending::Fix(Fix::KeySetup(h)),
            ..
        } => assert_eq!(h, "b31"),
        _ => panic!("no confirm"),
    }
    d.mode = Mode::Normal;
    // search across every section
    d.on_key(k(KeyCode::Char('/')));
    for ch in "ntfy".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("notify.ntfy") && s.contains("Notifications"),
        "{s}"
    );
    d.on_key(k(KeyCode::Esc));
    // the Doctor section (the last) shows every check, by machine
    for _ in 2..crate::config::SECTIONS.len() - 1 {
        d.on_key(k(KeyCode::Char(']')));
    }
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("╭ Doctor ") && s.contains("● b31") && s.contains("Key login"),
        "{s}"
    );
}

/// A doctor report with two machines (one with a problem), this computer and
/// the daemon, as `doctor::run_part` would stream it.
fn doctor_report() -> Vec<crate::doctor::Line> {
    use crate::doctor::{Fix, Line as DLine, St};
    let l = |st, label: &str, text: &str, fix: &[&str], action| DLine {
        st,
        label: label.into(),
        text: text.into(),
        fix: fix.iter().map(|s| s.to_string()).collect(),
        action,
    };
    vec![
        l(St::Head, "This Mac", "", &[], None),
        l(St::Ok, "aleph", "2.0.0  ·  ~/.cargo/bin/aleph", &[], None),
        l(St::Head, "Daemon", "", &[], None),
        l(
            St::Info,
            "service",
            "not installed — nothing starts the daemon at login",
            &["aleph daemon install — runs it at login and starts it again if it stops"],
            Some(Fix::DaemonInstall),
        ),
        l(St::Head, "Host · b31", "", &[], None),
        l(St::Ok, "connects to", "selene@10.0.0.49", &[], None),
        l(
            St::Ok,
            "key login",
            "works — no password prompts",
            &[],
            None,
        ),
        l(St::Head, "On b31", "", &[], None),
        l(
            St::Ok,
            "herdr",
            "0.9.5  ·  /home/selene/.local/bin/herdr",
            &[],
            None,
        ),
        l(St::Info, "system", "Linux x86_64", &[], None),
        l(St::Head, "Host · dex42", "", &[], None),
        l(St::Ok, "connects to", "selene@10.0.0.77", &[], None),
        l(St::Head, "On dex42", "", &[], None),
        l(
            St::Ok,
            "herdr",
            "0.9.5  ·  /home/selene/.local/bin/herdr",
            &[],
            None,
        ),
        l(
            St::Warn,
            "Claude Code",
            "2.1.309  ·  /home/selene/.local/bin/claude  ·  default  ·  no herdr integration",
            &["aleph --host dex42 agents setup claude   — herdr restores its sessions with it"],
            Some(Fix::Integration {
                host: "dex42".into(),
                name: "claude".into(),
            }),
        ),
        l(St::Head, "herdr · dex42", "", &[], None),
        l(St::Ok, "machine", "'dex42' is saved in herdr", &[], None),
    ]
}

#[test]
fn doctor_lists_checks_by_machine_problems_first() {
    let mut d = dash();
    doctor::set_lines(&mut d, doctor_report());
    d.view = View::Settings;
    settings::go_section(&mut d, "Doctor");
    let s = render(&mut d, 160, 40);
    // every machine's state on top, the one with a warning first
    let strip = s.lines().find(|l| l.contains("! dex42")).expect(&s);
    assert!(
        strip.find("! dex42") < strip.find("✓ This Mac") && strip.contains("✓ b31"),
        "{strip}"
    );
    // a machine's head: where ssh goes, its herdr, its tally
    assert!(s.contains("● dex42  selene@10.0.0.77 · herdr 0.9.5"), "{s}");
    // a machine's block: its head, then only what needs attention (the
    // warning: short — no path, the key fact, the version with the agent's
    // glyph), its passes folded
    let dex42 = s.find("● dex42").unwrap();
    assert!(s[dex42..].contains("! Claude Code"), "{s}");
    let row = s
        .lines()
        .find(|l| l.contains("! Claude Code") && l.contains("✻"))
        .unwrap();
    assert!(
        row.contains("no herdr integration · default agent")
            && row.contains("✻ 2.1.309")
            && !row.contains("/home/selene"),
        "{row}"
    );
    assert!(
        !s[dex42..].contains("Connects to") && s.contains("more · ↵ on the name shows them"),
        "{s}"
    );
    // the head is selected first: its whole checklist in the detail
    assert!(
        s.contains("herdr machine") && s.contains("'dex42' is saved in herdr"),
        "{s}"
    );
    // ↵ on the head opens the block: every check, the warning first
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    let ssh = s[dex42..].find("Connects to").unwrap() + dex42;
    assert!(s.find("┃● dex42").unwrap() < ssh, "{s}");
    d.on_key(k(KeyCode::Enter));
    // ↓ to the warning: found, version, path with ~, why, fix, f
    d.on_key(k(KeyCode::Down));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("found    no herdr integration")
            && s.contains("path     ~/.local/bin/claude")
            && s.contains("why      herdr restores its sessions with it")
            && s.contains("fix      aleph --host dex42 agents setup claude")
            && s.contains("install herdr's claude integration on dex42"),
        "{s}"
    );
    // f asks before fixing the selected check
    d.on_key(k(KeyCode::Char('f')));
    match &d.mode {
        Mode::Confirm {
            on_yes: Pending::Fix(crate::doctor::Fix::Integration { host, .. }),
            ..
        } => assert_eq!(host, "dex42"),
        _ => panic!("no confirm"),
    }
    d.mode = Mode::Normal;
    // ⇥ (the list focused): a block per agent, each problem titled by its machine
    d.set.focus = 1;
    d.on_key(k(KeyCode::Tab));
    let s = render(&mut d, 160, 40);
    let claude = s
        .lines()
        .find(|l| l.contains("Claude Code  on "))
        .expect(&s);
    assert!(claude.contains("dex42"), "{claude}");
    assert!(s.lines().any(|l| l.contains("! dex42")), "{s}");
    d.on_key(k(KeyCode::BackTab));
    // narrow: the detail goes under the list, where it is in its title
    d.doc.sel = 1;
    let s = render(&mut d, 80, 24);
    assert!(
        s.contains("! Claude Code  dex42 · agents") && s.contains("f   install herdr's"),
        "{s}"
    );
}

#[test]
fn four_tabs_and_the_focus_breadcrumb() {
    let mut d = dash();
    let s = render(&mut d, 140, 30);
    assert!(
        s.contains("1 Fleet")
            && s.contains("2 Monitor")
            && s.contains("3 Inbox")
            && s.contains("4 Settings"),
        "{s}"
    );
    assert!(
        !s.contains("Terminals") && !s.contains("Hosts") && !s.contains("Doctor"),
        "{s}"
    );
    // a session needs you: Fleet's dot says how many
    assert!(s.contains("Fleet • 1"), "{s}");
    // digits past the last tab do nothing (and never panic)
    for ch in ['5', '6', '7', '8', '9'] {
        d.on_key(k(KeyCode::Char(ch)));
        assert_eq!(d.view, View::Fleet);
    }
    d.on_key(k(KeyCode::Char('3')));
    assert_eq!(d.view, View::Inbox);
    d.on_key(k(KeyCode::Char('1')));
    // Focus from Fleet: Fleet stays lit, the breadcrumb says where you are
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p2".into()));
    d.on_key(k(KeyCode::Enter));
    leave_typing(&mut d);
    let s = render(&mut d, 140, 30);
    assert!(s.contains("› Focus · b31 › claude_temper"), "{s}");
    // no sub-headings in the overlay: ⇥ goes to the tab after the one it came from
    d.on_key(k(KeyCode::Tab));
    assert_eq!(d.view, View::Monitor);
}

#[test]
fn tab_cycles_the_sub_headings_wherever_you_are() {
    let mut d = dash();
    let _ = render(&mut d, 140, 30);
    // Monitor: ⇥ / ⇧⇥ its views
    d.on_key(k(KeyCode::Char('2')));
    let first = d.mon.sub;
    d.on_key(k(KeyCode::Tab));
    assert_ne!(d.mon.sub, first);
    d.on_key(k(KeyCode::BackTab));
    assert_eq!(d.mon.sub, first);
    assert_eq!(d.view, View::Monitor, "⇥ never leaves the view");
    // Inbox: its filters
    d.on_key(k(KeyCode::Char('3')));
    let show = d.inbox.show;
    d.on_key(k(KeyCode::Tab));
    assert_ne!(d.inbox.show, show);
    d.on_key(k(KeyCode::BackTab));
    assert_eq!(d.inbox.show, show);
    // Settings: its sections; in the Doctor (its list focused) by machine ⇄ by agent
    d.on_key(k(KeyCode::Char('4')));
    let sec = d.set.sec;
    d.set.focus = 0;
    d.on_key(k(KeyCode::Tab));
    assert_ne!(d.set.sec, sec);
    crate::tui::doctor::set_lines(&mut d, doctor_report());
    settings::go_section(&mut d, "Doctor");
    d.set.focus = 1;
    d.on_key(k(KeyCode::Tab));
    assert!(d.doc.by_agent);
    d.on_key(k(KeyCode::BackTab));
    assert!(!d.doc.by_agent);
    let s = render(&mut d, 140, 30);
    assert!(s.contains("⇥ sub-views"), "{s}");
}

#[test]
fn command_line_runs_and_completes() {
    let mut d = dash();
    let _ = render(&mut d, 140, 30);
    let typed = |d: &mut Dash, t: &str| {
        d.on_key(k(KeyCode::Char(':')));
        for ch in t.chars() {
            d.on_key(k(KeyCode::Char(ch)));
        }
    };
    // tab completes the command
    typed(&mut d, "fo");
    d.on_key(k(KeyCode::Tab));
    let Mode::Command(cl) = &d.mode else {
        panic!("no command line");
    };
    assert_eq!(cl.f.text, "focus ");
    let s = render(&mut d, 140, 30);
    assert!(
        s.contains("focus <session> — that session fullscreen"),
        "{s}"
    );
    d.on_key(k(KeyCode::Esc));
    // :go inbox
    typed(&mut d, "go inbox");
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Inbox);
    // :focus b31:temper
    typed(&mut d, "focus b31:temper");
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Wall);
    assert!(d.wall.in_focus());
    leave_typing(&mut d);
    d.on_key(k(KeyCode::Esc));
    // a typo stays open, saying why
    typed(&mut d, "zap");
    d.on_key(k(KeyCode::Enter));
    let Mode::Command(cl) = &d.mode else {
        panic!("closed on an error");
    };
    assert!(cl.err.as_deref().unwrap_or("").contains("no command"));
    d.on_key(k(KeyCode::Esc));
    // :tile b31:ignite gpu2:weave
    typed(&mut d, "tile b31:ignite gpu2:weave");
    d.on_key(k(KeyCode::Enter));
    assert!(d.wall.in_tile() && d.wall.window_len() == 2);
}

#[test]
fn chips_and_sections_are_clickable() {
    let mut d = dash();
    d.view = View::Inbox;
    let _ = render(&mut d, 160, 40);
    let (r, _) = *d
        .hits
        .chips
        .iter()
        .find(|(_, c)| *c == Chip::Inbox(5))
        .expect("the Launches chip");
    let click = |x: u16, y: u16| crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    };
    d.on_mouse(click(r.x + 1, r.y));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Launches"), "{s}");
    d.view = View::Settings;
    let _ = render(&mut d, 160, 40);
    let (r, _) = *d
        .hits
        .chips
        .iter()
        .find(|(_, c)| *c == Chip::Section(2))
        .expect("Machines");
    d.on_mouse(click(r.x + 1, r.y));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Health · Machines"), "{s}");
}

#[test]
fn no_internet_but_the_machines_answer_on_the_local_network() {
    let mut d = dash();
    d.net.feed(Some(Duration::from_millis(30)), true);
    d.net_toast();
    // the internet goes; both machines still answer
    d.net.feed(None, true);
    d.net.feed(None, true);
    d.net_toast();
    let s = render(&mut d, 160, 30);
    assert!(s.contains("◐ no internet · 2/2 machines answer"), "{s}");
    assert!(
        s.contains("the machines on your network still answer"),
        "{s}"
    );
    assert!(!d.cut_off("b31") && !d.cut_off("gpu2") && !d.cut_off("local"));
    // one stops answering: only it is out of reach
    d.fleet.host_mut("gpu2").unwrap().conn = crate::fleet::Conn::Down("timed out".into());
    assert!(!d.cut_off("b31") && d.cut_off("gpu2"));
    let s = render(&mut d, 160, 30);
    assert!(s.contains("no internet · 1/2 machines answer"), "{s}");
    // a prompt to b31 goes now, not into the queue
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Char('s')));
    for ch in "lan hello".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Enter));
    let q = crate::queue::for_session("b31", "ignite");
    assert!(!q.iter().any(|t| t.prompt == "lan hello"), "{q:?}");
}

#[test]
fn connection_indicator_and_offline_sends() {
    let mut d = dash();
    d.net.feed(Some(Duration::from_millis(30)), true);
    let s = render(&mut d, 160, 30);
    assert!(s.contains("● online 30 ms"), "{s}");
    // two failed probes, and no machine answers: offline, said once
    for h in ["b31", "gpu2"] {
        d.fleet.host_mut(h).unwrap().conn = crate::fleet::Conn::Down("timed out".into());
    }
    d.net.feed(None, false);
    d.net.feed(None, false);
    d.net_toast();
    let s = render(&mut d, 160, 30);
    assert!(s.contains("○ offline"), "{s}");
    assert!(
        s.contains("offline — the machines keep their sessions"),
        "{s}"
    );
    // a prompt typed while offline waits in the queue
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Char('s')));
    for ch in "offline hello".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Enter));
    let q = crate::queue::for_session("b31", "ignite");
    assert!(q.iter().any(|t| t.prompt == "offline hello"), "{q:?}");
    for t in q {
        crate::queue::remove(t.id);
    }
    // back: reconnecting, and a toast
    d.net.feed(Some(Duration::from_millis(30)), true);
    d.net_toast();
    let s = render(&mut d, 160, 30);
    assert!(
        s.contains("⟳ reconnecting") && s.contains("back online"),
        "{s}"
    );
}

#[test]
fn terminals_message_bar_sends_and_queues() {
    let mut d = dash();
    d.view = View::Wall;
    wall::add(&mut d, "b31|w1:p2".into());
    let _ = render(&mut d, 120, 30);
    d.on_key(k(KeyCode::Char('i')));
    assert!(d.wall.compose.is_some());
    for ch in "run the tests".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 120, 30);
    assert!(s.contains("✎ to") && s.contains("run the tests"), "{s}");
    assert!(s.contains("MESSAGE"), "{s}");
    // tab queues it for temper (blocked: after you answer)
    d.on_key(k(KeyCode::Tab));
    let q = crate::queue::for_session("b31", "temper");
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].prompt, "run the tests");
    let s = render(&mut d, 120, 30);
    assert!(s.contains("1 queued"), "{s}");
    // esc closes; Q opens the queue; d drops it
    d.on_key(k(KeyCode::Esc));
    assert!(d.wall.compose.is_none());
    d.on_key(k(KeyCode::Char('Q')));
    let s = render(&mut d, 120, 30);
    assert!(s.contains("queued for"), "{s}");
    d.on_key(k(KeyCode::Char('d')));
    assert!(crate::queue::for_session("b31", "temper").is_empty());
    assert!(d.wall.queue_view.is_none());
    // X asks before closing the session
    d.on_key(k(KeyCode::Char('X')));
    assert!(matches!(d.mode, Mode::Confirm { .. }));
}

#[test]
fn fleet_summary_states_agents_and_limits() {
    let mut d = dash();
    {
        let h = d.fleet.host_mut("gpu2").unwrap();
        h.sessions[0].limit = Some("in 2 hours 13 minutes".into());
        h.sessions[0].agent = "codex".into();
    }
    let t = fleet::tally(d.fleet.sessions());
    assert_eq!((t.working, t.blocked, t.done, t.limit), (1, 1, 0, 1));
    assert_eq!(t.agents[0], ("claude".to_string(), 2));
    assert_eq!(
        fleet::limit_text("in 2 hours 13 minutes"),
        "◷ limit · back in 2h 13m"
    );
    assert_eq!(
        fleet::limit_text("2:40am (europe/london)"),
        "◷ limit · resets 2:40am"
    );
    assert_eq!(fleet::limit_text(""), "◷ at limit");
    let s = render(&mut d, 160, 40);
    assert!(s.contains("1 at limit") && s.contains("needs you"), "{s}");
    assert!(s.contains("back in 2h 13m"), "{s}");
    assert!(s.contains("Claude Code 2") && s.contains("Codex 1"), "{s}");
}

#[test]
fn add_machine_form_explains_each_stage() {
    let mut d = dash();
    d.on_key(k(KeyCode::Char('+')));
    assert!(matches!(d.mode, Mode::HostForm(_)));
    let set = |d: &mut Dash, st: forms::HostStage| {
        if let Mode::HostForm(hf) = &mut d.mode {
            hf.f[1] = forms::Field::new("tester@box");
            hf.stage = st;
        }
    };
    set(
        &mut d,
        forms::HostStage::NewKey {
            fingerprints: vec!["ED25519  SHA256:abc".into()],
            lines: vec![],
        },
    );
    let s = render(&mut d, 120, 40);
    assert!(s.contains("SHA256:abc") && s.contains("trust it"), "{s}");
    set(&mut d, forms::HostStage::NeedKey);
    let s = render(&mut d, 120, 40);
    assert!(s.contains("never sees or stores"), "{s}");
    set(
        &mut d,
        forms::HostStage::Failed("the name doesn't resolve".into()),
    );
    let s = render(&mut d, 120, 40);
    assert!(
        s.contains("couldn't connect") && s.contains("save anyway"),
        "{s}"
    );
    // esc from a failure cancels the form
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
}

#[test]
fn inbox_puts_what_needs_you_first() {
    let mut d = dash();
    inbox::set_rows(&mut d, vec![]);
    d.view = View::Inbox;
    // temper is blocked in the fixture: it's live, at the top, with what to do
    let s = render(&mut d, 180, 40);
    assert!(s.contains("Needs you now"), "{s}");
    assert!(
        s.contains("What you can do") && s.contains("approve"),
        "{s}"
    );
    assert!(s.contains("Needs you now · 1"), "{s}");
    // v: the cards, then the table
    d.on_key(k(KeyCode::Char('v')));
    let s = render(&mut d, 180, 40);
    assert!(s.contains("Live now") && s.contains("temper ·"), "{s}");
    d.on_key(k(KeyCode::Char('v')));
    let s = render(&mut d, 180, 40);
    assert!(s.contains("by machine") && s.contains("SESSION"), "{s}");
}

#[test]
fn fleet_groups_machines_by_tag() {
    let mut d = dash();
    d.view = View::Fleet;
    // no tags: no headings
    let s = render(&mut d, 160, 40);
    assert!(!s.contains("#gpu") && !s.contains("untagged"), "{s}");
    // gpu2 tagged: its group first, b31 under "untagged"
    let _ = d.cfg.set("host.gpu2.tags", "gpu, lab");
    let s = render(&mut d, 160, 40);
    let (g, u) = (s.find("#gpu").unwrap(), s.find("untagged").unwrap());
    assert!(g < u, "{s}");
    assert!(
        s[g..].find("gpu2").unwrap() < s[g..].find("b31").unwrap(),
        "{s}"
    );
    // fleet.group = none: back to fleet order, no headings
    let _ = d.cfg.set("fleet.group", "none");
    let s = render(&mut d, 160, 40);
    assert!(!s.contains("#gpu"), "{s}");
    let _ = d.cfg.unset("fleet.group");
    let _ = d.cfg.unset("host.gpu2.tags");
}

#[test]
fn fleet_manages_machines_without_a_hosts_tab() {
    let mut d = dash();
    d.hv.rows.push(super::hosts::HostRow::for_test("gpu9"));
    d.view = View::Fleet;
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Not in fleet") && s.contains("1 machine in ~/.ssh/config"),
        "{s}"
    );
    assert!(!s.contains("gpu9"), "closed until opened: {s}");
    // open the group, select the machine, look at it
    d.fv.sel = Some(fleet::RowId::Others);
    d.on_key(k(KeyCode::Enter));
    assert!(d.fv.others_open);
    d.fv.sel = Some(fleet::RowId::Avail("gpu9".into()));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("gpu9") && s.contains("add to the fleet"), "{s}");
    // edit and pause work on fleet machines
    d.fv.sel = Some(fleet::RowId::Host("b31".into()));
    d.on_key(k(KeyCode::Char('e')));
    match &d.mode {
        Mode::HostForm(hf) => assert_eq!(hf.editing.as_deref(), Some("b31")),
        _ => panic!("e should open the edit form"),
    }
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Edit b31") && s.contains("Jump via"), "{s}");
    d.mode = Mode::Normal;
    // a first Z only asks (a slip of shift on z mustn't stop watching it)
    d.on_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT));
    assert!(!d.fleet.host("b31").unwrap().paused);
    assert!(d
        .toasts
        .iter()
        .any(|t| t.text.contains("Z again to pause b31")));
    d.on_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT));
    assert!(d.fleet.host("b31").unwrap().paused);
    let s = render(&mut d, 160, 40);
    assert!(s.contains("⏸ paused"), "{s}");
    d.on_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT));
    assert!(!d.fleet.host("b31").unwrap().paused);
}

#[test]
fn the_tour_shows_one_card_per_tab_then_ends() {
    let mut d = dash();
    d.mode = Mode::Tour(0);
    tour::show(&mut d, 0);
    let s = render(&mut d, 120, 32);
    assert!(s.contains("Fleet — every session"), "{s}");
    // the keys as the keymap has them
    let new = d.keys.key_for(Act::New);
    assert!(s.contains(&format!("{new} starts a new session")), "{s}");
    for (i, v) in [View::Monitor, View::Inbox, View::Settings]
        .iter()
        .enumerate()
    {
        d.on_key(k(KeyCode::Enter));
        assert!(matches!(d.mode, Mode::Tour(n) if n == i + 1));
        assert_eq!(d.view, *v);
        let _ = render(&mut d, 80, 24);
    }
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(d.mode, Mode::Normal));
    assert_eq!(d.view, View::Fleet);
    assert_eq!(d.cfg.get("tour").as_deref(), Some("done"));
    // small terminals don't break it
    d.mode = Mode::Tour(2);
    let _ = render(&mut d, 20, 5);
}

/// Every cell that paints a background has ink of its own: the terminal's
/// default foreground can be the other family's (light on a light pill).
#[test]
fn painted_backgrounds_have_ink() {
    let mut d = dash();
    let mut bad: Vec<String> = Vec::new();
    let mut check = |d: &mut Dash, what: &str| {
        let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
        t.draw(|f| d.draw(f)).unwrap();
        let b = t.backend().buffer();
        for y in 0..40 {
            for x in 0..140 {
                let c = &b[(x, y)];
                if c.bg != Color::Reset && c.fg == Color::Reset && c.symbol() != " " {
                    bad.push(format!("{what} ({x},{y}) {:?}", c.symbol()));
                }
            }
        }
    };
    for v in View::ALL {
        d.view = v;
        check(&mut d, &format!("{v:?}"));
    }
    d.view = View::Fleet;
    let mut q = super::quick::QuickStart::new(&d, "b31");
    q.loaded(Ok(vec![]), vec![]);
    d.mode = Mode::Quick(q);
    check(&mut d, "quick start");
    let s = d.fleet.host("b31").unwrap().sessions.clone();
    d.mode = Mode::CloseList(super::quick::CloseList::new("b31", s));
    check(&mut d, "close list");
    d.mode = Mode::Help(help::Help::default());
    check(&mut d, "help");
    d.mode = Mode::Tour(0);
    tour::show(&mut d, 0);
    check(&mut d, "tour");
    assert!(bad.is_empty(), "{bad:#?}");
}

#[test]
fn help_finds_keys_by_what_they_do() {
    let mut d = dash();
    d.view = View::Fleet;
    d.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
    assert!(matches!(d.mode, Mode::Help(_)));
    for c in "max".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    let s = render(&mut d, 140, 40);
    let sel = s
        .lines()
        .find(|l| l.contains('┃') && l.contains("↵ run") || l.contains("┃ z"))
        .unwrap_or("");
    // the layout's maximise (every view) first, Focus's zoom too
    assert!(sel.contains("maximise"), "max → maximise first: {s}");
    assert!(s.contains("Focus · Tile") && s.contains("zoom"), "{s}");
    // esc clears the search, esc again closes
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(&d.mode, Mode::Help(h) if h.q.is_empty()));
    // "kill" finds close; enter runs a Fleet action (here: close → it asks first)
    for c in "kill".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    let v = help::found(&d, "kill");
    assert!(v[0].what.contains("close"), "{}", v[0].what);
    let v = help::found(&d, "terminal");
    assert!(
        v.iter().take(3).any(|x| x.keys == "!"),
        "{:?}",
        v.iter().map(|x| &x.what).collect::<Vec<_>>()
    );
    let v = help::found(&d, "dismiss");
    assert!(v[0].keys == "U", "{}", v[0].keys);
    d.on_key(k(KeyCode::Esc));
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    // a view by name
    d.on_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT));
    for c in "inbox view".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Inbox);
}

#[test]
fn a_bare_machine_that_joins_gets_its_setup_in_a_tab() {
    let mut d = dash();
    std::env::remove_var("ALEPH_TAB_CMD");
    // the look after it joined: no herdr, no agents
    let bare = crate::probe::parse("box3", "CCO\tLinux x86_64\nCCW\t/home/t\n");
    hosts::on_msg(&mut d, hosts::Msg::Looked("box3".into(), Box::new(bare)));
    assert!(d.hv.setting_up.contains_key("box3"));
    let t = render(&mut d, 160, 40);
    assert!(
        t.contains("box3 has no herdr and no agents yet — opening its setup in a new tab"),
        "{t}"
    );
    // a terminal that can't open a tab (as in tests): offered on this screen
    let m = d.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    d.on_bg(m);
    assert!(
        matches!(&d.mode, Mode::Confirm { on_yes: Pending::Handoff(h), .. } if matches!(&**h, Handoff::HostSetup(a) if a == "box3")),
        "no fallback"
    );
    assert!(!d.hv.setting_up.contains_key("box3"));
    // one with herdr and an agent: nothing opens
    let mut d = dash();
    let full = crate::probe::parse(
        "gpu2",
        "CCA\tcodex\t/u/codex\t0.40.1\nCCH\t/u/herdr\therdr 0.8.0\nCCO\tLinux\n",
    );
    hosts::on_msg(&mut d, hosts::Msg::Looked("gpu2".into(), Box::new(full)));
    assert!(d.hv.setting_up.is_empty() && matches!(d.mode, Mode::Normal));
}

#[test]
fn a_machine_set_up_elsewhere_is_looked_at_again() {
    let mut d = dash();
    d.hv.setting_up
        .insert("gpu2".into(), Instant::now() - Duration::from_secs(30));
    // while its setup runs in a tab, its card says so
    let t = render(&mut d, 160, 40);
    assert!(t.contains("setting up in a tab"), "{t}");
    let n = crate::refresh::Note {
        host: "gpu2".into(),
        at: 1,
        what: "setup".into(),
        herdr: Some("0.9.5".into()),
        agents: vec![
            ("claude".into(), "2.1.200".into()),
            ("codex".into(), "0.50.0".into()),
        ],
        probed: true,
        ..Default::default()
    };
    d.machine_changed(&n);
    assert!(!d.hv.setting_up.contains_key("gpu2"));
    let t = render(&mut d, 160, 40);
    assert!(t.contains("gpu2 set up · herdr 0.9.5 · 2 agents"), "{t}");
    assert!(!t.contains("setting up in a tab"), "{t}");
}

// ------------------------------------------------------------------ showcase mode

/// Showcase's autopilot tour: ctrl+t starts it, a chip and a caption say so
/// (at 120×32 and 200×50), it walks Fleet and opens Focus on a working
/// session, and any key only stops it.
#[cfg(feature = "showcase")]
#[test]
fn the_autopilot_tour_walks_the_dashboard_and_any_key_stops_it() {
    let mut d = dash();
    crate::showcase::force("masked");
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "showcase.tour_views=fleet,focus,inbox",
    ]);
    let ctrl_t = KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL);
    // the palette offers it; the help finds it by what it does
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    for ch in "tour".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 140, 40);
    assert!(s.contains("Showcase: start the tour"), "{s}");
    d.on_key(k(KeyCode::Esc));
    let found = help::found(&d, "autopilot");
    assert_eq!(found.first().map(|e| e.keys.as_str()), Some("ctrl+t"));
    // ctrl+t: the tour starts on Fleet's first session
    d.switch(View::Inbox);
    d.on_key(ctrl_t);
    crate::showcase::autopilot(&mut d);
    assert_eq!(d.view, View::Fleet);
    assert_eq!(
        fleet::selected(&d),
        Some(fleet::RowId::Sess("b31|w1:p1".into()))
    );
    for (w, h) in [(120u16, 32u16), (200, 50)] {
        let s = render(&mut d, w, h);
        let head = s.lines().next().unwrap();
        assert!(head.contains("▶ tour 1/3"), "{w}×{h}: {head}");
        assert!(head.contains(" showcase "), "{head}");
        let cap = s.lines().rev().nth(1).unwrap();
        assert!(
            cap.contains("Fleet") && cap.contains("every agent"),
            "{w}×{h}: {cap}"
        );
        // the chip never takes the content's room: the header row alone
        assert_eq!(s.matches("▶ tour").count(), 1, "{s}");
    }
    // the next step: Focus on the working session
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "showcase.tour_views=focus,inbox",
    ]);
    crate::showcase::tour_input();
    crate::showcase::capture(&d.cfg, "tour");
    crate::showcase::autopilot(&mut d);
    assert_eq!(d.view, View::Wall);
    assert!(d.wall.in_focus());
    let s = render(&mut d, 120, 32);
    assert!(s.lines().next().unwrap().contains("▶ tour 1/2"), "{s}");
    assert!(s.contains("one agent, full screen"), "{s}");
    let focused = d.wall.focused_session(&d).map(|s| s.key());
    assert_eq!(focused.as_deref(), Some("b31|w1:p1"));
    // any key only stops it: `y` approves nothing, the view stays
    let before = d.view;
    d.on_key(k(KeyCode::Char('y')));
    assert_eq!(d.view, before);
    assert!(d.toasts.is_empty(), "the key did something");
    let s = render(&mut d, 120, 32);
    assert!(!s.contains("▶ tour"), "{s}");
    crate::showcase::autopilot(&mut d);
    assert!(!render(&mut d, 120, 32).contains("▶ tour"));
    // showcase off: ctrl+t is nothing
    crate::showcase::force("off");
    d.switch(View::Fleet);
    d.on_key(ctrl_t);
    crate::showcase::autopilot(&mut d);
    assert!(!render(&mut d, 120, 32).contains("▶ tour"));
}

/// Presenter sync: what the dashboard says it shows (its view, the machine
/// and the session selected; Focus's own there; Monitor's sub-view).
#[test]
fn the_dashboard_says_what_it_presents() {
    let mut d = dash();
    let p = d.presenting();
    assert_eq!(p.view, "fleet");
    assert_eq!(
        (p.host.as_deref(), p.pane.as_deref(), p.name.as_deref()),
        (Some("b31"), Some("w1:p1"), Some("ignite"))
    );
    d.on_key(k(KeyCode::Char(']')));
    let p = d.presenting();
    assert_eq!((p.host.as_deref(), p.name), (Some("gpu2"), None));
    wall::focus_on(&mut d, "b31|w1:p2", View::Fleet);
    let p = d.presenting();
    assert_eq!(
        (p.view.as_str(), p.name.as_deref()),
        ("focus", Some("temper"))
    );
    d.switch(View::Monitor);
    d.mon.sub = monitor_draw::Sub::Usage;
    assert_eq!(d.presenting().view, "usage");
    d.mon.sub = monitor_draw::Sub::Procs;
    assert_eq!(d.presenting().view, "processes");
    d.switch(View::Inbox);
    assert_eq!(d.presenting().view, "inbox");
}

/// Focus opens with keys going straight to the agent (`focus.keys`); its
/// leave keys give them back; `focus.keys = aleph` opens it on Aleph's.
#[test]
fn focus_opens_typing_into_the_agent_unless_asked_not_to() {
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    wall::focus_on(&mut d, "b31|w1:p1", View::Fleet);
    assert!(d.wall.typing);
    assert!(render(&mut d, 120, 30).contains("TYPING"));
    // ctrl+c is the agent's here, never a quit
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    assert!(!matches!(d.on_key(ctrl('c')), Some(Handoff::Quit)));
    // ctrl+] (as most terminals send it: ctrl+5) gives the keys back
    d.on_key(ctrl('5'));
    assert!(!d.wall.typing);
    assert!(matches!(d.on_key(ctrl('c')), Some(Handoff::Quit)));
    let _ = d.cfg.set("focus.keys", "aleph");
    wall::focus_on(&mut d, "b31|w1:p2", View::Fleet);
    assert!(!d.wall.typing);
}

#[cfg(feature = "showcase")]
#[test]
fn showcase_attach_opens_focus_and_herdr_asks_first() {
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    crate::showcase::force("masked");
    let cur = |d: &Dash| match d.wall.content_of_focus() {
        Some(wall::Content::Session(k)) => k.clone(),
        _ => String::new(),
    };
    // a: Focus (masked), not herdr's own screen
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    assert!(d.on_key(k(KeyCode::Char('a'))).is_none());
    assert_eq!(d.view, View::Wall);
    assert!(d.wall.in_focus());
    assert_eq!(cur(&d), "b31|w1:p1");
    // (it opens typing into the agent: ctrl+] gives Aleph its keys)
    leave_typing(&mut d);
    // a again in Focus: stays there, and says how to attach for real
    assert!(d.on_key(k(KeyCode::Char('a'))).is_none());
    assert!(d.wall.in_focus());
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text.contains("A attaches")));
    d.on_key(k(KeyCode::Esc));
    assert_eq!(d.view, View::Fleet);
    // enter_opens=attach: Focus too
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "enter_opens=attach",
    ]);
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p2".into()));
    assert!(d.on_key(k(KeyCode::Enter)).is_none());
    assert_eq!(cur(&d), "b31|w1:p2");
    leave_typing(&mut d);
    d.on_key(k(KeyCode::Esc));
    // A: asks first; yes attaches, no stays
    assert!(d.on_key(k(KeyCode::Char('A'))).is_none());
    assert!(matches!(d.mode, Mode::Confirm { .. }));
    let s = render(&mut d, 120, 30);
    assert!(s.contains("isn't masked"), "{s}");
    assert!(matches!(
        d.on_key(k(KeyCode::Char('y'))),
        Some(Handoff::Direct { .. })
    ));
    d.on_key(k(KeyCode::Char('A')));
    assert!(d.on_key(k(KeyCode::Char('n'))).is_none());
    assert!(matches!(d.mode, Mode::Normal));
    // O (herdr in a new tab): the same question
    d.on_key(k(KeyCode::Char('O')));
    assert!(matches!(d.mode, Mode::Confirm { .. }));
    d.on_key(k(KeyCode::Esc));
    // the demo world: nothing real to attach to
    crate::showcase::force("demo");
    assert!(d.on_key(k(KeyCode::Char('A'))).is_none());
    assert!(matches!(d.mode, Mode::Normal));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text.contains("nothing real")));
    d.on_key(k(KeyCode::Char('O')));
    assert!(matches!(d.mode, Mode::Normal));
    // off: as ever
    crate::showcase::force("off");
    assert!(matches!(
        d.on_key(k(KeyCode::Char('A'))),
        Some(Handoff::Direct { .. })
    ));
    assert!(matches!(
        d.on_key(k(KeyCode::Char('a'))),
        Some(Handoff::Attach { .. })
    ));
}

#[cfg(feature = "showcase")]
#[test]
fn showcase_dresses_the_dashboard_and_never_echoes_typing() {
    let mut d = dash();
    crate::showcase::force("masked");
    let s = render(&mut d, 140, 36);
    let head = s.lines().next().unwrap().to_string();
    assert!(head.contains(" showcase "), "{head}");
    assert!(
        head.contains("ℵ aleph") && head.contains("2/2 machines"),
        "{head}"
    );
    // a tagline between the tabs and their key hint
    let tabs = s.lines().nth(1).unwrap().to_string();
    assert!(
        tabs.contains("✦ ") && tabs.contains("Settings") && tabs.contains("palette"),
        "{tabs}"
    );
    // narrow: the wordmark, the chip and the machine count stay
    let s = render(&mut d, 60, 24);
    let head = s.lines().next().unwrap().to_string();
    assert!(
        head.contains(" showcase ") && head.contains("machines"),
        "{head}"
    );
    // the keys pressed, bottom right
    d.on_key(k(KeyCode::Char('j')));
    d.on_key(k(KeyCode::Char('j')));
    // (in the footer's spare room, or just above it when the hints fill it;
    // laid in after the lens, as the dashboard's loop does)
    let tail = |s: &str| s.lines().rev().take(2).collect::<Vec<_>>().join("\n");
    let laid = |d: &mut Dash| {
        let mut t = Terminal::new(TestBackend::new(140, 36)).unwrap();
        t.draw(|f| {
            d.draw(f);
            crate::showcase::buffer(f.buffer_mut());
            crate::showcase::finish(&d.cfg, f.buffer_mut(), "Fleet");
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..36)
            .map(|y| (0..140).map(|x| b[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let s = laid(&mut d);
    let foot = tail(&s);
    assert!(foot.contains(" j ×2"), "{foot}");
    // a filter's text never shows there
    d.on_key(k(KeyCode::Char('/')));
    for ch in "zq".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = laid(&mut d);
    let foot = tail(&s);
    assert!(
        foot.contains(" / ") && !foot.contains(" z ") && !foot.contains(" q "),
        "{foot}"
    );
    d.on_key(k(KeyCode::Esc));
    // a view's caption, just above the footer
    d.switch(View::Inbox);
    let s = render(&mut d, 140, 36);
    let cap = s.lines().rev().nth(1).unwrap().to_string();
    assert!(
        cap.contains("Inbox") && cap.contains("what needs you"),
        "{cap}"
    );
    // switched off part by part
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "showcase.flair=off",
    ]);
    d.switch(View::Fleet);
    let s = render(&mut d, 140, 36);
    assert!(!s.lines().next().unwrap().contains(" showcase "), "{s}");
    assert!(!s.contains("every agent on every machine"), "{s}");
    crate::showcase::force("off");
    let s = render(&mut d, 140, 36);
    assert!(!s.contains(" showcase "));
}

/// Showcase's delight, as pictures: dashboard frames with the cockpit
/// palette, a supernova, a pulsar's flash and a hyperspace streak, as HTML
/// pages to look at (`DELIGHT_HTML=dir cargo test --features showcase
/// delight_pages -- --ignored`). The palette is the whole process's: only
/// ever on its own.
#[cfg(feature = "showcase")]
#[test]
#[ignore]
fn delight_pages() {
    let Some(dir) = std::env::var_os("DELIGHT_HTML") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let page = |b: &ratatui::buffer::Buffer, light: bool| {
        let (bg, fg) = if light {
            ("#f4f1ea", "#222")
        } else {
            ("#0b0b0d", "#ccc")
        };
        // every cell exactly one column (braille and emoji fonts differ)
        let mut s = format!(
            "<html><body style='background:{bg};margin:12px'><style>i{{display:inline-block;width:9px;font-style:normal;overflow:visible}}</style><pre style='font:15px/19px Menlo,\"Apple Braille\",monospace;color:{fg}'>"
        );
        for y in 0..b.area.height {
            let mut covered = 0;
            for x in 0..b.area.width {
                let cell = &b[(x, y)];
                if covered > 0 {
                    covered -= 1;
                    continue;
                }
                covered = crate::ui::width(cell.symbol()).saturating_sub(1);
                if cell.skip || cell.symbol().is_empty() {
                    continue;
                }
                let sym = match cell.symbol() {
                    "<" => "&lt;",
                    ">" => "&gt;",
                    "&" => "&amp;",
                    o => o,
                };
                let mut st = String::new();
                if crate::ui::width(cell.symbol()) > 1 {
                    st += "width:18px;";
                }
                if let Color::Rgb(r, g, bl) = cell.fg {
                    st += &format!("color:#{r:02x}{g:02x}{bl:02x};");
                }
                if let Color::Rgb(r, g, bl) = cell.bg {
                    st += &format!("background:#{r:02x}{g:02x}{bl:02x};");
                }
                if cell.modifier.contains(Modifier::BOLD) {
                    st += "font-weight:bold;";
                }
                s += &format!("<i style='{st}'>{sym}</i>");
            }
            s.push('\n');
        }
        s + "</pre></body></html>"
    };
    let shot = |d: &mut Dash, name: &str, light: bool| {
        // as the loop draws it: the view, the lens, then the celebrations
        let mut t = Terminal::new(TestBackend::new(140, 36)).unwrap();
        t.draw(|f| {
            d.draw(f);
            crate::showcase::buffer(f.buffer_mut());
            crate::showcase::celebrations(f.buffer_mut());
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        std::fs::write(dir.join(format!("{name}.html")), page(&b, light)).unwrap();
    };
    let tr = |d: &Dash, key: &str, from: &str, to: &str| {
        let mut s = d
            .fleet
            .sessions()
            .into_iter()
            .find(|s| s.key() == key)
            .unwrap()
            .clone();
        s.status = to.into();
        crate::fleet::Transition {
            session: s,
            from: from.into(),
            took: 0,
        }
    };
    crate::showcase::force("masked");
    for (light, theme) in [(false, "dark"), (true, "light")] {
        crate::showcase::force_palette(false);
        crate::ui::c::init(theme, None);
        let mut d = dash();
        d.switch(View::Fleet);
        shot(&mut d, &format!("{theme}-normal"), light);
        crate::showcase::force_palette(true);
        shot(&mut d, &format!("{theme}-cockpit"), light);
        // a session finished: its supernova, as it goes
        let t = tr(&d, "gpu2|w1:p4", "working", "done");
        crate::showcase::transitions(&d.cfg, &[t], &[]);
        let t0 = Instant::now();
        for ms in [60u64, 250, 500, 800] {
            std::thread::sleep(Duration::from_millis(ms).saturating_sub(t0.elapsed()));
            shot(&mut d, &format!("{theme}-nova-{ms:04}"), light);
        }
        // a question answered: the pulsar
        let t = tr(&d, "b31|w1:p2", "blocked", "working");
        crate::showcase::transitions(&d.cfg, &[t], &[]);
        std::thread::sleep(Duration::from_millis(90));
        shot(&mut d, &format!("{theme}-pulsar"), light);
        std::thread::sleep(Duration::from_millis(700));
        // a view switched: the streak
        d.switch(View::Inbox);
        shot(&mut d, &format!("{theme}-streak-start"), light);
        std::thread::sleep(Duration::from_millis(120));
        shot(&mut d, &format!("{theme}-streak"), light);
    }
    crate::showcase::force_palette(false);
    crate::showcase::force("off");
}

#[cfg(feature = "showcase")]
#[test]
fn showcase_cockpit_hud_and_a_supernova_on_its_row() {
    let mut d = dash();
    // nothing working: no spinner moves between two frames
    d.fleet.host_mut("b31").unwrap().sessions[0].status = "idle".into();
    d.switch(View::Fleet);
    crate::showcase::force("masked");
    // a frame as the dashboard's loop draws it: the view, the lens, and
    // then the celebrations, into the masked frame
    let frame = |d: &mut Dash| {
        let mut t = Terminal::new(TestBackend::new(140, 36)).unwrap();
        t.draw(|f| {
            d.draw(f);
            crate::showcase::buffer(f.buffer_mut());
            crate::showcase::celebrations(f.buffer_mut());
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..36)
            .map(|y| (0..140).map(|x| b[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
    };
    let s = render(&mut d, 140, 36);
    // the readouts at the right of the status bar, the brackets at the rule's ends
    let rule = s.lines().nth(2).unwrap().to_string();
    assert!(rule.ends_with('┓') && !rule.contains("working"), "{rule}");
    let bar = s.lines().last().unwrap().to_string();
    assert!(
        bar.contains("⟡ 2/2") && bar.contains("✻ 0") && bar.contains("▲ 1"),
        "{bar}"
    );
    // a session finishes: light round its row (braille), every letter of
    // the masked frame where it was
    let braille = |s: &[String]| {
        s.iter()
            .flat_map(|l| l.chars())
            .filter(|c| ('\u{2801}'..='\u{28ff}').contains(c))
            .count()
    };
    d.on_key(k(KeyCode::Char('?')));
    let under_help = braille(&frame(&mut d));
    d.on_key(k(KeyCode::Esc));
    let shown = frame(&mut d);
    let before = braille(&shown);
    let mut w = d
        .fleet
        .sessions()
        .into_iter()
        .find(|s| s.name == "weave")
        .unwrap()
        .clone();
    w.status = "done".into();
    let t = crate::fleet::Transition {
        session: w,
        from: "working".into(),
        took: 0,
    };
    // a machine's first poll isn't news
    crate::showcase::transitions(&d.cfg, std::slice::from_ref(&t), &["gpu2".to_string()]);
    assert_eq!(braille(&frame(&mut d)), before);
    crate::showcase::transitions(&d.cfg, &[t], &[]);
    std::thread::sleep(Duration::from_millis(250));
    // nothing plays over a popup
    d.on_key(k(KeyCode::Char('?')));
    assert_eq!(braille(&frame(&mut d)), under_help);
    d.on_key(k(KeyCode::Esc));
    let after = frame(&mut d);
    assert!(braille(&after) > before + 5, "{}", after.join("\n"));
    // (the list panel: the header, the HUD, the summary strip, the detail pane and the footer have spinners of their own)
    for y in 4..35 {
        for (x, (a, b)) in shown[y].chars().zip(after[y].chars()).enumerate().take(80) {
            assert!(
                a == ' ' || a == b,
                "({x},{y}): {a} became {b}\n{}",
                after.join("\n")
            );
        }
    }
    // both parts can go
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "showcase.flair=header",
    ]);
    let s = frame(&mut d);
    assert!(!s[2].contains('⟡'), "{}", s.join("\n"));
    assert_eq!(braille(&s), before);
    crate::showcase::force("off");
    let s = render(&mut d, 140, 36);
    assert!(!s.contains("⟡ 2/2") && !s.lines().nth(2).unwrap().contains('┓'));
}

/// Showcase capture: keycaps say what a key did, the record key records
/// (the header's chip, a cast of the frames), the palette has both, the
/// watermark sits in the tabs' rule.
#[cfg(feature = "showcase")]
#[test]
fn showcase_capture_names_keys_records_and_signs() {
    let mut d = dash();
    crate::showcase::force("masked");
    let frame = |d: &mut Dash| {
        let mut t = Terminal::new(TestBackend::new(140, 36)).unwrap();
        t.draw(|f| {
            d.draw(f);
            crate::showcase::buffer(f.buffer_mut());
            crate::showcase::celebrations(f.buffer_mut());
            crate::showcase::finish(&d.cfg, f.buffer_mut(), "Fleet");
        })
        .unwrap();
        let b = t.backend().buffer().clone();
        (0..36)
            .map(|y| (0..140).map(|x| b[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
    };
    let foot = |s: &[String]| s[34..].join("\n");
    // names: the key map's action, a global key
    d.on_key(k(KeyCode::Char('j')));
    d.on_key(k(KeyCode::Char('?')));
    d.on_key(k(KeyCode::Esc));
    let s = frame(&mut d);
    assert!(
        foot(&s).contains(" j  next session") && foot(&s).contains(" ?  help"),
        "{}",
        foot(&s)
    );
    // keys only
    d.cfg = Config::in_memory(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "showcase.keycast=keys",
        "showcase.watermark=alex.dev",
    ]);
    d.on_key(k(KeyCode::Char('k')));
    let s = frame(&mut d);
    assert!(
        foot(&s).contains(" k ") && !foot(&s).contains("previous session"),
        "{}",
        foot(&s)
    );
    // the watermark, in the rule under the tabs
    assert!(s[2].contains("┤ ✦ alex.dev ├"), "{}", s[2]);
    // the record key: a chip while it runs (and the key isn't keycast)
    let ctrl_r = KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL);
    d.on_key(ctrl_r);
    let s = frame(&mut d);
    assert!(s[0].contains(" ● rec 0:0"), "{}", s[0]);
    assert!(!foot(&s).contains("⌃R"));
    // the palette offers to stop it, and a poster
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    for ch in "showcase".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = frame(&mut d).join("\n");
    assert!(
        s.contains("Showcase: stop recording") && s.contains("Showcase: poster"),
        "{s}"
    );
    d.on_key(k(KeyCode::Esc));
    d.on_key(ctrl_r);
    let mut said = Vec::new();
    while let Some((ok, m)) = crate::showcase::capture_note() {
        said.push((ok, m));
    }
    let done = said.iter().find(|(_, m)| m.starts_with("recorded "));
    let (ok, m) = done.unwrap_or_else(|| panic!("{said:?}"));
    assert!(*ok && m.contains(".cast"), "{m}");
    assert!(!frame(&mut d)[0].contains(" rec "));
    crate::showcase::force("off");
}

#[test]
fn settings_step_pick_and_tick() {
    let mut d = dash();
    d.view = View::Settings;
    settings::go_section(&mut d, "Appearance");
    d.on_key(k(KeyCode::Enter));
    // to ui.splash: → steps it, enter opens every value with what it does
    for _ in 0..30 {
        if render(&mut d, 160, 40).contains("┃ui.splash ") {
            break;
        }
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Right));
    assert_eq!(d.cfg.get("ui.splash").as_deref(), Some("cockpit"));
    d.on_key(k(KeyCode::Left));
    assert_eq!(d.cfg.get("ui.splash").as_deref(), Some("theme"));
    // enter: the splash gallery, each previewed
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(d.mode, Mode::Splashes(_)));
    d.on_key(k(KeyCode::Esc));
    // ui.splash_end: enter opens every value with what it does
    for _ in 0..30 {
        if render(&mut d, 160, 40).contains("┃ui.splash_end") {
            break;
        }
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Set ui.splash_end") && s.contains("the particles gather"),
        "{s}"
    );
    // random, form, fall, warp: the last
    for _ in 0..3 {
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.cfg.get("ui.splash_end").as_deref(), Some("warp"));
    // ui.splash_fx: chips in the row, a checklist on enter
    for _ in 0..30 {
        if render(&mut d, 160, 40).contains("┃ui.splash_fx") {
            break;
        }
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("Set ui.splash_fx") && s.contains("☑ tunnel") && s.contains("warp rings"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char('n')));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Down));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.cfg.get("ui.splash_fx").as_deref(), Some("tunnel,shock"));
    let s = render(&mut d, 160, 40);
    let row = s
        .lines()
        .find(|l| l.contains("┃ui.splash_fx"))
        .unwrap_or("");
    assert!(
        row.contains(" tunnel ") && row.contains(" shock ") && !row.contains("comet"),
        "{s}"
    );
    // on a text row, ← still goes back to the sections
    d.on_key(k(KeyCode::Char('h')));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("┃Appearance") || s.contains("▌Appearance") || !s.contains("Set ui."),
        "{s}"
    );
}

#[test]
fn layout_keys_hide_maximise_and_resize_and_are_kept() {
    let mut d = dash();
    d.cfg.set("stats", "on").unwrap();
    let preview = |s: &str| s.contains("task of ignite  ") || s.contains("│ task of ignite");
    let s = render(&mut d, 160, 40);
    assert!(s.contains("Machines & sessions") && preview(&s), "{s}");
    // p hides the preview (the list takes the width); p again brings it back
    d.on_key(k(KeyCode::Char('p')));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("hidden"));
    let s = render(&mut d, 160, 40);
    assert!(!s.contains("│ task of ignite"), "{s}");
    d.on_key(k(KeyCode::Char('p')));
    assert_eq!(d.cfg.get("layout.fleet"), None);
    // z: the left side, the list alone, the resources alone, the preview alone, back
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("max main"));
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("max top"));
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("max bottom"));
    let s = render(&mut d, 160, 40);
    assert!(!s.contains("Machines & sessions"), "{s}");
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("max side"));
    let s = render(&mut d, 160, 40);
    assert!(
        !s.contains("Machines & sessions") && s.contains("task of ignite"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(d.cfg.get("layout.fleet"), None);
    // } moves the divider down; = puts it back
    d.on_key(KeyEvent::new(KeyCode::Char('}'), KeyModifiers::SHIFT));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("top 55"));
    d.on_key(k(KeyCode::Char('=')));
    assert_eq!(d.cfg.get("layout.fleet"), None);
    // L: layout mode, ← widens the side, esc keeps it
    d.on_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
    assert_eq!(d.layout_mode, Some("fleet"));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("maximise"), "{s}");
    d.on_key(k(KeyCode::Left));
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("side 45"));
    // 1–4 maximise there (not the views), q leaves it (not the dashboard)
    d.on_key(k(KeyCode::Char('4')));
    assert_eq!(d.view, View::Fleet);
    assert_eq!(
        d.cfg.get("layout.fleet").as_deref(),
        Some("side 45 · max side")
    );
    d.on_key(k(KeyCode::Char('4')));
    d.on_key(k(KeyCode::Char('2')));
    assert_eq!(
        d.cfg.get("layout.fleet").as_deref(),
        Some("side 45 · max top")
    );
    d.on_key(k(KeyCode::Char('2')));
    assert!(d.on_key(k(KeyCode::Char('q'))).is_none());
    assert_eq!(d.layout_mode, None);
    d.on_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
    d.on_key(k(KeyCode::Esc));
    assert_eq!(d.layout_mode, None);
    // esc in another view's layout mode stays in that view
    d.view = View::Inbox;
    d.on_key(KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT));
    assert_eq!(d.layout_mode, Some("inbox"));
    d.on_key(k(KeyCode::Char('3')));
    assert_eq!(d.view, View::Inbox);
    d.on_key(k(KeyCode::Esc));
    assert_eq!((d.view, d.layout_mode), (View::Inbox, None));
    // the same keys in another view: Settings' detail hides
    d.view = View::Settings;
    d.on_key(k(KeyCode::Char('p')));
    assert_eq!(d.cfg.get("layout.settings").as_deref(), Some("hidden"));
}

#[test]
fn whats_new_card_shows_the_notes_then_the_commits_and_closes_seen() {
    let mut d = dash();
    news::open(&mut d);
    assert!(matches!(d.mode, Mode::News(_)));
    let first = crate::whatsnew::all()[0].clone();
    let s = render(&mut d, 120, 40);
    assert!(s.contains("What's new") && s.contains(&first.title), "{s}");
    assert!(
        s.contains("enter  commits") && s.contains("esc  close"),
        "{s}"
    );
    // enter: the deploy's commits under its notes
    d.on_key(k(KeyCode::Enter));
    let s = render(&mut d, 120, 40);
    assert!(
        s.contains(&format!("· {}", &first.commits[0][..20])) && s.contains("enter  fewer"),
        "{s}"
    );
    // a small window scrolls instead of cutting it off
    let s = render(&mut d, 60, 14);
    assert!(s.contains("scroll"), "{s}");
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    assert_eq!(crate::whatsnew::seen().as_deref(), Some(first.id.as_str()));
    // seen: nothing to show after the next start
    assert!(crate::whatsnew::to_show(&d.cfg).is_empty());
}

#[test]
fn workspaces_save_where_you_are_and_go_back() {
    let mut d = dash();
    // the picker lists the starters
    d.on_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT));
    assert!(matches!(d.mode, Mode::Spaces(_)));
    let s = render(&mut d, 120, 32);
    assert!(
        s.contains("Workspaces") && s.contains("GPU watch") && s.contains("starter"),
        "{s}"
    );
    // 1: GPU watch is Monitor's GPU view
    d.on_key(k(KeyCode::Char('1')));
    assert_eq!(d.view, View::Monitor);
    assert_eq!(d.mon.sub, monitor_draw::Sub::Gpu);
    // save Fleet with its preview hidden as "Night shift"
    d.switch(View::Fleet);
    d.on_key(k(KeyCode::Char('p')));
    d.on_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT));
    d.on_key(k(KeyCode::Char('s')));
    assert!(matches!(d.mode, Mode::Input(_)));
    for c in "Night shift".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(k(KeyCode::Enter));
    assert_eq!(
        d.cfg.get("workspace.night-shift.view").as_deref(),
        Some("fleet")
    );
    assert_eq!(
        d.cfg.get("workspace.night-shift.layout").as_deref(),
        Some("hidden")
    );
    // elsewhere, with the preview back: the workspace puts both back
    d.on_key(k(KeyCode::Char('p')));
    d.switch(View::Inbox);
    let w = crate::workspace::find(&d.cfg, "Night shift").unwrap();
    spaces::apply(&mut d, &w);
    assert_eq!(d.view, View::Fleet);
    assert_eq!(d.cfg.get("layout.fleet").as_deref(), Some("hidden"));
    // d twice deletes a saved one
    d.on_key(KeyEvent::new(KeyCode::Char('W'), KeyModifiers::SHIFT));
    d.on_key(k(KeyCode::Char('d')));
    let s = render(&mut d, 120, 32);
    assert!(s.contains("d again deletes"), "{s}");
    d.on_key(k(KeyCode::Char('d')));
    assert!(d.cfg.get("workspace.night-shift.view").is_none());
}

#[test]
fn splash_gallery_previews_keeps_and_toggles_the_logo() {
    let mut d = dash();
    splashes::open(&mut d);
    let s = render(&mut d, 140, 40);
    assert!(
        s.contains("Splash gallery")
            && s.contains("singularity")
            && s.contains("random")
            && s.contains("Preview · the theme's: cockpit")
            && s.contains("play full screen"),
        "{s}"
    );
    for _ in 0..10 {
        if render(&mut d, 140, 40).contains("Preview · singularity") {
            break;
        }
        d.on_key(k(KeyCode::Down));
    }
    // its preview plays: a second and a half in, the scene's own line
    splashes::skip(&mut d, 1500);
    let s = render(&mut d, 140, 40);
    assert!(s.contains("a point becoming everything"), "{s}");
    // l: it ends its own way; enter keeps the splash and that
    d.on_key(k(KeyCode::Char('l')));
    assert!(render(&mut d, 140, 40).contains("ℵ ending  off"));
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(d.mode, Mode::Normal));
    assert_eq!(d.cfg.get("ui.splash").as_deref(), Some("singularity"));
    assert_eq!(d.cfg.get("ui.splash_logo").as_deref(), Some("off"));
    // off: nothing to preview; esc keeps what was set
    splashes::open(&mut d);
    assert!(render(&mut d, 140, 40).contains("● singularity"));
    d.on_key(k(KeyCode::End));
    let s = render(&mut d, 140, 40);
    assert!(s.contains("no splash — the dashboard comes at once"), "{s}");
    d.on_key(k(KeyCode::Esc));
    assert_eq!(d.cfg.get("ui.splash").as_deref(), Some("singularity"));
    // a small screen: the preview says what it shows instead
    splashes::open(&mut d);
    let s = render(&mut d, 60, 16);
    assert!(s.contains("Splash gallery"), "{s}");
    d.on_key(k(KeyCode::Esc));
    // help's search finds both galleries
    let v = help::found(&d, "splash");
    assert!(v[0].what.starts_with("splash gallery"), "{}", v[0].what);
    let v = help::found(&d, "colours");
    assert!(v[0].what.starts_with("theme gallery"), "{}", v[0].what);
}

#[test]
fn theme_gallery_moves_through_themes_and_accents() {
    let mut d = dash();
    gallery::open(&mut d);
    assert!(matches!(d.mode, Mode::Gallery(_)));
    let s = render(&mut d, 120, 36);
    assert!(
        s.contains("Theme gallery")
            && s.contains("Nord")
            && s.contains("Tokyo Night")
            && s.contains("claude orange"),
        "{s}"
    );
    assert!(s.contains("┃ ▆▆▆▆▆▆▆ Auto"), "{s}");
    d.on_key(k(KeyCode::Down));
    let s = render(&mut d, 120, 36);
    assert!(s.contains("┃ ▆▆▆▆▆▆▆ Graphite"), "{s}");
    // each theme once: space shows the light versions in their places
    assert!(
        !s.contains("Pulsar (light)") && s.contains("Pulsar ◐"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char(' ')));
    let s = render(&mut d, 120, 36);
    assert!(
        s.contains("┃ ▆▆▆▆▆▆▆ Paper") && s.contains("Pulsar (light)"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char(' ')));
    let s = render(&mut d, 120, 36);
    assert!(s.contains("┃ ▆▆▆▆▆▆▆ Graphite"), "{s}");
    // → the accents
    d.on_key(k(KeyCode::Right));
    d.on_key(k(KeyCode::Down));
    let s = render(&mut d, 120, 36);
    assert!(s.contains("┃ ● claude orange"), "{s}");
    // esc puts back what you had
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    assert_eq!(d.cfg.get("theme"), None);
}

#[test]
fn the_prompt_library_fills_variables_and_saves_what_you_wrote() {
    let mut d = dash();
    let mut s = sess("b31", "w1:p2", "hunt", "idle");
    s.cwd = "/srv/px".into();
    let input = |s: &Session| {
        Mode::Input(forms::Input::new(
            "Send to hunt".into(),
            "a prompt",
            String::new(),
            forms::For::Send(vec![s.clone()]),
        ))
    };
    let field = |d: &Dash| match &d.mode {
        Mode::Input(i) => i.f.text.clone(),
        _ => panic!("the input stays open"),
    };
    let ctrl = |c: char| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    d.mode = input(&s);
    let r = render(&mut d, 120, 34);
    assert!(r.contains("ctrl+p  prompts"), "{r}");
    // ctrl+p: the library, with the starters
    d.on_key(ctrl('p'));
    let r = render(&mut d, 120, 34);
    assert!(r.contains("@review") && r.contains("@explain"), "{r}");
    // explain needs {topic}: asked, then filled in with the session's folder
    for _ in 0..4 {
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    let r = render(&mut d, 120, 34);
    assert!(r.contains("@explain needs") && r.contains("{topic}"), "{r}");
    for c in "LUT fitting".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(k(KeyCode::Enter));
    assert!(
        field(&d).starts_with("Explain how LUT fitting works"),
        "{}",
        field(&d)
    );
    // @review and enter: filled in, not sent yet
    d.mode = input(&s);
    for c in "@review".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(k(KeyCode::Enter));
    assert!(field(&d).contains("changes in px since"), "{}", field(&d));
    // ctrl+s: saved as a prompt, then back to the text
    d.mode = input(&s);
    for c in "check {ticket} again".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(ctrl('s'));
    for c in "Recheck".chars() {
        d.on_key(k(KeyCode::Char(c)));
    }
    d.on_key(k(KeyCode::Enter));
    assert_eq!(
        d.cfg.get("prompt.recheck").as_deref(),
        Some("check {ticket} again")
    );
    assert_eq!(field(&d), "check {ticket} again");
}

#[test]
fn h_lists_who_can_take_over() {
    let mut d = dash();
    let s = render(&mut d, 120, 32);
    assert!(s.contains("claude_ignite") || s.contains("ignite"), "{s}");
    d.fv.sel = Some(fleet::RowId::Sess(
        d.fleet
            .sessions()
            .into_iter()
            .find(|x| x.name == "ignite")
            .unwrap()
            .key(),
    ));
    d.on_key(k(KeyCode::Char('h')));
    let s = render(&mut d, 120, 32);
    assert!(s.contains("Hand claude_ignite to Codex"), "{s}");
    assert!(
        !s.contains("to Claude Code"),
        "not to the agent it already is: {s}"
    );
}

#[test]
fn a_race_shows_its_diffs_side_by_side() {
    let mut d = dash();
    let racer = |agent: &str, name: &str| crate::race::Racer {
        agent: agent.into(),
        name: name.into(),
        pane: "w9:p1".into(),
        dir: format!("/srv/px-{name}"),
        branch: format!("race/1530-a-{agent}"),
    };
    let race = crate::race::Race {
        id: "1530-a".into(),
        host: "b31".into(),
        dir: "/srv/px".into(),
        base: "abc".into(),
        prompt: "make the LUT fit faster".into(),
        racers: vec![racer("claude", "plover"), racer("codex", "loom")],
        ..Default::default()
    };
    d.mode = Mode::Race(race::RaceView::with(
        race,
        vec![
            (
                " src/lut.rs | 4 ++--".into(),
                String::new(),
                "@@ -1 +1 @@\n-old\n+fast".into(),
            ),
            (String::new(), String::new(), String::new()),
        ],
    ));
    let s = render(&mut d, 140, 30);
    assert!(
        s.contains("race 1530-a") && s.contains("“make the LUT fit faster”"),
        "{s}"
    );
    assert!(
        s.contains("Claude Code") && s.contains("Codex") && s.contains("plover · gone"),
        "{s}"
    );
    assert!(s.contains("+fast") && s.contains("no changes yet"), "{s}");
    d.on_key(k(KeyCode::Right));
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
}

// ------------------------------------------------------------------ the fleet at a glance

/// A session's row in Fleet's list (its tree guide sets it apart from the
/// preview's title).
fn row_of<'a>(s: &'a str, name: &str) -> &'a str {
    s.lines()
        .find(|l| l.contains(name) && (l.contains("├─") || l.contains("└─")))
        .unwrap_or_else(|| panic!("no row for {name}: {s}"))
}

fn render_buf(d: &mut Dash, w: u16, h: u16) -> ratatui::buffer::Buffer {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| d.draw(f)).unwrap();
    t.backend().buffer().clone()
}

/// A dash with its sessions' states going back a few hours: the log has
/// ignite working, asking, then done; temper idle then working; drift, which
/// ended an hour ago. b31's sessions are in their current state for the last
/// ten minutes.
fn dash_with_history(extra: &[&str]) -> Dash {
    use crate::statelog::{St, Stretch};
    let mut d = dash();
    let mut cfg = vec!["hosts=b31,gpu2", "stats=off", "local=false"];
    cfg.extend_from_slice(extra);
    d.cfg = Config::in_memory_own(&cfg);
    let now = crate::cache::now();
    for s in d.fleet.host_mut("b31").unwrap().sessions.iter_mut() {
        s.since = Some(now - 600);
    }
    let st = |a: u64, b: u64, pane: &str, name: &str, st: St| Stretch {
        start: now - a,
        end: now - b,
        host: "b31".into(),
        pane: pane.into(),
        name: name.into(),
        agent: "claude".into(),
        st,
    };
    d.tl.set_log(vec![
        st(5 * 3600, 3 * 3600, "w1:p1", "ignite", St::Working),
        st(3 * 3600, 2 * 3600, "w1:p1", "ignite", St::Needs),
        st(2 * 3600, 600, "w1:p1", "ignite", St::Done),
        st(4 * 3600, 3600, "w1:p2", "temper", St::Idle),
        st(3600, 600, "w1:p2", "temper", St::Working),
        st(4 * 3600, 3600, "w1:p7", "drift", St::Working),
    ]);
    d
}

#[test]
fn a_rows_model_sits_between_its_name_and_its_state() {
    use crate::usage::dash::{Data, Live};
    let mut d = dash();
    d.fleet.host_mut("b31").unwrap().sessions[0].native_id = Some("conv-1".into());
    let name = d.fleet.host_mut("b31").unwrap().sessions[0]
        .display()
        .to_string();
    let mut data = Data::default();
    data.live.insert(
        "conv-1".into(),
        Live {
            model: "claude-opus-5-5-20261001".into(),
            tokens: 5000,
            ..Default::default()
        },
    );
    d.usage.data = Some(data);
    let s = render(&mut d, 220, 40);
    let row = s
        .lines()
        .find(|l| l.contains(&name) && l.contains(" tok"))
        .unwrap_or_default();
    let (n, m) = (
        row.find(&name).expect(row),
        row.find("opus-5-5").expect(row),
    );
    assert!(n < m, "{row}");
    assert!(m < row.find("tok").unwrap_or(usize::MAX), "{row}");
    // a narrow list leaves it to the preview
    let s = render(&mut d, 90, 30);
    assert!(
        !s.lines()
            .any(|l| l.contains(&name) && l.contains("opus-5-5")),
        "{s}"
    );
}

#[test]
fn a_session_that_ended_leaves_focus_and_tile() {
    let keys = || vec!["b31|w1:p1".to_string(), "b31|w1:p2".to_string()];
    let mut d = dash_own();
    let _ = render(&mut d, 160, 40);
    wall::tile(&mut d, keys(), View::Fleet);
    let _ = render(&mut d, 160, 40);
    wall::drop_ended(&mut d);
    assert_eq!(d.wall.places().len(), 2);
    // one ends: its tile goes, the other stays
    d.fleet
        .host_mut("b31")
        .unwrap()
        .sessions
        .retain(|s| s.pane_id != "w1:p2");
    wall::drop_ended(&mut d);
    let _ = render(&mut d, 160, 40);
    assert_eq!(d.wall.places().len(), 1);
    assert_eq!(d.view, View::Wall);
    // the last one: back where you came from
    d.fleet
        .host_mut("b31")
        .unwrap()
        .sessions
        .retain(|s| s.pane_id != "w1:p1");
    wall::drop_ended(&mut d);
    assert_eq!(d.view, View::Fleet);

    // close.exited = keep: it stays, saying the session ended
    let mut d = dash_own();
    let _ = d.cfg.set("close.exited", "keep");
    let _ = render(&mut d, 160, 40);
    wall::focus_on(&mut d, "b31|w1:p1", View::Fleet);
    wall::drop_ended(&mut d);
    d.fleet
        .host_mut("b31")
        .unwrap()
        .sessions
        .retain(|s| s.pane_id != "w1:p1");
    wall::drop_ended(&mut d);
    assert_eq!(d.view, View::Wall);
    assert!(render(&mut d, 160, 40).contains("this session ended"));
}

#[test]
fn a_rows_lead_is_as_wide_as_the_axis_assumes() {
    let d = dash();
    for s in d.fleet.sessions() {
        for (guide, mw) in [("├─", 0), ("└─", 0), ("├─", 14)] {
            let w: usize = fleet::lead(&d, s, (12, mw, 18), guide)
                .iter()
                .map(|x| ui::width(&x.content))
                .sum();
            assert_eq!(w, fleet::lead_w((12, mw, 18)), "{}", s.display());
        }
    }
}

#[test]
fn fleet_timeline_rows_render_with_an_axis() {
    for (w, h) in [(120, 32), (200, 50)] {
        let mut d = dash_with_history(&["fleet.timeline=true"]);
        let s = render(&mut d, w, h);
        assert!(s.contains("timeline 6h"), "{w}x{h}: {s}");
        // the axis: hours, under its label
        assert!(s.contains("last 6h") && s.contains(":00"), "{w}x{h}: {s}");
        let row = row_of(&s, "claude_ignite");
        // working and asking (full height), done (half)
        assert!(row.contains("██") && row.contains("▄▄"), "{w}x{h}: {row}");
        // the folder and title make way for it
        assert!(!row.contains("/home/u/ignite"), "{w}x{h}: {row}");
    }
    // g turns it off again (the rows' folders come back)
    let mut d = dash_with_history(&["fleet.timeline=true"]);
    d.on_key(k(KeyCode::Char('g')));
    assert!(!d.cfg.flag("fleet.timeline"));
    let s = render(&mut d, 200, 50);
    assert!(!s.contains("timeline 6h"), "{s}");
    assert!(s.contains("/home/u/ignite"), "{s}");
}

#[test]
fn fleet_timeline_keeps_the_layout_keys() {
    let mut d = dash_with_history(&["fleet.timeline=true"]);
    // the preview has a strip of the selected session's states either way
    let s = render(&mut d, 200, 50);
    assert!(s.contains("last 6h  "), "{s}");
    // z maximises the list (the side goes): the board, each session's
    // timeline in a column of its own
    let row_w = |s: &str| {
        s.lines()
            .find(|l| l.contains("claude_ignite") && (l.contains('├') || l.contains('└')))
            .map_or(0, |l| l.matches(['█', '▄', '▁']).count())
    };
    d.on_key(k(KeyCode::Char('z')));
    assert_eq!(
        super::panes::shape(&d, "fleet").max,
        super::panes::Max::Main
    );
    let s = render(&mut d, 200, 50);
    assert!(
        d.fb.on && s.contains("Machines & sessions ·") && !s.contains("recent output"),
        "{s}"
    );
    assert!(row_w(&s) > 0, "{s}");
    // = puts it back; p hides the preview
    d.on_key(k(KeyCode::Char('=')));
    d.on_key(k(KeyCode::Char('p')));
    let s = render(&mut d, 120, 32);
    assert!(
        s.contains("timeline 6h") && !s.contains("recent output"),
        "{s}"
    );
}

#[test]
fn monitor_timeline_renders_every_session() {
    use super::monitor_draw::Sub;
    for (w, h) in [(120, 32), (200, 50)] {
        let mut d = dash_with_history(&[]);
        d.view = View::Monitor;
        d.mon.sub = Sub::Timeline;
        let s = render(&mut d, w, h);
        assert!(s.contains("Timeline · last 6h"), "{w}x{h}: {s}");
        // the legend, the axis, a heading per machine
        assert!(
            s.contains("needs you") && s.contains("at limit"),
            "{w}x{h}: {s}"
        );
        assert!(s.contains(":00") && s.contains("● b31"), "{w}x{h}: {s}");
        // what each did: ignite worked 2h (and the last 10 minutes), asked once
        let row = s.lines().find(|l| l.contains("claude_ignite")).unwrap();
        assert!(row.contains("2h 10m busy · ▲ 1"), "{w}x{h}: {row}");
        // the ended one, dimmed
        assert!(s.contains("○ ✻ drift"), "{w}x{h}: {s}");
    }
    let mut d = dash_with_history(&[]);
    d.view = View::Monitor;
    d.mon.sub = Sub::Timeline;
    // e hides the ended ones; ↓ and enter open the selected one in Focus
    d.on_key(k(KeyCode::Char('e')));
    let s = render(&mut d, 120, 32);
    assert!(!s.contains("drift"), "{s}");
    d.on_key(k(KeyCode::Down));
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Wall);
    // t jumps to it from another sub-view; + shows more hours
    d.switch(View::Monitor);
    d.mon.sub = Sub::Cpu;
    d.on_key(k(KeyCode::Char('t')));
    assert_eq!(d.mon.sub, Sub::Timeline);
    d.on_key(k(KeyCode::Char('+')));
    assert_eq!(super::timechart::hours(&d.cfg), 12);
    let s = render(&mut d, 120, 32);
    assert!(s.contains("last 12h"), "{s}");
}

#[test]
fn row_bars_show_tokens_and_context_and_keep_the_columns() {
    use crate::usage::dash::{Data, Live};
    let now = crate::cache::now();
    let mut d = dash();
    d.fleet.host_mut("b31").unwrap().sessions[0].native_id = Some("s1".into());
    let mut data = Data::default();
    data.live.insert(
        "s1".into(),
        Live {
            tokens: 400_000,
            ctx: Some(150_000),
            window: Some(200_000),
            recent: vec![
                (now - 20 * 60, 30_000),
                (now - 120, 200_000),
                (now - 30, 150_000),
            ],
            ..Default::default()
        },
    );
    d.usage.data = Some(data);
    let s = render(&mut d, 200, 50);
    let ignite = row_of(&s, "claude_ignite");
    let temper = row_of(&s, "claude_temper");
    // the sparkline (quiet, then busy at its end); the meter three-quarters full
    assert!(ignite.contains("▁") && ignite.contains("███▊"), "{ignite}");
    // a session without numbers keeps the room: the folders line up
    let col = |l: &str, w: &str| l.find(w).map(|i| ui::width(&l[..i]));
    assert!(col(ignite, "/home/u/ignite").is_some(), "{ignite}");
    assert_eq!(
        col(ignite, "/home/u/ignite"),
        col(temper, "/home/u/temper"),
        "{ignite}\n{temper}"
    );
    // a narrow list leaves the bars out for every row
    let s = render(&mut d, 120, 32);
    let ignite = row_of(&s, "claude_ignite");
    assert!(!ignite.contains("███▊"), "{ignite}");
    // ui.row_bars = off: none at any width
    d.cfg = Config::in_memory_own(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "ui.row_bars=off",
    ]);
    let s = render(&mut d, 200, 50);
    let ignite = row_of(&s, "claude_ignite");
    assert!(!ignite.contains("███▊"), "{ignite}");
}

#[test]
fn motion_breathes_where_you_are_needed_and_ticks_a_finish() {
    // the cell of temper's ▲ in its row
    let glyph = |d: &mut Dash| {
        let b = render_buf(d, 200, 50);
        for y in 0..50 {
            let line: Vec<String> = (0..200).map(|x| b[(x, y)].symbol().to_string()).collect();
            let text: String = line.concat();
            if text.contains("claude_temper") && (text.contains("├─") || text.contains("└─"))
            {
                let x = line.iter().position(|c| c == "▲").unwrap() as u16;
                let cell = &b[(x, y)];
                return (cell.fg, cell.bg);
            }
        }
        panic!("no temper row");
    };
    let mut d = dash();
    // at two moments of a breath
    d.t0 = Instant::now();
    let (full, bg) = glyph(&mut d);
    d.t0 = Instant::now() - Duration::from_millis(1200);
    let (half, _) = glyph(&mut d);
    assert_ne!(full, half, "it breathes");
    // only the text's colour moves: never a background
    assert_eq!(bg, Color::Reset);
    // reduced: still
    d.cfg = Config::in_memory_own(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "ui.motion=reduced",
    ]);
    d.t0 = Instant::now();
    let (a, _) = glyph(&mut d);
    d.t0 = Instant::now() - Duration::from_millis(1200);
    let (b, _) = glyph(&mut d);
    assert_eq!(a, b);
    // a session that finished gets the tick (reduced keeps it)
    let weave = d.fleet.host("gpu2").unwrap().sessions[0].clone();
    let tr = crate::fleet::Transition {
        session: weave.clone(),
        from: "working".into(),
        took: 60,
    };
    super::motion::on_transitions(&mut d, std::slice::from_ref(&tr), &[]);
    for (w, h) in [(120, 32), (200, 50)] {
        let s = render(&mut d, w, h);
        let row = row_of(&s, "claude_weave");
        assert!(row.contains("✓ finished"), "{w}x{h}: {row}");
    }
    // a machine's first poll isn't news; off has no tick at all
    let mut d = dash();
    super::motion::on_transitions(&mut d, std::slice::from_ref(&tr), &["gpu2".into()]);
    assert!(super::motion::ticking(&d, &weave).is_none());
    d.cfg = Config::in_memory_own(&[
        "hosts=b31,gpu2",
        "stats=off",
        "local=false",
        "ui.motion=off",
    ]);
    super::motion::on_transitions(&mut d, std::slice::from_ref(&tr), &[]);
    let s = render(&mut d, 200, 50);
    let row = row_of(&s, "claude_weave");
    assert!(!row.contains("finished") && row.contains("done"), "{row}");
}

#[test]
fn alerts_popup_lists_what_you_missed_and_the_last_toasts() {
    use crate::events::{Event, Kind};
    let ctrl_n = KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL);
    let now = crate::cache::now();
    let mut d = dash();
    // ctrl+n: in every view (the Inbox reads its own events again: first)
    for v in [View::Inbox, View::Monitor, View::Settings, View::Fleet] {
        d.switch(v);
        d.on_key(ctrl_n);
        assert!(matches!(d.mode, Mode::Alerts(_)), "{v:?}");
        d.on_key(k(KeyCode::Esc));
        assert!(matches!(d.mode, Mode::Normal));
    }
    inbox::set_rows(
        &mut d,
        vec![Event {
            t: now - 120,
            host: "b31".into(),
            name: "temper".into(),
            kind: Kind::Blocked,
            title: "Run tests?".into(),
            agent: "claude".into(),
            ..Default::default()
        }],
    );
    d.toast(Level::Warn, "gpu2 is slow to answer");
    d.note("copied ignite's recent output");
    d.on_key(ctrl_n);
    for (w, h) in [(120, 32), (200, 50)] {
        let s = render(&mut d, w, h);
        assert!(s.contains("Alerts") && s.contains("ctrl+n"), "{w}x{h}: {s}");
        assert!(
            s.contains("Missed") && s.contains("since you last opened the Inbox"),
            "{w}x{h}: {s}"
        );
        assert!(
            s.contains("needs you") && s.contains("temper · b31"),
            "{w}x{h}: {s}"
        );
        assert!(
            s.contains("Toasts") && s.contains("gpu2 is slow to answer"),
            "{w}x{h}: {s}"
        );
        assert!(
            s.contains("warning") && s.contains("copied ignite"),
            "{w}x{h}: {s}"
        );
    }
    // enter on the missed alert: its session's row in Fleet
    d.on_key(k(KeyCode::Esc));
    d.switch(View::Monitor);
    d.on_key(ctrl_n);
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.view, View::Fleet);
    assert_eq!(
        fleet::selected(&d),
        Some(fleet::RowId::Sess("b31|w1:p2".into()))
    );
    // r marks the missed ones read
    d.on_key(ctrl_n);
    d.on_key(k(KeyCode::Char('r')));
    assert_eq!(inbox::unread(&d), 0);
    let s = render(&mut d, 120, 32);
    assert!(s.contains("you've seen every alert"), "{s}");
}

#[test]
fn the_alerts_key_is_in_the_key_map_help_and_palette() {
    // rebinding it moves it
    let keys = Keys::from_config(&Config::in_memory(&["key.alerts=ctrl+e"]));
    assert_eq!(keys.owner("ctrl+e"), Some(Act::Alerts));
    assert_eq!(keys.owner("ctrl+n"), None);
    let mut d = dash();
    // help: "toast" finds it, under Everywhere; "timeline" finds g
    let found = help::found(&d, "toast history");
    let top = found.first().unwrap();
    assert_eq!((top.keys.as_str(), top.section), ("ctrl+n", "Everywhere"));
    assert!(help::found(&d, "timeline").iter().any(|e| e.keys == "g"));
    // the palette: the alerts, Monitor's timeline
    let mut p = palette::Palette::new(&d);
    for ch in "missed and recent toasts".chars() {
        p.key(k(KeyCode::Char(ch)));
    }
    assert!(matches!(p.chosen(), Some(palette::Item::Alerts)));
    palette::run(&mut d, p);
    assert!(matches!(d.mode, Mode::Alerts(_)));
    d.mode = Mode::Normal;
    let mut p = palette::Palette::new(&d);
    for ch in "timeline of every session".chars() {
        p.key(k(KeyCode::Char(ch)));
    }
    palette::run(&mut d, p);
    assert_eq!(d.view, View::Monitor);
    assert_eq!(d.mon.sub, monitor_draw::Sub::Timeline);
}

#[test]
fn marked_sessions_get_an_action_bar_and_act_together() {
    let mut d = dash();
    // nothing marked: the usual hints
    let s = render(&mut d, 160, 36);
    assert!(!s.contains("marked"), "{s}");
    d.on_key(k(KeyCode::Char(' '))); // ignite (working)
    d.on_key(k(KeyCode::Char(' '))); // temper (asking)
    let s = render(&mut d, 160, 36);
    let bar = s.lines().last().unwrap().to_string();
    for w in [
        "◉ 2 marked",
        "y  approve 1",
        "s  send",
        "Q  queue",
        "x  close",
        "esc  clear",
    ] {
        assert!(bar.contains(w), "{w}: {bar}");
    }
    // narrow: still the count, approve, close and esc (queue and send go first)
    let s = render(&mut d, 60, 30);
    let bar = s.lines().last().unwrap().to_string();
    assert!(
        bar.contains("◉ 2 marked")
            && bar.contains("approve")
            && bar.contains("close")
            && bar.contains("esc")
            && !bar.contains("queue"),
        "{bar}"
    );
    // s: one prompt for both (what b does with marks too)
    d.on_key(k(KeyCode::Char('s')));
    match &d.mode {
        Mode::Input(i) => {
            assert_eq!(i.title, "Send to 2 sessions");
            assert!(matches!(&i.purpose, forms::For::Send(v) if v.len() == 2));
        }
        _ => panic!("no send form"),
    }
    // tab in it queues the prompt for each instead
    for ch in "rerun".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let Mode::Input(mut i) = std::mem::replace(&mut d.mode, Mode::Normal) else {
        panic!()
    };
    assert!(matches!(i.key(k(KeyCode::Tab)), forms::Res::Submit));
    assert!(matches!(&i.purpose, forms::For::QueueEach(v) if v.len() == 2));
    d.on_key(k(KeyCode::Char('b')));
    assert!(
        matches!(&d.mode, Mode::Input(i) if matches!(&i.purpose, forms::For::Send(v) if v.len() == 2))
    );
    d.mode = Mode::Normal;
    // Q: the task queued for each
    d.on_key(k(KeyCode::Char('Q')));
    match &d.mode {
        Mode::Input(i) => {
            assert_eq!(i.title, "Queue for 2 sessions");
            assert!(matches!(&i.purpose, forms::For::QueueEach(v) if v.len() == 2));
        }
        _ => panic!("no queue form"),
    }
    d.mode = Mode::Normal;
    // y: only the one asking is approved; the toast says so first
    d.on_key(k(KeyCode::Char('y')));
    assert!(
        d.current_toast()
            .is_some_and(|t| t.text == "approving 1 session…"),
        "{:?}",
        d.current_toast().map(|t| t.text.clone())
    );
    // x: one confirm for both, which says there's a grace
    d.on_key(k(KeyCode::Char('x')));
    match &d.mode {
        Mode::Confirm {
            title,
            text,
            on_yes: Pending::Close(v),
        } => {
            assert_eq!(title, "Close 2 sessions?");
            assert!(
                text.contains("closes in 5s") && text.contains("u keeps it"),
                "{text}"
            );
            assert_eq!(v.len(), 2);
        }
        _ => panic!("no confirm"),
    }
    d.mode = Mode::Normal;
    // none asking: y says so and does nothing
    d.fv.marked.clear();
    d.fv.marked.insert("b31|w1:p1".into());
    d.on_key(k(KeyCode::Char('y')));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text == "the marked session isn't asking anything"));
    // esc clears the marks, and the bar goes
    d.on_key(k(KeyCode::Esc));
    assert!(d.fv.marked.is_empty());
    assert!(batch::action_bar(&d, 160).is_none());
    // help finds them by what people type
    assert!(help::found(&d, "approve all marked")
        .iter()
        .any(|e| e.keys == "space y"));
    assert!(help::found(&d, "bulk close")
        .iter()
        .any(|e| e.keys == "space x"));
}

/// The one test that writes closing.json (tests share a state folder).
#[test]
fn a_close_waits_its_grace_greyed_out_and_u_keeps_it() {
    let mut d = dash();
    // a session of its own: other tests' fleets don't have it
    let zest = Session {
        label: "zest".into(),
        ..sess("b31", "w1:p77", "zest", "working")
    };
    d.fleet.host_mut("b31").unwrap().sessions.push(zest.clone());
    d.fv.sel = Some(fleet::RowId::Sess(zest.key()));
    d.accept(Pending::Close(vec![zest.clone()]));
    assert!(
        d.current_toast()
            .is_some_and(|t| t.text == "closing zest in 5s · u undo"),
        "{:?}",
        d.current_toast().map(|t| t.text.clone())
    );
    // on disk, for whoever runs the hub; the agent is untouched until then
    let e = crate::closing::pending("b31", "w1:p77").expect("pending");
    assert_eq!((e.by.as_str(), e.worktree), ("dashboard", false));
    assert!(d.fleet.find(&zest.key()).is_some());
    let s = render(&mut d, 160, 36);
    // (the list's half of the screen: the preview is on the right)
    let row = s
        .lines()
        .map(|l| l.chars().take(90).collect::<String>())
        .find(|l| l.contains("zest") && !l.contains('╭'))
        .unwrap_or_else(|| panic!("{s}"));
    assert!(
        (row.contains("closing in 5s") || row.contains("closing in 4s")) && row.contains("u undo"),
        "{row}\n{s}"
    );
    assert!(!row.contains("task of zest"), "its title gives way: {row}");
    // (the toast has the bar for now: the hints under it)
    assert!(
        fleet::hints(&d)
            .first()
            .is_some_and(|h| h.0 == "u" && h.1.contains("keep it")),
        "{:?}",
        fleet::hints(&d)
    );
    // the palette offers to keep it too
    let mut p = palette::Palette::new(&d);
    for ch in "undo close".chars() {
        p.key(k(KeyCode::Char(ch)));
    }
    assert!(matches!(p.chosen(), Some(palette::Item::Undo)));
    // u on it: kept, and the row is back
    d.on_key(k(KeyCode::Char('u')));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text == "kept zest — it won't close"));
    assert!(crate::closing::pending("b31", "w1:p77").is_none());
    let s = render(&mut d, 160, 36);
    assert!(!s.contains("closing in"), "{s}");
    d.on_key(k(KeyCode::Char('u')));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text == "nothing is closing"));
    // closed from another view (the Inbox's u marks read): u keeps it there
    // too, while its grace lasts
    d.accept(Pending::Close(vec![zest.clone()]));
    d.view = View::Inbox;
    d.on_key(k(KeyCode::Char('u')));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text == "kept zest — it won't close"));
    // closing it again sets its deadline again (one entry)
    d.view = View::Fleet;
    d.accept(Pending::Close(vec![zest.clone()]));
    let first = crate::closing::pending("b31", "w1:p77").unwrap().deadline;
    std::thread::sleep(std::time::Duration::from_millis(5));
    d.accept(Pending::Close(vec![zest.clone()]));
    let v: Vec<_> = crate::closing::load()
        .into_iter()
        .filter(|e| e.pane == "w1:p77")
        .collect();
    assert_eq!(v.len(), 1);
    assert!(v[0].deadline > first);
    // u elsewhere in the list keeps the last close
    d.fv.sel = Some(fleet::RowId::Sess("b31|w1:p1".into()));
    d.on_key(k(KeyCode::Char('u')));
    assert!(crate::closing::pending("b31", "w1:p77").is_none());
    // close.grace = 0: no grace, nothing written
    d.cfg = Config::in_memory(&["hosts=b31,gpu2", "close.grace=0"]);
    d.accept(Pending::Close(vec![zest]));
    assert!(crate::closing::pending("b31", "w1:p77").is_none());
}

#[test]
fn x_on_an_agent_outside_herdr_asks_before_stopping_it() {
    let mut d = dash();
    d.cfg.set("stats", "on").unwrap();
    let st = crate::telemetry::Stats {
        agent_list: vec![crate::telemetry::AgentProc {
            agent: "claude".into(),
            pid: "4242".into(),
            in_herdr: false,
            ..Default::default()
        }],
        ..Default::default()
    };
    d.tele
        .insert("b31".into(), crate::telemetry::Telemetry::fixed("b31", st));
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("outside herdr") || s.contains("4242") || s.contains("Claude Code"),
        "{s}"
    );
    d.fv.sel = Some(fleet::RowId::Outside("b31|4242".into()));
    d.on_key(k(KeyCode::Char('x')));
    match &d.mode {
        Mode::Confirm { title, text, .. } => {
            assert_eq!(title, "Stop this agent?");
            assert!(
                text.contains("pid 4242") && text.contains("outside herdr"),
                "{text}"
            );
        }
        _ => panic!("x on an outside agent asks first"),
    }
    // n: nothing stopped
    d.on_key(k(KeyCode::Char('n')));
    assert!(matches!(d.mode, Mode::Normal));
    // enter: reopen it in herdr — what will run, before a yes
    d.on_key(k(KeyCode::Enter));
    match &d.mode {
        Mode::Confirm { title, text, .. } => {
            assert_eq!(title, "Reopen it in herdr?");
            assert!(
                text.contains("pid 4242") && text.contains("claude --continue"),
                "{text}"
            );
        }
        _ => panic!("enter on an outside agent asks to reopen it"),
    }
    d.on_key(k(KeyCode::Char('n')));
    assert!(matches!(d.mode, Mode::Normal));
}

#[test]
fn the_journal_reads_like_notes() {
    let mut d = dash();
    let today = crate::ui::local_day(crate::cache::now());
    let md = "# Journal · Saturday 24 October 2026\n\n\
_Updated 14:32 · 1 session on 1 machine · worked 44m · 353k tok · $1.50_\n\n\
> Prompts are the ones sent through Aleph. Text typed straight into an agent in herdr isn't seen here.\n\n\
## ignite · b31 · Claude Code\n\n\
`~/work/ivy` · worked 44m · 353k tok · $1.50 · 2 turns\n\n\
**Prompts**\n\n\
- 09:12 · Fix the flaky test in tests/net.rs\n\n\
**Files** · 1 commit · 2 files · +40 −3\n\n\
- `a1b2c3d` 09:30 Fix flaky port reuse — 2 files, +40 −3\n\n\
<!-- aleph journal: updated 1 -->\n";
    d.mode = Mode::Journal(journal::JournalView::with(
        today,
        journal::Text::Saved(md.into()),
    ));
    let s = render(&mut d, 120, 34);
    assert!(s.contains(" journal ") && s.contains("today"), "{s}");
    assert!(s.contains("ℵ Journal · Saturday 24 October 2026"), "{s}");
    assert!(s.contains("┃ ignite · b31 · Claude Code"), "{s}");
    assert!(
        s.contains("• 09:12 · Fix the flaky test in tests/net.rs"),
        "{s}"
    );
    assert!(s.contains("• a1b2c3d 09:30 Fix flaky port reuse"), "{s}");
    assert!(
        s.contains("│ Prompts are the ones sent through Aleph"),
        "{s}"
    );
    assert!(!s.contains("<!--") && !s.contains("**"), "{s}");
    // ← the day before: no file yet, so it's being put together
    d.on_key(k(KeyCode::Left));
    let s = render(&mut d, 120, 34);
    assert!(
        s.contains("yesterday") && s.contains("putting the day together"),
        "{s}"
    );
    // esc closes it
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
    // the palette lists it; the Inbox's J opens it
    d.on_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
    for ch in "journal".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    let s = render(&mut d, 120, 34);
    assert!(
        s.contains("Journal: today") && s.contains("Journal: yesterday"),
        "{s}"
    );
    d.on_key(k(KeyCode::Esc));
    d.mode = Mode::Normal;
    d.view = View::Inbox;
    d.on_key(KeyEvent::new(KeyCode::Char('J'), KeyModifiers::SHIFT));
    assert!(matches!(d.mode, Mode::Journal(_)));
}

#[test]
fn inbox_says_what_a_finished_turn_took() {
    let mut d = dash_own();
    // finished turns listed, as they were before inbox.finished
    let _ = d.cfg.set("inbox.finished", "all");
    let now = crate::cache::now();
    use crate::events::{Event, Kind};
    let done = Event {
        t: now - 90,
        host: "gpu2".into(),
        name: "weave".into(),
        kind: Kind::Done,
        title: "Fix the flaky test".into(),
        agent: "claude".into(),
        took: 360,
        native: "5f0c-uuid".into(),
        ..Default::default()
    };
    inbox::set_rows(&mut d, vec![done.clone()]);
    d.view = View::Inbox;
    d.on_key(k(KeyCode::Char('/')));
    for ch in "weave".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Enter));
    // not worked out yet: nothing said (never a made-up number)
    let s = render(&mut d, 160, 44);
    assert!(!s.contains("this turn"), "{s}");
    // worked out, but the machine's usage not read since: so far, and it says so
    let turn = crate::turncost::Turn {
        tokens: 412_000,
        cost: 1.2,
        secs: 360,
        requests: 14,
        complete: false,
        ..Default::default()
    };
    d.inbox.turns.put(&done, turn.clone());
    let s = render(&mut d, 160, 44);
    assert!(
        s.contains("this turn") && s.contains("412k tok · $1.20 · 6 min"),
        "{s}"
    );
    assert!(s.contains("reading…"), "{s}");
    // all in, an estimate: marked
    d.inbox.turns.put(
        &done,
        crate::turncost::Turn {
            complete: true,
            estimated: true,
            ..turn
        },
    );
    let s = render(&mut d, 160, 44);
    assert!(s.contains("$1.20*") && !s.contains("reading…"), "{s}");
    // Fleet's preview: the session's last turn
    d.view = View::Fleet;
    d.fv.sel = Some(fleet::RowId::Sess("gpu2|w1:p4".into()));
    let s = render(&mut d, 160, 44);
    assert!(
        s.contains("turn") && s.contains("412k tok · $1.20* · 6 min") && s.contains("1m ago"),
        "{s}"
    );
}

#[test]
fn typing_into_a_pane_starts_from_fleet_and_is_easy_to_leave() {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    // I in Fleet: Focus on the session, typing at once
    d.on_key(k(KeyCode::Char('j')));
    d.on_key(k(KeyCode::Char('I')));
    assert!(d.wall.typing, "I types straight into it");
    let t = render(&mut d, 120, 30);
    assert!(
        t.contains("ctrl+q") && t.contains("leave") && t.contains("Aleph's keys"),
        "{t}"
    );
    // ctrl+q leaves Focus at once, typing or not (esc would interrupt the agent)
    d.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(!d.wall.typing);
    assert_eq!(d.view, View::Fleet, "back where it came from");
    d.on_key(k(KeyCode::Char('I')));
    assert!(d.wall.typing && d.view == View::Wall);
    // esc while typing is the agent's: Focus stays, keys still go to it
    d.on_key(k(KeyCode::Esc));
    assert!(d.wall.typing && d.view == View::Wall);
    // ctrl+] gives the keys back to Aleph and stays in Focus
    d.on_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL));
    assert!(!d.wall.typing && d.view == View::Wall);
    // ctrl+q from Aleph's keys leaves too
    d.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert_eq!(d.view, View::Fleet);
    d.on_key(k(KeyCode::Char('I')));
    // a click outside the pane stops it too
    d.on_key(k(KeyCode::Char('I')));
    assert!(d.wall.typing);
    let _ = render(&mut d, 120, 30);
    wall::mouse(
        &mut d,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(!d.wall.typing, "a click outside the pane stops typing");
}

#[test]
fn the_palette_opens_monitor_pages_and_each_machines_own() {
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    let pick = |d: &Dash, q: &str| {
        let mut p = palette::Palette::new(d);
        for ch in q.chars() {
            p.key(k(KeyCode::Char(ch)));
        }
        p
    };
    // "usage" opens Monitor › Usage
    let p = pick(&d, "usage");
    assert!(
        matches!(
            p.chosen(),
            Some(palette::Item::Page(None, monitor_draw::Sub::Usage))
        ),
        "{:?}",
        p.chosen()
    );
    palette::run(&mut d, p);
    assert_eq!(
        (d.view, d.mon.sub),
        (View::Monitor, monitor_draw::Sub::Usage)
    );
    // a machine's name lists its pages: "gpu2 gpu" opens gpu2's GPU page
    d.switch(View::Fleet);
    let p = pick(&d, "gpu2 gpu");
    assert!(
        matches!(p.chosen(), Some(palette::Item::Page(Some(ref h), monitor_draw::Sub::Gpu)) if h == "gpu2"),
        "{:?}",
        p.chosen()
    );
    palette::run(&mut d, p);
    assert_eq!((d.view, d.mon.sub), (View::Monitor, monitor_draw::Sub::Gpu));
    assert_eq!(d.fleet.hosts[d.mon.sel].alias, "gpu2");
    let p = pick(&d, "b31 proc");
    assert!(
        matches!(p.chosen(), Some(palette::Item::Page(Some(ref h), monitor_draw::Sub::Procs)) if h == "b31")
    );
}

/// A full-screen app's screen (OpenCode's: a centred logo and input box, a
/// footer across the width) in a narrower preview: fitted, never wrapped —
/// the logo's lines stay one block, the footer keeps both ends.
#[test]
fn a_full_screen_apps_preview_is_fitted_not_wrapped() {
    let pad = |n: usize| " ".repeat(n);
    let screen: Vec<Line<'static>> = [
        format!("{}█▀▀█ █▀▀█ █▀▀ █▀▀▄", pad(50)),
        format!("{}█▄▄█ █▄▄█ █▀▀ █  █", pad(50)),
        String::new(),
        format!("{}┃ Ask anything…{}┃", pad(40), pad(30)),
        format!("{}┃ Build · gpt-5{}┃", pad(40), pad(30)),
        format!(
            "~/work/Projects/claude-herdr:main{}shift+tab agents   ctrl+p commands",
            pad(60)
        ),
    ]
    .into_iter()
    .map(Line::raw)
    .collect();
    assert!(fleet::screen_like(&screen));
    let fit: Vec<String> = screen
        .iter()
        .map(|l| crate::ansi::plain(&fleet::fit_screen_line(l, 60)))
        .collect();
    assert!(fit.iter().all(|l| crate::ui::width(l) <= 60), "{fit:#?}");
    // the logo: one block, its two lines still aligned
    let at = |l: &str| l.len() - l.trim_start().len();
    assert_eq!(at(&fit[0]), at(&fit[1]));
    assert!(fit[0].contains("█▀▀█ █▀▀█"));
    // the footer: its gap closed, both ends there
    assert!(
        fit[5].starts_with("~/work/Projects/claude-herdr:main  shift+tab"),
        "{}",
        fit[5]
    );
    // plain output isn't a screen: it wraps as before
    let prose: Vec<Line<'static>> = (0..6)
        .map(|i| Line::raw(format!("⏺ step {i}: ran the tests, 41 passed")))
        .collect();
    assert!(!fleet::screen_like(&prose));
}

#[test]
fn the_approval_board_walks_a_form_page_by_page() {
    let page = |n: u32| {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/ask/claude-form-{n}.txt"));
        std::fs::read_to_string(p).unwrap()
    };
    let mut d = dash();
    let _ = render(&mut d, 170, 40);
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    super::approvals::read_back(&mut d, "b31|w1:p2", &page(1));
    let s = render(&mut d, 170, 40);
    assert!(
        s.contains("Question 1 of 2") && s.contains("☐ Colour"),
        "{s}"
    );
    assert!(s.contains("Red") && s.contains("Blue"), "{s}");
    // Y (approve all) never answers a form: nothing goes on its way
    d.on_key(k(KeyCode::Char('Y')));
    d.mode = Mode::Approvals;
    assert!(!render(&mut d, 170, 40).contains("waiting for it to move on"));
    // an answer: on its way, until the next page's read shows it landed
    d.on_key(k(KeyCode::Char('1')));
    assert!(render(&mut d, 170, 40).contains("waiting for it to move on"));
    super::approvals::read_back(&mut d, "b31|w1:p2", &page(2));
    let s = render(&mut d, 170, 40);
    assert!(!s.contains("waiting for it to move on"), "{s}");
    assert!(
        s.contains("Question 2 of 2") && s.contains("☒ Colour"),
        "{s}"
    );
    assert!(s.contains("☐ Small") && s.contains("next"), "{s}");
    // a tick is the same question, and its answer has landed too
    d.on_key(k(KeyCode::Char('1')));
    super::approvals::read_back(&mut d, "b31|w1:p2", &page(3));
    let s = render(&mut d, 170, 40);
    assert!(
        s.contains("☒ Small") && !s.contains("waiting for it to move on"),
        "{s}"
    );
    // ⇥: on to the review page, where y sends
    d.on_key(k(KeyCode::Tab));
    super::approvals::read_back(&mut d, "b31|w1:p2", &page(4));
    let s = render(&mut d, 170, 40);
    assert!(
        s.contains("Review your answers") && s.contains("Submit answers"),
        "{s}"
    );
}

/// ctrl+a from any view: every agent waiting, its question as it shows it,
/// its options to answer; cards on v; a question on the deny list only at
/// the terminal; one no longer asked is marked, then leaves.
#[test]
fn the_approval_board_shows_each_question_and_answers_it() {
    let mut d = dash();
    let _ = render(&mut d, 150, 40);
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Approvals));
    super::approvals::read_back(
        &mut d,
        "b31|w1:p2",
        "⏺ Bash(pytest -x)\nBash command\n  pytest -x tests/\nDo you want to proceed?\n❯ 1. Yes\n  2. Yes, and don't ask again for pytest\n  3. No, and tell Claude what to do differently (esc)",
    );
    let s = render(&mut d, 150, 40);
    assert!(s.contains("Approvals · 1 waiting"), "{s}");
    assert!(
        s.contains("pytest -x tests/") && s.contains("Do you want to proceed?"),
        "{s}"
    );
    assert!(
        s.contains("approve all") && s.contains("Yes, and don't ask again"),
        "{s}"
    );
    // cards, and back
    d.on_key(k(KeyCode::Char('v')));
    assert!(render(&mut d, 150, 40).contains("› 1 Yes"));
    d.on_key(k(KeyCode::Char('v')));
    // y: on its way (read again and checked before the keys go)
    d.on_key(k(KeyCode::Char('y')));
    assert!(matches!(d.mode, Mode::Approvals));
    assert!(render(&mut d, 150, 40).contains("approved — waiting for it to move on"));
    // it stops asking: gone from the board at once
    d.fleet.host_mut("b31").unwrap().sessions[1].status = "idle".into();
    super::approvals::pump(&mut d);
    let s = render(&mut d, 150, 40);
    assert!(
        s.contains("nothing waiting") && s.contains("Nothing is waiting for you"),
        "{s}"
    );
    // a question on the deny list: only at the terminal
    d.fleet.host_mut("b31").unwrap().sessions[1].status = "blocked".into();
    d.fleet.host_mut("b31").unwrap().sessions[1].seq = 7;
    super::approvals::read_back(
        &mut d,
        "b31|w1:p2",
        "Bash command\n  rm -rf build/\nDo you want to proceed?\n❯ 1. Yes\n  2. No (esc)",
    );
    let s = render(&mut d, 150, 40);
    assert!(s.contains("deny list"), "{s}");
    d.on_key(k(KeyCode::Char('y')));
    assert!(d
        .current_toast()
        .is_some_and(|t| t.text.contains("answer it at the terminal")));
    // esc closes it
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Normal));
}

#[test]
fn fleet_maximised_is_a_board_of_machines_and_sessions() {
    let mut d = dash_own();
    let _ = d.cfg.set("layout.fleet", "max top");
    let s = render(&mut d, 200, 50);
    assert!(d.fb.on, "{s}");
    assert!(s.contains("Machines & sessions · needs you first"), "{s}");
    // a band per machine, its sessions under it: the one that needs you first
    let at = |s: &str, name: &str| s.find(name).unwrap_or(usize::MAX);
    assert!(at(&s, "b31") < at(&s, "claude_temper"), "{s}");
    assert!(at(&s, "claude_temper") < at(&s, "claude_ignite"), "{s}");
    assert!(at(&s, "claude_ignite") < at(&s, "gpu2"), "{s}");
    // j walks the board's order; , changes it
    d.fv.sel = Some(fleet::RowId::Host("b31".into()));
    d.on_key(k(KeyCode::Char('j')));
    assert_eq!(d.fv.sel, Some(fleet::RowId::Sess("b31|w1:p2".into())));
    d.on_key(k(KeyCode::Char(',')));
    assert_eq!(d.cfg.get_or("fleet.board_order"), "working");
    let s = render(&mut d, 200, 50);
    assert!(at(&s, "claude_ignite") < at(&s, "claude_temper"), "{s}");
    // the smallest it takes, and a very big screen
    for (w, h) in [(104, 28), (320, 90), (140, 30)] {
        let _ = render(&mut d, w, h);
        assert!(d.fb.on, "{w}×{h}");
    }
    // narrow: the list as before
    let _ = render(&mut d, 80, 30);
    assert!(!d.fb.on);
}

#[test]
fn the_board_marks_several_and_can_group_the_same_question() {
    let mut d = dash_own();
    let _ = render(&mut d, 160, 40);
    d.fleet.host_mut("b31").unwrap().sessions[0].status = "blocked".into();
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    let q = "Bash command\n  npm test\nDo you want to proceed?\n❯ 1. Yes\n  2. No (esc)";
    super::approvals::read_back(&mut d, "b31|w1:p1", q);
    super::approvals::read_back(&mut d, "b31|w1:p2", q);
    // each row: the agent, its session, the machine, how long; what it asks
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("2 waiting") && s.contains("Bash command · npm test"),
        "{s}"
    );
    assert!(
        s.contains("Claude Code · claude_ignite") && s.contains("b31"),
        "{s}"
    );
    // space marks a row and moves on; esc drops the marks, the board stays
    d.on_key(k(KeyCode::Char(' ')));
    assert!(render(&mut d, 160, 40).contains("1 marked"));
    d.on_key(k(KeyCode::Esc));
    assert!(matches!(d.mode, Mode::Approvals));
    assert!(!render(&mut d, 160, 40).contains("marked"));
    // mark both: y answers each
    d.on_key(k(KeyCode::Up));
    d.on_key(k(KeyCode::Char(' ')));
    d.on_key(k(KeyCode::Char(' ')));
    assert!(render(&mut d, 160, 40).contains("2 marked"));
    d.on_key(k(KeyCode::Char('y')));
    let s = render(&mut d, 160, 40);
    assert_eq!(
        s.matches("approved — waiting for it to move on").count(),
        2,
        "{s}"
    );
    // approvals.group = same: the two asking the same thing share a row
    let mut d = dash_own();
    let _ = d.cfg.set("approvals.group", "same");
    let _ = render(&mut d, 160, 40);
    d.fleet.host_mut("b31").unwrap().sessions[0].status = "blocked".into();
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    super::approvals::read_back(&mut d, "b31|w1:p1", q);
    super::approvals::read_back(&mut d, "b31|w1:p2", q);
    let s = render(&mut d, 160, 40);
    assert!(
        s.contains("2 agents") && !s.contains("claude_ignite  "),
        "{s}"
    );
    d.on_key(k(KeyCode::Char('y')));
    let s = render(&mut d, 160, 40);
    assert!(s.contains("approved — waiting for it to move on"), "{s}");
}

#[test]
fn the_board_answers_in_focus_and_comes_back() {
    let mut d = dash();
    let _ = render(&mut d, 150, 40);
    d.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
    super::approvals::read_back(
        &mut d,
        "b31|w1:p2",
        "Do you want to proceed?\n❯ 1. Yes\n  2. No (esc)",
    );
    assert!(render(&mut d, 150, 40).contains("answer in Focus"));
    // enter: Focus on it, typing into the agent, the board out of the way
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(d.mode, Mode::Normal));
    assert!(d.view == View::Wall && d.wall.in_focus() && d.wall.typing);
    // leaving Focus while it still asks: the board again
    d.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Approvals));
    // a click on a card does the same
    d.on_key(k(KeyCode::Char('v')));
    let _ = render(&mut d, 150, 40);
    d.on_mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: 10,
        row: 6,
        modifiers: KeyModifiers::NONE,
    });
    assert!(d.view == View::Wall && d.wall.in_focus() && matches!(d.mode, Mode::Normal));
    // answered: leaving Focus doesn't bring the board back
    d.fleet.host_mut("b31").unwrap().sessions[1].status = "idle".into();
    d.on_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL));
    assert!(matches!(d.mode, Mode::Normal));
}

/// ctrl+k "usage b31" / "usage claude": Monitor › Usage for one machine or
/// one agent, said in its scope; x shows everything again.
#[test]
fn the_palette_opens_usage_for_one_machine_or_agent() {
    let mut d = dash();
    let _ = render(&mut d, 140, 36);
    let pick = |d: &Dash, q: &str| {
        let mut p = palette::Palette::new(d);
        for ch in q.chars() {
            p.key(k(KeyCode::Char(ch)));
        }
        p
    };
    use super::usage::Only;
    let p = pick(&d, "usage b31");
    assert!(
        matches!(p.chosen(), Some(palette::Item::Usage(Some(Only::Host(h)))) if h == "b31"),
        "{:?}",
        p.chosen()
    );
    palette::run(&mut d, p);
    assert_eq!(
        (d.view, d.mon.sub),
        (View::Monitor, monitor_draw::Sub::Usage)
    );
    assert_eq!(d.usage.only, Some(Only::Host("b31".into())));
    let p = pick(&d, "usage claude");
    assert!(
        matches!(p.chosen(), Some(palette::Item::Usage(Some(Only::Agent(a)))) if a == "claude"),
        "{:?}",
        p.chosen()
    );
    palette::run(&mut d, p);
    assert_eq!(d.usage.only, Some(Only::Agent("claude".into())));
    // x: everything again
    d.on_key(k(KeyCode::Char('x')));
    assert_eq!(d.usage.only, None);
}

#[test]
fn the_inbox_counts_what_it_lists() {
    use crate::events::{Event, Kind};
    let now = crate::cache::now();
    let ev = |name: &str, kind, t, title: &str| Event {
        t,
        host: "b31".into(),
        name: name.into(),
        kind,
        title: title.into(),
        agent: "claude".into(),
        ..Default::default()
    };
    let rows = || {
        vec![
            ev("a", Kind::Blocked, now - 10, "Run tests?"),
            // finished turns, which inbox.finished = none leaves out of the list
            ev("b", Kind::Done, now - 20, "done"),
            ev("c", Kind::Done, now - 30, "done"),
            // a run of the same event is one row
            ev("d", Kind::Error, now - 40, "boom"),
            ev("d", Kind::Error, now - 50, "boom"),
            // never counted
            ev("e", Kind::New, now - 60, "started"),
        ]
    };
    let mut d = dash_own();
    d.inbox.seen = now - 1000;
    let _ = d.cfg.set("inbox.finished", "none");
    inbox::set_rows(&mut d, rows());
    assert_eq!(inbox::unread(&d), 2, "the question and the error");
    assert_eq!(inbox::unread_for(&d, "b31"), 2);
    assert_eq!(inbox::unread_for(&d, "gpu2"), 0);
    assert_eq!(inbox::missed(&d, 10).len(), 2);
    // listed: counted
    let _ = d.cfg.set("inbox.finished", "all");
    inbox::set_rows(&mut d, rows());
    assert_eq!(inbox::unread(&d), 4);
    // archived or read: gone from the count
    d.inbox.dismissed.insert(crate::events::key(&rows()[0]));
    assert_eq!(inbox::unread(&d), 3);
    d.inbox.seen = now;
    assert_eq!(inbox::unread(&d), 0);
}

#[test]
fn a_question_the_hub_archived_leaves_a_running_inbox() {
    use crate::events::{Event, Kind};
    let now = crate::cache::now();
    let e = Event {
        // far enough from any other test's rows in the shared log
        t: now - 77_777,
        host: "hub-archive-test".into(),
        name: "asked".into(),
        kind: Kind::Blocked,
        title: "Run tests?".into(),
        agent: "claude".into(),
        ..Default::default()
    };
    crate::events::append_all(std::slice::from_ref(&e));
    let mut d = dash_own();
    inbox::init(&mut d);
    d.inbox.seen = now - 100_000;
    assert_eq!(inbox::unread_for(&d, "hub-archive-test"), 1);
    // another process (the daemon's hub) archives it
    crate::events::dismiss_keys(&[crate::events::key(&e)]);
    inbox::look_again(&mut d);
    d.inbox.seen = now - 100_000;
    assert_eq!(inbox::unread_for(&d, "hub-archive-test"), 0);
}

#[test]
fn the_services_page_shows_only_while_looking_for_servers() {
    use super::monitor_draw::Sub;
    let mut d = dash_own();
    let _ = d.cfg.set("services", "off");
    d.view = View::Monitor;
    let _ = render(&mut d, 160, 40);
    assert!(!monitor::pages(&d).contains(&Sub::Services));
    assert!(!render(&mut d, 160, 40).contains("Services"));
    // its key, and stepping through the pages, pass it by
    d.mon.sub = Sub::Net;
    d.on_key(k(KeyCode::Char('s')));
    assert_eq!(d.mon.sub, Sub::Net);
    monitor::sub_step(&mut d, true);
    assert_eq!(d.mon.sub, Sub::Timeline);
    monitor::sub_step(&mut d, false);
    assert_eq!(d.mon.sub, Sub::Net);
    // the palette doesn't offer it either
    let p = palette::Palette::new(&d);
    assert!(!format!("{:?}", p.chosen()).contains("Services"));
    // on: all of it is back
    let _ = d.cfg.set("services", "on");
    assert!(monitor::pages(&d).contains(&Sub::Services));
    d.on_key(k(KeyCode::Char('s')));
    assert_eq!(d.mon.sub, Sub::Services);
    assert!(render(&mut d, 160, 40).contains("Services"));
    // turned off while it's open: back to the first page, not a dead one
    let _ = d.cfg.set("services", "off");
    let _ = render(&mut d, 160, 40);
    assert_eq!(d.mon.sub, Sub::Fleet);
}

#[test]
fn sessions_starting_and_ending_stay_out_of_the_inbox_unless_asked() {
    use crate::events::{Event, Kind};
    let now = crate::cache::now();
    let ev = |name: &str, kind| Event {
        t: now - 60,
        host: "b31".into(),
        name: name.into(),
        kind,
        title: format!("{name} title"),
        agent: "claude".into(),
        ..Default::default()
    };
    let rows = || {
        vec![
            ev("startedone", Kind::New),
            ev("endedone", Kind::Gone),
            ev("askingone", Kind::Blocked),
        ]
    };
    let mut d = dash_own();
    let _ = d.cfg.set("inbox.lifecycle", "false");
    inbox::apply_settings(&mut d);
    inbox::set_rows(&mut d, rows());
    d.view = View::Inbox;
    let s = render(&mut d, 180, 40);
    assert!(s.contains("askingone"), "{s}");
    assert!(!s.contains("startedone") && !s.contains("endedone"), "{s}");
    // asked for: listed as before
    let _ = d.cfg.set("inbox.lifecycle", "true");
    inbox::apply_settings(&mut d);
    let s = render(&mut d, 180, 40);
    assert!(s.contains("startedone") && s.contains("endedone"), "{s}");
}

#[test]
fn a_folder_chosen_in_quick_start_stays_chosen_when_the_machine_answers() {
    use super::quick::{Place, QuickStart};
    let d = dash();
    let ws = || {
        vec![herdr::Workspace {
            id: "w1".into(),
            label: "other-project".into(),
            number: 1,
            tabs: 1,
            focused: true,
            status: "idle".into(),
        }]
    };
    // chosen before the machine's workspaces arrive: the session would have
    // opened in the workspace that's open there
    let mut q = QuickStart::new(&d, "b31").with_folders(&["/work/chosen"]);
    q.key(k(KeyCode::Enter));
    q.loaded(Ok(ws()), vec![]);
    assert_eq!(q.place(), Some(Place::Folder("/work/chosen".into())));
    // nothing chosen or moved: it starts on the open workspace
    let mut q = QuickStart::new(&d, "b31").with_folders(&["/work/chosen"]);
    q.loaded(Ok(ws()), vec![]);
    assert!(matches!(q.place(), Some(Place::Workspace(_))));
}

#[test]
fn t_in_focus_flips_what_is_on_screen_on_the_first_press() {
    let mut d = dash();
    let _ = render(&mut d, 200, 40);
    wall::focus_on(&mut d, "b31|w1:p1", View::Fleet);
    leave_typing(&mut d);
    // wide: the timeline shows before any t, so one press hides it
    let _ = render(&mut d, 200, 40);
    d.on_key(k(KeyCode::Char('t')));
    assert_eq!(d.wall.timeline, Some(false));
    let _ = render(&mut d, 200, 40);
    d.on_key(k(KeyCode::Char('t')));
    assert_eq!(d.wall.timeline, Some(true));
    // narrower: it doesn't show by itself, so one press shows it
    d.wall.timeline = None;
    let _ = render(&mut d, 120, 40);
    d.on_key(k(KeyCode::Char('t')));
    assert_eq!(d.wall.timeline, Some(true));
}

#[test]
fn a_click_on_another_tile_focuses_it_and_the_keys_follow() {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let click = |d: &mut Dash, r: ratatui::layout::Rect| {
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            wall::mouse(
                d,
                MouseEvent {
                    kind,
                    column: r.x + r.width / 2,
                    row: r.y + r.height / 2,
                    modifiers: KeyModifiers::NONE,
                },
            );
        }
    };
    let keys = || vec!["b31|w1:p1".to_string(), "b31|w1:p2".to_string()];
    let mut d = dash();
    let _ = render(&mut d, 160, 40);
    wall::tile(&mut d, keys(), View::Fleet);
    let _ = render(&mut d, 160, 40);
    let places = d.wall.places();
    let second = places.iter().find(|(k, _)| k.ends_with("p2")).unwrap().1;
    let first = places.iter().find(|(k, _)| k.ends_with("p1")).unwrap().1;
    // from Aleph's keys or while typing: one click and the keys are that tile's
    click(&mut d, second);
    assert!(d.wall.typing, "the keys follow the click");
    assert_eq!(d.wall.focused_session(&d).unwrap().pane_id, "w1:p2");
    click(&mut d, first);
    assert!(d.wall.typing, "still typing, into the tile clicked");
    assert_eq!(d.wall.focused_session(&d).unwrap().pane_id, "w1:p1");
    // never zoomed by a click that only moves focus
    assert!(d.wall.win().is_some_and(|w| !w.zoom));

    // focus.keys = aleph: Aleph's keys stay until a second click
    let mut d = dash_own();
    let _ = d.cfg.set("focus.keys", "aleph");
    let _ = render(&mut d, 160, 40);
    wall::tile(&mut d, keys(), View::Fleet);
    let _ = render(&mut d, 160, 40);
    let second = d
        .wall
        .places()
        .iter()
        .find(|(k, _)| k.ends_with("p2"))
        .unwrap()
        .1;
    click(&mut d, second);
    assert!(!d.wall.typing, "focus moved, the keys are still Aleph's");
    assert_eq!(d.wall.focused_session(&d).unwrap().pane_id, "w1:p2");
    click(&mut d, second);
    assert!(d.wall.typing, "the second click types");
}

#[test]
fn cmd_keys_are_never_letters_or_commands() {
    let cmd = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::SUPER);
    let mut d = dash();
    let _ = render(&mut d, 120, 30);
    wall::focus_on(&mut d, "b31|w1:p1", View::Fleet);
    leave_typing(&mut d);
    let windows = d.wall.window_len();
    // c would have opened a window, x asked to close the session
    assert!(d.on_key(cmd('c')).is_none());
    assert!(d.on_key(cmd('x')).is_none());
    assert_eq!(d.wall.window_len(), windows);
    assert!(matches!(d.mode, Mode::Normal));
    assert!(render(&mut d, 120, 30).contains("to copy"));
}

#[test]
fn usage_says_how_each_machine_was_read() {
    use crate::usage::ReadState;
    let d = dash();
    let read = |host: &str, at, error: Option<&str>, waiting| ReadState {
        host: host.into(),
        at,
        error: error.map(str::to_string),
        waiting,
    };
    let text = |reads: &[ReadState], w| -> String {
        super::usage::reads_line(&d.cfg, reads, 10_000, w)
            .spans
            .iter()
            .map(|s| s.content.to_string())
            .collect()
    };
    let t = text(
        &[
            read("local", 9_990, None, 0),
            read(
                "gpu2",
                9_990,
                Some("the log reader failed on 1 of 1 files"),
                0,
            ),
            read("b31", 9_990, None, 39),
            read("new", 0, None, 0),
        ],
        200,
    );
    assert!(t.contains("local ✓ 10s"), "{t}");
    assert!(t.contains("gpu2 ✕ the log reader failed"), "{t}");
    assert!(t.contains("b31 ⟳ 39 files waiting"), "{t}");
    assert!(t.contains("new never read"), "{t}");
    assert!(t.contains("aleph usage sources"), "{t}");
    // all well: no pointer to the details
    assert!(!text(&[read("local", 9_990, None, 0)], 200).contains("sources"));
    // never past the edge
    assert!(ui::width(&t) > 40 && ui::width(&text(&[read("gpu2", 9_990, Some("x"), 0)], 8)) <= 8);
}

#[test]
fn usage_filters_chips_pickers_typed_and_a_calendar() {
    use super::monitor_draw::Sub;
    use super::usage_filters::{Dim, UChip};
    use crate::usage::dash::{Data, Opt};
    use crate::usage::Row;
    let mut d = dash();
    d.view = View::Monitor;
    d.mon.sub = Sub::Usage;
    let opt = |kind: char, id: &str, label: &str, cost: f64| Opt {
        kind,
        id: id.into(),
        label: label.into(),
        cost,
        parent: if kind == 'c' {
            "claude".into()
        } else {
            String::new()
        },
        ..Default::default()
    };
    d.usage.data = Some(Data {
        total: Row {
            key: "total".into(),
            input: 10,
            requests: 1,
            cost_estimated: 1.0,
            ..Default::default()
        },
        breakdown: vec![Row {
            key: "claude".into(),
            input: 10,
            requests: 1,
            ..Default::default()
        }],
        options: vec![
            opt('a', "claude", "Claude Code", 3.0),
            opt('c', "aaaa000000000000", "me@work.com · Max 20x", 2.0),
            opt('c', "bbbb000000000000", "me@home.dev · Pro", 1.0),
            opt('a', "codex", "Codex", 1.0),
            opt('h', "b31", "b31", 2.0),
            opt('p', "r:github.com/acme/app", "app", 2.0),
        ],
        ..Default::default()
    });
    let s = render(&mut d, 140, 36);
    assert!(s.contains("⏱ 7 days ▾") && s.contains("+ filter"), "{s}");
    // a: agents and their accounts; search, pick one account, apply
    d.on_key(k(KeyCode::Char('a')));
    let s = render(&mut d, 140, 36);
    assert!(
        s.contains("Agents and their accounts") && s.contains("me@work.com"),
        "{s}"
    );
    for ch in "work".chars() {
        d.on_key(k(KeyCode::Char(ch)));
    }
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.usage.f.accounts, vec!["aaaa000000000000".to_string()]);
    assert!(matches!(d.mode, Mode::Normal));
    let s = render(&mut d, 140, 36);
    assert!(
        s.contains("me@work.com · Max 20x") && s.contains("x clears"),
        "{s}"
    );
    // the chip's ×, by its click
    let drop = d
        .hits
        .chips
        .iter()
        .find(|(_, c)| *c == Chip::Usage(UChip::Drop(Dim::Agents)))
        .map(|(r, _)| *r)
        .expect("the chip's ×");
    d.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: drop.x,
        row: drop.y,
        modifiers: KeyModifiers::NONE,
    });
    assert!(d.usage.f.accounts.is_empty());
    // typed: an agent, a machine, a project, a period
    d.on_key(k(KeyCode::Char('/')));
    if let Mode::Input(i) = &mut d.mode {
        i.f = super::forms::Field::new("codex b31 app 30d");
    }
    d.on_key(k(KeyCode::Enter));
    let f = &d.usage.f;
    assert_eq!(
        (f.agents.clone(), f.hosts.clone()),
        (vec!["codex".to_string()], vec!["b31".to_string()])
    );
    assert_eq!(f.projects, vec!["r:github.com/acme/app".to_string()]);
    assert_eq!(f.period_label(), "30 days");
    // something it can't place says so, and keeps what you typed
    d.on_key(k(KeyCode::Char('/')));
    if let Mode::Input(i) = &mut d.mode {
        i.f = super::forms::Field::new("zzz-nope");
    }
    d.on_key(k(KeyCode::Enter));
    assert!(
        matches!(&d.mode, Mode::Input(i) if i.err.as_deref().is_some_and(|e| e.contains("zzz-nope")))
    );
    d.on_key(k(KeyCode::Esc));
    // x clears; t → custom… → the calendar: two ends
    d.on_key(k(KeyCode::Char('x')));
    assert!(!d.usage.f.narrowed());
    d.on_key(k(KeyCode::Char('t')));
    for _ in 0..crate::usage::PERIODS.len() {
        d.on_key(k(KeyCode::Down));
    }
    d.on_key(k(KeyCode::Enter));
    assert!(matches!(d.mode, Mode::UsageCal(_)));
    let s = render(&mut d, 140, 36);
    assert!(s.contains("From (enter)") && s.contains("Mo Tu We"), "{s}");
    d.on_key(k(KeyCode::Up));
    d.on_key(k(KeyCode::Enter));
    d.on_key(k(KeyCode::Down));
    d.on_key(k(KeyCode::Enter));
    let (a, b) = d.usage.f.custom.expect("a custom range");
    assert_eq!(b - a, 7);
    let s = render(&mut d, 140, 36);
    assert!(s.contains(" – "), "{s}");
}

#[test]
fn usage_tabs_page_and_pivot_draw_at_any_size_and_drill_in() {
    use super::monitor_draw::Sub;
    use crate::usage::dash::Data;
    use crate::usage::Row;
    let mut d = dash();
    d.view = View::Monitor;
    d.mon.sub = Sub::Usage;
    let row = |k: &str, cost: f64| Row {
        key: k.into(),
        input: 100,
        output: 20,
        requests: 4,
        prompts: 2,
        sessions: 1,
        cost_estimated: cost,
        ..Default::default()
    };
    let st = crate::usage::Stats {
        prompt_cost: crate::usage::Dist::of(vec![0.02, 0.1, 0.4, 1.5]),
        session_cost: crate::usage::Dist::of(vec![0.5, 2.0]),
        prompt_secs: crate::usage::Dist::of(vec![30.0, 400.0]),
        ..Default::default()
    };
    d.usage.data = Some(Data {
        total: row("total", 3.0),
        prev: row("total", 2.0),
        more: crate::usage::explore::More {
            breakup: vec![
                ("aaaa".into(), "me@work.com".into(), 2.0, 900),
                ("bbbb".into(), "me@home.dev".into(), 1.0, 300),
            ],
            series: vec![("aaaa".into(), "me@work.com".into())],
            daily: (0..30)
                .map(|i| (20_700 + i, format!("d{i}"), vec![i as f64 / 10.0]))
                .collect(),
            cum: (0..30).map(|i| i as f64).collect(),
            cum_prev: (0..30).map(|i| i as f64 / 2.0).collect(),
            models: vec!["opus-5-5".into()],
            model_mix: (0..30).map(|i| (format!("d{i}"), vec![1.0])).collect(),
            stats: st,
            tree: vec![
                (row("claude", 2.0), vec![row("aaaa", 2.0)]),
                (row("codex", 1.0), vec![row("bbbb", 1.0)]),
            ],
            calendar: vec![(20_729, 3.0)],
            insights: vec![("streak".into(), "3 days now".into())],
            ..Default::default()
        },
        ..Default::default()
    });
    for layout in 0..3 {
        for tab in 0..5 {
            d.usage.view.layout = Some(layout);
            d.usage.view.tab = tab;
            for (w, h) in [(1, 1), (30, 10), (80, 24), (140, 40), (300, 100)] {
                let _ = render(&mut d, w, h);
            }
        }
    }
    d.usage.view.layout = Some(0);
    d.usage.view.tab = 3;
    let s = render(&mut d, 160, 44);
    assert!(
        s.contains("$ a prompt · 4 of them") && s.contains("median"),
        "{s}"
    );
    d.usage.view.tab = 2;
    let s = render(&mut d, 160, 44);
    assert!(s.contains("cost so far") && s.contains("model mix"), "{s}");
    // the pivot names its four choices; the page scrolls
    d.usage.view.layout = Some(2);
    let s = render(&mut d, 160, 44);
    assert!(
        s.contains("rows agent (b)") && s.contains("(n)") && s.contains("(c)"),
        "{s}"
    );
    d.on_key(k(KeyCode::Char('c')));
    assert_eq!(d.usage.view.chart, 1);
    d.usage.view.layout = Some(1);
    d.on_key(k(KeyCode::Char('j')));
    assert_eq!(d.usage.view.scroll, 1);
    // enter on a breakdown row narrows to it; backspace goes back
    d.usage.view.layout = Some(0);
    d.usage.view.tab = 1;
    d.usage.sel = 1;
    d.on_key(k(KeyCode::Enter));
    assert_eq!(d.usage.f.agents, vec!["codex".to_string()]);
    d.on_key(k(KeyCode::Backspace));
    assert!(d.usage.f.agents.is_empty());
}

/// A conversation's tree for the timeline panels: one prompt, two requests,
/// four reads (a run) and a failed command.
fn tl_tree(session: &str) -> crate::timeline::Tree {
    use crate::usage::steps::{Kind, Step};
    let b = 1_792_659_000_000u64;
    let st = |kind, ms, key: &str, name: &str| Step {
        kind,
        ms,
        host: "b31".into(),
        agent: "claude".into(),
        session: session.into(),
        lane: String::new(),
        key: key.into(),
        name: name.into(),
        class: "tool".into(),
        n1: 0,
        n2: 0,
    };
    let mut steps = vec![st(Kind::Prompt, b, "p1", "u")];
    for i in 0..4u64 {
        let k = format!("r{i}");
        steps.push(st(Kind::ToolStart, b + 3_000 + i * 1_000, &k, "Read"));
        steps.push(st(Kind::ToolEnd, b + 3_500 + i * 1_000, &k, "ok"));
    }
    steps.push(st(Kind::ToolStart, b + 13_000, "x", "Bash"));
    steps.push(st(Kind::ToolEnd, b + 13_800, "x", "err"));
    let rec = |ts: u64| crate::usage::parse::Record {
        ts,
        host: "b31".into(),
        agent: "claude".into(),
        session: session.into(),
        key: format!("k{ts}"),
        model: "claude-sonnet-4-5".into(),
        input: 1_000,
        output: 200,
        cost: Some(0.05),
        ..Default::default()
    };
    let recs = vec![rec(b / 1000 + 2), rec(b / 1000 + 12)];
    crate::timeline::build(
        "b31",
        "claude",
        session,
        &steps,
        &recs,
        &crate::prices::Table::default(),
    )
}

#[test]
fn fleet_g_shows_the_conversation_in_the_preview() {
    let mut d = dash();
    d.fleet.host_mut("b31").unwrap().sessions[0].native_id = Some("conv-1".into());
    d.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    assert!(d.fv.timeline_panel);
    super::tree_view::show(&mut d, "b31", "claude", "conv-1", true);
    super::tree_view::set_tree(&mut d, tl_tree("conv-1"));
    let s = render(&mut d, 140, 36);
    // ignite is working: its last prompt is open, the run of reads folded
    assert!(s.contains("┌ prompt 1 · you"), "{s}");
    assert!(s.contains("Read + 3 more"), "{s}");
    assert!(s.contains("Bash · failed"), "{s}");
    assert!(s.contains("$0.10"), "{s}");
    // a click on the run opens it
    let y = s.lines().position(|l| l.contains("Read + 3 more")).unwrap() as u16;
    let x = s
        .lines()
        .nth(y as usize)
        .unwrap()
        .chars()
        .position(|c| c == '▸')
        .unwrap_or(100) as u16;
    d.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    });
    let s = render(&mut d, 140, 36);
    assert_eq!(s.matches("› Read").count(), 4, "{s}");
    // G again: recent output back
    d.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
    let s = render(&mut d, 140, 36);
    assert!(!s.contains("Read + 3 more"), "{s}");
}

#[test]
fn focus_side_panel_draws_the_conversation() {
    let mut d = dash();
    d.fleet.host_mut("b31").unwrap().sessions[0].native_id = Some("conv-2".into());
    wall::focus_on(&mut d, "b31|w1:p1", View::Fleet);
    leave_typing(&mut d);
    super::tree_view::show(&mut d, "b31", "claude", "conv-2", true);
    super::tree_view::set_tree(&mut d, tl_tree("conv-2"));
    let s = render(&mut d, 200, 50);
    assert!(s.contains("prompt 1"), "{s}");
    assert!(s.contains("Read + 3 more"), "{s}");
    // the wheel over it scrolls it, not the pane (a short one: nothing to scroll)
    let r = d.tv.area.expect("drawn");
    d.on_mouse(crossterm::event::MouseEvent {
        kind: crossterm::event::MouseEventKind::ScrollDown,
        column: r.x + 2,
        row: r.y + 1,
        modifiers: KeyModifiers::NONE,
    });
    assert_eq!(d.tv.scroll, 0);
}

#[test]
fn monitor_timeline_lists_earlier_conversations_and_shows_their_tree() {
    let mut d = dash();
    d.view = View::Monitor;
    d.mon.sub = monitor_draw::Sub::Timeline;
    d.tl.set_earlier(vec![crate::timeline::Brief {
        host: "b31".into(),
        agent: "claude".into(),
        session: "old-1".into(),
        project: "aleph".into(),
        last: crate::cache::now() - 7_200,
        cost: 1.25,
        prompts: 3,
        requests: 12,
        ..Default::default()
    }]);
    let s = render(&mut d, 200, 50);
    assert!(s.contains("earlier · 1 conversation"), "{s}");
    assert!(s.contains("▪ aleph"), "{s}");
    assert!(s.contains("3 prompts · 12 req · $1.25"), "{s}");
    // a live session without the agent's id: nothing to show
    assert!(s.contains("Pick a conversation"), "{s}");
    // to the earlier one: its tree beside the list
    d.on_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    super::tree_view::show(&mut d, "b31", "claude", "old-1", false);
    super::tree_view::set_tree(&mut d, tl_tree("old-1"));
    let s = render(&mut d, 200, 50);
    assert!(s.contains("Conversation · claude · b31"), "{s}");
    // finished: smart folding closes it to its line, which says a call failed
    assert!(s.contains("▸ prompt 1 · you"), "{s}");
    assert!(
        s.contains("1 failed") && !s.contains("Read + 3 more"),
        "{s}"
    );
    // t: the keys are the tree's; enter opens the prompt; esc gives them back
    d.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
    assert!(d.tl.tree_keys);
    d.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("┌ prompt 1 · you") && s.contains("Read + 3 more"),
        "{s}"
    );
    assert!(s.contains("esc  the list"), "{s}");
    d.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let s = render(&mut d, 200, 50);
    assert!(s.contains("▸ prompt 1 · you"), "{s}");
    // open mode: the prompt opens, the repeats stay folded; Z opens them all
    d.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
    let s = render(&mut d, 200, 50);
    assert!(
        s.contains("┌ prompt 1") && s.contains("Read + 3 more"),
        "open: {s}"
    );
    d.on_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::NONE));
    let s = render(&mut d, 200, 50);
    assert_eq!(s.matches("› Read").count(), 4, "all open: {s}");
    d.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!d.tl.tree_keys);
    assert_eq!(d.view, View::Monitor);
    // narrow: the list alone, the tree in its place on t
    let s = render(&mut d, 120, 32);
    assert!(!s.contains("Conversation ·"), "{s}");
    d.on_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE));
    let s = render(&mut d, 120, 32);
    assert!(
        s.contains("Conversation ·") && !s.contains("earlier ·"),
        "{s}"
    );
}
