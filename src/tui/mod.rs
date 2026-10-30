//! The master dashboard (bare `claude`): every machine in the fleet in one
//! view, plus the wall, hosts, history, settings and doctor.
//!
//! * `mod.rs` — state, the event loop, background results, header / tabs /
//!   footer / toasts, key map and mouse, and `run_dashboard` (the hand-off loop
//!   for anything that needs the real terminal).
//! * one file per view (`fleet`, `wall`, `hosts`, `history`, `settings`,
//!   `doctor`) and per popup (`palette`, `help`, `forms`).

// ctrl+n: the alerts you missed and the last 20 toasts
mod alerts;
mod ambience;
mod approvals;
mod batch;
mod board;
pub(crate) mod charts;
mod cmdline;
mod doctor;
mod fleet;
mod forms;
mod gallery;
mod help;
mod hosts;
pub(crate) mod inbox;
mod journal;
mod machine;
mod monitor;
mod monitor_draw;
// light motion: needs-you rows breathe, a finished one ticks (ui.motion)
mod motion;
mod news;
pub(crate) mod outside;
mod palette;
mod panes;
// showcase's autopilot tour drives the dashboard through this (navigation only)
mod pilot;
mod quick;
mod race;
// Fleet rows' tokens-per-minute and context bars (ui.row_bars)
mod rowbars;
mod settings;
mod spaces;
mod splash;
mod splashes;
// each session's states over the last hours (Fleet `g`, Monitor › Timeline)
mod timechart;
mod timeline;
mod tree_view;
mod usage;
mod usage_filters;
mod usage_tabs;
mod wall;

// `aleph setup` (full screen) and the tour after it
pub(crate) mod onboard;
mod tour;

// showcase builds only: a view drawn offscreen for a poster (`aleph showcase snap`)
#[cfg(feature = "showcase")]
pub(crate) mod snap;

use crate::config::{Config, Target};
use crate::fleet::{Fleet, Transition};
use crate::herdr::{self, NewOpts, Session};
use crate::telemetry::Telemetry;
use crate::term::Tui;
use crate::ui::{self, c};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum View {
    #[default]
    Fleet,
    Monitor,
    Inbox,
    Settings,
    /// Focus and Tile: an overlay opened from a tab, not a tab of its own
    Wall,
}

impl View {
    /// The tabs, in order (their digits are their places).
    const ALL: [View; 4] = [View::Fleet, View::Monitor, View::Inbox, View::Settings];

    /// A tab by its name, any case (`aleph ui monitor`).
    pub fn named(s: &str) -> Option<View> {
        View::ALL
            .into_iter()
            .find(|v| v.name().eq_ignore_ascii_case(s))
    }
    fn name(self) -> &'static str {
        match self {
            View::Fleet => "Fleet",
            View::Wall => "Focus",
            View::Monitor => "Monitor",
            View::Settings => "Settings",
            View::Inbox => "Inbox",
        }
    }
}

/// The profile these settings were read with (`aleph profile`), if any.
pub(crate) fn profile_of(cfg: &Config) -> Option<String> {
    cfg.files()
        .into_iter()
        .find(|(what, _)| what == "profile")
        .and_then(|(_, p)| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
}

/// `aleph splash [NAME|random] [--loop] [--ms 3s] [--no-logo] [--end form|fall|warp]`:
/// one splash full screen, outside the dashboard (any key ends it) — for
/// recording and demos. The fleet it shows is the cached snapshot's.
pub fn splash_cmd(a: crate::cli::Args) -> std::process::ExitCode {
    use crate::out::*;
    let opt = |k: &str| {
        a.rest
            .iter()
            .position(|x| x == k)
            .and_then(|i| a.rest.get(i + 1))
            .cloned()
    };
    let name = a
        .rest
        .iter()
        .enumerate()
        .find(|(i, x)| {
            !x.starts_with('-') && !(*i > 0 && matches!(a.rest[i - 1].as_str(), "--ms" | "--end"))
        })
        .map(|(_, x)| x.clone());
    let seed = splash::seed_now();
    let kind = match name.as_deref() {
        None => splash::Kind::pick(&a.cfg, seed),
        // the theme's own (what `ui.splash = theme` plays)
        Some("theme") => splash::Kind::of_theme(&a.cfg.get_or("theme")),
        Some("random") => {
            let all: Vec<splash::Kind> = splash::Kind::ALL
                .into_iter()
                .filter(|k| *k != splash::Kind::Classic && k.built())
                .collect();
            all[(seed % all.len() as u64) as usize]
        }
        Some(n) => match splash::Kind::named(n).filter(|k| k.built()) {
            Some(k) => k,
            None => {
                let names: Vec<&str> = splash::Kind::ALL
                    .into_iter()
                    .filter(|k| k.built())
                    .map(|k| k.name())
                    .collect();
                return fail(&format!(
                    "{n}: not a splash — theme, {}, random",
                    names.join(", ")
                ));
            }
        },
    };
    let len = match opt("--ms") {
        Some(v) => match crate::log::parse_duration(&v) {
            Some(d) => Some((d.as_millis() as u64).clamp(1000, 8000)),
            None => return fail(&format!("--ms {v}: a length like 3s or 4500ms")),
        },
        None => splash::len_of(&a.cfg),
    };
    let mut opts = splash::Opts::of(&a.cfg, seed);
    if a.has("--no-logo") {
        opts.logo = false;
        if opts.end == splash::End::Form {
            opts.end = splash::End::Fall;
        }
    }
    if let Some(e) = opt("--end") {
        opts.end = match e.as_str() {
            "form" if opts.logo => splash::End::Form,
            "fall" | "form" => splash::End::Fall,
            "warp" => splash::End::Warp,
            _ => return fail(&format!("--end {e}: form, fall or warp (the cockpit's)")),
        };
    }
    if !crate::term::interactive() {
        return fail("aleph splash plays on a terminal");
    }
    let fleet = crate::fleet::Fleet::new(&a.cfg, a.flag.as_deref());
    let info = splash::Info::from_fleet(&fleet);
    let mut tui = match Tui::enter() {
        Ok(t) => t,
        Err(e) => return fail(&format!("terminal: {e}")),
    };
    splash::play(&mut tui, kind, opts, len, &info, seed, a.has("--loop"));
    drop(tui);
    std::process::ExitCode::SUCCESS
}

/// How the dashboard opens.
#[derive(Default)]
pub struct Start {
    pub view: View,
    /// `--host` override (only that machine)
    pub flag: Option<String>,
    /// sessions (names or host:name) to pin on the wall
    pub wall: Vec<String>,
    /// session to select first
    pub select: Option<String>,
    /// session (name, host:name or pane) to open in Focus once it's seen
    pub focus: Option<String>,
    /// open Fleet on the machines you could add (`aleph host pick`)
    pub machines: bool,
    /// after setup: the coach-mark tour of the tabs
    pub tour: bool,
    /// a saved workspace to open in (`aleph ui NAME`)
    pub workspace: Option<String>,
}

/// Work that needs the real terminal; `run_dashboard` runs it and comes back.
#[allow(clippy::large_enum_variant)] // one per hand-off
pub enum Handoff {
    Quit,
    /// the binary was updated: start again on the new build (same options)
    Restart,
    /// attach in herdr (launch card, then herdr on the same screen)
    Attach {
        target: Target,
        pane: String,
        label: String,
    },
    /// `herdr terminal attach` straight into one agent's (or shell's) terminal
    Direct {
        target: Target,
        pane: String,
        takeover: bool,
    },
    New {
        target: Target,
        opts: NewOpts,
    },
    Browse {
        target: Target,
        /// from Quick start: the folder goes back there, chosen (nothing starts)
        quick: Option<Box<quick::QuickStart>>,
    },
    MachineAdd(String),
    /// `aleph host setup` on this screen (when no new tab could open)
    HostSetup(String),
    KeySetup(String),
    /// install an agent on a machine (the guided `agents install` flow), then
    /// reopen the new-session form for it
    Install {
        agent: String,
        host: String,
        dir: Option<String>,
    },
    Hub,
    /// the settings file in `$EDITOR` (checked; put back on errors)
    EditConfig,
    /// one step of setup, on this screen (Settings › `S`)
    Setup(onboard::Step),
    /// a doctor fix that needs the terminal (`sudo` over `ssh -t`)
    Fix(crate::doctor::Fix),
}

/// Results from background threads.
pub(crate) enum Bg {
    Preview {
        key: String,
        recent: Result<String, String>,
        detect: Option<String>,
    },
    /// an action finished: toast it, and re-poll `refresh` if given
    Done {
        ok: bool,
        msg: String,
        refresh: Option<String>,
    },
    /// the direct attach refused because a client is attached
    Busy {
        target: Target,
        pane: String,
    },
    Hosts(hosts::Msg),
    /// a session started for a Focus / Tile pane: (pane, `host|pane` or why not)
    PaneStarted(u32, Result<String, String>),
    Doc(doctor::Msg),
    /// Quick start's workspaces (and the focused repo's worktrees) arrived
    Quick {
        host: String,
        gen: u64,
        ws: Result<Vec<herdr::Workspace>, String>,
        wt: Vec<herdr::Worktree>,
    },
    /// a session Quick start began: (`host:pane`, its name), and open Focus?
    Started {
        host: String,
        result: Result<(String, String), String>,
        focus: bool,
    },
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Level {
    Ok,
    Info,
    Warn,
    Err,
}

pub(crate) struct Toast {
    text: String,
    level: Level,
    at: Instant,
}

/// Overlays on top of the current view.
#[allow(clippy::large_enum_variant)] // exactly one at a time
pub(crate) enum Mode {
    Normal,
    Confirm {
        title: String,
        text: String,
        on_yes: Pending,
    },
    Input(forms::Input),
    NewForm(forms::NewForm),
    HostForm(forms::HostForm),
    Palette(palette::Palette),
    Help(help::Help),
    Quick(quick::QuickStart),
    CloseList(quick::CloseList),
    /// the Inbox's `U`: which alerts to clear
    DismissList(inbox::DismissList),
    /// the `:` command line
    Command(cmdline::CmdLine),
    /// the tour after setup: which card
    Tour(usize),
    /// what's new since you last opened Aleph (once after an update)
    News(news::Card),
    /// `W`: the saved workspaces
    Spaces(spaces::Picker),
    /// the theme gallery: themes and accents, live
    Gallery(gallery::Gallery),
    /// the splash gallery: every splash, a live preview
    Splashes(splashes::Splashes),
    /// a race's diffs, side by side
    Race(race::RaceView),
    /// ctrl+n: the alerts you missed and the last toasts
    Alerts(alerts::Popup),
    /// ctrl+a: every agent waiting on you, its question as it shows it
    Approvals,
    /// the session journal of a day
    Journal(journal::JournalView),
    /// Usage's filters: a picker (period, agents, machines, projects)
    UsagePick(usage_filters::Picker),
    /// Usage's custom from–to
    UsageCal(usage_filters::Cal),
}

/// What a confirm / input does when accepted.
pub(crate) enum Pending {
    Handoff(Box<Handoff>),
    Close(Vec<Session>),
    Takeover(Target, String),
    /// take a machine out of the fleet
    Leave(String),
    /// carry out a doctor check's fix (Settings › Health `f`)
    Fix(crate::doctor::Fix),
    /// delete Inbox rows for good (their events and launches)
    Forget(inbox::Forget),
    /// the Inbox's `U`: archive or delete the alerts picked
    Dismiss(inbox::Dismiss),
    /// stop an agent running outside herdr (its process; `x` on its row)
    Stop(outside::Outside),
    /// answer these questions (the approval board's Y), then back to it
    Approve(Vec<approvals::Answer>),
    /// reopen an agent running outside herdr in herdr (enter on its row)
    Reopen(outside::Outside),
    /// settings from a file or URL, after their preview
    Import(Vec<(String, String)>),
    /// one setting (key, value) and what to say once it's set
    Set(String, String, String),
    /// showcase mode: a hand-off to a screen that isn't masked, said yes to
    Unmasked(Box<Handoff>),
    /// showcase mode: herdr in a new terminal tab, said yes to
    OpenTab(Box<Session>),
}

/// The digit key that opens a view (`1` for the first tab).
pub(crate) fn digit(v: View) -> &'static str {
    const D: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
    View::ALL
        .iter()
        .position(|x| *x == v)
        .and_then(|i| D.get(i).copied())
        .unwrap_or("")
}

/// Rendered screen regions, for mouse hit-testing.
#[derive(Default)]
pub(crate) struct Hits {
    tabs: Vec<(Rect, View)>,
    list: Rect,
    list_first: u16,
    list_offset: usize,
    list_len: usize,
    /// clickable buttons drawn this frame (each runs its Fleet action)
    pub(crate) buttons: Vec<(Rect, Act)>,
    /// clickable chips: filters and sections
    pub(crate) chips: Vec<(Rect, Chip)>,
}

/// A chip a click switches to.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Chip {
    /// Inbox's kind filter (index into its chips)
    Inbox(usize),
    /// a Settings section
    Section(usize),
    /// one of the doctor's checks (Settings › Doctor)
    Check(usize),
    /// a Usage filter's chip
    Usage(usage_filters::UChip),
}

pub struct Dash {
    pub cfg: Config,
    flag: Option<String>,
    view: View,
    pub(crate) mode: Mode,
    pub(crate) tx: Sender<Bg>,
    rx: Receiver<Bg>,
    pub(crate) t0: Instant,
    pub(crate) fleet: Fleet,
    tele: HashMap<String, Telemetry>,
    /// when each machine's panes last got their stats tokens (`herdr_tokens`)
    tokens_at: HashMap<String, Instant>,
    pub(crate) keys: Keys,
    toasts: VecDeque<Toast>,
    pub(crate) hits: Hits,
    last_click: Option<(Instant, u16, u16)>,
    pub(crate) fv: fleet::State,
    /// Fleet's board (the list maximised)
    pub(crate) fb: board::State,
    /// layout mode (`L`) on this view: arrows resize its panels
    pub(crate) layout_mode: Option<&'static str>,
    pub(crate) wall: wall::State,
    pub(crate) hv: hosts::State,
    pub(crate) set: settings::State,
    pub(crate) doc: doctor::State,
    pub(crate) inbox: inbox::State,
    pub(crate) mon: monitor::State,
    /// usage numbers (Monitor › Usage, and beside sessions and machines)
    pub(crate) usage: usage::State,
    /// agents running outside herdr (their folders), listed under machines
    pub(crate) outside: outside::State,
    /// the approval board (ctrl+a): who's waiting, their questions
    pub(crate) appr: approvals::State,
    /// sessions/hosts we've already rung the bell for
    rung: HashSet<String>,
    /// machines whose herdr version drift we've already mentioned
    drift_told: HashSet<String>,
    /// auto-approve, phone notifications, the task queue, self-heal — run
    /// here only while no `aleph watch` is running
    hub: crate::auto::Hub,
    owner: (bool, Instant),
    /// held while this dashboard runs the hub (no daemon running)
    pub(crate) hub_lock: Option<crate::auto::HubLock>,
    /// the daemon's socket, served by this dashboard while no daemon runs
    /// (Raycast, the web console and `aleph api` keep working)
    api: Option<std::sync::Arc<crate::daemon::Shared>>,
    api_seen: Option<Instant>,
    /// which agents each machine has installed (probed off the UI thread)
    pub(crate) agents: crate::agentwatch::AgentWatch,
    /// local LLM servers per machine (only while `services` is on)
    pub(crate) services: Vec<(String, crate::services::Watcher)>,
    /// running in the `aleph` tmux session Moshi opens: follow the session
    /// a phone card was about (`moshibridge::hint_focus`), and rest while
    /// nobody is attached
    pub(crate) hints: MoshiSession,
    /// a session to select once a poll has seen it (`aleph focus`, Quick
    /// start): (selector, open it in Focus, since)
    pending_focus: Option<(String, bool, Instant)>,
    /// a newer build on disk (the resting tmux dashboard restarts on it)
    upgrade: crate::watch::Upgrade,
    /// a newer build is installed than the one running (the header says so;
    /// the palette restarts on it)
    pub(crate) new_build: bool,
    /// is this computer online (the header's indicator)
    pub(crate) net: crate::net::Net,
    /// what the indicator said last (a toast when that changes)
    net_was: u8,
    /// the terminal has focus (it says when it loses it; one that never
    /// says is taken to have it)
    pub(crate) focused: bool,
    /// when the on-screen record was last written, and what it said
    onscreen_at: Option<(Instant, Vec<String>, bool)>,
    /// notes that a machine was just set up (`refresh.rs`): look at it again
    pub(crate) changed: crate::refresh::Watch,
    /// the next hand-off was asked about already (showcase mode's gate)
    ungated: bool,
    /// the sessions' state log, as the timelines draw it
    pub(crate) tl: timechart::State,
    /// the timeline drawn (one conversation's tree): Fleet's panel, Focus's side, Monitor › Timeline
    pub(crate) tv: tree_view::State,
    /// light motion: sessions that just finished
    pub(crate) motion: motion::State,
    /// the last toasts (ctrl+n)
    pub(crate) alerts: alerts::State,
}

impl Dash {
    pub fn new(cfg: Config, start: &Start) -> Dash {
        let net_target = crate::net::target(&cfg);
        let (tx, rx) = channel();
        let mut fleet = Fleet::new(&cfg, start.flag.as_deref());
        // the Inbox under the rules of 2026-10-26, once
        if !cfg!(test) {
            crate::events::tidy_once();
        }
        fleet.scan_limits = true;
        fleet.listen(&cfg);
        fleet.refresh(None);
        let keys = Keys::from_config(&cfg);
        let aliases: Vec<String> = fleet.hosts.iter().map(|h| h.alias.clone()).collect();
        let agents = crate::agentwatch::AgentWatch::start(&aliases);
        let services = aliases
            .iter()
            .cloned()
            .zip(crate::services::watch(&cfg, &aliases))
            .collect();
        let mut d = Dash {
            fv: fleet::State::new(start.select.clone()),
            fb: board::State::default(),
            layout_mode: None,
            wall: wall::State::new(&start.wall),
            hv: hosts::State::default(),
            set: settings::State::default(),
            doc: doctor::State::default(),
            inbox: inbox::State::default(),
            mon: monitor::State::default(),
            usage: Default::default(),
            outside: Default::default(),
            appr: Default::default(),
            cfg,
            flag: start.flag.clone(),
            view: start.view,
            mode: Mode::Normal,
            tx,
            rx,
            t0: Instant::now(),
            fleet,
            tele: HashMap::new(),
            tokens_at: HashMap::new(),
            keys,
            toasts: VecDeque::new(),
            hits: Hits::default(),
            last_click: None,
            rung: HashSet::new(),
            drift_told: HashSet::new(),
            hub: crate::auto::Hub::new(),
            owner: (false, Instant::now() - Duration::from_secs(60)),
            hub_lock: None,
            api: None,
            api_seen: None,
            agents,
            services,
            hints: MoshiSession::new(in_moshi_session()),
            pending_focus: start.focus.clone().map(|q| (q, true, Instant::now())),
            upgrade: crate::watch::Upgrade::start(),
            new_build: false,
            net: crate::net::Net::start(net_target, Duration::from_secs(5)),
            net_was: 0,
            focused: true,
            onscreen_at: None,
            changed: crate::refresh::Watch::new(),
            ungated: false,
            tl: Default::default(),
            tv: Default::default(),
            motion: Default::default(),
            alerts: Default::default(),
        };
        inbox::init(&mut d);
        // machines in ~/.ssh/config, for Fleet's "Not in fleet" group
        hosts::reload(&mut d);
        if start.machines {
            d.fv.others_open = true;
            d.fv.sel = Some(fleet::RowId::Others);
        }
        herdr_view(&d, true);
        let v = d.view;
        d.switch(v);
        if start.tour {
            d.mode = Mode::Tour(0);
            tour::show(&mut d, 0);
        }
        if let Some(w) = start
            .workspace
            .as_deref()
            .and_then(|n| crate::workspace::find(&d.cfg, n))
        {
            spaces::apply(&mut d, &w);
        }
        d
    }

    /// In the `aleph` tmux session: when a phone card was just sent about a
    /// session, select it (checked once a second; only hints from the last
    /// ten minutes count).
    fn follow_hint(&mut self) {
        if !self.hints.on || self.hints.checked.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.hints.checked = Instant::now();
        self.hints.attached = tmux_attached();
        let p = crate::config::state_dir().join("focus-hint");
        let Ok(m) = std::fs::metadata(&p).and_then(|m| m.modified()) else {
            return;
        };
        if self.hints.seen == Some(m) {
            return;
        }
        let first = self.hints.seen.is_none();
        self.hints.seen = Some(m);
        let fresh = m.elapsed().is_ok_and(|a| a < Duration::from_secs(600));
        if !fresh || (first && m.elapsed().is_ok_and(|a| a > Duration::from_secs(120))) {
            return;
        }
        let Ok(t) = std::fs::read_to_string(&p) else {
            return;
        };
        let key = t.trim().to_string();
        if self.fleet.find(&key).is_some() {
            self.switch(View::Fleet);
            self.fv.sel = Some(fleet::RowId::Sess(key));
        }
    }

    // ------------------------------------------------------------ toasts

    pub(crate) fn toast(&mut self, level: Level, text: impl Into<String>) {
        let text = text.into();
        // kept for ctrl+n (marked when Focus or Tile was up)
        self.alerts.push(level, &text, self.view == View::Wall);
        self.toasts.push_back(Toast {
            text,
            level,
            at: Instant::now(),
        });
        while self.toasts.len() > 4 {
            self.toasts.pop_front();
        }
    }
    pub(crate) fn note(&mut self, m: impl Into<String>) {
        self.toast(Level::Ok, m);
    }
    pub(crate) fn error(&mut self, m: impl Into<String>) {
        self.toast(Level::Err, m);
    }
    pub fn flash(&mut self, m: impl Into<String>, err: bool) {
        self.toast(if err { Level::Err } else { Level::Ok }, m);
    }
    /// A toast held a few seconds longer than a note (a saved file's path).
    pub(crate) fn held(&mut self, ok: bool, m: impl Into<String>) {
        self.flash(m, !ok);
        if let Some(t) = self.toasts.back_mut() {
            t.at = Instant::now() + Duration::from_secs(4);
        }
    }
    fn current_toast(&self) -> Option<&Toast> {
        self.toasts
            .back()
            .filter(|t| t.at.elapsed() < self.toast_for(t.level))
    }

    /// How long the status bar shows a message: what a key did briefly
    /// (`ui.note_for`, 1 s), a warning or an error longer (`ui.alert_for`, 6 s).
    fn toast_for(&self, level: Level) -> Duration {
        let (key, dflt) = match level {
            Level::Ok | Level::Info => ("ui.note_for", Duration::from_secs(1)),
            Level::Warn | Level::Err => ("ui.alert_for", Duration::from_secs(6)),
        };
        crate::log::parse_duration(&self.cfg.get_or(key)).unwrap_or(dflt)
    }

    fn bell(&self) {
        if let Ok(mut t) = crate::term::open_tty() {
            let _ = t.write_all(b"\x07");
        }
    }

    // ------------------------------------------------------------ fleet + telemetry

    pub(crate) fn target_of(&self, host: &str) -> Target {
        self.fleet
            .host(host)
            .map(|h| h.target.clone())
            .unwrap_or_else(|| self.cfg.target_for(host, herdr::machine_for))
    }

    fn on_transitions(&mut self, v: Vec<Transition>) {
        for t in v {
            let s = &t.session;
            let who = format!("{} on {}", s.display(), s.host);
            match s.status.as_str() {
                "blocked" => {
                    self.toast(
                        Level::Warn,
                        format!("{} {who} needs you", crate::look::needs()),
                    );
                    if self.rung.insert(s.key()) {
                        self.bell();
                    }
                }
                "done" | "idle" if t.from == "working" => {
                    self.rung.remove(&s.key());
                    self.finished_toast(s, who);
                }
                _ if t.from.is_empty() => self.toast(Level::Info, format!("+ {who} started")),
                _ => {
                    self.rung.remove(&s.key());
                }
            }
        }
    }

    /// "finished", unless the agent only woke for a subagent's report with
    /// others still out. Only Claude Code's logs tell, and they're read off
    /// this thread, so its toast may come a few seconds later.
    fn finished_toast(&mut self, s: &Session, who: String) {
        let may_be_woken = !cfg!(test)
            && crate::agents::id_of(&s.agent) == "claude"
            && s.native_id.is_some()
            && !self.cfg.flag("notify.subagents")
            && crate::usage::enabled(&self.cfg);
        if !may_be_woken {
            self.toast(Level::Ok, format!("✓ {who} finished"));
            return;
        }
        let e = crate::events::Event {
            t: crate::cache::now(),
            host: s.host.clone(),
            name: s.name.clone(),
            ..Default::default()
        };
        let (cfg, tx, s) = (self.cfg.clone(), self.tx.clone(), s.clone());
        std::thread::spawn(move || {
            if !crate::turncost::subagent_turn(&cfg, &e, Some(&s)) {
                let _ = tx.send(Bg::Done {
                    ok: true,
                    msg: format!("✓ {who} finished"),
                    refresh: None,
                });
            }
        });
    }

    /// One toast per machine whose herdr version differs from this computer's.
    fn tell_drift(&mut self) {
        let new: Vec<(String, String, String)> = self
            .fleet
            .hosts
            .iter()
            .filter(|h| !self.drift_told.contains(&h.alias))
            .filter_map(|h| h.drift().map(|(t, o)| (h.alias.clone(), t, o)))
            .collect();
        for (a, theirs, ours) in new {
            self.drift_told.insert(a.clone());
            self.toast(
                Level::Warn,
                format!(
                    "{a} runs herdr {theirs}, this computer {ours} — not compatible as far as herdr can tell · aleph update"
                ),
            );
        }
    }

    /// Keep one stats stream per host that should have one (`stats` setting),
    /// and none while another view is up.
    fn sync_telemetry(&mut self) {
        let want: Vec<String> = if self.view == View::Monitor {
            // the Monitor compares every machine
            self.fleet.hosts.iter().map(|h| h.alias.clone()).collect()
        } else if !matches!(self.view, View::Fleet) {
            vec![]
        } else {
            match self.cfg.get_or("stats").as_str() {
                "off" => vec![],
                "selected" => self.fv.selected_host(self).into_iter().collect(),
                _ => self.fleet.hosts.iter().map(|h| h.alias.clone()).collect(),
            }
        };
        self.tele.retain(|a, _| want.contains(a));
        let iv = self.cfg.num("stats_interval").max(1);
        for a in want {
            self.tele
                .entry(a.clone())
                .or_insert_with(|| Telemetry::start(&a, iv));
        }
    }

    /// The settings changed on disk (`aleph settings`, another dashboard, a
    /// profile switched): read them again and apply what applies live. Looked
    /// at every 2 s, or now (`force`, after a switch from here).
    pub(crate) fn settings_moved(&mut self, force: bool) {
        static LOOK: std::sync::Mutex<Option<(Instant, u64)>> = std::sync::Mutex::new(None);
        let now = Instant::now();
        {
            let mut g = LOOK.lock().unwrap_or_else(|e| e.into_inner());
            let (at, seen) = *g.get_or_insert((now, self.cfg.stamp()));
            if !force && now.duration_since(at) < Duration::from_secs(2) {
                return;
            }
            // taken before the read: a change while reading shows next time
            let s = self.cfg.stamp();
            *g = Some((now, s));
            if !force && s == seen {
                return;
            }
        }
        let before = profile_of(&self.cfg);
        self.cfg = Config::load();
        crate::theme::assign_machine_colors(&mut self.cfg);
        self.fleet.outages = crate::fleet::Outages::from_cfg(&self.cfg);
        // a machine paused or resumed elsewhere (another dashboard, aleph settings)
        self.fleet.follow_paused(&self.cfg);
        settings::reapply_all(self);
        let after = profile_of(&self.cfg);
        if before != after {
            match after {
                Some(n) => self.toast(Level::Ok, format!("profile {n} on")),
                None => self.toast(Level::Ok, "your own settings again".to_string()),
            }
        }
    }

    /// A machine was just set up (`aleph host setup` in a tab, `agents
    /// install`, `hosts.refresh`): the notes it left, once a second — at once
    /// when the socket this dashboard serves was told.
    fn look_again(&mut self) {
        let told = self
            .api
            .as_ref()
            .is_some_and(|s| !s.take_refresh().is_empty());
        for n in self.changed.poll(told) {
            self.machine_changed(&n);
        }
    }

    /// Look at `n.host` again: the fleet polls it (joining it when the
    /// settings now have it), herdr's version and the agents report are read
    /// again, the stats sampler and the LLM-server scan start over; then the
    /// toast says what it has.
    pub(crate) fn machine_changed(&mut self, n: &crate::refresh::Note) {
        let h = n.host.clone();
        self.cfg = Config::load();
        crate::theme::assign_machine_colors(&mut self.cfg);
        let in_fleet = self.cfg.fleet().contains(&h);
        if in_fleet && self.fleet.host(&h).is_none() {
            let cfg = self.cfg.clone();
            self.fleet.add_host(&cfg, &h);
        }
        crate::refresh::forget(&h, false);
        if let Some(x) = self.fleet.host_mut(&h) {
            x.herdr_info = None;
        }
        self.drift_told.remove(&h);
        self.fleet.refresh(Some(&h));
        if in_fleet {
            if n.probed {
                self.agents.reload(&h);
            } else {
                self.agents.refresh(&h);
            }
            // the sampler starts over (OS, GPUs); so does the LLM-server scan
            self.tele.remove(&h);
            self.services.retain(|(a, _)| *a != h);
            if let Some(w) = crate::services::watch(&self.cfg, std::slice::from_ref(&h)).pop() {
                self.services.push((h.clone(), w));
            }
        }
        hosts::reload(self);
        self.hv.setting_up.remove(&h);
        self.fv.invalidate_preview();
        let (good, text) = n.toast();
        self.toast(if good { Level::Ok } else { Level::Warn }, text);
    }

    pub(crate) fn stats(&self, host: &str) -> Option<crate::telemetry::Stats> {
        let mut st = self.tele.get(host).map(Telemetry::snapshot)?;
        // whose each GPU process is: the session in its folder
        if let Some(h) = self.fleet.host(host).filter(|_| !st.gpu_procs.is_empty()) {
            for p in st.gpu_procs.iter_mut() {
                if let Some(s) = crate::telemetry::owner(p, &h.sessions) {
                    p.owner = s.display().to_string();
                }
            }
        }
        Some(st)
    }

    /// The GPU processes `s` started (`telemetry::owner`), biggest first.
    pub(crate) fn gpu_of(&self, s: &Session) -> Vec<crate::telemetry::GpuProc> {
        let (Some(t), Some(h)) = (self.tele.get(&s.host), self.fleet.host(&s.host)) else {
            return Vec::new();
        };
        t.gpu_procs()
            .into_iter()
            .filter(|p| {
                crate::telemetry::owner(p, &h.sessions)
                    .is_some_and(|o| o.pane_id == s.pane_id && o.name == s.name)
            })
            .collect()
    }

    /// `herdr_tokens = on`: each machine's stats onto its agent panes, as
    /// tokens herdr's sidebar can show (`tokens.rs`); every 30 s at most,
    /// one shell line per machine, off the UI thread.
    fn push_tokens(&mut self) {
        if !self.cfg.flag("herdr_tokens") {
            return;
        }
        for h in &self.fleet.hosts {
            if h.conn != crate::fleet::Conn::Up || h.sessions.is_empty() {
                continue;
            }
            if self
                .tokens_at
                .get(&h.alias)
                .is_some_and(|t| t.elapsed() < crate::tokens::EVERY)
            {
                continue;
            }
            let Some(st) = self.stats(&h.alias).filter(|s| !s.hostname.is_empty()) else {
                continue;
            };
            self.tokens_at.insert(h.alias.clone(), Instant::now());
            let panes: Vec<String> = h.sessions.iter().map(|s| s.pane_id.clone()).collect();
            let line = crate::chan::herdr_env(&h.alias)
                + &crate::tokens::script(
                    &crate::chan::herdr_path(&h.alias),
                    &panes,
                    &crate::tokens::values(&st),
                );
            let host = h.alias.clone();
            std::thread::spawn(move || {
                let _ = crate::proc::run(
                    &mut crate::sshcfg::shell(&host, &line),
                    Duration::from_secs(15),
                    None,
                );
            });
        }
    }

    /// Drop every live connection we own (before herdr takes the terminal).
    fn drop_connections(&mut self) {
        self.tele.clear();
        self.wall.drop_all();
    }

    // ------------------------------------------------------------ background

    /// While another program has the terminal, lend the fleet and the hub to a
    /// thread so automation never pauses (only when this dashboard runs it).
    fn tick_in_background(&mut self) -> Option<Ticker> {
        if !self.owner.0 {
            return None;
        }
        let mut fleet = std::mem::replace(&mut self.fleet, Fleet::placeholder());
        let mut hub = std::mem::take(&mut self.hub);
        let cfg = self.cfg.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let st = stop.clone();
        let api = self.api.clone();
        let h = std::thread::spawn(move || {
            let mut seen = None;
            while !st.load(std::sync::atomic::Ordering::Relaxed) {
                let fresh: Vec<String> = fleet
                    .hosts
                    .iter()
                    .filter(|h| h.conn == crate::fleet::Conn::Unknown)
                    .map(|h| h.alias.clone())
                    .collect();
                let tr = fleet.tick();
                hub.tick(&cfg, &fleet, &tr, &fresh);
                // while the terminal is handed over, the API keeps up
                if let Some(sh) = &api {
                    if seen != Some(fleet.changed_at) {
                        seen = Some(fleet.changed_at);
                        sh.publish(fleet.to_json(&cfg), true);
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            (fleet, hub)
        });
        Some(Ticker { stop, h })
    }

    fn take_back(&mut self, t: Ticker) {
        t.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok((f, hub)) = t.h.join() {
            self.fleet = f;
            self.hub = hub;
        }
    }

    /// The machines in the fleet that answer now, of all of them (this
    /// computer aside: it's always there).
    pub(crate) fn machines_up(&self) -> (usize, usize) {
        let remote: Vec<&crate::fleet::Host> = self
            .fleet
            .hosts
            .iter()
            .filter(|h| !crate::sshcfg::is_local(&h.alias))
            .collect();
        let up = remote
            .iter()
            .filter(|h| matches!(h.conn, crate::fleet::Conn::Up) && !h.stale)
            .count();
        (up, remote.len())
    }

    /// Is `host` out of reach now: the internet down and that machine not
    /// answering? (One on the local network still answers without it.)
    pub(crate) fn cut_off(&self, host: &str) -> bool {
        if !self.net.state().is_offline() || crate::sshcfg::is_local(host) {
            return false;
        }
        !self
            .fleet
            .host(host)
            .is_some_and(|h| matches!(h.conn, crate::fleet::Conn::Up) && !h.stale)
    }

    /// The sessions on screen now, for the notifier (`onscreen.rs`): Focus
    /// or Tile's, the one selected in Fleet, the approval board's; written
    /// when it changes, else every 5 s.
    fn tell_onscreen(&mut self) {
        let mut keys: Vec<String> = Vec::new();
        let key = |s: &Session| format!("{}|{}", s.host, crate::events::name_of(s));
        match self.view {
            View::Wall => keys.extend(wall::shown(self).iter().map(key)),
            View::Fleet => keys.extend(fleet::selected_session(self).as_ref().map(key)),
            _ => {}
        }
        if matches!(self.mode, Mode::Approvals) {
            keys.extend(approvals::shown(self).iter().map(key));
        }
        let due = match &self.onscreen_at {
            Some((t, k, f)) => {
                *k != keys || *f != self.focused || t.elapsed() >= Duration::from_secs(5)
            }
            None => true,
        };
        if due {
            crate::onscreen::write(&keys, self.focused);
            self.onscreen_at = Some((Instant::now(), keys, self.focused));
        }
    }

    /// A toast when the connection changes: gone, gone but the machines
    /// still answer (a local network), or back.
    fn net_toast(&mut self) {
        use crate::net::State;
        let now = match self.net.state() {
            State::Offline { .. } if self.machines_up().0 > 0 => 3,
            State::Offline { .. } => 2,
            State::Weak { .. } => 1,
            State::Unknown => self.net_was,
            _ => 0,
        };
        if now == self.net_was {
            return;
        }
        let was = std::mem::replace(&mut self.net_was, now);
        match (was, now) {
            (_, 3) => self.toast(
                Level::Info,
                "no internet — the machines on your network still answer; the others wait",
            ),
            (_, 2) => self.toast(
                Level::Warn,
                "offline — the machines keep their sessions; Aleph catches up when it's back",
            ),
            (2 | 3, _) => {
                self.toast(Level::Ok, "back online — catching up with every machine");
                self.fleet.refresh(None);
            }
            (0, 1) => self.toast(Level::Info, "the connection is slow — updates may lag"),
            _ => {}
        }
    }

    /// `aleph focus <session>` / Quick start: select it (and open Focus) as
    /// soon as a poll has seen it. `aleph focus` gives up with a toast once
    /// every machine has answered; a fresh start waits up to 20 s.
    fn open_pending_focus(&mut self) {
        let Some((q, focus, t0)) = self.pending_focus.clone() else {
            return;
        };
        let all: Vec<Session> = self.fleet.sessions().into_iter().cloned().collect();
        if let Some(s) = herdr::pick(&all, &q) {
            self.pending_focus = None;
            let key = s.key();
            if self.view != View::Fleet && !focus {
                return;
            }
            self.fv.sel = Some(fleet::RowId::Sess(key.clone()));
            if focus {
                wall::focus_on(self, &key, View::Fleet);
                // a plain terminal is for typing into
                if s.agent == crate::agents::SHELL {
                    self.wall.typing = true;
                }
            }
            return;
        }
        let answered = self
            .fleet
            .hosts
            .iter()
            .all(|h| h.conn != crate::fleet::Conn::Unknown);
        let started = q.contains(':') && t0.elapsed() < Duration::from_secs(20);
        if (answered && !started) || t0.elapsed() > Duration::from_secs(20) {
            self.pending_focus = None;
            if focus {
                self.toast(Level::Warn, format!("no session '{q}' — showing the fleet"));
            }
        }
    }

    /// Run `f` off the UI thread; its result becomes a toast.
    pub(crate) fn spawn(
        &self,
        refresh: Option<String>,
        f: impl FnOnce() -> Result<String, String> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        thread::spawn(move || {
            let (ok, msg) = match f() {
                Ok(m) => (true, m),
                Err(e) => (false, e),
            };
            let _ = tx.send(Bg::Done { ok, msg, refresh });
        });
    }

    fn on_bg(&mut self, ev: Bg) {
        match ev {
            Bg::Preview {
                key,
                recent,
                detect,
            } => self.fv.on_preview(key, recent, detect),
            Bg::Quick { host, gen, ws, wt } => {
                if let Mode::Quick(q) = &mut self.mode {
                    if q.host == host && q.gen == gen {
                        q.loaded(ws, wt);
                    }
                }
            }
            Bg::Started {
                host,
                result,
                focus,
            } => match result {
                Ok((sel, name)) => {
                    self.toast(Level::Ok, format!("started {name} on {host}"));
                    self.fleet.refresh(Some(&host));
                    self.pending_focus = Some((sel, focus, Instant::now()));
                }
                Err(e) => self.toast(Level::Err, format!("{host}: {e}")),
            },
            Bg::Done { ok, msg, refresh } => {
                if !msg.is_empty() {
                    self.toast(if ok { Level::Ok } else { Level::Err }, msg);
                }
                if let Some(h) = refresh {
                    self.fleet.refresh(Some(&h));
                    self.fv.invalidate_preview();
                }
            }
            Bg::Busy { target, pane } => {
                self.mode = Mode::Confirm {
                    title: "Already attached elsewhere".into(),
                    text: "Another terminal is attached to this agent. Take it over? The other client is detached.".into(),
                    on_yes: Pending::Takeover(target, pane),
                };
            }
            Bg::Hosts(m) => hosts::on_msg(self, m),
            Bg::PaneStarted(id, r) => wall::started(self, id, r),
            Bg::Doc(m) => doctor::on_msg(self, m),
        }
    }

    // ------------------------------------------------------------ views

    /// ⇥ / ⇧⇥ in a view: its next / previous sub-heading.
    pub(crate) fn sub_step(&mut self, fwd: bool) {
        match self.view {
            View::Monitor => monitor::sub_step(self, fwd),
            View::Inbox => inbox::filter_step(self, fwd),
            View::Settings => settings::sub_step(self, fwd),
            // no sub-headings here (Fleet, Focus / Tile): the next tab
            _ => {
                let here = self.tab_active();
                let i = View::ALL.iter().position(|v| *v == here).unwrap_or(0);
                let n = View::ALL.len();
                self.switch(View::ALL[if fwd { (i + 1) % n } else { (i + n - 1) % n }]);
            }
        }
    }

    pub(crate) fn switch(&mut self, v: View) {
        if v != self.view {
            self.layout_mode = None;
            // the theme's glitch between views (cyberpunk's)
            ambience::switched();
        }
        if v != self.view || self.t0.elapsed() < Duration::from_secs(1) {
            crate::term::set_title(&format!("{} — {}", crate::brand::BIN, v.name()));
            // showcase mode: a caption naming what this view is for
            let name = if v == View::Wall && self.wall.in_tile() {
                "Tile"
            } else {
                v.name()
            };
            crate::showcase::switched(name);
        }
        self.view = v;
        match v {
            View::Settings => settings::enter(self),
            View::Wall => wall::enter(self),
            View::Inbox => inbox::enter(self),
            View::Monitor => monitor::enter(self),
            _ => {}
        }
        if v != View::Wall {
            self.wall.drop_all();
        }
    }

    /// Called by `run_dashboard` after a hand-off that comes back here.
    pub fn after_handoff(&mut self, h: &Handoff) {
        match h {
            Handoff::MachineAdd(a) => {
                let m = herdr::machine_for(a);
                if self.cfg.get("ssh_host").as_deref() == Some(a.as_str()) {
                    let _ = self.cfg.set("machine", &m);
                } else if m != *a {
                    let _ = self.cfg.set(&format!("host.{a}.machine"), &m);
                }
                hosts::reload(self);
            }
            // its note (refresh.rs) brings the rest: the toast, a new look
            Handoff::HostSetup(_) => hosts::reload(self),
            Handoff::KeySetup(a) => hosts::test(self, a),
            Handoff::EditConfig | Handoff::Setup(_) => settings::reapply_all(self),
            Handoff::Install { agent, host, dir } => {
                if let Some(w) = crate::agentwatch::AgentWatch::current() {
                    w.refresh(host);
                }
                // back to the form, ready to start it
                let mut nf = forms::NewForm::new(self, host, dir.as_deref());
                nf.set_agent(agent);
                self.mode = Mode::NewForm(nf);
            }
            _ => {}
        }
        self.fleet.refresh(None);
        self.fv.invalidate_preview();
        if self.view == View::Wall {
            wall::enter(self);
        }
    }

    // ------------------------------------------------------------ keys

    /// A key, and what it asks for (past showcase mode's gate).
    fn on_key(&mut self, k: KeyEvent) -> Option<Handoff> {
        // showcase: the panic key pauses (and resumes) every window, before
        // anything else sees it; while paused no other key does anything
        if crate::showcase::panic_key(&key_str(&k)) {
            return None;
        }
        // showcase: a key while the autopilot tour runs only stops it
        if crate::showcase::tour_input() {
            return None;
        }
        let h = self.key(k)?;
        self.gate(h)
    }

    /// Showcase mode: a hand-off to a screen the lens can't reach (herdr's
    /// own, an editor on the settings file) opens Focus instead, asks
    /// first, or is refused (`showcase::gate`). Otherwise it goes ahead.
    fn gate(&mut self, h: Handoff) -> Option<Handoff> {
        use crate::showcase::{Gate, Outside};
        if std::mem::take(&mut self.ungated) {
            return Some(h);
        }
        let what = match &h {
            Handoff::Attach { .. } => Outside::Attach,
            Handoff::Direct { .. } | Handoff::Hub => Outside::Herdr,
            Handoff::EditConfig => Outside::Editor,
            _ => return Some(h),
        };
        match crate::showcase::gate(what) {
            Gate::Open => Some(h),
            Gate::Focus(note) => {
                if let Handoff::Attach { target, pane, .. } = &h {
                    self.focus_pane(&target.ssh, pane, note);
                }
                None
            }
            Gate::Ask { title, text } => {
                self.mode = Mode::Confirm {
                    title: title.into(),
                    text: text.into(),
                    on_yes: Pending::Unmasked(Box::new(h)),
                };
                None
            }
            Gate::Refuse(why) => {
                self.toast(Level::Info, why);
                None
            }
        }
    }

    /// The session in this pane, in Focus (`note` when it's there already).
    fn focus_pane(&mut self, ssh: &str, pane: &str, note: &str) {
        let key = self
            .fleet
            .sessions()
            .iter()
            .find(|s| s.pane_id == pane && (s.host == ssh || self.target_of(&s.host).ssh == ssh))
            .map(|s| s.key());
        let Some(key) = key else {
            self.toast(Level::Warn, format!("no session in {pane} on {ssh} now"));
            return;
        };
        let here =
            matches!(self.wall.content_of_focus(), Some(wall::Content::Session(k)) if *k == key);
        if self.view == View::Wall && self.wall.in_focus() && here {
            self.toast(Level::Info, note);
            return;
        }
        let from = if self.view == View::Wall {
            self.wall.back.unwrap_or(View::Fleet)
        } else {
            self.view
        };
        wall::focus_on(self, &key, from);
    }

    /// `launch::run` came back saying Focus (showcase mode): open that pane
    /// there once a poll has seen it.
    fn focus_later(&mut self, t: &Target, pane: &str) {
        let alias = self
            .fleet
            .hosts
            .iter()
            .find(|h| h.alias == t.ssh || h.target.ssh == t.ssh)
            .map_or_else(|| t.ssh.clone(), |h| h.alias.clone());
        self.fleet.refresh(Some(&alias));
        self.pending_focus = Some((format!("{alias}:{pane}"), true, Instant::now()));
    }

    /// What a launch card ended with: a failure toasts, Focus opens there.
    fn landed(&mut self, t: &Target, o: crate::launch::Outcome) {
        match o {
            crate::launch::Outcome::Failed(e) => self.flash(e, true),
            crate::launch::Outcome::Focus(p) => self.focus_later(t, &p),
            _ => {}
        }
    }

    fn key(&mut self, k: KeyEvent) -> Option<Handoff> {
        // Cmd + a key is never a command here, and never a letter for the agent
        // (a terminal that passes Cmd+C on would type a c, or start a window)
        if k.modifiers.contains(KeyModifiers::SUPER) {
            if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('C')) {
                self.toast(
                    Level::Info,
                    "to copy: hold shift or option as you drag, then Cmd+C",
                );
            }
            return None;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // (typing into an agent, ctrl+c is the agent's: it interrupts it)
        let typing =
            self.view == View::Wall && self.wall.typing && matches!(self.mode, Mode::Normal);
        if ctrl && matches!(k.code, KeyCode::Char('c')) && !typing {
            return Some(Handoff::Quit);
        }
        // overlays first
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Normal => {}
            Mode::Confirm {
                title,
                text,
                on_yes,
            } => {
                return match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => self.accept(on_yes),
                    KeyCode::Char('n') | KeyCode::Esc | KeyCode::Char('q') => None,
                    _ => {
                        self.mode = Mode::Confirm {
                            title,
                            text,
                            on_yes,
                        };
                        None
                    }
                };
            }
            Mode::Input(mut i) => {
                return match i.key_in(k, &self.cfg) {
                    forms::Res::Keep => {
                        self.mode = Mode::Input(i);
                        None
                    }
                    forms::Res::Cancel => None,
                    forms::Res::Submit => forms::submit_input(self, i),
                };
            }
            Mode::NewForm(mut nf) => {
                return match nf.key(k, &self.fleet) {
                    forms::Res::Keep => {
                        self.mode = Mode::NewForm(nf);
                        None
                    }
                    forms::Res::Cancel => None,
                    forms::Res::Submit => forms::submit_new(self, nf),
                };
            }
            Mode::HostForm(hf) => return hosts::form_key(self, hf, k),
            Mode::Palette(mut p) => {
                return match p.key(k) {
                    forms::Res::Keep => {
                        self.mode = Mode::Palette(p);
                        None
                    }
                    forms::Res::Cancel => None,
                    forms::Res::Submit => palette::run(self, p),
                };
            }
            Mode::Quick(mut q) => {
                return match q.key(k) {
                    quick::QuickRes::Keep => {
                        self.mode = Mode::Quick(q);
                        None
                    }
                    quick::QuickRes::Cancel => None,
                    quick::QuickRes::Start { focus } => quick::start(self, q, focus),
                    quick::QuickRes::Browse => Some(Handoff::Browse {
                        target: self.target_of(&q.host),
                        quick: Some(Box::new(q)),
                    }),
                };
            }
            Mode::Command(mut cl) => {
                return match cl.key(self, k) {
                    forms::Res::Keep => {
                        self.mode = Mode::Command(cl);
                        None
                    }
                    forms::Res::Cancel => None,
                    forms::Res::Submit => cmdline::run(self, cl),
                };
            }
            Mode::CloseList(mut cl) => {
                match cl.key(k) {
                    forms::Res::Keep => self.mode = Mode::CloseList(cl),
                    forms::Res::Cancel => {}
                    forms::Res::Submit => quick::confirm_close(self, cl),
                }
                return None;
            }
            Mode::DismissList(mut dl) => {
                match dl.key(k) {
                    forms::Res::Keep => self.mode = Mode::DismissList(dl),
                    forms::Res::Cancel => {}
                    forms::Res::Submit => inbox::confirm_dismiss(self, dl),
                }
                return None;
            }
            Mode::Tour(i) => {
                if let Some(n) = tour::key(self, i, k) {
                    self.mode = Mode::Tour(n);
                }
                return None;
            }
            Mode::Help(h) => return help::key(self, h, k),
            Mode::News(card) => {
                news::key(self, card, k);
                return None;
            }
            Mode::Spaces(p) => {
                spaces::key(self, p, k);
                return None;
            }
            Mode::Gallery(g) => {
                gallery::key(self, g, k);
                return None;
            }
            Mode::Splashes(g) => {
                splashes::key(self, g, k);
                return None;
            }
            Mode::Race(v) => {
                race::key(self, v, k);
                return None;
            }
            Mode::Alerts(p) => {
                alerts::key(self, p, k);
                return None;
            }
            Mode::Approvals => return approvals::key(self, k),
            Mode::UsagePick(mut p) => {
                match p.key(k) {
                    forms::Res::Keep => self.mode = Mode::UsagePick(p),
                    forms::Res::Cancel => {}
                    forms::Res::Submit => usage_filters::apply(self, p),
                }
                return None;
            }
            Mode::UsageCal(mut c) => {
                if k.code == KeyCode::Esc {
                    return None;
                }
                match c.key(k) {
                    Some((a, b)) => {
                        self.usage.f.custom = Some((a, b));
                        usage_filters::changed(self);
                    }
                    None => self.mode = Mode::UsageCal(c),
                }
                return None;
            }
            Mode::Journal(v) => {
                journal::key(self, v, k);
                return None;
            }
        }
        // text entry in a view (filter, wall typing) swallows everything
        if self.view == View::Fleet && self.fv.filtering {
            fleet::filter_key(self, k);
            return None;
        }
        if self.view == View::Monitor && self.mon.filtering {
            monitor::filter_key(self, k);
            return None;
        }
        if self.view == View::Inbox && self.inbox.searching {
            inbox::search_key(self, k);
            return None;
        }
        if self.view == View::Wall && self.wall.captures() {
            return wall::on_key(self, k);
        }
        if self.view == View::Settings && self.set.searching {
            settings::search_key(self, k);
            return None;
        }
        if self.view == View::Settings && self.set.captures() {
            return settings::on_key(self, k);
        }
        // global keys
        let ks = key_str(&k);
        // showcase mode: the record key starts or stops a recording (and
        // isn't keycast)
        if crate::showcase::capture_key(&self.cfg, &ks) {
            return None;
        }
        // showcase mode's keycast: only here, past every overlay and text
        // field, so nothing typed is ever shown (with what the key does)
        crate::showcase::key(&self.cfg, &ks, || self.key_does(&ks));
        // showcase mode: the autopilot tour's key starts it
        if crate::showcase::tour_key(&self.cfg, &ks) {
            return None;
        }
        if ks == "ctrl+k" || (ks == "p" && self.view == View::Wall) {
            self.mode = Mode::Palette(palette::Palette::new(self));
            return None;
        }
        // the alerts you missed and the last toasts: every view (key.alerts)
        if self.keys.owner(&ks) == Some(Act::Alerts) {
            alerts::open(self);
            return None;
        }
        // every agent waiting on you: every view (key.approvals) — unless
        // it's the panes' prefix in Focus or Tile
        if self.keys.owner(&ks) == Some(Act::Approvals)
            && !(self.view == View::Wall && wall::is_prefix(self, &ks))
        {
            approvals::open(self);
            return None;
        }
        // a close this dashboard asked, still in its grace: u keeps it from
        // any view (Fleet's own u does the same, and more)
        if self.view != View::Fleet
            && self.keys.owner(&ks) == Some(Act::Undo)
            && batch::undo_window(self)
        {
            batch::undo(self);
            return None;
        }
        // layout mode has its own 1–4, q and esc: before the views' keys
        if self.layout_mode.is_some() {
            if let Some((name, parts)) = self.panes() {
                panes::key(self, name, parts, &k);
                return None;
            }
            self.layout_mode = None;
        }
        match k.code {
            KeyCode::Char(':') => {
                self.mode = Mode::Command(cmdline::CmdLine::new());
                return None;
            }
            // saved workspaces (Focus and Tile keep W for themselves)
            KeyCode::Char('W') if self.view != View::Wall => {
                spaces::open(self);
                return None;
            }
            KeyCode::Char('?') if self.view == View::Wall => {
                self.wall.cheats = true;
                return None;
            }
            KeyCode::Char('?') => {
                self.mode = Mode::Help(help::Help::default());
                return None;
            }
            // Focus / Tile: a question on screen takes its option numbers
            KeyCode::Char(d @ '1'..='9')
                if self.view == View::Wall && wall::answer_key(self, d) =>
            {
                return None;
            }
            KeyCode::Char(d @ '1'..='9') => {
                if let Some(v) = View::ALL.get((d as u8 - b'1') as usize) {
                    self.switch(*v);
                }
                return None;
            }
            // ⇥ / ⇧⇥: the next / previous sub-heading wherever you are —
            // Monitor's views, the Inbox's filters, Settings' sections (the
            // Doctor's arrangement while its list has focus); 1–4 switch views
            KeyCode::Tab | KeyCode::BackTab => {
                self.sub_step(k.code == KeyCode::Tab);
                return None;
            }
            KeyCode::Char('q') if self.view == View::Fleet => return Some(Handoff::Quit),
            // Monitor › Timeline's tree gives its keys back first
            KeyCode::Esc
                if self.view == View::Monitor
                    && self.mon.sub == monitor_draw::Sub::Timeline
                    && self.tl.tree_keys =>
            {
                self.tl.tree_keys = false;
                return None;
            }
            KeyCode::Esc | KeyCode::Char('q') if self.view != View::Fleet => {
                if self.view == View::Wall {
                    return wall::on_key(self, k);
                }
                self.switch(View::Fleet);
                return None;
            }
            _ => {}
        }
        // the panels' layout: the same keys in every view that has panels
        if let Some((name, parts)) = self.panes() {
            if panes::key(self, name, parts, &k) {
                return None;
            }
        }
        match self.view {
            View::Fleet => fleet::on_key(self, k),
            View::Wall => wall::on_key(self, k),
            View::Settings => settings::on_key(self, k),
            View::Inbox => inbox::on_key(self, k),
            View::Monitor => monitor::on_key(self, k),
        }
    }

    /// This view's panels for the layout keys (`panes`): its name and what
    /// it has. None: no panels to lay out (Focus / Tile have their own keys).
    pub(crate) fn panes(&self) -> Option<(&'static str, panes::Parts)> {
        let side = |stacked| panes::Parts {
            side: true,
            stacked,
        };
        match self.view {
            // the list over its machine's resources (when stats are on)
            View::Fleet => Some(("fleet", side(self.cfg.get_or("stats") != "off"))),
            View::Monitor if self.mon.sub == monitor_draw::Sub::Overview => {
                Some(("monitor", side(false)))
            }
            View::Inbox => Some(("inbox", side(false))),
            View::Settings => Some(("settings", side(false))),
            _ => None,
        }
    }

    /// What a key does here, for showcase mode's keycast (asked only when it
    /// names keys): (the key map's action, its words) in Fleet, else the
    /// global keys' and the footer hints' words.
    fn key_does(&self, ks: &str) -> Option<(&'static str, String)> {
        let say = |w: &str| Some(("", w.to_string()));
        match ks {
            "ctrl+k" => return say("palette"),
            "p" if !matches!(self.view, View::Settings | View::Monitor | View::Inbox) => {
                return say("palette")
            }
            ":" => return say("command line"),
            "W" if self.view != View::Wall => return say("workspaces"),
            "?" if self.view == View::Wall => return say("keys"),
            "?" => return say("help"),
            "tab" => return say("next view"),
            "shift+tab" => return say("previous view"),
            "q" if self.view == View::Fleet => return say("quit"),
            "q" | "esc" => return say("back"),
            k if self.keys.owner(k) == Some(Act::Alerts) => return say("alerts"),
            k if crate::showcase::is_tour_key(&self.cfg, k) => return say("autopilot tour"),
            d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() && self.view != View::Wall => {
                let i = d.as_bytes()[0].checked_sub(b'1')? as usize;
                return View::ALL.get(i).and_then(|v| say(v.name()));
            }
            _ => {}
        }
        if self.view == View::Fleet {
            if let Some(a) = self.keys.owner(ks) {
                if let Some((_, name, _, help)) = ACTS.iter().find(|x| x.0 == a) {
                    return Some((*name, help.to_string()));
                }
            }
        }
        let hints = match self.view {
            View::Fleet => fleet::hints(self),
            View::Wall => wall::hints(self),
            View::Settings => settings::hints(self),
            View::Inbox => inbox::hints(self),
            View::Monitor => monitor::hints(self),
        };
        hints
            .iter()
            .find(|(k, _)| crate::showcase::hint_has(k, ks))
            .and_then(|(_, w)| say(w))
    }

    fn accept(&mut self, p: Pending) -> Option<Handoff> {
        match p {
            Pending::Handoff(h) => Some(*h),
            Pending::Close(v) => {
                // after close.grace (u undoes), per machine (`batch.rs`)
                batch::close(self, v);
                None
            }
            Pending::Takeover(target, pane) => {
                // (the attach it takes over was let through already)
                self.ungated = true;
                Some(Handoff::Direct {
                    target,
                    pane,
                    takeover: true,
                })
            }
            Pending::Unmasked(h) => {
                self.ungated = true;
                Some(*h)
            }
            Pending::OpenTab(s) => {
                open_tab_now(self, &s);
                None
            }
            Pending::Leave(alias) => {
                hosts::leave(self, &alias);
                None
            }
            Pending::Stop(o) => {
                outside::stop(self, o);
                None
            }
            Pending::Approve(v) => {
                approvals::send(self, v);
                self.mode = Mode::Approvals;
                None
            }
            Pending::Reopen(o) => {
                outside::reopen(self, o);
                None
            }
            Pending::Fix(f) => doctor::apply(self, f),
            Pending::Forget(f) => {
                inbox::forget(self, f);
                None
            }
            Pending::Dismiss(x) => {
                inbox::dismiss(self, x);
                None
            }
            Pending::Import(v) => {
                settings::import(self, v);
                None
            }
            Pending::Set(k, v, said) => {
                settings::save(self, &k, &v);
                self.toast(Level::Ok, said);
                None
            }
        }
    }

    // ------------------------------------------------------------ mouse

    /// A click or a scroll, and what it asks for (past showcase mode's gate).
    fn on_mouse(&mut self, m: MouseEvent) -> Option<Handoff> {
        // showcase: nothing to click while paused
        if crate::showcase::paused() {
            return None;
        }
        // ... and a click or a scroll while the autopilot tour runs only stops it
        if m.kind != MouseEventKind::Moved && crate::showcase::tour_input() {
            return None;
        }
        let h = self.mouse(m)?;
        self.gate(h)
    }

    fn mouse(&mut self, m: MouseEvent) -> Option<Handoff> {
        // the approval board: a click on a card answers it in Focus
        if matches!(self.mode, Mode::Approvals) {
            if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                approvals::click(self, m.column, m.row);
            }
            return None;
        }
        if !matches!(self.mode, Mode::Normal) {
            return None;
        }
        let inside = |r: Rect| {
            m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
        };
        if let MouseEventKind::Down(MouseButton::Left) = m.kind {
            if let Some((_, v)) = self.hits.tabs.iter().find(|(r, _)| inside(*r)) {
                let v = *v;
                self.switch(v);
                return None;
            }
            if let Some((_, a)) = self.hits.buttons.iter().find(|(r, _)| inside(*r)) {
                let a = *a;
                return fleet::run_act(self, a);
            }
            if let Some((_, ch)) = self.hits.chips.iter().find(|(r, _)| inside(*r)) {
                match *ch {
                    Chip::Inbox(i) => inbox::set_show(self, i),
                    Chip::Section(i) => settings::set_section(self, i),
                    Chip::Check(i) => settings::click_check(self, i),
                    Chip::Usage(c) => usage_filters::click(self, c),
                }
                return None;
            }
        }
        // the timeline's panel, wherever it's drawn: the wheel scrolls it, a click folds
        if tree_view::mouse(self, &m) {
            return None;
        }
        if self.view == View::Wall {
            wall::mouse(self, m);
            return None;
        }
        if self.view == View::Monitor {
            monitor::mouse(self, m);
            return None;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let l = self.hits.list;
                if inside(l) && m.row >= self.hits.list_first {
                    let i = (m.row - self.hits.list_first) as usize + self.hits.list_offset;
                    if i < self.hits.list_len {
                        let double = self.last_click.is_some_and(|(t, x, y)| {
                            t.elapsed() < Duration::from_millis(400) && x == m.column && y == m.row
                        });
                        self.last_click = Some((Instant::now(), m.column, m.row));
                        return match self.view {
                            View::Fleet => fleet::click(self, i, double),
                            View::Inbox => inbox::click(self, i, double),
                            View::Settings => {
                                settings::click(self, i, double);
                                None
                            }
                            _ => None,
                        };
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let code = if m.kind == MouseEventKind::ScrollDown {
                    KeyCode::Down
                } else {
                    KeyCode::Up
                };
                return self.key(KeyEvent::new(code, KeyModifiers::NONE));
            }
            _ => {}
        }
        None
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self, f: &mut Frame) {
        // the theme changed (the gallery, a setting): the machines' colours
        // are made again from it
        static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let gen = ui::c::generation();
        if SEEN.swap(gen, std::sync::atomic::Ordering::Relaxed) != gen {
            self.fleet.recolor(&self.cfg);
        }
        let area = f.area();
        // clear to the terminal's own background (never painted over)
        f.render_widget(
            ratatui::widgets::Block::new().style(Style::new().bg(Color::Reset)),
            area,
        );
        let [head, tabs, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, head);
        self.draw_tabs(f, tabs);
        self.hits.list = Rect::default();
        self.hits.buttons.clear();
        self.hits.chips.clear();
        let content = body;
        let body = ui::margin(body);
        match self.view {
            View::Fleet => fleet::draw(self, f, body),
            View::Wall => wall::draw(self, f, body),
            View::Settings => settings::draw(self, f, body),
            View::Inbox => inbox::draw(self, f, body),
            View::Monitor => monitor::draw(self, f, body),
        }
        self.draw_footer(f, foot);
        // showcase mode: the cockpit's brackets, and the celebrations this
        // frame plays where sessions are on screen (none under a popup;
        // their places asked for only while one plays)
        let quiet = !matches!(self.mode, Mode::Normal);
        crate::showcase::content(&self.cfg, f.buffer_mut(), content, quiet, || self.places());
        if matches!(self.mode, Mode::Approvals) {
            approvals::draw(self, f, area);
        }
        match &self.mode {
            Mode::Normal | Mode::Approvals => {}
            Mode::Confirm { title, text, .. } => {
                let w = 60u16.min(area.width.saturating_sub(2));
                let lines: Vec<Line> = ui::wrap(text, w as usize - 6)
                    .into_iter()
                    .map(Line::from)
                    .chain([Line::raw(""), ui::hints(&[("y", "yes"), ("n", "no")])])
                    .collect();
                let r = ui::centered(area, w, lines.len() as u16 + 2);
                crate::ui::clear(f, r);
                f.render_widget(
                    Paragraph::new(lines).block(ui::panel(title.as_str(), true)),
                    r,
                );
            }
            Mode::Input(i) => i.draw(f, area),
            Mode::NewForm(nf) => nf.draw(self, f, area),
            Mode::HostForm(hf) => hf.draw(f, area),
            Mode::Palette(p) => p.draw(f, area, self.t0),
            Mode::Help(h) => help::draw(self, f, area, h),
            Mode::Quick(q) => q.draw(self, f, area),
            Mode::Command(cl) => cl.draw(self, f, area),
            Mode::CloseList(cl) => cl.draw(self, f, area),
            Mode::DismissList(dl) => dl.draw(self, f, area),
            Mode::Tour(i) => tour::draw(self, f, area, *i),
            Mode::News(card) => news::draw(f, area, card),
            Mode::Spaces(p) => spaces::draw(f, area, p),
            Mode::Gallery(g) => gallery::draw(f, area, g),
            Mode::Splashes(g) => splashes::draw(self, f, area, g),
            Mode::Race(v) => race::draw(self, f, area, v),
            Mode::Alerts(p) => alerts::draw(self, f, area, p),
            Mode::UsagePick(p) => p.draw(f, area),
            Mode::UsageCal(c) => c.draw(f, area),
            Mode::Journal(v) => journal::draw(self, f, area, v),
        }
        // showcase mode: the caption of the view just opened (part of the
        // frame, so the lens sees it too)
        let quiet = !matches!(self.mode, Mode::Normal);
        crate::showcase::overlay(&self.cfg, f.buffer_mut(), area, quiet);
        // the theme's living details (a glitch between views, its ambience
        // in empty space), then its last touches: its frames' arms, its ground
        ui::finish(f.buffer_mut());
        ambience::frame(self, f.buffer_mut(), content);
        // the key art's black bars (cyberpunk-light): the header, the footer
        if crate::look::get().band {
            let a = f.area();
            ui::band(f.buffer_mut(), a.y);
            ui::band(f.buffer_mut(), a.bottom().saturating_sub(1));
        }
    }

    /// Where each session is on this frame: its Fleet row, or its tile in
    /// Focus / Tile (showcase mode's celebrations land there).
    fn places(&self) -> Vec<(String, Rect)> {
        match self.view {
            View::Fleet => {
                let (l, off) = (self.hits.list, self.hits.list_offset);
                let rows = fleet::rows(self);
                self.fv
                    .linemap
                    .iter()
                    .enumerate()
                    .skip(off)
                    .take(l.height as usize)
                    .filter_map(|(j, m)| match rows.get((*m)?)? {
                        fleet::RowId::Sess(k) => Some((
                            k.clone(),
                            Rect::new(l.x, l.y + (j - off) as u16, l.width, 1),
                        )),
                        _ => None,
                    })
                    .collect()
            }
            View::Wall => self.wall.places(),
            _ => Vec::new(),
        }
    }

    fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let up = self
            .fleet
            .hosts
            .iter()
            .filter(|h| h.conn == crate::fleet::Conn::Up)
            .count();
        let s = self.fleet.sessions();
        let n = |st: &str| s.iter().filter(|x| x.status == st).count();
        (up, s.len(), n("working"), n("blocked"), n("done"))
    }

    fn draw_header(&self, f: &mut Frame, r: Rect) {
        let (up, total, working, blocked, done) = self.counts();
        let hosts = self.fleet.hosts.len();
        let caps = crate::look::get().titles == crate::look::Titles::Caps;
        let mut left = vec![
            Span::styled(
                format!(
                    " {} {}",
                    crate::brand::MARK,
                    if caps { "ALEPH" } else { "aleph" }
                ),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::raw("   "),
        ];
        // the theme's living detail (cyberpunk's data readout)
        left.extend(ambience::ornament(
            self.t0.elapsed().as_millis(),
            motion::of(self),
        ));
        // showcase mode: the wordmark shimmers, a chip says so
        crate::showcase::header(&self.cfg, &mut left);
        // ... and `● rec 0:42` while a recording runs
        crate::showcase::rec_chip(&self.cfg, &mut left);
        // ... and `▶ tour 2/5` while the autopilot tour runs
        crate::showcase::tour_chip(&self.cfg, &mut left);
        // the profile in use (aleph profile)
        if let Some(p) = profile_of(&self.cfg) {
            left.push(Span::styled(
                format!(" ◆ {p} "),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ));
            left.push(Span::raw("  "));
        }
        // what always stays when the line is cut to fit: the wordmark and
        // the machine count
        let keep = left.len() + 2;
        let col = if up == hosts {
            c::GREEN()
        } else if up == 0 && hosts > 0 && !self.fleet.polling() {
            c::RED()
        } else {
            c::YELLOW()
        };
        left.push(Span::styled("● ", ui::fg(col)));
        left.push(Span::styled(
            format!("{up}/{hosts} machine{}", if hosts == 1 { "" } else { "s" }),
            ui::muted(),
        ));
        left.push(Span::styled(
            format!("  ·  {total} session{}", if total == 1 { "" } else { "s" }),
            ui::muted(),
        ));
        if working > 0 {
            left.push(Span::styled(
                format!("  ·  {} {working} working", ui::spinner(self.t0)),
                ui::fg(c::WORKING()),
            ));
        }
        if blocked > 0 {
            left.push(Span::styled(
                format!(
                    "  ·  {} {blocked} need{} you",
                    crate::look::get().glyphs.needs,
                    if blocked == 1 { "s" } else { "" }
                ),
                ui::fg(c::NEEDS()).add_modifier(Modifier::BOLD),
            ));
        }
        if done > 0 {
            left.push(Span::styled(
                format!("  ·  ✓ {done} done"),
                ui::fg(c::DONE()),
            ));
        }
        let limited = self
            .fleet
            .sessions()
            .iter()
            .filter(|s| s.limit.is_some())
            .count();
        if limited > 0 {
            left.push(Span::styled(
                format!("  ·  ◷ {limited} at limit"),
                ui::fg(c::LIMIT()),
            ));
        }
        if self.cfg.flag("auto_approve") {
            left.push(Span::styled("  ·  ↯ auto", ui::fg(c::ACCENT())));
        }
        if crate::router::dnd(&self.cfg).is_some() {
            left.push(Span::styled("  ·  ⊘ do not disturb", ui::fg(c::YELLOW())));
        }
        // the OpenRouter account's own spend, beside today's $ (its API, every
        // 5 minutes off this thread)
        let mut or_chip = self.usage.or.shown().map(|s| {
            let today = crate::usage::dash::money_short(s.today.unwrap_or(0.0), false);
            let mut t = format!("  ·  OpenRouter {today}");
            let mut col = c::MUTED();
            if let Some((left, of)) = s.headroom() {
                t.push_str(&format!(
                    " · {} left",
                    crate::usage::dash::money_short(left, false)
                ));
                if of > 0.0 && left / of < 0.1 {
                    col = c::RED();
                } else if of > 0.0 && left / of < 0.25 {
                    col = c::YELLOW();
                }
            }
            if s.stale(crate::cache::now()) {
                t.push('~');
            }
            Span::styled(t, ui::fg(col))
        });
        // today's spend and the fullest plan, as the daemon has them (every minute)
        if let Some(u) = crate::usage::now::cached() {
            use serde_json::Value;
            let cost = u
                .pointer("/total/today/cost")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let est = u
                .pointer("/total/today/estimated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if cost > 0.0 {
                // (scaled or hidden in showcase mode: `showcase.money`)
                let cost = crate::usage::dash::money_short(cost, est);
                left.push(Span::styled(format!("  ·  {cost} today"), ui::muted()));
            }
            if let Some(s) = or_chip.take() {
                left.push(s);
            }
            let fullest = u
                .get("accounts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|a| {
                    let used = a["windows"]
                        .as_array()?
                        .iter()
                        .filter_map(|w| w["used"].as_f64())
                        .fold(f64::NAN, f64::max);
                    (!used.is_nan()).then(|| (used, a["plan"].as_str().unwrap_or("").to_string()))
                })
                .max_by(|a, b| a.0.total_cmp(&b.0));
            if let Some((used, plan)) = fullest {
                let col = if used >= 90.0 {
                    c::RED()
                } else if used >= 70.0 {
                    c::PEACH()
                } else {
                    c::GREEN()
                };
                let plan = if plan.is_empty() {
                    "plan".to_string()
                } else {
                    plan
                };
                left.push(Span::styled(format!("  ·  {plan} {used:.0}%"), ui::fg(col)));
            }
        }
        if let Some(s) = or_chip {
            left.push(s);
        }
        let live = self
            .fleet
            .hosts
            .iter()
            .filter(|h| self.fleet.live(&h.alias) == Some(crate::listen::LState::Live))
            .count();
        let right = if live > 0 {
            Span::styled(
                format!("● live {live}/{} ", self.fleet.hosts.len()),
                ui::fg(c::GREEN()),
            )
        } else {
            let ago = self
                .fleet
                .hosts
                .iter()
                .filter_map(|h| h.last_poll)
                .map(|t| t.elapsed().as_secs())
                .min();
            Span::styled(
                ago.map(|s| format!("updated {} ", ui::ago(s)))
                    .unwrap_or_default(),
                ui::faint(),
            )
        };
        // the connection: online · weak · offline · reconnecting — and with
        // the internet down, the machines that still answer (a local network)
        let state = self.net.state();
        let (up, total) = self.machines_up();
        let (g, t) = crate::net::label_with(&state, up, total);
        let ncol = match state {
            crate::net::State::Online { .. } => c::GREEN(),
            crate::net::State::Weak { .. } => c::YELLOW(),
            crate::net::State::Offline { .. } if up > 0 => c::YELLOW(),
            crate::net::State::Offline { .. } => c::DOWN(),
            crate::net::State::Reconnecting { .. } => c::ACCENT(),
            crate::net::State::Unknown => c::FAINT(),
        };
        let net = if t.is_empty() {
            Span::raw("")
        } else {
            Span::styled(format!("{g} {t}   "), ui::fg(ncol))
        };
        let fresh = if self.new_build {
            Span::styled(
                "⟳ new build   ",
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        };
        let rline = Line::from(vec![fresh, net, right]);
        let rw = ui::line_width(&rline) as u16;
        let mut l = Line::from(left);
        while ui::line_width(&l) as u16 + rw + 1 > r.width && l.spans.len() > keep {
            l.spans.pop();
        }
        f.render_widget(Paragraph::new(l), r);
        if rw + 40 < r.width {
            f.render_widget(Paragraph::new(rline).right_aligned(), r);
        }
    }

    /// The tab that's lit: the view's, or under Focus / Tile the one it was
    /// opened from.
    fn tab_active(&self) -> View {
        if self.view == View::Wall {
            self.wall
                .back
                .filter(|v| *v != View::Wall)
                .unwrap_or(View::Fleet)
        } else {
            self.view
        }
    }

    /// `› Focus · b31 › temper` / `› Tile · 3 sessions` after the tabs.
    fn breadcrumb(&self) -> Vec<Span<'static>> {
        let what = if self.wall.in_focus() {
            "Focus"
        } else if self.wall.in_tile() {
            "Tile"
        } else {
            "Panes"
        };
        let mut v = vec![
            Span::styled(" › ", ui::faint()),
            Span::styled(what, ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)),
        ];
        match self.wall.focused_session(self) {
            Some(s) if self.wall.in_focus() => {
                v.push(Span::styled(" · ", ui::faint()));
                v.push(Span::styled(
                    s.host.clone(),
                    ui::fg(crate::theme::machine(&self.cfg, &s.host)),
                ));
                v.push(Span::styled(" › ", ui::faint()));
                v.push(Span::styled(
                    s.display().to_string(),
                    ui::fg(s.color()).add_modifier(Modifier::BOLD),
                ));
            }
            _ => {
                let n = self.wall.window_len();
                v.push(Span::styled(
                    format!(" · {n} pane{}", if n == 1 { "" } else { "s" }),
                    ui::muted(),
                ));
            }
        }
        v.push(Span::styled("  esc back", ui::faint()));
        v
    }

    /// A tab's badge — always a glyph or a word with the number, so it never
    /// reads as a second key ("1 Fleet 1"); a colour makes it a pill (alerts).
    fn badge(&self, v: View) -> (String, Option<Color>) {
        match v {
            View::Fleet => {
                let (_, _, working, blocked, _) = self.counts();
                let limited = self
                    .fleet
                    .sessions()
                    .iter()
                    .filter(|s| s.limit.is_some())
                    .count();
                let down = self
                    .fleet
                    .hosts
                    .iter()
                    .filter(|h| {
                        matches!(&h.conn, crate::fleet::Conn::Down(e) if !crate::fleet::not_running(e))
                    })
                    .count();
                if blocked > 0 {
                    (
                        format!("{} {blocked}", crate::look::needs()),
                        Some(c::NEEDS()),
                    )
                } else if limited > 0 {
                    (format!("◷ {limited}"), Some(c::LIMIT()))
                } else if down > 0 {
                    (format!("✕ {down}"), Some(c::DOWN()))
                } else if working > 0 {
                    (format!("✻ {working}"), None)
                } else {
                    (String::new(), None)
                }
            }
            View::Settings => doctor::badge(self),
            View::Inbox => {
                let n = inbox::unread(self);
                (
                    if n > 0 {
                        format!("{n} new")
                    } else {
                        String::new()
                    },
                    Some(c::ACCENT()),
                )
            }
            _ => (String::new(), None),
        }
    }

    /// The tab bar: numbered tabs (the number is the key), the active one as a
    /// filled pill, badges in their own colour, a hint on the right.
    fn draw_tabs(&mut self, f: &mut Frame, r: Rect) {
        let mut top: Vec<Span> = vec![Span::raw(" ")];
        let mut x = r.x + 1;
        self.hits.tabs.clear();
        // full names when they fit, else initials for the inactive tabs
        let full: usize = View::ALL.iter().map(|v| v.name().len() + 7).sum();
        let short = full + 2 > r.width as usize;
        let lit = self.tab_active();
        for (i, v) in View::ALL.iter().enumerate() {
            let active = *v == lit;
            let (badge, bcol) = self.badge(*v);
            let name = if short && !active {
                &v.name()[..1]
            } else {
                v.name()
            };
            // the key as a quiet hint, the name, and — only when something
            // there wants you — a coloured dot (the counts live in the header)
            let mut spans: Vec<Span> = Vec::new();
            let mono = c::mono();
            spans.push(Span::styled(
                format!("{} ", i + 1),
                if active {
                    ui::fg(c::ACCENT())
                } else {
                    ui::faint()
                },
            ));
            spans.push(Span::styled(
                name.to_string(),
                if active {
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                } else {
                    ui::muted()
                },
            ));
            if let (false, Some(col)) = (badge.is_empty(), bcol) {
                // the dot, and what it counts (▲ 2 · 3 new · ✗1)
                spans.push(Span::styled(
                    if mono { " *" } else { " •" },
                    ui::fg(col).add_modifier(Modifier::BOLD),
                ));
                if !short {
                    let count = badge.trim_start_matches(|c: char| !c.is_alphanumeric());
                    let count = if badge.contains("new") {
                        badge.as_str()
                    } else {
                        count
                    };
                    spans.push(Span::styled(format!(" {count}"), ui::fg(col)));
                }
            }
            spans.push(Span::raw(" "));
            let w: usize = spans.iter().map(|s| ui::width(&s.content)).sum();
            self.hits.tabs.push((Rect::new(x, r.y, w as u16, 2), *v));
            x += w as u16 + 1;
            top.extend(spans);
            top.push(Span::raw(" "));
        }
        // under an overlay: where you are, as a breadcrumb (esc goes back)
        if self.view == View::Wall {
            top.extend(self.breadcrumb());
        }
        let used: usize = top.iter().map(|s| ui::width(&s.content)).sum();
        let hint = format!(
            "1–{} views · ⇥ sub-views · ? keys · ctrl+k palette ",
            View::ALL.len()
        );
        // agents waiting on you: the approval board's key, ahead of the hint
        let waiting = self.fleet.sessions().iter().filter(|s| s.blocked()).count();
        // (showcase mode: that room is its tagline's)
        let chip = (waiting > 0 && !crate::showcase::on()).then(|| {
            format!(
                " {} {waiting} waiting · {} ",
                crate::look::get().glyphs.needs,
                self.keys.key_for(Act::Approvals)
            )
        });
        // (no room for both: the hint stays — the header says who waits)
        let chip =
            chip.filter(|c| used + ui::width(&hint) + ui::width(c) + 2 + 4 < r.width as usize);
        let cw = chip.as_deref().map_or(0, |c| ui::width(c) + 2);
        // the room between the tabs and that hint (showcase mode's tagline)
        let mut gap = r.width as usize - used.min(r.width as usize);
        if used + ui::width(&hint) + cw + 4 < r.width as usize {
            gap -= ui::width(&hint) + cw;
            top.push(Span::raw(
                " ".repeat(r.width as usize - used - ui::width(&hint) - cw),
            ));
            if let Some(chip) = chip {
                let x = r.x + (r.width as usize - ui::width(&hint) - cw) as u16;
                self.hits.buttons.push((
                    Rect::new(x, r.y, ui::width(&chip) as u16, 1),
                    Act::Approvals,
                ));
                top.push(Span::styled(
                    chip,
                    Style::new()
                        .fg(c::BASE())
                        .bg(c::NEEDS())
                        .add_modifier(Modifier::BOLD),
                ));
                top.push(Span::raw("  "));
            }
            top.push(Span::styled(hint, ui::faint()));
        }
        // the rule under the tabs, accent under the active one
        let mut under: Vec<Span> = Vec::new();
        let mut cx = r.x;
        for (rect, v) in &self.hits.tabs {
            if rect.x > cx {
                under.push(Span::styled(
                    "─".repeat((rect.x - cx) as usize),
                    ui::fg(c::LINE()),
                ));
            }
            let active = *v == lit;
            under.push(Span::styled(
                if active { "━" } else { "─" }.repeat(rect.width as usize),
                ui::fg(if active { c::ACCENT() } else { c::LINE() }),
            ));
            cx = rect.x + rect.width;
        }
        under.push(Span::styled(
            "─".repeat((r.x + r.width).saturating_sub(cx) as usize),
            ui::fg(c::LINE()),
        ));
        f.render_widget(Paragraph::new(vec![Line::from(top), Line::from(under)]), r);
        let spare = Rect::new(r.x + used as u16, r.y, gap as u16, 1).intersection(r);
        crate::showcase::spare(&self.cfg, f.buffer_mut(), spare);
    }

    /// What showcase mode's cockpit HUD reads out.
    fn gauges(&self) -> crate::showcase::Gauges {
        let (up, _, working, needs, _) = self.counts();
        crate::showcase::Gauges {
            up,
            machines: self.fleet.hosts.len(),
            working,
            needs,
        }
    }

    fn draw_footer(&self, f: &mut Frame, r: Rect) {
        if let Some(t) = self.current_toast() {
            let col = match t.level {
                Level::Ok => c::GREEN(),
                Level::Info => c::BLUE(),
                Level::Warn => c::YELLOW(),
                Level::Err => c::RED(),
            };
            let icon = match t.level {
                Level::Ok => "✓",
                Level::Info => "●",
                Level::Warn => "▲",
                Level::Err => "✗",
            };
            let more = self
                .toasts
                .iter()
                .filter(|x| x.at.elapsed() < self.toast_for(x.level))
                .count()
                .saturating_sub(1);
            let mut l = vec![
                Span::raw(" "),
                Span::styled(
                    format!(" {icon} "),
                    Style::new()
                        .fg(c::BASE())
                        .bg(col)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {}", ui::trunc(&t.text, r.width as usize - 12)),
                    ui::fg(col),
                ),
            ];
            if more > 0 {
                l.push(Span::styled(format!("  +{more}"), ui::faint()));
            }
            let l = Line::from(l);
            let used = ui::line_width(&l) as u16;
            f.render_widget(Paragraph::new(l), r);
            crate::showcase::footer(&self.cfg, f.buffer_mut(), r, used);
            return;
        }
        if self.view == View::Wall && !self.wall.captures() {
            if let Some(l) = wall::answer_line(self, r.width) {
                let used = ui::line_width(&l) as u16;
                f.render_widget(Paragraph::new(l), r);
                crate::showcase::footer(&self.cfg, f.buffer_mut(), r, used);
                return;
            }
        }
        let items: Vec<(&str, &str)> = match self.view {
            _ if self.layout_mode.is_some() => panes::hints(),
            View::Fleet => fleet::hints(self),
            View::Wall => wall::hints(self),
            View::Settings => settings::hints(self),
            View::Inbox => inbox::hints(self),
            View::Monitor => monitor::hints(self),
        };
        let mut items = items;
        if self.view != View::Wall {
            items.push(("?", "help"));
        }
        // showcase mode: room kept at the right end for the cockpit's readouts
        let keep = crate::showcase::status_room(&self.cfg, &self.gauges());
        let w = r.width.saturating_sub(keep);
        let bar = (self.view == View::Fleet && self.layout_mode.is_none() && !self.fv.filtering)
            .then(|| batch::action_bar(self, w))
            .flatten();
        let l = bar.unwrap_or_else(|| ui::hints_fit(&items, w));
        let used = ui::line_width(&l) as u16;
        f.render_widget(Paragraph::new(l), r);
        // showcase mode: the cockpit's live readouts at the bar's right end,
        // and the keys just pressed in the room left
        crate::showcase::status(&self.cfg, f.buffer_mut(), r, used, || self.gauges());
        crate::showcase::footer(&self.cfg, f.buffer_mut(), r, used);
    }

    // ------------------------------------------------------------ loop

    pub fn run(&mut self, tui: &mut Tui) -> io::Result<Handoff> {
        loop {
            while let Ok(ev) = self.rx.try_recv() {
                self.on_bg(ev);
            }
            // showcase mode: where a recording or a poster went (held a
            // while, to be read)
            while let Some((ok, m)) = crate::showcase::capture_note() {
                self.held(ok, m);
            }
            crate::showcase::capture_alerts(|m| self.toast(Level::Warn, m));
            self.follow_hint();
            self.open_pending_focus();
            // showcase's demo world went on or off: this screen starts again
            // as the other world (same alternate screen, no flash)
            if crate::showcase::world_changed() {
                self.drop_connections();
                self.fleet.stop_listening();
                // the new process draws on a blank screen, from a blank buffer
                let _ = tui.term.clear();
                crate::showcase::restart();
            }
            // the probe slows down while nobody's looking
            self.net
                .set_every(Duration::from_secs(if self.hints.resting() {
                    30
                } else {
                    5
                }));
            if self.hints.resting() {
                // nobody's looking and a newer build is installed: quit, and
                // the session's loop starts the new one
                if self.upgrade.replaced() {
                    crate::log::info("dashboard", "a newer build is installed — restarting");
                    return Ok(Handoff::Quit);
                }
                // detached in the `aleph` tmux session: the daemon does the
                // watching; check back every second without polling or drawing
                std::thread::sleep(Duration::from_millis(900));
                if event::poll(Duration::from_millis(10))? {
                    let _ = event::read()?;
                }
                continue;
            }
            self.net_toast();
            // a newer build installed while this one runs: said once, then
            // the header's chip; the palette restarts on it
            if !self.new_build && self.upgrade.replaced() {
                self.new_build = true;
                self.toast(
                    Level::Info,
                    "Aleph was updated — ctrl+k › Restart on the new build (you're on the old one)",
                );
            }
            // the doctor, once, a little after start (Settings' dot)
            if self.t0.elapsed() > Duration::from_secs(10) {
                doctor::ensure(self);
            }
            let fresh: Vec<String> = self
                .fleet
                .hosts
                .iter()
                .filter(|h| h.conn == crate::fleet::Conn::Unknown)
                .map(|h| h.alias.clone())
                .collect();
            let tr = self.fleet.tick();
            if self.owner.1.elapsed() > Duration::from_secs(5) {
                // run the hub only while no daemon does (and hand it over the
                // moment one starts)
                if crate::auto::Hub::owner(&self.cfg, false) {
                    if self.hub_lock.is_none() {
                        self.hub_lock = crate::auto::HubLock::try_take();
                    }
                    // and the daemon's socket, while no daemon runs
                    if self.hub_lock.is_some() && self.api.is_none() {
                        if let crate::daemon::Bind::Ours(s) = crate::daemon::bind() {
                            let sh = crate::daemon::Shared::of("dashboard");
                            crate::daemon::serve(s, sh.clone());
                            self.api = Some(sh);
                            self.api_seen = None;
                        }
                    }
                } else {
                    self.hub_lock = None;
                }
                self.owner = (self.hub_lock.is_some(), Instant::now());
            }
            if let Some(sh) = &self.api {
                if self.api_seen != Some(self.fleet.changed_at) {
                    self.api_seen = Some(self.fleet.changed_at);
                    sh.publish(self.fleet.to_json(&self.cfg), self.owner.0);
                }
            }
            if !self.owner.0 {
                // closes due while nobody runs the hub (`closing.rs`)
                self.hub.closer.tick(&self.cfg, &self.fleet, false);
                // and needs-you alerts that have gone stale
                self.hub.purge_borrowed(&self.cfg, &self.fleet);
            }
            if self.owner.0 {
                self.hub.tick(&self.cfg, &self.fleet, &tr, &fresh);
                // no daemon: this dashboard reads the agents' logs now and then
                self.usage.bg.tick(&self.cfg, &self.fleet.unreachable());
            }
            // showcase mode: a supernova where a session finished, a pulsar's
            // flash where a question was answered
            crate::showcase::transitions(&self.cfg, &tr, &fresh);
            // ... and everyone's light motion: a tick where one finished
            motion::on_transitions(self, &tr, &fresh);
            self.on_transitions(tr);
            self.look_again();
            self.settings_moved(false);
            self.tell_drift();
            self.tell_onscreen();
            self.sync_telemetry();
            self.push_tokens();
            fleet::pump(self);
            wall::pump(self);
            inbox::pump(self);
            usage::pump(self);
            outside::pump(self);
            timechart::pump(self);
            tree_view::pump(self);
            approvals::pump(self);
            // showcase mode: the autopilot tour's next move (navigation only),
            // and what this dashboard shows, for presenter sync
            crate::showcase::autopilot(self);
            crate::showcase::present(&self.cfg, || self.presenting());
            // showcase mode: the cockpit palette follows the mode and its setting
            crate::showcase::repaint(&self.cfg);
            // a paste goes to the agent as one while keys go straight to it
            crate::term::bracketed_paste(
                self.view == View::Wall && self.wall.typing && matches!(self.mode, Mode::Normal),
            );
            // the frame shown whole (DEC mode 2026: the terminal holds what it
            // gets until the end, so it never shows half of one); a terminal
            // without it ignores both
            let _ = crossterm::queue!(
                tui.term.backend_mut(),
                crossterm::terminal::BeginSynchronizedUpdate
            );
            tui.term.draw(|f| {
                self.draw(f);
                crate::logo::images::frame(f.buffer_mut());
                // showcase mode: every private word swapped, cell for cell
                crate::showcase::buffer(f.buffer_mut());
                // …and then its celebrations, into the masked frame
                crate::showcase::celebrations(f.buffer_mut());
                // ... then your watermark, a recording's frame, a poster
                crate::showcase::finish(&self.cfg, f.buffer_mut(), self.view.name());
            })?;
            // logos as images (kitty, Ghostty, iTerm2, WezTerm): the pictures
            // this frame asked for (showcase: none while paused)
            let pics = crate::showcase::pictures(crate::logo::images::take());
            {
                use std::io::Write;
                let b = tui.term.backend_mut();
                if !pics.is_empty() {
                    let _ = b.write_all(pics.as_bytes());
                }
                let _ = crossterm::execute!(b, crossterm::terminal::EndSynchronizedUpdate);
            }
            // a splash previewing: drawn as smoothly as the splash itself
            let wait = if splashes::animating(self) { 16 } else { 60 };
            if event::poll(Duration::from_millis(wait))? {
                let h = match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
                    Event::Mouse(m) => self.on_mouse(m),
                    Event::FocusGained => {
                        self.focused = true;
                        None
                    }
                    Event::FocusLost => {
                        self.focused = false;
                        None
                    }
                    Event::Paste(t) if self.view == View::Wall && self.wall.typing => {
                        wall::paste(self, &t);
                        None
                    }
                    // anywhere else: as if typed (a line at a time, no enter)
                    Event::Paste(t) => {
                        for c in t.chars().filter(|c| !c.is_control()) {
                            if let Some(h) =
                                self.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                            {
                                return Ok(h);
                            }
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(h) = h {
                    return Ok(h);
                }
            }
            // the splash gallery's p: that one full screen, then back
            splashes::play_asked(self, tui);
        }
    }
}

/// The fleet and hub, lent to a thread during a hand-off.
struct Ticker {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    h: std::thread::JoinHandle<(Fleet, crate::auto::Hub)>,
}

/// The dashboard inside the `aleph` tmux session.
pub(crate) struct MoshiSession {
    pub on: bool,
    pub seen: Option<std::time::SystemTime>,
    pub checked: Instant,
    /// someone (a phone, a terminal) is attached to the session
    pub attached: bool,
}

impl MoshiSession {
    pub fn new(on: bool) -> MoshiSession {
        MoshiSession {
            on,
            seen: None,
            checked: Instant::now() - Duration::from_secs(5),
            attached: true,
        }
    }
    /// Nobody is looking: poll and draw at a crawl.
    pub fn resting(&self) -> bool {
        self.on && !self.attached
    }
}

/// Clients attached to the tmux session this process runs in.
/// "Open in herdr ↗": attach to this session in a new tab of the terminal
/// Aleph runs in (Warp, iTerm2, Terminal, Ghostty, kitty, WezTerm); Aleph
/// stays where it is.
pub(crate) fn open_in_herdr_tab(d: &mut Dash, s: &Session) {
    use crate::showcase::{Gate, Outside};
    // showcase mode: herdr's own screen isn't masked
    match crate::showcase::gate(Outside::Herdr) {
        Gate::Open | Gate::Focus(_) => open_tab_now(d, s),
        Gate::Ask { title, text } => {
            d.mode = Mode::Confirm {
                title: title.into(),
                text: text.into(),
                on_yes: Pending::OpenTab(Box::new(s.clone())),
            };
        }
        Gate::Refuse(why) => d.toast(Level::Info, why),
    }
}

fn open_tab_now(d: &mut Dash, s: &Session) {
    let cmd = crate::termlaunch::resume_cmd(&s.host, &s.pane_id);
    let setting = d.cfg.get_or("terminal");
    let label = s.display().to_string();
    d.toast(Level::Info, format!("opening {label} in a new tab…"));
    d.spawn(None, move || {
        crate::termlaunch::open(&setting, &cmd, &label)
            .map(|t| format!("{label} is open in herdr ({})", t.title()))
    });
}

/// `herdr_view = needs-you-first`: herdr's sidebar lists what needs you
/// first while the dashboard runs (and herdr's own order comes back after).
/// Off the UI thread; on the way out it waits at most a second and a half.
fn herdr_view(d: &Dash, on: bool) {
    if d.cfg.get_or("herdr_view") != "needs-you-first" {
        return;
    }
    let hosts: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    for h in hosts {
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = crate::sock::view(&h, on);
            let _ = tx.send(());
        });
    }
    if !on {
        drop(tx);
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(1500) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(()) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => {}
            }
        }
    }
}

fn tmux_attached() -> bool {
    crate::proc::cap(
        "tmux",
        &["display-message", "-p", "#{session_attached}"],
        Duration::from_secs(2),
    )
    .text()
    .trim()
    .parse::<u32>()
    .map_or(true, |n| n > 0)
}

/// Is this dashboard running inside the `aleph` tmux session (the one
/// Moshi's session picker and its cards open)?
fn in_moshi_session() -> bool {
    std::env::var_os("TMUX").is_some()
        && crate::proc::cap(
            "tmux",
            &["display-message", "-p", "#S"],
            Duration::from_secs(2),
        )
        .text()
        .trim()
            == crate::moshi::TMUX_SESSION
}

// ------------------------------------------------------------------ key map

/// Fleet-view actions; each can be rebound with `key.<action>=<key>`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) enum Act {
    Up,
    Down,
    PrevHost,
    NextHost,
    Mark,
    Filter,
    Refresh,
    Open,
    Attach,
    Direct,
    OpenTab,
    New,
    Presets,
    Note,
    Duplicate,
    /// a plain terminal (no agent) on the machine, opened in Focus
    Shell,
    Mute,
    Browse,
    Wall,
    Send,
    Approve,
    Keys,
    Broadcast,
    Rename,
    Close,
    Copy,
    Tile,
    TypeInto,
    Hub,
    /// hand the session's work to another agent (`handoff.rs`)
    Handoff,
    /// open the session's folder in your editor (`editor.rs`)
    Editor,
    Collapse,
    Expand,
    Queue,
    Heal,
    StatsMode,
    AddHost,
    RemoveHost,
    EditHost,
    TestHost,
    KeySetup,
    MakeDefault,
    PauseHost,
    HostColor,
    HerdrSetup,
    Wake,
    /// Fleet's rows as timelines of their states (`fleet.timeline`)
    Timeline,
    /// the selected session's timeline (its prompts, turns, tool calls) in the right panel
    TimelinePanel,
    /// the alerts you missed and the last toasts (every view)
    Alerts,
    Approvals,
    /// keep a session that's closing (`close.grace`)
    Undo,
}

/// (action, config name, default keys, help)
pub(crate) const ACTS: &[(Act, &str, &str, &str)] = &[
    (Act::Up, "up", "k", "move up"),
    (Act::Down, "down", "j", "move down"),
    (Act::PrevHost, "prev_machine", "[", "previous machine"),
    (Act::NextHost, "next_machine", "]", "next machine"),
    (
        Act::Mark,
        "mark",
        "space",
        "mark a session (or fold a machine); then y s Q x act on every marked one",
    ),
    (
        Act::Filter,
        "filter",
        "/",
        "filter by name, folder, title or machine",
    ),
    (
        Act::Refresh,
        "refresh",
        "r",
        "refresh now (on a machine: reconnect to it)",
    ),
    (
        Act::Open,
        "open",
        "enter",
        "open in Focus: fullscreen here, with messages, queue and splits (enter_opens)",
    ),
    (
        Act::Attach,
        "attach",
        "a",
        "attach in herdr, in this terminal",
    ),
    (Act::Direct, "direct", "A", "attach straight to the agent"),
    (
        Act::TypeInto,
        "type_into",
        "I",
        "type straight into it, here (Focus; ctrl+q leaves)",
    ),
    (
        Act::OpenTab,
        "open_tab",
        "O",
        "open in herdr in a new terminal tab (Aleph stays)",
    ),
    (
        Act::New,
        "new",
        "n",
        "new session: pick a workspace or folder, and an agent",
    ),
    (
        Act::Presets,
        "presets",
        "P",
        "start a preset… (sessions marked: save them as one)",
    ),
    (
        Act::Duplicate,
        "duplicate",
        "D",
        "another of the same agent, folder and arguments, beside it",
    ),
    (
        Act::Shell,
        "shell",
        "!",
        "a plain terminal (no agent) on the machine, in the session's folder, opened in Focus to type",
    ),
    (
        Act::Mute,
        "mute",
        "m",
        "mute the session's notifications for an hour (again: unmute)",
    ),
    (
        Act::Note,
        "note",
        "N",
        "a note on the session (#tags; kept after it ends)",
    ),
    (Act::Browse, "browse", "i", "new session in a folder…"),
    (
        Act::Wall,
        "wall",
        "w",
        "tile the marked sessions (or the machine's) side by side",
    ),
    (
        Act::Send,
        "send",
        "s",
        "send a prompt (marked: the same one to each)",
    ),
    (
        Act::Approve,
        "approve",
        "y",
        "approve its question with its yes option (marked: every marked one that asks)",
    ),
    (Act::Keys, "keys", "K", "send keys"),
    (
        Act::Broadcast,
        "broadcast",
        "b",
        "broadcast to marked / machine",
    ),
    (Act::Rename, "rename", "R", "rename"),
    (
        Act::Close,
        "close",
        "x",
        "close, after close.grace (on a machine: tick which of its sessions; marked: all of them)",
    ),
    (
        Act::Undo,
        "undo",
        "u",
        "keep a session that's closing: the selected one, else the last close",
    ),
    (Act::Copy, "copy", "c", "copy recent output"),
    (
        Act::Tile,
        "tile",
        "t",
        "move the marked sessions into one herdr tab",
    ),
    (Act::Hub, "hub", "H", "open the herdr hub"),
    (
        Act::Editor,
        "editor",
        "o",
        "open its folder in your editor (Remote-SSH for another machine)",
    ),
    (
        Act::Handoff,
        "handoff",
        "h",
        "hand the work to another agent: a hand-off note, then a new session",
    ),
    (Act::Collapse, "collapse", "left", "fold machine"),
    (Act::Expand, "expand", "right", "unfold machine"),
    (
        Act::Queue,
        "queue",
        "Q",
        "queue a task for this machine's next idle agent (marked: for each, sent once it's free)",
    ),
    (
        Act::Heal,
        "heal",
        "S",
        "start herdr on this machine and restore its sessions",
    ),
    (
        Act::StatsMode,
        "stats_mode",
        "v",
        "machine panel: summary · GPU · processes",
    ),
    (
        Act::AddHost,
        "add_machine",
        "+",
        "add a machine (tests the connection first)",
    ),
    (
        Act::RemoveHost,
        "remove_machine",
        "-",
        "take the selected machine out of the fleet",
    ),
    (
        Act::EditHost,
        "edit_machine",
        "e",
        "edit the machine's address, key and jump host (tests it first)",
    ),
    (
        Act::TestHost,
        "test_machine",
        "T",
        "test key login to the machine",
    ),
    (
        Act::KeySetup,
        "key_setup",
        "l",
        "set up key login (ssh-copy-id asks the password once, in ssh)",
    ),
    (
        Act::MakeDefault,
        "default_machine",
        "*",
        "make it the default machine for new sessions",
    ),
    (
        Act::PauseHost,
        "pause_machine",
        "Z",
        "pause or resume polling the machine (it stays in the fleet)",
    ),
    (
        Act::HostColor,
        "machine_color",
        "C",
        "next colour for the machine",
    ),
    (
        Act::HerdrSetup,
        "herdr_setup",
        "M",
        "set the machine up in a new tab: herdr, then a checklist of agents (asks before installing)",
    ),
    (
        Act::Wake,
        "wake",
        "W",
        "wake a sleeping machine (Wake-on-LAN, sent from another machine or this computer)",
    ),
    (
        Act::Timeline,
        "timeline",
        "g",
        "each session's states over the last hours, in its row (again: folders and titles)",
    ),
    (
        Act::TimelinePanel,
        "timeline_panel",
        "G",
        "the selected session's timeline in the right panel — its prompts, turns, tool calls and subagents, with time, tokens and $ (again: its recent output)",
    ),
    (
        Act::Alerts,
        "alerts",
        "ctrl+n",
        "the alerts you missed and the last 20 toasts (in every view)",
    ),
    (
        Act::Approvals,
        "approvals",
        "ctrl+a",
        "every agent waiting on you, its question as it shows it: y approve · n deny · 1–9 · Y all (in every view)",
    ),
];

pub(crate) struct Keys {
    map: HashMap<String, Act>,
    pub(crate) custom: Vec<(String, String, String)>,
}

impl Keys {
    pub(crate) fn from_config(cfg: &Config) -> Keys {
        let mut map = HashMap::new();
        // defaults first, then the user's bindings, so an override always wins
        let mut user = Vec::new();
        for (a, name, def, _) in ACTS {
            match cfg.get(&format!("key.{name}")) {
                Some(k) => user.push((*a, k)),
                None => {
                    map.insert(def.to_string(), *a);
                }
            }
        }
        for (a, k) in user {
            for k in k.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                map.insert(k.to_string(), a);
            }
        }
        // arrows always work, whatever the bindings
        map.entry("delete".into()).or_insert(Act::RemoveHost);
        map.insert("up".into(), Act::Up);
        map.insert("down".into(), Act::Down);
        let custom = cfg
            .with_prefix("action.")
            .into_iter()
            .filter_map(|(k, v)| {
                let (label, cmd) = v.split_once('|')?;
                Some((k, label.trim().to_string(), cmd.trim().to_string()))
            })
            .collect();
        Keys { map, custom }
    }
    pub(crate) fn act(&self, k: &KeyEvent) -> Option<Act> {
        self.map.get(&key_str(k)).copied()
    }
    /// The action a key (`x`, `ctrl+o`) runs now.
    pub(crate) fn owner(&self, key: &str) -> Option<Act> {
        self.map.get(key).copied()
    }
    /// The first key bound to `a`, for hints.
    pub(crate) fn key_for(&self, a: Act) -> String {
        let mut v: Vec<&String> = self
            .map
            .iter()
            .filter(|(_, x)| **x == a)
            .map(|(k, _)| k)
            .collect();
        v.sort_by_key(|k| (k.len(), k.to_string()));
        v.first().map(|s| s.to_string()).unwrap_or_default()
    }
}

/// `j`, `K`, `ctrl+k`, `enter`, `space`, `shift+tab`, `f5` …
pub(crate) fn key_str(k: &KeyEvent) -> String {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let base = match k.code {
        KeyCode::Char(' ') => "space".to_string(),
        KeyCode::Char(ch) => {
            if ctrl {
                ch.to_ascii_lowercase().to_string()
            } else {
                ch.to_string()
            }
        }
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::F(n) => format!("f{n}"),
        _ => String::new(),
    };
    let mut s = String::new();
    if ctrl {
        s.push_str("ctrl+");
    }
    if alt {
        s.push_str("alt+");
    }
    s + &base
}

// ------------------------------------------------------------------ hand-off loop

/// Copy text to the clipboard: OSC 52 (works over ssh / in most terminals)
/// plus a native tool when there is one.
pub(crate) fn copy(text: &str) -> bool {
    copy_as(text, false)
}

/// Copy an agent's screen (showcase: as the screen shows it).
pub(crate) fn copy_screen(text: &str) -> bool {
    copy_as(text, true)
}

/// Why a copy didn't happen.
pub(crate) fn copy_failed() -> &'static str {
    crate::showcase::clip_refused().unwrap_or("couldn't reach a clipboard")
}

fn copy_as(text: &str, screen: bool) -> bool {
    // showcase: the masked text, the real one, or nothing (`showcase.clipboard`)
    let Some(text) = crate::showcase::clip(text, screen) else {
        return false;
    };
    let text = text.as_ref();
    let b = b64_encode(text.as_bytes());
    let mut ok = false;
    if let Ok(mut t) = crate::term::open_tty() {
        ok = t.write_all(format!("\x1b]52;c;{b}\x07").as_bytes()).is_ok();
    }
    for (prog, args) in [
        ("pbcopy", &[][..]),
        ("wl-copy", &[][..]),
        ("xclip", &["-selection", "clipboard"][..]),
    ] {
        if crate::proc::which(prog).is_some() {
            let r = crate::proc::run(
                std::process::Command::new(prog).args(args),
                Duration::from_secs(5),
                Some(text.as_bytes()),
            );
            if r.is_ok_and(|o| o.ok()) {
                return true;
            }
        }
    }
    ok
}

/// Standard base64 (OSC 52 payloads; no extra dependency).
pub(crate) fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for ch in data.chunks(3) {
        let n = (ch[0] as u32) << 16
            | (*ch.get(1).unwrap_or(&0) as u32) << 8
            | *ch.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= ch.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The dashboard with its hand-off loop: anything that needs the real
/// terminal (attach, password prompts, setup) runs here and the dashboard
/// comes back afterwards.
pub fn run_dashboard(cfg: Config, start: Start) -> std::process::ExitCode {
    match Tui::enter() {
        Ok(t) => run_dashboard_in(t, cfg, start),
        Err(e) => crate::out::fail(&format!("terminal: {e}")),
    }
}

/// The dashboard on a terminal that's already up (setup hands its own over,
/// so the screen never flashes).
pub fn run_dashboard_in(tui: Tui, cfg: Config, start: Start) -> std::process::ExitCode {
    use crate::out::*;
    let mut tui = tui;
    // this screen draws logos as images (pictures found and written each frame)
    crate::logo::images::set_output(true);
    // showcase mode: new stand-ins for everything, every time it starts
    crate::showcase::started();
    let mut d = Dash::new(cfg, &start);
    // the splash while the first polls go out (`ui.splash`)
    splash::run(&mut tui, &d.cfg, &splash::Info::of(&d));
    // after an update: what's new, once (not over the tour)
    news::open_if_new(&mut d);
    let mut ticker: Option<Ticker> = None;
    loop {
        if let Some(t) = ticker.take() {
            d.take_back(t);
        }
        // showcase mode: a recording paused for a hand-off goes on
        crate::showcase::cast_back();
        let h = match d.run(&mut tui) {
            Ok(h) => h,
            Err(e) => {
                drop(tui);
                return fail(&format!("terminal: {e}"));
            }
        };
        // ... and pauses (or stops) while something else has the terminal;
        // on the way out it's saved, and said once the screen is gone
        let saved = crate::showcase::cast_away(&d.cfg, matches!(h, Handoff::Quit));
        // our pictures go before anyone else gets the terminal (sent again later)
        let gone = crate::logo::images::clear();
        if !gone.is_empty() {
            use std::io::Write;
            let b = tui.term.backend_mut();
            let _ = b.write_all(gone.as_bytes());
            let _ = Write::flush(b);
        }
        // nothing of ours holds ssh while herdr (or a prompt) has the terminal
        d.drop_connections();
        // the hub keeps going while we're away (auto-approve, queue, Moshi)
        if !matches!(h, Handoff::Quit) {
            ticker = d.tick_in_background();
        }
        let wait = d.cfg.flag("return_to_dashboard");
        let pause = || {
            let _ = ask("Press enter to return to the dashboard", "");
        };
        match &h {
            Handoff::Quit => {
                drop(tui);
                herdr_view(&d, false);
                // closes still in their grace, with nobody else to carry them out
                crate::closing::on_quit(&d.cfg, d.hub_lock.take());
                if let Some(m) = saved {
                    ok(&m);
                }
                return std::process::ExitCode::SUCCESS;
            }
            Handoff::Restart => {
                drop(tui);
                crate::closing::on_quit(&d.cfg, d.hub_lock.take());
                // the same program and options, on the new build (same pid)
                #[cfg(unix)]
                {
                    use std::os::unix::process::CommandExt;
                    if let Ok(exe) = std::env::current_exe() {
                        let e = std::process::Command::new(exe)
                            .args(std::env::args_os().skip(1))
                            .exec();
                        crate::out::err(&format!("couldn't start the new build: {e}"));
                    }
                }
                return std::process::ExitCode::FAILURE;
            }
            Handoff::Attach {
                target,
                pane,
                label,
            } => {
                let col = crate::theme::machine(&d.cfg, &target.ssh);
                let job = crate::launch::Job::Attach {
                    pane: pane.clone(),
                    label: label.clone(),
                };
                let (t2, o) = crate::launch::run(tui, target, job, wait, col);
                tui = t2;
                d.landed(target, o);
                d.after_handoff(&h);
                continue;
            }
            Handoff::New { target, opts } => {
                let col = crate::theme::machine(&d.cfg, &target.ssh);
                let (t2, o) = crate::launch::run(
                    tui,
                    target,
                    crate::launch::Job::New(opts.clone()),
                    wait,
                    col,
                );
                tui = t2;
                d.landed(target, o);
                d.after_handoff(&h);
                continue;
            }
            Handoff::Direct {
                target,
                pane,
                takeover,
            } => {
                let _ = tui.suspend_keep_screen();
                // `terminal attach` takes any pane (`agent attach` refuses a
                // plain shell); the same direct attach
                let mut cmd = if crate::sshcfg::is_local(&target.ssh) {
                    let mut c = std::process::Command::new(herdr::bin());
                    c.args(["terminal", "attach", pane.as_str()]);
                    if *takeover {
                        c.arg("--takeover");
                    }
                    c
                } else {
                    let herdr_bin = crate::chan::herdr_path(&target.ssh);
                    let mut args: Vec<String> = crate::sshcfg::interactive();
                    args.push("-t".into());
                    args.push(target.ssh.clone());
                    let mut remote = format!(
                        "{}{} terminal attach {}",
                        crate::chan::herdr_env(&target.ssh),
                        crate::proc::sh_quote(&herdr_bin),
                        crate::proc::sh_quote(pane)
                    );
                    if *takeover {
                        remote.push_str(" --takeover");
                    }
                    args.push(remote);
                    let mut c = std::process::Command::new(crate::sshcfg::ssh_bin());
                    c.args(&args);
                    c
                };
                let out = cmd
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .and_then(|c| c.wait_with_output());
                let _ = tui.resume();
                match out {
                    Ok(o) if !o.status.success() => {
                        let e = String::from_utf8_lossy(&o.stderr).to_string();
                        if e.contains("already") && !takeover {
                            let _ = d.tx.send(Bg::Busy {
                                target: target.clone(),
                                pane: pane.clone(),
                            });
                        } else {
                            let msg = e
                                .lines()
                                .rev()
                                .find(|l| !l.trim().is_empty())
                                .unwrap_or("the attach failed")
                                .to_string();
                            d.flash(msg, true);
                        }
                    }
                    Err(e) => d.flash(format!("ssh: {e}"), true),
                    _ => {}
                }
                d.after_handoff(&h);
                continue;
            }
            Handoff::Browse { target, quick } => {
                let mut bo = crate::browse::BrowseOpts::from_config(&d.cfg, target);
                bo.sessions = d.fleet.host(&target.ssh).map(|h| h.sessions.clone());
                let col = bo.color;
                match crate::browse::run(&mut tui, target, bo) {
                    // from Quick start: back there with the folder chosen
                    Ok(Some(crate::browse::Picked::New(dir))) if quick.is_some() => {
                        if let Some(q) = quick {
                            let mut q = (**q).clone();
                            q.choose_folder(&crate::path::show(&dir));
                            d.mode = Mode::Quick(q);
                        }
                    }
                    Ok(Some(crate::browse::Picked::New(dir))) => {
                        let mut o = match crate::cli::parse_new(&[], &d.cfg) {
                            Ok(x) => x.opts,
                            Err(e) => {
                                d.flash(e, true);
                                continue;
                            }
                        };
                        o.dir = Some(dir);
                        o.live = d.fleet.live_names();
                        crate::cli::fill_names(&mut o);
                        let (t2, r) =
                            crate::launch::run(tui, target, crate::launch::Job::New(o), wait, col);
                        tui = t2;
                        d.landed(target, r);
                    }
                    Ok(Some(crate::browse::Picked::Attach(s))) => {
                        let (t2, r) = crate::launch::run(
                            tui,
                            target,
                            crate::launch::Job::Attach {
                                pane: s.pane_id.clone(),
                                label: s.display().to_string(),
                            },
                            wait,
                            col,
                        );
                        tui = t2;
                        d.landed(target, r);
                    }
                    Ok(None) => {}
                    Err(e) => d.flash(format!("folder browser: {e}"), true),
                }
                d.after_handoff(&h);
                continue;
            }
            Handoff::Hub => {
                let _ = tui.suspend_keep_screen();
                let r = crate::proc::interactive(&herdr::bin(), &[] as &[&str]);
                let _ = tui.resume();
                if let Err(e) = r {
                    d.flash(format!("couldn't run herdr: {e}"), true);
                }
                d.after_handoff(&h);
                continue;
            }
            Handoff::EditConfig => {
                // the editor draws its own screen
                let _ = tui.suspend_keep_screen();
                match crate::configcmd::edit_checked(&d.cfg) {
                    Ok(m) => d.flash(m, false),
                    Err(e) => d.flash(e, true),
                }
            }
            Handoff::Setup(step) => {
                if let Err(e) = onboard::run_step_in(&mut tui, Config::load(), *step) {
                    d.flash(e, true);
                }
            }
            Handoff::MachineAdd(a) => {
                let _ = tui.suspend();
                let mut cfg = Config::load();
                let m = herdr::machine_for(a);
                crate::onboard::run(&mut cfg, a, &m);
                pause();
            }
            Handoff::HostSetup(a) => {
                let _ = tui.suspend();
                let mut cfg = Config::load();
                crate::hostsetup::run(&mut cfg, a, &crate::hostsetup::Opts::default());
                pause();
            }
            Handoff::KeySetup(a) => {
                let _ = tui.suspend();
                if let Err(e) = crate::sshcfg::setup_key(a) {
                    err(&e);
                }
                pause();
            }
            Handoff::Fix(f) => {
                let _ = tui.suspend();
                crate::out::head(&f.label());
                if let Err(e) = crate::doctor::run_remote(f) {
                    err(&e);
                }
                pause();
            }
            Handoff::Install { agent, host, .. } => {
                // setup's install screen, right here: the agent, then its herdr
                // integration (a fresh agent's missing folder is made first)
                use onboard::{Action, Step};
                let mut want = vec![(
                    Step::Agents,
                    Action::AgentInstall {
                        host: host.clone(),
                        agent: agent.clone(),
                    },
                )];
                if let Some(name) = crate::agents::find(agent).and_then(|a| a.integration) {
                    want.push((
                        Step::Agents,
                        Action::Integration {
                            host: host.clone(),
                            name: name.to_string(),
                        },
                    ));
                }
                if let Err(e) = onboard::run_actions_in(&mut tui, Config::load(), want) {
                    d.flash(e, true);
                }
            }
        }
        if let Err(e) = tui.resume() {
            return fail(&format!("terminal: {e}"));
        }
        d.cfg = Config::load();
        d.after_handoff(&h);
    }
}

#[cfg(test)]
mod tests;
