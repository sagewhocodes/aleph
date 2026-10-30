//! Command palette (ctrl+k / p): sessions on every machine, machines, views,
//! actions, presets (`@name`) and recent folders, fuzzy-ranked.

use super::fleet::RowId;
use super::forms::{Field, Res};
use super::{Dash, Handoff, Mode, View};
use crate::ui::{self, c};
use crate::{fuzzy, history};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::time::Instant;

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
}

pub(crate) struct Palette {
    q: Field,
    items: Vec<(Item, String, String, Color)>,
    sel: usize,
    /// its title and what the empty search line says
    words: (&'static str, &'static str),
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
        }
    }

    /// Only presets (Fleet's `P`).
    pub fn presets(d: &Dash) -> Palette {
        let mut p = Palette::new(d);
        p.items
            .retain(|(it, _, _, _)| matches!(it, Item::Preset(_)));
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
        // or something new in it, on any machine
        if d.view == View::Wall {
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
        for h in &d.fleet.hosts {
            for s in &h.sessions {
                let (st, col) = super::fleet::status_for(d, s);
                items.push((
                    Item::Session(s.key()),
                    format!("{} {}", s.glyph(), s.display()),
                    format!("{} · {st} · {}", h.alias, s.title),
                    col,
                ));
            }
        }
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
        Palette {
            q: Field::default(),
            words: (
                "Go to anything",
                "sessions, machines, views, @presets, folders…",
            ),
            items,
            sel: 0,
        }
    }

    fn ranked(&self) -> Vec<(usize, Vec<usize>)> {
        let q = self.q.text.trim();
        let mut v: Vec<(i32, usize, Vec<usize>)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let hay = format!("{} {}", it.1, it.2);
                fuzzy::score(q, &hay).map(|(s, pos)| (s, i, pos))
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
        let w = 80u16.min(area.width.saturating_sub(2));
        let h = 18u16.min(area.height.saturating_sub(2));
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
        let room = inner.height.saturating_sub(2) as usize;
        let ranked = self.ranked();
        let start = self.sel.saturating_sub(room.saturating_sub(1));
        for (n, (i, pos)) in ranked.iter().enumerate().skip(start).take(room) {
            let (_, title, sub, col) = &self.items[*i];
            let on = n == self.sel;
            let mut l = vec![Span::styled(
                if on { "┃ " } else { "  " },
                ui::fg(c::ACCENT()),
            )];
            // highlight matched letters in the title
            let tl = title.chars().count();
            for (ci, ch) in title.chars().enumerate() {
                let hit = pos.contains(&ci);
                let mut st = if ci == 0 { ui::fg(*col) } else { Style::new() };
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
                l.push(Span::styled(
                    format!("   {}", ui::trunc(sub, rest)),
                    ui::faint(),
                ));
            }
            let mut line = Line::from(l);
            if on {
                line = line.style(ui::selected());
            }
            lines.push(line);
        }
        if ranked.is_empty() {
            lines.push(Line::from(Span::styled("  nothing matches", ui::muted())));
        }
        f.render_widget(Paragraph::new(lines), inner);
    }
}

pub(crate) fn run(d: &mut Dash, p: Palette) -> Option<Handoff> {
    match p.chosen()? {
        Item::Session(k) if d.view == View::Wall => super::wall::add(d, k),
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
