//! Command palette (ctrl+k / p). Before you type: what you pinned (ctrl+p, up to
//! three), your sessions by machine, a few actions. A session is one aligned
//! line: name, agent, the model for agents that run on any, state, and how long
//! while it works. Type and everything else is there too (machines, views, pages,
//! every command, presets, folders): each word matches on its own, in any order,
//! so "b31 claude" lists Claude Code's sessions on b31.

use super::fleet::RowId;
use super::forms::{Field, Res};
use super::{Dash, Handoff, Mode, View};
use crate::ui::{self, c};
use crate::{fuzzy, history};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::time::{Duration, Instant};

/// Sessions on top of the palette, at most.
const MAX_PINS: usize = 3;
/// The longest name a session's line gives room to.
const MAX_NAME: usize = 26;
/// What a session gets over anything else in a search: about one letter's worth.
const SESSION_LEAD: i32 = 20;
/// Untitled sessions a machine's screens are read for, on one open.
const TRAIL_READS: usize = 12;
/// A screen's line is good for this long.
const TRAIL_FRESH: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) enum Item {
    Session(String),
    /// a terminal on this machine, in this folder
    TerminalHere(String, Option<String>),
    /// the split picker: start an agent in the empty pane
    PaneNew,
    /// the split picker: a terminal on this machine in the empty pane
    PaneShell(String),
    /// Focus on this session (Fleet's selected one): its terminal here
    Focus(String),
    /// leave Focus or Tile for the view it was opened from
    LeaveFocus,
    /// keys back to Aleph from the agent (Focus stays)
    StopTyping,
    /// a newer build is installed: start again on it
    Restart,
    Host(String),
    View(View),
    /// a Monitor page: the fleet's (None), or a machine's
    Page(Option<String>, super::monitor_draw::Sub),
    Refresh,
    Hub,
    NewOn(String),
    /// the folder browser, for the machine's default agent
    StartIn(String),
    Preset(String),
    Folder(String, String),
    QueueOn(String),
    Toggle(&'static str),
    /// showcase mode (in a build that has it): off, masked, the demo world
    Showcase(&'static str),
    /// showcase's capture (in a build that has it): record, poster
    Capture(&'static str),
    /// the What's new card
    News,
    /// use a settings profile ("" turns the one in use off)
    Profile(String),
    /// go to a saved workspace ("" opens the picker)
    Workspace(String),
    /// the theme gallery
    Gallery,
    /// the splash gallery
    Splashes,
    /// hand session (its key) to agent
    Handoff(String, &'static str),
    /// start a race in the selected session's folder ("") or open one (its id)
    Race(String),
    /// an agent outside herdr (`host|pid`): reopen it in herdr (true) or stop it
    Outside(String, bool),
    /// the alerts you missed and the last toasts (ctrl+n)
    Alerts,
    /// the approval board (ctrl+a)
    Approvals,
    /// Monitor › Usage for one machine or agent (None: everything): "usage b31"
    Usage(Option<super::usage::Only>),
    /// Monitor › Timeline
    Timeline,
    /// keep the sessions whose close is waiting (`close.grace`)
    Undo,
    /// the session journal, this many days back (0: today)
    Journal(i64),
    /// one of Fleet's commands, on what's selected (or the session in Focus)
    Act(super::Act),
    /// one of Focus's and Tile's commands, on the pane you're in
    Mux(super::wall::Cmd),
    /// a window of your own, by name
    Window(usize),
    /// every key there is
    Help,
}

/// Fleet's commands that have a better entry already, or make no sense from here.
const NOT_LISTED: &[super::Act] = &[
    super::Act::Up,
    super::Act::Down,
    super::Act::Open,
    super::Act::Collapse,
    super::Act::Expand,
    super::Act::Alerts,
    super::Act::Approvals,
    super::Act::Hub,
    super::Act::Refresh,
];

/// Fleet's commands that only mean something with Fleet on screen.
const NEED_FLEET: &[super::Act] = &[
    super::Act::Filter,
    super::Act::Mark,
    super::Act::Wall,
    super::Act::PrevSession,
    super::Act::NextSession,
    super::Act::PrevHost,
    super::Act::NextHost,
    super::Act::StatsMode,
    super::Act::Timeline,
    super::Act::TimelinePanel,
];

/// Does ctrl+k have an entry of its own for this command (tests)?
#[cfg(test)]
pub(crate) fn is_listed(a: super::Act) -> bool {
    !NOT_LISTED.contains(&a)
}

/// Focus's and Tile's commands, as ctrl+k lists them there.
fn mux_commands(locked: bool) -> Vec<(super::wall::Cmd, &'static str, &'static str)> {
    use super::wall::Cmd;
    vec![
        (Cmd::SplitSide, "Split side by side", "v"),
        (Cmd::SplitStack, "Split stacked", "s"),
        (Cmd::Next, "Next pane", "o · alt+arrows while typing"),
        (Cmd::Prev, "Previous pane", ";"),
        (Cmd::Close, "Close this pane", "x · the session lives on"),
        (Cmd::Zoom, "Maximise this pane, or go back", "z"),
        (
            Cmd::Lock,
            if locked {
                "Unlock this layout"
            } else {
                "Lock this layout"
            },
            "L · nothing splits, closes, swaps or resizes it",
        ),
        (Cmd::NextLayout, "Next layout", "space"),
        (Cmd::SwapNext, "Swap with the next pane", "}"),
        (Cmd::SwapPrev, "Swap with the previous pane", "{"),
        (Cmd::Equalize, "Equalise the panes", "="),
        (Cmd::Numbers, "Show pane numbers", "#"),
        (Cmd::NewWindow, "New window", "c"),
        (Cmd::NextWindow, "Next window", "w"),
        (Cmd::PrevWindow, "Previous window", "W"),
        (Cmd::Rename, "Rename this window", ","),
        (Cmd::CloseWindow, "Close this window", "&"),
        (Cmd::Type, "Type into this pane", "i"),
    ]
}

pub(crate) struct Palette {
    q: Field,
    items: Vec<(Item, String, String, Color)>,
    sel: usize,
    /// its title and what the empty search line says
    words: (&'static str, &'static str),
    /// choosing what goes in an empty pane: a session goes in it
    picker: bool,
    /// ctrl+s: this session goes beside the pane you're in
    split: bool,
    /// opened in Focus or Tile
    in_wall: bool,
    /// what each session's line shows, by its key
    rows: std::collections::HashMap<String, SessionRow>,
    /// with nothing typed, only these, in this order, under headings (the full
    /// palette; a picker just lists what it has)
    shelf: Option<Vec<usize>>,
}

/// One session's line.
#[derive(Clone)]
struct SessionRow {
    glyph: String,
    glyph_col: Color,
    name: String,
    kind: String,
    agent: String,
    model: String,
    host: String,
    host_col: Color,
    state: String,
    state_col: Color,
    pinned: bool,
    /// what it's been doing, said under the list for the one you're on
    trail: String,
}

/// Where a line sits with nothing typed.
#[derive(Clone, Copy, PartialEq)]
enum Part {
    Top,
    Pinned,
    Sessions,
    Actions,
}

/// The columns of the sessions on show.
struct Widths {
    name: usize,
    agent: usize,
    model: usize,
    host: usize,
}

/// What a pin is saved as: the machine and the session's name, which outlast
/// its pane.
fn pin_of(host: &str, s: &crate::herdr::Session) -> String {
    format!("{host}:{}", s.display())
}

fn pins(d: &Dash) -> Vec<String> {
    d.cfg
        .get_or("palette.pins")
        .split(',')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(String::from)
        .collect()
}

/// ctrl+p on a session: pinned on top of ctrl+k, or not any more. Pins of sessions
/// that have gone don't count against the three (one may come back), but
/// make room for new ones.
pub(crate) fn toggle_pin(d: &mut Dash, p: &mut Palette) {
    let Some(Item::Session(k)) = p.chosen() else {
        d.toast(super::Level::Info, "ctrl+p pins sessions");
        return;
    };
    let Some(s) = super::fleet::session(d, &k) else {
        return;
    };
    let pin = pin_of(&s.host, &s);
    let mut saved = pins(d);
    if saved.contains(&pin) {
        saved.retain(|x| *x != pin);
    } else {
        let live: Vec<String> = d
            .fleet
            .sessions()
            .iter()
            .map(|x| pin_of(&x.host, x))
            .collect();
        let (mut here, gone): (Vec<String>, Vec<String>) =
            saved.into_iter().partition(|x| live.contains(x));
        if here.len() >= MAX_PINS {
            d.toast(
                super::Level::Info,
                format!("{MAX_PINS} are pinned: ctrl+p on one of them unpins it"),
            );
            return;
        }
        here.push(pin);
        let room = MAX_PINS.saturating_sub(here.len());
        here.extend(gone.into_iter().rev().take(room));
        saved = here;
    }
    super::settings::save(d, "palette.pins", &saved.join(","));
    // (a picker keeps its list: the pin shows next time)
    if p.shelf.is_none() {
        return;
    }
    let (q, sel, words) = (p.q.clone(), p.sel, p.words);
    let at = p
        .items
        .iter()
        .position(|x| matches!(&x.0, Item::Session(j) if *j == k));
    *p = Palette::new(d);
    (p.q, p.words) = (q, words);
    // the cursor stays on it, wherever it went
    p.sel = at
        .and(
            p.ranked()
                .iter()
                .position(|(i, _)| matches!(&p.items[*i].0, Item::Session(j) if *j == k)),
        )
        .unwrap_or(sel);
}

/// ctrl+k: the palette. Sessions with no title say what's on their screen,
/// read off the UI thread, and they fill in as they answer.
pub(crate) fn open(d: &mut Dash) {
    d.mode = Mode::Palette(Palette::new(d));
    if cfg!(test) {
        return;
    }
    for h in &d.fleet.hosts {
        let want: Vec<(String, String)> = h
            .sessions
            .iter()
            .filter(|s| s.title.trim().is_empty() && s.title != "-")
            .map(|s| (s.key(), s.pane_id.clone()))
            .filter(|(k, _)| {
                d.trails
                    .get(k)
                    .is_none_or(|(at, _)| at.elapsed() > TRAIL_FRESH)
            })
            .take(TRAIL_READS)
            .collect();
        if want.is_empty() {
            continue;
        }
        let (t, tx) = (d.target_of(&h.alias), d.tx.clone());
        std::thread::spawn(move || {
            let x = crate::herdr::Herdr::new(&t);
            for (key, pane) in want {
                if let Ok(text) = x.read_pane(&pane, 12) {
                    let line = super::board::last_line(&text);
                    if !line.is_empty() {
                        let line = crate::sentlog::scrub(&line);
                        let _ = tx.send(super::Bg::Trail { key, line });
                    }
                }
            }
        });
    }
}

/// A screen's line came back: keep it, and show it if the palette's open.
pub(crate) fn on_trail(d: &mut Dash, key: String, line: String) {
    d.trails.insert(key.clone(), (Instant::now(), line));
    if !matches!(d.mode, Mode::Palette(_)) {
        return;
    }
    let s = super::fleet::session(d, &key);
    let sub = s.as_ref().map(|s| session_sub(d, s));
    let trail = s.as_ref().map(|s| trail_of(d, s)).unwrap_or_default();
    if let (Mode::Palette(p), Some(sub)) = (&mut d.mode, sub) {
        for it in &mut p.items {
            if matches!(&it.0, Item::Session(k) if *k == key) {
                it.2 = sub.clone();
            }
        }
        if let Some(r) = p.rows.get_mut(&key) {
            r.trail = trail;
        }
    }
}

/// What it's been doing: its title, the command it ran, else its screen's last line.
fn trail_of(d: &Dash, s: &crate::herdr::Session) -> String {
    let title = s.title.trim();
    if !title.is_empty() && title != "-" {
        title.to_string()
    } else if let Some(r) = d.fleet.commands_of(&s.key()).first() {
        r.cmd.clone()
    } else if let Some((_, l)) = d.trails.get(&s.key()) {
        l.clone()
    } else {
        String::new()
    }
}

/// Its state, and how long only while it works (or until a limit lifts).
fn state_of(d: &Dash, s: &crate::herdr::Session) -> (String, Color) {
    let (st, col) = super::fleet::status_for(d, s);
    let text = if s.limit.is_some() || s.status == "working" {
        st.replacen(" · ", " ", 1)
    } else {
        st.split(" · ").next().unwrap_or(&st).to_string()
    };
    (text, col)
}

/// The model it's on, for an agent that could be on any.
fn model_of(d: &Dash, s: &crate::herdr::Session) -> String {
    if !crate::agents::many_models(&s.agent) {
        return String::new();
    }
    super::usage::live_for(d, s)
        .map(|l| crate::timeline::short_model(&l.model))
        .unwrap_or_default()
}

/// The actions shown before you type: what you'd reach for here, and what's waiting.
fn on_the_shelf(item: &Item, here: Option<&str>) -> bool {
    match item {
        Item::Restart
        | Item::LeaveFocus
        | Item::StopTyping
        | Item::TerminalHere(..)
        | Item::Undo
        | Item::Help
        | Item::Act(super::Act::Prompts) => true,
        Item::NewOn(h) => here == Some(h.as_str()),
        _ => false,
    }
}

/// A session's line: its agent, machine, state, model, and what it's been
/// doing (its title, the command it ran, else its screen's last line).
fn session_sub(d: &Dash, s: &crate::herdr::Session) -> String {
    let (st, _) = super::fleet::status_for(d, s);
    let mut parts = vec![
        if s.agent == crate::agents::SHELL {
            "Terminal".to_string()
        } else {
            crate::agents::label(&s.agent)
        },
        s.host.clone(),
        st,
    ];
    let model = super::usage::live_for(d, s)
        .map(|l| crate::timeline::short_model(&l.model))
        .unwrap_or_default();
    if !model.is_empty() {
        parts.push(model);
    }
    let trail = trail_of(d, s);
    if !trail.is_empty() {
        parts.push(trail);
    }
    parts.join(" · ")
}

/// Every typed word matches on its own, in any order, and has letters of its
/// own: longest first, so in "gpu2 gpu" the machine's name doesn't answer for both.
fn word_match(q: &str, hay: &str) -> Option<(i32, Vec<usize>)> {
    let mut words: Vec<&str> = q.split_whitespace().collect();
    words.sort_by_key(|w| std::cmp::Reverse(w.chars().count()));
    let mut left: Vec<char> = hay.chars().collect();
    let (mut score, mut pos) = (0, Vec::new());
    for w in words {
        let (s, p) = fuzzy::score(w, &left.iter().collect::<String>())?;
        for &i in &p {
            left[i] = '\0';
        }
        score += s;
        pos.extend(p);
    }
    Some((score, pos))
}

impl Palette {
    /// The agents a session's work can go to (Fleet's `h`): those on its
    /// machine, the one it'd go to first.
    pub fn handoff(d: &Dash, s: &crate::herdr::Session) -> Palette {
        let first = crate::handoff::default_to(&d.cfg, &s.agent);
        let mut ids: Vec<&'static str> =
            crate::probe::cached(&s.host, std::time::Duration::from_secs(30 * 86400))
                .map(|p| p.installed().into_iter().map(|(a, _)| a.id).collect())
                .unwrap_or_else(|| vec!["claude", "codex", "opencode", "gemini"]);
        ids.retain(|a| *a != s.agent && *a != crate::agents::SHELL);
        ids.sort_by_key(|a| *a != first);
        let by = if d.cfg.get_or("handoff.summary") == "aleph" {
            "Aleph writes the note (git, its screen)"
        } else {
            "it writes its own note, then"
        };
        let items = ids
            .into_iter()
            .map(|a| {
                (
                    Item::Handoff(s.key(), a),
                    format!("Hand {} to {}", s.display(), crate::agents::label(a)),
                    format!("{by} {} starts in {}", crate::agents::label(a), s.cwd),
                    c::ACCENT(),
                )
            })
            .collect();
        Palette {
            q: Field::new(""),
            words: (
                "Go to anything",
                "sessions, machines, views, @presets, folders…",
            ),
            items,
            sel: 0,
            picker: false,
            split: false,
            in_wall: false,
            rows: Default::default(),
            shelf: None,
        }
    }

    /// Only presets (Fleet's `P`).
    pub fn presets(d: &Dash) -> Palette {
        let mut p = Palette::new(d);
        p.items
            .retain(|(it, _, _, _)| matches!(it, Item::Preset(_)));
        p.shelf = None;
        p
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Only sessions (Focus / Tile `a` in an empty pane: pick one for it).
    pub fn sessions_only(d: &Dash) -> Palette {
        let mut p = Palette::new(d);
        p.items
            .retain(|(it, _, _, _)| matches!(it, Item::Session(_)));
        p.shelf = None;
        // or something new in it, on any machine
        if d.view == View::Wall {
            p.picker = true;
            p.words = (
                "What goes in this pane",
                "a session on any machine, a new agent, a terminal…",
            );
            p.items.push((
                Item::PaneNew,
                "New agent in this pane…".into(),
                "any machine, any agent, any folder".into(),
                c::ACCENT(),
            ));
            for h in &d.fleet.hosts {
                p.items.push((
                    Item::PaneShell(h.alias.clone()),
                    format!("Terminal on {}", h.alias),
                    "a plain shell in this pane".into(),
                    c::ACCENT(),
                ));
            }
        }
        p
    }

    pub fn new(d: &Dash) -> Palette {
        let mut items = Vec::new();
        if d.new_build {
            items.push((
                Item::Restart,
                "Restart on the new build".into(),
                "a newer Aleph is installed — this one is the old build".into(),
                c::ACCENT(),
            ));
        }
        // where you are, first: in Focus or Tile the ways out, in Fleet
        // Focus on the selected session
        if d.view == View::Wall && (d.wall.in_focus() || d.wall.in_tile()) {
            let (what, back) = (
                if d.wall.in_focus() { "Focus" } else { "Tile" },
                d.wall.back.unwrap_or(View::Fleet),
            );
            let key = super::wall::exit_key(d).map_or_else(String::new, |k| format!(" ({k})"));
            items.push((
                Item::LeaveFocus,
                format!("Leave {what}"),
                format!("back to {}{key}", back.name()),
                c::ACCENT(),
            ));
            if d.wall.typing {
                items.push((
                    Item::StopTyping,
                    "Keys back to Aleph".into(),
                    format!(
                        "stop typing into the agent, {what} stays ({})",
                        super::wall::keys_back(d)
                    ),
                    c::ACCENT(),
                ));
            }
        } else if d.view == View::Fleet {
            if let Some(s) = super::fleet::selected_session(d) {
                items.push((
                    Item::Focus(s.key()),
                    format!("Focus {}", s.display()),
                    format!("its terminal here, keys straight to it · {}", s.host),
                    c::ACCENT(),
                ));
            }
        }
        // a terminal where this session is, from wherever you see it
        let here = if d.view == View::Wall {
            super::wall::focused_session(d)
        } else if d.view == View::Fleet {
            super::fleet::selected_session(d)
        } else {
            None
        };
        if let Some(s) = here {
            let dir = Some(s.cwd.clone()).filter(|c| c.starts_with('/'));
            items.push((
                Item::TerminalHere(s.host.clone(), dir.clone()),
                format!(
                    "Terminal in {}",
                    dir.as_deref().unwrap_or("this machine's home")
                ),
                format!("on {} (!)", s.host),
                c::ACCENT(),
            ));
        }
        // pinned sessions on top of everything, in the order they were pinned
        let pinned = pins(d);
        let mut on_top: Vec<(usize, (Item, String, String, Color))> = Vec::new();
        let mut session_rows = std::collections::HashMap::new();
        for h in &d.fleet.hosts {
            for s in &h.sessions {
                let (_, col) = super::fleet::status_for(d, s);
                let at = pinned.iter().position(|p| *p == pin_of(&h.alias, s));
                let mark = if at.is_some() { "⚑ " } else { "" };
                let (state, state_col) = state_of(d, s);
                session_rows.insert(
                    s.key(),
                    SessionRow {
                        glyph: s.glyph().to_string(),
                        glyph_col: s.color(),
                        name: s.display().to_string(),
                        kind: s.agent.clone(),
                        agent: crate::agents::short_label(&s.agent),
                        model: model_of(d, s),
                        host: h.alias.clone(),
                        host_col: h.color,
                        state,
                        state_col,
                        pinned: at.is_some(),
                        trail: trail_of(d, s),
                    },
                );
                let entry = (
                    Item::Session(s.key()),
                    format!("{mark}{} {}", s.glyph(), s.display()),
                    session_sub(d, s),
                    col,
                );
                match at {
                    Some(i) => on_top.push((i, entry)),
                    None => items.push(entry),
                }
            }
        }
        on_top.sort_by_key(|x| x.0);
        items.splice(0..0, on_top.into_iter().map(|x| x.1));
        for h in &d.fleet.hosts {
            items.push((
                Item::Host(h.alias.clone()),
                format!("▌{}", h.alias),
                "machine".into(),
                h.color,
            ));
            items.push((
                Item::NewOn(h.alias.clone()),
                format!("New session on {}", h.alias),
                "n · where, then which agent or a terminal".into(),
                c::ACCENT(),
            ));
            items.push((
                Item::StartIn(h.alias.clone()),
                format!(
                    "Start {} in a folder on {}…",
                    crate::agents::default_agent_for(&d.cfg, &h.alias).name,
                    h.alias
                ),
                "action".into(),
                c::ACCENT(),
            ));
        }
        for v in View::ALL {
            items.push((
                Item::View(v),
                format!("Go to {}", v.name()),
                "view".into(),
                c::BLUE(),
            ));
        }
        // Monitor's pages: usage, timeline, … and each machine's own (typing
        // its name lists them: "b31 gpu")
        use super::monitor_draw::Sub;
        let pages = super::monitor::pages(d);
        for s in pages.iter().copied() {
            items.push((
                Item::Page(None, s),
                format!("Monitor › {}", s.name()),
                "view".into(),
                c::BLUE(),
            ));
        }
        for h in &d.fleet.hosts {
            for s in [
                Sub::Overview,
                Sub::Cpu,
                Sub::Memory,
                Sub::Gpu,
                Sub::Procs,
                Sub::Net,
                Sub::Services,
            ]
            .into_iter()
            .filter(|s| pages.contains(s))
            {
                items.push((
                    Item::Page(Some(h.alias.clone()), s),
                    format!("{} › {}", h.alias, s.name()),
                    "machine page".into(),
                    h.color,
                ));
            }
        }
        // usage for one machine, or one agent: "usage b31", "usage claude"
        for h in &d.fleet.hosts {
            items.push((
                Item::Usage(Some(super::usage::Only::Host(h.alias.clone()))),
                format!("Monitor › Usage on {}", h.alias),
                format!("tokens, cost and time on {}", h.alias),
                h.color,
            ));
        }
        let mut agents: Vec<String> = d
            .fleet
            .sessions()
            .iter()
            .map(|s| s.agent.clone())
            .chain(d.usage.data.iter().flat_map(|x| x.series.iter().cloned()))
            .filter(|a| crate::agents::find(a).is_some())
            .collect();
        agents.sort();
        agents.dedup();
        for a in agents {
            items.push((
                Item::Usage(Some(super::usage::Only::Agent(a.clone()))),
                format!("Monitor › Usage of {} ({a})", crate::agents::label(&a)),
                format!(
                    "{}'s tokens, cost and time on every machine",
                    crate::agents::label(&a)
                ),
                c::BLUE(),
            ));
        }
        if d.usage.only.is_some() {
            items.push((
                Item::Usage(None),
                "Monitor › Usage of everything".into(),
                "every machine, every agent".into(),
                c::BLUE(),
            ));
        }
        for h in &d.fleet.hosts {
            items.push((
                Item::QueueOn(h.alias.clone()),
                format!("Queue a task on {}", h.alias),
                "the next idle agent there picks it up".into(),
                c::BLUE(),
            ));
        }
        for (key, what) in [
            ("auto_approve", "Auto-approve"),
            ("auto_heal", "Self-heal herdr"),
        ] {
            let on = d.cfg.flag(key);
            items.push((
                Item::Toggle(key),
                format!("{what}: turn {}", if on { "off" } else { "on" }),
                if key == "auto_approve" {
                    format!("{} rule(s) · aleph auto", crate::auto::rules(&d.cfg).len())
                } else {
                    "restart herdr where it stopped and restore sessions".into()
                },
                c::ACCENT(),
            ));
        }
        // agents outside herdr: into herdr, or stopped
        for h in d
            .fleet
            .hosts
            .iter()
            .map(|h| h.alias.clone())
            .collect::<Vec<_>>()
        {
            for o in super::outside::list(d, &h) {
                let label = crate::agents::label(&o.p.agent);
                let wher = o.cwd.clone().unwrap_or_default();
                items.push((
                    Item::Outside(o.key(), true),
                    format!("Reopen {label} (pid {}) on {h} in herdr", o.p.pid),
                    format!("same folder, same conversation · {wher}"),
                    c::ACCENT(),
                ));
                items.push((
                    Item::Outside(o.key(), false),
                    format!("Stop {label} (pid {}) on {h}", o.p.pid),
                    format!("it runs outside herdr · {wher}"),
                    c::RED(),
                ));
            }
        }
        // races: start one where the selected session is, or see one
        items.push((
            Item::Race(String::new()),
            "Start a race…".into(),
            format!(
                "one prompt to {} at once, each in its own worktree of the selected session's folder",
                crate::race::default_agents(&d.cfg).join(", ")
            ),
            c::BLUE(),
        ));
        for r in crate::race::all().into_iter().take(5) {
            items.push((
                Item::Race(r.id.clone()),
                format!("Race {}: {}", r.id, r.short()),
                r.racers
                    .iter()
                    .map(|x| x.name.clone())
                    .collect::<Vec<_>>()
                    .join(" vs "),
                c::BLUE(),
            ));
        }
        items.push((
            Item::Gallery,
            "Theme gallery".into(),
            "every theme and accent, live: enter keeps one".into(),
            c::ACCENT(),
        ));
        items.push((
            Item::Splashes,
            "Splash gallery".into(),
            "every splash, previewed live: enter keeps one, p plays it full screen".into(),
            c::ACCENT(),
        ));
        // saved workspaces
        for w in crate::workspace::all(&d.cfg) {
            items.push((
                Item::Workspace(w.name.clone()),
                format!("Workspace: {}", w.title),
                w.what(),
                c::BLUE(),
            ));
        }
        items.push((
            Item::Workspace(String::new()),
            "Workspaces…".into(),
            "W: go to one, save where you are now".into(),
            c::BLUE(),
        ));
        // settings profiles: switch to another, or back to your own
        let using = super::profile_of(&d.cfg);
        if let Some(p) = &using {
            items.push((
                Item::Profile(String::new()),
                format!("Profile: {p} off"),
                "your own settings again".into(),
                c::ACCENT(),
            ));
        }
        for (n, _) in crate::profile::names() {
            if using.as_deref() != Some(n.as_str()) {
                let about = crate::profile::read(&n)
                    .map(|p| p.about)
                    .ok()
                    .filter(|a| !a.is_empty())
                    .or_else(|| {
                        crate::profile::STARTERS
                            .iter()
                            .find(|s| s.0 == n)
                            .map(|s| s.1.to_string())
                    })
                    .unwrap_or_default();
                items.push((
                    Item::Profile(n.clone()),
                    format!("Profile: use {n}"),
                    about,
                    c::ACCENT(),
                ));
            }
        }
        // closes in their grace: keep them
        let closing: Vec<crate::closing::Entry> = crate::closing::current()
            .into_iter()
            .filter(|e| e.claimed.is_none())
            .collect();
        if !closing.is_empty() {
            let now = crate::closing::now_ms();
            items.push((
                Item::Undo,
                format!(
                    "Undo close: {}",
                    closing
                        .iter()
                        .map(|e| e.display.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!(
                    "{} · keep the last close (closing in {}s; the agent is still running)",
                    d.keys.key_for(super::Act::Undo),
                    closing.iter().map(|e| e.left(now)).min().unwrap_or(0)
                ),
                c::PEACH(),
            ));
        }
        let waiting = d.fleet.sessions().iter().filter(|s| s.blocked()).count();
        items.push((
            Item::Approvals,
            if waiting == 0 {
                "Approvals: nothing waiting".to_string()
            } else {
                format!("Approvals: {waiting} waiting on you")
            },
            format!(
                "{} · every question as its agent shows it — y approve · n deny · 1–9 · Y all",
                d.keys.key_for(super::Act::Approvals)
            ),
            c::NEEDS(),
        ));
        items.push((
            Item::Alerts,
            "Alerts: missed and recent toasts".into(),
            format!(
                "{} · what the status bar said, and what came while you looked away",
                d.keys.key_for(super::Act::Alerts)
            ),
            c::ACCENT(),
        ));
        // the session journal (journal.include off: there's none)
        if crate::journal::include(&d.cfg).any() {
            for (back, label) in [(0, "today"), (1, "yesterday")] {
                items.push((
                    Item::Journal(back),
                    format!("Journal: {label}"),
                    "what each session did: prompts, final words, files, time and $ (aleph journal)"
                        .into(),
                    c::ACCENT(),
                ));
            }
        }
        items.push((
            Item::Timeline,
            "Timeline of every session".into(),
            "Monitor › Timeline: working, needs you, done and idle over the last hours".into(),
            c::BLUE(),
        ));
        let rows = d.cfg.flag("fleet.timeline");
        items.push((
            Item::Toggle("fleet.timeline"),
            format!(
                "Fleet's timeline rows: turn {}",
                if rows { "off" } else { "on" }
            ),
            "g in Fleet: each session's states in its row".into(),
            c::ACCENT(),
        ));
        items.push((
            Item::News,
            "What's new".into(),
            "what changed in recent updates".into(),
            c::ACCENT(),
        ));
        // showcase mode, from inside the app (only a build with the feature lists any)
        let now = crate::showcase::mode_name();
        for (m, label, what) in crate::showcase::modes() {
            let current = *m == now || (*m == "off" && now == "real");
            if !current {
                items.push((
                    Item::Showcase(m),
                    label.to_string(),
                    what.to_string(),
                    c::ACCENT(),
                ));
            }
        }
        // ... and its capture: record the screen, a poster of it
        for (id, label, what) in crate::showcase::captures(&d.cfg) {
            items.push((Item::Capture(id), label, what, c::ACCENT()));
        }
        items.push((
            Item::Refresh,
            "Refresh every machine".into(),
            "action".into(),
            c::ACCENT(),
        ));
        items.push((
            Item::Hub,
            "Open the herdr hub".into(),
            "action".into(),
            c::ACCENT(),
        ));
        for p in d.cfg.presets() {
            let what = format!(
                "preset · {}{}",
                p.host.clone().unwrap_or_default(),
                p.dir
                    .as_deref()
                    .map(|x| format!(" {x}"))
                    .unwrap_or_default()
            );
            items.push((
                Item::Preset(p.name.clone()),
                format!("@{}", p.name),
                what,
                c::MAUVE(),
            ));
        }
        for r in history::recent_dirs(None).into_iter().take(20) {
            items.push((
                Item::Folder(r.host.clone(), r.dir.clone()),
                r.dir.clone(),
                format!("recent folder on {} → new session", r.host),
                c::TEAL(),
            ));
        }
        // your windows, by name, from wherever you are
        for (i, w) in d.wall.windows.iter().enumerate() {
            let shown = match w.name.as_str() {
                super::wall::FOCUS => "Focus".to_string(),
                super::wall::TILE => "Tile".to_string(),
                n => n.to_string(),
            };
            items.push((
                Item::Window(i),
                format!("Window {shown}"),
                super::wall::window_names(d, i).join(", "),
                c::BLUE(),
            ));
        }
        // the pane you're in: everything its keys do
        if d.view == View::Wall {
            for (cmd, title, key) in mux_commands(super::wall::is_locked(d)) {
                items.push((Item::Mux(cmd), title.into(), key.into(), c::ACCENT()));
            }
        }
        // every command there is, with the key it has
        for (a, _, _, help) in super::ACTS {
            if NOT_LISTED.contains(a) {
                continue;
            }
            let mut title = help.to_string();
            if let Some(first) = title.get_mut(0..1) {
                first.make_ascii_uppercase();
            }
            // (the one that's on the short list says it shortly)
            let (title, what) = if *a == super::Act::Prompts {
                (
                    "Prompts…".to_string(),
                    format!("{} · browse, use, edit and save them", d.keys.key_for(*a)),
                )
            } else {
                (
                    title,
                    format!("{} · on the selected session", d.keys.key_for(*a)),
                )
            };
            items.push((Item::Act(*a), title, what, c::MUTED()));
        }
        items.push((
            Item::Help,
            "Every key".into(),
            "? · searchable, by view".into(),
            c::MUTED(),
        ));
        // with nothing typed: what you pinned, your sessions, a few actions
        let here_host =
            d.fv.selected_host(d)
                .or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone()));
        let waiting_now = d.fleet.sessions().iter().any(|s| s.blocked());
        let of = |want: &dyn Fn(&Item) -> bool| -> Vec<usize> {
            items
                .iter()
                .enumerate()
                .filter(|(_, it)| want(&it.0))
                .map(|(i, _)| i)
                .collect()
        };
        let is_pinned = |it: &Item| matches!(it, Item::Session(k) if session_rows.get(k).is_some_and(|r: &SessionRow| r.pinned));
        let top = |it: &Item| matches!(it, Item::Restart | Item::LeaveFocus | Item::StopTyping);
        let mut shelf = of(&top);
        shelf.extend(of(&is_pinned));
        // (by machine, as the fleet lists them)
        for h in &d.fleet.hosts {
            shelf.extend(of(&|it| {
                !is_pinned(it)
                    && matches!(it, Item::Session(k) if session_rows.get(k).is_some_and(|r| r.host == h.alias))
            }));
        }
        shelf.extend(of(&|it| {
            !top(it)
                && (on_the_shelf(it, here_host.as_deref())
                    || (waiting_now && matches!(it, Item::Approvals)))
        }));
        // the cursor starts on the session you're on: enter is Focus on it
        let sel = match (&d.fv.sel, d.view) {
            (Some(RowId::Sess(k)), View::Fleet) => shelf
                .iter()
                .position(|i| matches!(&items[*i].0, Item::Session(x) if x == k))
                .unwrap_or(0),
            _ => 0,
        };
        Palette {
            q: Field::default(),
            words: (
                "Go to anything",
                "sessions, commands, machines, views, @presets, folders…",
            ),
            items,
            sel,
            picker: false,
            split: false,
            in_wall: d.view == View::Wall,
            rows: session_rows,
            shelf: Some(shelf),
        }
    }

    /// Where this item sits with nothing typed.
    fn part_of(&self, i: usize) -> Part {
        match &self.items[i].0 {
            Item::Restart | Item::LeaveFocus | Item::StopTyping => Part::Top,
            Item::Session(k) if self.rows.get(k).is_some_and(|r| r.pinned) => Part::Pinned,
            Item::Session(_) => Part::Sessions,
            _ => Part::Actions,
        }
    }

    /// The columns for the sessions in this list, in a line `room` wide.
    fn widths(&self, ranked: &[(usize, Vec<usize>)], room: usize, hosts: bool) -> Widths {
        let shown: Vec<&SessionRow> = ranked
            .iter()
            .filter_map(|(i, _)| match &self.items[*i].0 {
                Item::Session(k) => self.rows.get(k),
                _ => None,
            })
            .collect();
        let most = |f: &dyn Fn(&SessionRow) -> usize| shown.iter().map(|r| f(r)).max().unwrap_or(0);
        let mut w = Widths {
            name: most(&|r| ui::width(&r.name)).min(MAX_NAME),
            agent: most(&|r| ui::width(&r.agent)),
            model: most(&|r| ui::width(&r.model)).min(18),
            host: if hosts {
                most(&|r| ui::width(&r.host) + 2)
            } else {
                0
            },
        };
        let state = most(&|r| ui::width(&r.state));
        // the bar and glyph, the agent's icon, the gaps
        let fixed = 4 + 2 + 2 + 2 + 2 + 2;
        let need = |w: &Widths| fixed + w.name + w.agent + w.model + w.host + state;
        if need(&w) > room {
            w.model = 0;
        }
        if need(&w) > room {
            w.name = w.name.saturating_sub(need(&w) - room).max(8);
        }
        w
    }

    /// A session's line.
    fn session_line(
        &self,
        r: &SessionRow,
        title_len: usize,
        pos: &[usize],
        w: &Widths,
        on: bool,
    ) -> Line<'static> {
        let mut l = vec![
            Span::styled(if on { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(
                if r.pinned && w.host > 0 {
                    "⚑ ".to_string()
                } else {
                    format!("{} ", r.glyph)
                },
                ui::fg(r.glyph_col),
            ),
        ];
        // the name, with what matched picked out (the hay starts with the title)
        let name = ui::pad(&ui::trunc(&r.name, w.name), w.name);
        let from = title_len.saturating_sub(r.name.chars().count());
        for (ci, ch) in name.chars().enumerate() {
            let st = if pos.contains(&(from + ci)) {
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
            } else if on {
                ui::bold()
            } else {
                Style::new()
            };
            l.push(Span::styled(ch.to_string(), st));
        }
        l.push(Span::raw("  "));
        l.push(crate::icons::agent(&r.kind));
        l.push(Span::styled(
            format!(" {}", ui::pad(&r.agent, w.agent)),
            ui::muted(),
        ));
        if w.model > 0 {
            l.push(Span::styled(
                format!("  {}", ui::pad(&ui::trunc(&r.model, w.model), w.model)),
                ui::fg(c::ACCENT()),
            ));
        }
        if w.host > 0 {
            l.push(Span::styled(
                format!("  {}", ui::pad(&format!("[{}]", r.host), w.host)),
                ui::fg(r.host_col),
            ));
        }
        l.push(Span::styled(format!("  {}", r.state), ui::fg(r.state_col)));
        let line = Line::from(l);
        if on {
            line.style(ui::selected())
        } else {
            line
        }
    }

    fn ranked(&self) -> Vec<(usize, Vec<usize>)> {
        let q = self.q.text.trim();
        if let (true, Some(shelf)) = (q.is_empty(), &self.shelf) {
            return shelf.iter().map(|i| (*i, Vec::new())).collect();
        }
        let mut v: Vec<(i32, usize, Vec<usize>)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let hay = format!("{} {}", it.1, it.2);
                // about the same match: the session, ahead of an action naming it
                let lead = if matches!(it.0, Item::Session(_)) {
                    SESSION_LEAD
                } else {
                    0
                };
                word_match(q, &hay).map(|(s, pos)| (s + lead, i, pos))
            })
            .collect();
        if !q.is_empty() {
            v.sort_by_key(|x| (-x.0, x.1));
        }
        v.into_iter().map(|x| (x.1, x.2)).collect()
    }

    pub fn key(&mut self, k: KeyEvent) -> Res {
        match k.code {
            KeyCode::Esc => Res::Cancel,
            KeyCode::Enter => Res::Submit,
            KeyCode::Char('s')
                if k.modifiers.contains(KeyModifiers::CONTROL)
                    && self.in_wall
                    && matches!(self.chosen(), Some(Item::Session(_))) =>
            {
                self.split = true;
                Res::Submit
            }
            KeyCode::Down | KeyCode::Tab => {
                self.sel = (self.sel + 1).min(self.ranked().len().saturating_sub(1));
                Res::Keep
            }
            KeyCode::Up | KeyCode::BackTab => {
                self.sel = self.sel.saturating_sub(1);
                Res::Keep
            }
            _ => {
                if self.q.key(&k) {
                    self.sel = 0;
                }
                Res::Keep
            }
        }
    }

    /// Is this command listed (tests)?
    #[cfg(test)]
    pub(crate) fn has_act(&self, a: super::Act) -> bool {
        self.items
            .iter()
            .any(|x| matches!(x.0, Item::Act(y) if y == a))
    }

    /// Put the cursor on a session (tests).
    #[cfg(test)]
    pub(crate) fn select_session(&mut self, key: &str) {
        if let Some(i) = self
            .ranked()
            .iter()
            .position(|(i, _)| matches!(&self.items[*i].0, Item::Session(k) if k == key))
        {
            self.sel = i;
        }
    }

    /// Put the cursor on a preset (`:preset name`).
    pub fn choose_preset(&mut self, name: &str) {
        let r = self.ranked();
        if let Some(i) = r
            .iter()
            .position(|(i, _)| matches!(&self.items[*i].0, Item::Preset(n) if n == name))
        {
            self.sel = i;
        }
    }

    pub fn chosen(&self) -> Option<Item> {
        let r = self.ranked();
        r.get(self.sel).map(|(i, _)| self.items[*i].0.clone())
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, _t0: Instant) {
        let w = 96u16.min(area.width.saturating_sub(2));
        let h = 20u16.min(area.height.saturating_sub(2));
        let r = Rect {
            x: area.x + (area.width.saturating_sub(w)) / 2,
            y: area.y + 2,
            width: w,
            height: h,
        };
        crate::ui::clear(f, r);
        let block = ui::panel(self.words.0, true);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines = Vec::new();
        let mut ql = vec![Span::styled(
            "› ",
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        )];
        ql.extend(self.q.spans(true, self.words.1));
        lines.push(Line::from(ql));
        lines.push(Line::from(Span::styled(
            "─".repeat(inner.width as usize),
            ui::fg(c::LINE()),
        )));
        // the search, the rule, what the one you're on has been doing, the keys
        let room = inner.height.saturating_sub(4) as usize;
        let ranked = self.ranked();
        let shelf = self.shelf.is_some() && self.q.text.trim().is_empty();
        let widths = self.widths(&ranked, inner.width as usize, !shelf);
        let mut body: Vec<Line> = Vec::new();
        let mut at = 0;
        let mut part: Option<Part> = None;
        let mut host: Option<String> = None;
        for (n, (i, pos)) in ranked.iter().enumerate() {
            let (item, title, sub, col) = &self.items[*i];
            let on = n == self.sel;
            let session = match item {
                Item::Session(k) => self.rows.get(k),
                _ => None,
            };
            if shelf {
                let this = self.part_of(*i);
                if part != Some(this) {
                    part = Some(this);
                    host = None;
                    let word = match this {
                        Part::Top => None,
                        Part::Pinned => Some("PINNED"),
                        Part::Sessions => Some("SESSIONS"),
                        Part::Actions => Some("ACTIONS"),
                    };
                    if let Some(word) = word {
                        body.push(Line::from(Span::styled(
                            format!(" {word}"),
                            ui::muted().add_modifier(Modifier::BOLD),
                        )));
                    }
                }
                // under SESSIONS, a heading for each machine
                if let (Part::Sessions, Some(r)) = (this, session) {
                    if host.as_deref() != Some(r.host.as_str()) {
                        host = Some(r.host.clone());
                        body.push(Line::from(vec![
                            Span::styled(" ▌", ui::fg(r.host_col)),
                            Span::styled(r.host.clone(), ui::fg(r.host_col)),
                        ]));
                    }
                }
            }
            if on {
                at = body.len();
            }
            if let Some(r) = session {
                body.push(self.session_line(r, title.chars().count(), pos, &widths, on));
                continue;
            }
            let mut l = vec![Span::styled(
                if on { "┃ " } else { "  " },
                ui::fg(c::ACCENT()),
            )];
            // highlight matched letters in the title
            let tl = title.chars().count();
            for (ci, ch) in title.chars().enumerate() {
                let hit = pos.contains(&ci);
                // (a leading mark, ▌ or @, takes the entry's colour; words don't)
                let mut st = if ci == 0 && !ch.is_alphanumeric() {
                    ui::fg(*col)
                } else {
                    Style::new()
                };
                if on {
                    st = st.add_modifier(Modifier::BOLD);
                }
                if hit {
                    st = st
                        .fg(c::ACCENT())
                        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                l.push(Span::styled(ch.to_string(), st));
            }
            let used = 2 + tl;
            let rest = (inner.width as usize).saturating_sub(used + 3);
            if rest > 4 {
                l.push(Span::styled("   ", ui::faint()));
                // matched letters show here too: the hay is the title, a space, this
                for (ci, ch) in ui::trunc(sub, rest).chars().enumerate() {
                    let st = if pos.contains(&(tl + 1 + ci)) {
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                    } else {
                        ui::faint()
                    };
                    l.push(Span::styled(ch.to_string(), st));
                }
            }
            let mut line = Line::from(l);
            if on {
                line = line.style(ui::selected());
            }
            body.push(line);
        }
        // the one you're on stays in view
        let start = (at + 1).saturating_sub(room);
        lines.extend(body.into_iter().skip(start).take(room));
        if ranked.is_empty() {
            lines.push(Line::from(Span::styled("  nothing matches", ui::muted())));
        }
        f.render_widget(Paragraph::new(lines), inner);
        // what the one you're on has been doing
        let trail = match self.chosen() {
            Some(Item::Session(k)) => self.rows.get(&k).map(|r| r.trail.clone()),
            _ => None,
        }
        .unwrap_or_default();
        if !trail.is_empty() {
            let line = Rect {
                y: inner.y + inner.height.saturating_sub(2),
                height: 1,
                ..inner
            };
            f.render_widget(
                Paragraph::new(Span::styled(
                    format!(
                        "  {}",
                        ui::trunc(&trail, inner.width.saturating_sub(4) as usize)
                    ),
                    ui::faint(),
                )),
                line,
            );
        }
        // what the keys do on what's under the cursor
        let on_session = matches!(self.chosen(), Some(Item::Session(_)));
        let mut hints = vec![("↑↓", "pick"), ("enter", "go")];
        if on_session {
            hints.push(("ctrl+p", "pin"));
            if self.in_wall && !self.picker {
                hints.push(("ctrl+s", "add as a split"));
            }
        }
        hints.push(("esc", "close"));
        let foot = Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..inner
        };
        f.render_widget(Paragraph::new(ui::hints_fit(&hints, foot.width)), foot);
    }
}

pub(crate) fn run(d: &mut Dash, p: Palette) -> Option<Handoff> {
    match p.chosen()? {
        // the picker's pane, or ctrl+s: beside the pane you're in
        Item::Session(k) if p.picker || p.split => super::wall::add(d, k),
        // in Focus or Tile a session is somewhere to go
        Item::Session(k) if d.view == View::Wall => super::wall::jump_to(d, &k),
        Item::Act(a) => {
            // in Focus, it acts on the session on screen
            if d.view == View::Wall {
                if let Some(s) = super::wall::focused_session(d) {
                    d.fv.sel = Some(RowId::Sess(s.key()));
                }
            }
            if NEED_FLEET.contains(&a) {
                d.switch(View::Fleet);
            }
            return super::fleet::run_act(d, a);
        }
        Item::Mux(cmd) => super::wall::run(d, cmd),
        Item::Window(i) => super::wall::go_to_window(d, i),
        Item::Help => d.mode = Mode::Help(super::help::Help::default()),
        Item::TerminalHere(h, dir) => super::quick::shell(d, &h, dir.as_deref()),
        Item::PaneNew => super::wall::new_in_pane(d, None),
        Item::PaneShell(h) => super::wall::new_in_pane(d, Some(h)),
        // a session: what enter does in Fleet (Focus, unless enter_opens says otherwise)
        Item::Session(k) => {
            let from = d.view;
            d.switch(View::Fleet);
            d.fv.sel = Some(RowId::Sess(k.clone()));
            if d.cfg.get_or("enter_opens") == "focus" {
                super::wall::focus_on(d, &k, from);
            }
        }
        Item::Focus(k) => {
            let from = d.view;
            super::wall::focus_on(d, &k, from);
        }
        Item::LeaveFocus => super::wall::exit(d),
        Item::Restart => return Some(Handoff::Restart),
        Item::StopTyping => super::wall::stop_typing(d),
        Item::Host(h) => {
            d.switch(View::Fleet);
            if let Some(x) = d.fleet.host_mut(&h) {
                x.collapsed = false;
            }
            d.fv.sel = Some(RowId::Host(h));
        }
        Item::View(v) => d.switch(v),
        Item::Page(host, s) => {
            d.switch(View::Monitor);
            if let Some(i) = host
                .as_ref()
                .and_then(|h| d.fleet.hosts.iter().position(|x| x.alias == *h))
            {
                d.mon.sel = i;
            }
            super::monitor::set_sub(d, s);
        }
        Item::Refresh => {
            d.fleet.refresh(None);
            d.fv.invalidate_preview();
        }
        Item::Hub => return Some(Handoff::Hub),
        Item::QueueOn(h) => {
            d.mode = Mode::Input(super::forms::Input::new(
                format!("Queue a task on {h}"),
                "the next idle agent on this machine takes it",
                String::new(),
                super::forms::For::Queue(h, None),
            ))
        }
        Item::Toggle(key) => {
            let on = !d.cfg.flag(key);
            super::settings::save(d, key, if on { "true" } else { "false" });
        }
        Item::Showcase(m) => match crate::showcase::switch(m) {
            Ok(msg) => d.toast(super::Level::Ok, msg),
            Err(e) => d.error(e),
        },
        Item::Capture(id) => crate::showcase::capture(&d.cfg, id),
        Item::News => super::news::open(d),
        Item::Undo => super::batch::undo(d),
        Item::Gallery => super::gallery::open(d),
        Item::Splashes => super::splashes::open(d),
        Item::Outside(key, true) => super::outside::ask_reopen(d, &key),
        Item::Outside(key, false) => {
            d.switch(View::Fleet);
            d.fv.sel = Some(RowId::Outside(key));
            return super::fleet::on_key(d, KeyEvent::from(KeyCode::Char('x')));
        }
        Item::Race(id) if id.is_empty() => match super::fleet::selected_session(d) {
            Some(s) if !s.cwd.is_empty() => {
                d.mode = Mode::Input(super::forms::Input::new(
                    format!("Race in {}", s.cwd),
                    &format!(
                    "the prompt {} all get, each in a worktree of its own (ctrl+p: your prompts)",
                    crate::race::default_agents(&d.cfg).join(", ")
                ),
                    String::new(),
                    super::forms::For::Race(s.host.clone(), s.cwd.clone()),
                ))
            }
            _ => d.toast(
                super::Level::Info,
                "select a session in the repository to race in, then Start a race…",
            ),
        },
        Item::Race(id) => {
            if let Some(r) = crate::race::get(&id) {
                super::race::open(d, r);
            }
        }
        Item::Handoff(key, to) => {
            let s = d
                .fleet
                .sessions()
                .into_iter()
                .find(|s| s.key() == key)
                .cloned()?;
            let t = d.target_of(&s.host);
            let cfg = d.cfg.clone();
            let by_agent = cfg.get_or("handoff.summary") != "aleph";
            d.toast(
                super::Level::Info,
                format!(
                    "handing {} to {}{}",
                    s.display(),
                    crate::agents::label(to),
                    if by_agent && !crate::handoff::at_limit(&s) {
                        " · asking for its note first (up to 6 min)"
                    } else {
                        ""
                    }
                ),
            );
            let host = s.host.clone();
            d.spawn(Some(host), move || {
                crate::handoff::run(&cfg, &t, &s, to, by_agent, &mut |_| {}).map(|done| {
                    let new = done
                        .started
                        .first()
                        .map(|x| x.name.clone())
                        .unwrap_or_default();
                    format!(
                        "{new} took over from {}{}",
                        s.display(),
                        done.fell_back
                            .map(|w| format!(" (Aleph's note: {w})"))
                            .unwrap_or_default()
                    )
                })
            });
        }
        Item::Alerts => super::alerts::open(d),
        Item::Approvals => super::approvals::open(d),
        Item::Usage(only) => super::usage::show_only(d, only),
        Item::Journal(back) => {
            let today = crate::ui::local_day(crate::cache::now());
            super::journal::open(d, today - back);
        }
        Item::Timeline => {
            d.switch(View::Monitor);
            d.mon.sub = super::monitor_draw::Sub::Timeline;
        }
        Item::Workspace(n) if n.is_empty() => super::spaces::open(d),
        Item::Workspace(n) => {
            if let Some(w) = crate::workspace::find(&d.cfg, &n) {
                super::spaces::apply(d, &w);
            }
        }
        Item::Profile(n) => {
            let r = if n.is_empty() {
                crate::profile::off()
            } else {
                crate::profile::use_profile(&n)
            };
            match r {
                Ok(m) => {
                    d.settings_moved(true);
                    d.toast(super::Level::Ok, m);
                }
                Err(e) => d.error(e),
            }
        }
        Item::NewOn(h) => super::quick::open(d, &h),
        Item::StartIn(h) => {
            return Some(Handoff::Browse {
                target: d.target_of(&h),
                quick: None,
                terminal: false,
            })
        }
        Item::Folder(h, dir) => {
            d.mode = Mode::NewForm(super::forms::NewForm::new(d, &h, Some(&dir)))
        }
        Item::Preset(name) => {
            let parsed = crate::cli::parse_new(&[format!("@{name}")], &d.cfg);
            match parsed {
                Ok(mut a) => {
                    let host = a
                        .host
                        .clone()
                        .or_else(|| d.fv.selected_host(d))
                        .or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone()))?;
                    a.opts.live = d.fleet.live_names();
                    crate::cli::fill_names(&mut a.opts);
                    return Some(Handoff::New {
                        target: d.target_of(&host),
                        opts: a.opts,
                    });
                }
                Err(e) => d.error(e),
            }
        }
    }
    None
}
