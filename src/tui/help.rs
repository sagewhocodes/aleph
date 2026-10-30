//! The `?` overlay: every key, with the user's own bindings. Typing searches
//! them by what they do (`helpsearch`: "max" finds zoom, "kill" finds
//! close); enter runs the one picked.

use super::{Act, Dash, Handoff, Level, Mode, View, ACTS};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

/// The overlay's state: what's typed, the pick, the scroll (no query).
#[derive(Default)]
pub(crate) struct Help {
    pub q: String,
    pub sel: usize,
    pub scroll: u16,
}

/// What enter on an entry does.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Run {
    /// a Fleet action (on the selection there)
    Act(Act),
    View(View),
    Palette,
    /// the theme gallery
    Themes,
    /// the splash gallery
    Splashes,
    /// switch to the view, then press the key there
    Key(View, KeyCode, KeyModifiers),
    /// only means something somewhere else: say where
    Note(&'static str),
    /// one of showcase mode's palette entries (the autopilot tour, recording)
    Showcase(&'static str),
}

/// One key for the search.
pub(crate) struct Entry {
    pub keys: String,
    pub what: String,
    pub section: &'static str,
    pub words: &'static [&'static str],
    pub run: Run,
}

fn e(keys: &str, what: &str, section: &'static str, run: Run) -> Entry {
    Entry {
        keys: keys.into(),
        what: what.into(),
        section,
        words: &[],
        run,
    }
}

/// What people type for acting on several sessions at once.
const MARKED: &[&str] = &[
    "select", "selected", "multi", "multiple", "many", "batch", "bulk", "several", "all", "marked",
    "every",
];

const FOCUS: Run = Run::Note("that one works in Focus or Tile — enter on a session opens Focus");

/// Every key on its own (what the search ranks).
pub(crate) fn entries(d: &Dash) -> Vec<Entry> {
    let mut v: Vec<Entry> = Vec::new();
    for (i, x) in View::ALL.iter().enumerate() {
        v.push(Entry {
            words: &["tab", "switch", "go", "view"],
            ..e(
                &(i + 1).to_string(),
                &format!("the {} view", x.name()),
                "Everywhere",
                Run::View(*x),
            )
        });
    }
    v.push(Entry {
        words: &["search", "find", "jump", "goto"],
        ..e(
            "ctrl+k",
            "command palette — sessions, machines, @presets, folders",
            "Everywhere",
            Run::Palette,
        )
    });
    v.push(Entry {
        words: &[
            "theme", "themes", "colours", "colors", "palette", "accent", "dark", "light", "look",
        ],
        ..e(
            "ctrl+k",
            "theme gallery: every theme and accent, live across the dashboard",
            "Everywhere",
            Run::Themes,
        )
    });
    v.push(Entry {
        words: &[
            "splash",
            "splashes",
            "intro",
            "start",
            "startup",
            "animation",
            "logo",
            "screen",
        ],
        ..e(
            "ctrl+k",
            "splash gallery: every splash previewed live; p plays one full screen",
            "Everywhere",
            Run::Splashes,
        )
    });
    v.push(Entry {
        words: &["workspace", "workspaces", "saved", "place", "bookmark", "preset view", "layout"],
        ..e(
            "W",
            "saved workspaces: go to one (a tab, its layout, Fleet's filter, a machine), or save where you are",
            "Everywhere",
            Run::Key(View::Fleet, KeyCode::Char('W'), KeyModifiers::SHIFT),
        )
    });
    v.push(Entry {
        words: &[
            "toast",
            "toasts",
            "notifications",
            "history",
            "missed",
            "messages",
            "alerts",
            "status",
            "log",
            "recent",
        ],
        ..e(
            &d.keys.key_for(Act::Alerts),
            "alerts: the ones you missed (since you last opened the Inbox) and the last 20 toasts",
            "Everywhere",
            Run::Act(Act::Alerts),
        )
    });
    v.push(Entry {
        words: &[
            "approve", "approvals", "deny", "questions", "permission", "allow", "waiting", "board",
            "yes", "answer",
        ],
        ..e(
            &d.keys.key_for(Act::Approvals),
            "approvals: every agent waiting on you, its question as it shows it (y · n · 1–9 · Y all)",
            "Everywhere",
            Run::Act(Act::Approvals),
        )
    });
    v.push(e(
        "⇥",
        "the next sub-view (Monitor's views, Inbox kinds, Settings sections)",
        "Everywhere",
        Run::Note("press ⇥ in Monitor, Inbox or Settings"),
    ));
    for (k, w, words) in [
        (
            "p",
            "hide or show the side panel (Fleet's preview, Monitor's processes, Inbox's and Settings' detail)",
            &["preview", "collapse", "side", "panel", "hide"][..],
        ),
        (
            "z",
            "maximise in turn: the left side · the top panel · the bottom panel · the side · back",
            &["max", "maximise", "maximize", "fullscreen", "zoom", "expand"][..],
        ),
        (
            "{ }",
            "move the divider between the stacked panels up · down (Fleet's list and resources)",
            &["resize", "divider", "split", "taller", "shorter"][..],
        ),
        ("=", "put the layout back", &["reset", "layout", "default"][..]),
        (
            "L",
            "layout mode: ↑↓ the divider, ←→ the side's width, 1–4 maximise, p, 0 reset, esc done",
            &["layout", "resize", "panels", "arrange"][..],
        ),
    ] {
        v.push(Entry {
            words,
            ..e(k, w, "Layout", Run::Note("press it in Fleet, Monitor, Inbox or Settings"))
        });
    }
    // (the alerts key works everywhere: it's listed there)
    for (a, _, _, help) in ACTS
        .iter()
        .filter(|x| !matches!(x.0, Act::Alerts | Act::Approvals))
    {
        let words: &'static [&'static str] = match a {
            Act::Timeline => &["timeline", "history", "chart", "gantt", "states", "graph"],
            Act::TimelinePanel => &[
                "timeline",
                "conversation",
                "prompts",
                "turns",
                "tools",
                "steps",
                "trace",
                "fold",
                "subagents",
                "cost",
            ],
            Act::Undo => &[
                "undo", "keep", "cancel", "restore", "revert", "oops", "grace", "closing",
            ],
            Act::Mark => MARKED,
            _ => &[],
        };
        v.push(Entry {
            words,
            ..e(&d.keys.key_for(*a), help, "Fleet", Run::Act(*a))
        });
    }
    // what the marks are for: every marked session at once
    for (a, w) in [
        (
            Act::Approve,
            "approve every marked session that asks, each with its own yes option (the rest are skipped, and the toast says why)",
        ),
        (Act::Send, "send the same prompt to every marked session"),
        (
            Act::Queue,
            "queue a task for every marked session, sent to each once it's free",
        ),
        (
            Act::Close,
            "close every marked session (after close.grace; u keeps them)",
        ),
    ] {
        v.push(Entry {
            words: MARKED,
            ..e(
                &format!("space {}", d.keys.key_for(a)),
                w,
                "Fleet",
                Run::Act(a),
            )
        });
    }
    // showcase mode's own keys (a build without it has none)
    for (k, w, id, words) in crate::showcase::help_keys(&d.cfg) {
        let run = if id.is_empty() {
            Run::Note("press it in any view")
        } else {
            Run::Showcase(id)
        };
        v.push(Entry {
            words,
            ..e(&k, &w, "Showcase", run)
        });
    }
    for (k, label, cmd) in &d.keys.custom {
        let run = match k.chars().collect::<Vec<_>>().as_slice() {
            [c] => Run::Key(View::Fleet, KeyCode::Char(*c), KeyModifiers::NONE),
            _ => Run::Note("press its key in Fleet"),
        };
        v.push(e(k, &format!("{label} — {cmd}"), "Your actions", run));
    }
    for (k, w) in [
        ("]", "Focus, Tile: the next session, on any machine"),
        ("[", "Focus, Tile: the previous session, on any machine"),
        ("ctrl+]", "Focus, Tile: the next machine (or ))"),
        ("ctrl+[", "Focus, Tile: the previous machine (or ()"),
        ("alt+.", "typing: the next session"),
        ("alt+,", "typing: the previous session"),
        ("}", "Focus: the next session that needs you"),
        ("{", "Focus: the previous session that needs you"),
        ("hjkl", "move between panes (or ctrl+w hjkl, ctrl+b arrows)"),
        ("v", "split side by side, then pick a session"),
        ("s", "split stacked, then pick a session"),
        (
            "i",
            "type into the pane — ctrl+q (or a click outside) stops; ctrl+q again goes to Fleet",
        ),
        ("n", "a new session in this window"),
        ("S", "a new shell (plain terminal) in this window"),
        ("a", "add an existing session to the window"),
        ("y", "approve its question"),
        ("K", "send keys"),
        ("m", "write a prompt"),
        ("b", "broadcast a prompt to every pane in the window"),
        (
            "z",
            "zoom the pane: fullscreen in the window, again to undo",
        ),
        ("x", "close the pane"),
        ("space", "the next layout"),
        ("c", "a new window"),
        (",", "rename the window"),
        ("< >", "resize the pane narrower · wider"),
        ("- +", "resize the pane shorter · taller"),
        ("=", "equalise the panes"),
        ("A", "straight into the agent's own terminal"),
        ("O", "open in herdr in a new terminal tab"),
        ("T", "push the window into herdr"),
    ] {
        // "max" is what people type for zoom (maximise the pane)
        let words: &'static [&'static str] = if k == "z" { &["max", "maximise"] } else { &[] };
        v.push(Entry {
            words,
            ..e(k, w, "Focus · Tile", FOCUS)
        });
    }
    let mon = |k: KeyCode| Run::Key(View::Monitor, k, KeyModifiers::NONE);
    v.push(Entry {
        words: &[
            "timeline", "history", "chart", "gantt", "states", "graph", "hours", "conversation",
            "prompts", "earlier", "trace",
        ],
        ..e(
            "t",
            "Monitor › Timeline: every session's states over the last hours, earlier conversations by project, the selected one's prompts, turns and tool calls (t: its keys)",
            "Monitor",
            mon(KeyCode::Char('t')),
        )
    });
    v.push(e(
        "⇥",
        "Monitor: the next view (overview, processes, usage, …)",
        "Monitor",
        mon(KeyCode::Tab),
    ));
    v.push(e(
        "]",
        "Monitor: the next machine",
        "Monitor",
        mon(KeyCode::Char(']')),
    ));
    v.push(e(
        "[",
        "Monitor: the previous machine",
        "Monitor",
        mon(KeyCode::Char('[')),
    ));
    let ib = |k: char| {
        let m = if k.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        Run::Key(View::Inbox, KeyCode::Char(k), m)
    };
    for (k, w) in [
        ('o', "Inbox: open its session in a new terminal tab"),
        ('X', "Inbox: close its session (asks first)"),
        ('y', "Inbox: approve its question"),
        ('s', "Inbox: reply to it"),
        ('Q', "Inbox: queue a message for when it's free"),
        ('x', "Inbox: archive it (or bring it back)"),
        ('A', "Inbox: the archive"),
        ('D', "Inbox: delete it for good (asks first)"),
        ('u', "Inbox: mark all read"),
        (
            'U',
            "Inbox: dismiss alerts… pick which kinds, then archive or delete",
        ),
        ('G', "Inbox: group by machine, day, agent, type or session"),
        ('v', "Inbox: its layout (overview, cards, table)"),
        ('e', "Inbox: hide ended sessions"),
        ('r', "Inbox: start it again"),
        ('P', "Inbox: save as a preset"),
        ('N', "Inbox: a note on its session"),
        ('g', "Inbox: only one agent"),
        ('m', "Inbox: only one machine"),
        ('f', "Inbox: only one kind"),
        ('/', "Inbox: search"),
        (
            'J',
            "Inbox: the session journal — what each session did today (standup notes)",
        ),
    ] {
        v.push(e(&k.to_string(), w, "Inbox", ib(k)));
    }
    v.push(e(
        "enter",
        "Inbox: go to its session (Focus) or machine",
        "Inbox",
        Run::Key(View::Inbox, KeyCode::Enter, KeyModifiers::NONE),
    ));
    let st = |k: KeyCode| Run::Key(View::Settings, k, KeyModifiers::NONE);
    v.push(e(
        "enter",
        "Settings: edit a setting",
        "Settings",
        st(KeyCode::Enter),
    ));
    v.push(e(
        "space",
        "Settings: toggle a setting",
        "Settings",
        st(KeyCode::Char(' ')),
    ));
    v.push(e(
        "d",
        "Settings: reset a setting to its default",
        "Settings",
        st(KeyCode::Char('d')),
    ));
    v.push(e(
        "/",
        "Settings: search every setting",
        "Settings",
        st(KeyCode::Char('/')),
    ));
    v.push(e(
        "⇥",
        "Settings: the next section (the Doctor: by machine ⇄ by agent)",
        "Settings",
        st(KeyCode::Tab),
    ));
    v.push(e(
        "f",
        "Settings › Doctor: carry out a check's fix",
        "Settings",
        st(KeyCode::Char('f')),
    ));
    v
}

/// The entries `q` finds, best first.
pub(crate) fn found(d: &Dash, q: &str) -> Vec<Entry> {
    let mut v: Vec<(i32, usize, Entry)> = entries(d)
        .into_iter()
        .enumerate()
        .filter_map(|(i, x)| {
            let keys: Vec<&str> = x.keys.split(['·', ' ']).filter(|k| !k.is_empty()).collect();
            let extra: Vec<&str> = x.words.iter().copied().chain([x.section]).collect();
            crate::helpsearch::rank(q, &keys, &x.what, &extra).map(|s| (s, i, x))
        })
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    v.into_iter().map(|x| x.2).collect()
}

/// A key while the overlay is open: typing searches, arrows pick (or
/// scroll), enter runs the pick, esc clears the search, then closes.
pub(crate) fn key(d: &mut Dash, mut h: Help, k: KeyEvent) -> Option<Handoff> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    match k.code {
        KeyCode::Esc if !h.q.is_empty() => {
            h.q.clear();
            h.sel = 0;
        }
        KeyCode::Esc => return None,
        KeyCode::Char('?') if h.q.is_empty() => return None,
        KeyCode::Enter if !h.q.is_empty() => {
            let v = found(d, &h.q);
            let Some(x) = v.into_iter().nth(h.sel) else {
                d.mode = Mode::Help(h);
                return None;
            };
            return run(d, x.run);
        }
        KeyCode::Backspace => {
            h.q.pop();
            h.sel = 0;
        }
        KeyCode::Char('u') if ctrl => {
            h.q.clear();
            h.sel = 0;
        }
        KeyCode::Down if !h.q.is_empty() => h.sel += 1,
        KeyCode::Up if !h.q.is_empty() => h.sel = h.sel.saturating_sub(1),
        KeyCode::Down => h.scroll += 1,
        KeyCode::Up => h.scroll = h.scroll.saturating_sub(1),
        KeyCode::PageDown => h.scroll += 10,
        KeyCode::PageUp => h.scroll = h.scroll.saturating_sub(10),
        KeyCode::Char(c) if !ctrl => {
            h.q.push(c);
            h.sel = 0;
        }
        _ => {}
    }
    if !h.q.is_empty() {
        let n = found(d, &h.q).len();
        h.sel = h.sel.min(n.saturating_sub(1));
    }
    d.mode = Mode::Help(h);
    None
}

/// Carry out what an entry does.
pub(crate) fn run(d: &mut Dash, r: Run) -> Option<Handoff> {
    match r {
        Run::Act(Act::Alerts) => {
            super::alerts::open(d);
            None
        }
        Run::Act(Act::Approvals) => {
            super::approvals::open(d);
            None
        }
        Run::Act(a) => {
            if d.view != View::Fleet {
                d.switch(View::Fleet);
            }
            super::fleet::run_act(d, a)
        }
        Run::View(v) => {
            d.switch(v);
            None
        }
        Run::Palette => {
            d.mode = Mode::Palette(super::palette::Palette::new(d));
            None
        }
        Run::Themes => {
            super::gallery::open(d);
            None
        }
        Run::Splashes => {
            super::splashes::open(d);
            None
        }
        Run::Key(v, code, m) => {
            if d.view != v {
                d.switch(v);
            }
            d.on_key(KeyEvent::new(code, m))
        }
        Run::Note(t) => {
            d.toast(Level::Info, t);
            None
        }
        Run::Showcase(id) => {
            crate::showcase::capture(&d.cfg, id);
            None
        }
    }
}

pub(crate) fn lines(d: &Dash) -> Vec<Line<'static>> {
    let head = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ))
    };
    let row = |k: String, what: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(ui::pad(&k, 12), ui::bold()),
            Span::styled(what.to_string(), ui::muted()),
        ])
    };
    let mut v = vec![head("Everywhere")];
    let alerts_key = d.keys.key_for(Act::Alerts);
    let approvals_key = d.keys.key_for(Act::Approvals);
    let views: Vec<&str> = super::View::ALL.iter().map(|x| x.name()).collect();
    let switch = format!("switch view ({})", views.join(" "));
    let digits = format!("1–{} · tab", views.len());
    for (k, w) in [
        (digits.as_str(), switch.as_str()),
        (
            "ctrl+k",
            "command palette — sessions, machines, @presets, folders",
        ),
        ("?", "this help"),
        ("W", "saved workspaces: go to one, or save where you are"),
        (
            alerts_key.as_str(),
            "alerts: the ones you missed and the last 20 toasts",
        ),
        (
            approvals_key.as_str(),
            "approvals: every agent waiting on you — y · n · 1–9 · Y all",
        ),
        ("q · esc", "back / quit"),
        ("ctrl+c", "quit"),
    ] {
        v.push(row(k.into(), w));
    }
    v.push(Line::raw(""));
    v.push(head(
        "Layout (Fleet, Monitor, Inbox, Settings) — remembered per view",
    ));
    for (k, w) in [
        (
            "p",
            "hide or show the side panel (preview, processes, detail)",
        ),
        (
            "z",
            "maximise in turn: left side · top · bottom · side · back",
        ),
        ("{ }", "move the divider between the stacked panels"),
        ("=", "put the layout back"),
        (
            "L",
            "layout mode: ↑↓ divider · ←→ side width · 1–4 maximise · p · 0 reset · esc",
        ),
    ] {
        v.push(row(k.into(), w));
    }
    v.push(Line::raw(""));
    v.push(head("Fleet"));
    for (a, _, _, help) in ACTS
        .iter()
        .filter(|x| !matches!(x.0, Act::Alerts | Act::Approvals))
    {
        v.push(row(d.keys.key_for(*a), help));
    }
    if !d.keys.custom.is_empty() {
        v.push(Line::raw(""));
        v.push(head("Your actions (action.<key>=label|command)"));
        for (k, label, cmd) in &d.keys.custom {
            v.push(row(k.clone(), &format!("{label} — {cmd}")));
        }
    }
    // showcase mode's own keys (a build without it has none)
    let showcase = crate::showcase::help_keys(&d.cfg);
    if !showcase.is_empty() {
        v.push(Line::raw(""));
        v.push(head("Showcase — in any view"));
        for (k, w, _, _) in showcase {
            v.push(row(k, w.trim_start_matches("showcase: ")));
        }
    }
    v.push(Line::raw(""));
    v.push(head(
        "Focus (enter on a session) · Tile (w) — ? there shows every key",
    ));
    for (k, w) in [
        (
            "[ ] · { }",
            "previous · next session, on any machine; Focus: previous · next that needs you",
        ),
        (
            "ctrl+[ ] · ( )",
            "previous · next machine (( ) where a terminal can't tell ctrl+[ from esc)",
        ),
        (
            "alt+, alt+.",
            "typing: previous · next session (or the prefix and [ ])",
        ),
        ("esc", "back to where you opened it from"),
        (
            "hjkl · arrows",
            "move between panes (or ctrl+w hjkl, ctrl+b arrows)",
        ),
        (
            "v | · s _",
            "split side by side · stacked, then pick a session (or ctrl+b % \" · ctrl+w v s)",
        ),
        (
            "i · enter",
            "type into the pane — ctrl+q (or a click outside) stops, ctrl+q again goes to Fleet; ctrl+b works while typing",
        ),
        (
            "n · S · a",
            "new session here · new shell · add an existing session",
        ),
        (
            "y · K · m · b",
            "approve · keys · prompt · broadcast to the window",
        ),
        ("z · x · space", "zoom · close pane · next layout"),
        (
            "c · w W · ,",
            "new window · next / previous window · rename",
        ),
        ("< > - + =", "resize · equalise (mouse: drag a divider)"),
        (
            "a · A · O · T",
            "attach in herdr · straight to the agent · herdr in a new terminal tab · push to herdr",
        ),
    ] {
        v.push(row(k.into(), w));
    }
    v.push(Line::raw(""));
    v.push(head(&format!(
        "Monitor ({})",
        super::digit(super::View::Monitor)
    )));
    v.push(row(
        "⇥ · ←→".into(),
        "the next view (overview, processes, usage, …)",
    ));
    v.push(row(
        "[ ] · ctrl+[ ] · ( ) · ↑↓".into(),
        "pick a machine (Timeline: [ ] a session)",
    ));
    v.push(row(
        "t".into(),
        "Timeline: every session's states over the last hours (+ - hours · e ended · enter Focus); earlier conversations by project; the selected one's tree beside it",
    ));
    v.push(row(
        "t · esc".into(),
        "the tree's keys: j k · enter fold · ← → · z Z all · ( ) prompts · e next error · f follow · w words · o smart / open / folded",
    ));
    v.push(Line::raw(""));
    v.push(head(&format!(
        "Inbox ({})",
        super::digit(super::View::Inbox)
    )));
    for (k, w) in [
        (
            "enter · o · X",
            "go to the session · open it in a new tab · close it (asks first)",
        ),
        ("y · s · Q", "approve · reply · queue for when it's free"),
        (
            "x · A · D",
            "archive (or bring back) · the archive · delete for good (asks first)",
        ),
        (
            "u · U",
            "mark all read · dismiss… (pick which kinds, then archive or delete)",
        ),
        (
            "G · e",
            "group by machine, day, agent, type or session · hide ended sessions",
        ),
        ("r · p · N", "start it again · save as preset · note"),
        (
            "g · m · f",
            "agent · machine · kind (Launches: what you started)",
        ),
    ] {
        v.push(row(k.into(), w));
    }
    v.push(Line::raw(""));
    v.push(head(&format!(
        "Settings ({})",
        super::digit(super::View::Settings)
    )));
    for (k, w) in [
        ("enter · space", "edit · toggle a setting"),
        ("d", "reset a setting to its default"),
        (
            "/ · ⇥",
            "search every setting · the next section (the Doctor: by machine ⇄ by agent)",
        ),
    ] {
        v.push(row(k.into(), w));
    }
    v.push(Line::raw(""));
    v.push(Line::from(Span::styled(
        "  Rebind any Fleet key with key.<action>=<key> (aleph settings). Mouse: click, double-click, wheel.",
        ui::faint(),
    )));
    v
}

pub(crate) fn draw(d: &Dash, f: &mut Frame, area: Rect, h: &Help) {
    let w = 84u16.min(area.width.saturating_sub(2));
    let hh = area.height.saturating_sub(4).min(44);
    let r = ui::centered(area, w, hh);
    crate::ui::clear(f, r);
    let bottom = if h.q.is_empty() {
        " type to search · ↑↓ scroll · esc closes "
    } else {
        " ↑↓ pick · enter runs it · esc clears "
    };
    let block = ui::panel("Keys", true)
        .title_bottom(Line::from(Span::styled(bottom, ui::faint())).right_aligned());
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height < 3 {
        return;
    }
    // the search line
    let mut ql = vec![Span::styled(" ⌕ ", ui::fg(c::ACCENT()))];
    if h.q.is_empty() {
        ql.push(Span::styled(
            "search keys by what they do — max, kill, split, terminal…",
            ui::faint(),
        ));
    } else {
        ql.push(Span::styled(h.q.clone(), ui::bold()));
        ql.push(Span::styled("▏", ui::fg(c::ACCENT())));
    }
    let top = Rect { height: 1, ..inner };
    f.render_widget(Paragraph::new(Line::from(ql)), top);
    let body = Rect {
        y: inner.y + 2,
        height: inner.height - 2,
        ..inner
    };
    if h.q.is_empty() {
        let l = lines(d);
        let max = (l.len() as u16).saturating_sub(body.height);
        f.render_widget(Paragraph::new(l).scroll((h.scroll.min(max), 0)), body);
        return;
    }
    let v = found(d, &h.q);
    if v.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("  nothing does “{}” — try another word", h.q),
                ui::faint(),
            ))),
            body,
        );
        return;
    }
    let rows = body.height as usize;
    let skip = h.sel.saturating_sub(rows.saturating_sub(1));
    let bw = body.width as usize;
    let lines: Vec<Line> = v
        .iter()
        .enumerate()
        .skip(skip)
        .take(rows)
        .map(|(i, x)| {
            let sel = i == h.sel;
            let chip = format!(" {} ", x.section);
            let runs = !matches!(x.run, Run::Note(_));
            let tail = if sel && runs { "  ↵ run" } else { "" };
            let room = bw.saturating_sub(2 + 12 + ui::width(&chip) + 2 + tail.len());
            let what = ui::trunc(&x.what, room);
            let pad = room.saturating_sub(ui::width(&what));
            Line::from(vec![
                Span::styled(if sel { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
                Span::styled(
                    ui::pad(&ui::trunc(&x.keys, 11), 12),
                    if sel {
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                    } else {
                        ui::bold()
                    },
                ),
                Span::styled(what, if sel { ui::bold() } else { ui::muted() }),
                Span::raw(" ".repeat(pad + 2)),
                Span::styled(chip, ui::faint().bg(c::SEL())),
                Span::styled(tail, ui::fg(c::ACCENT())),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), body);
}
