//! Quick start (`n` on a machine): where — one of its herdr workspaces, a
//! recent folder, a worktree, or the folder browser — and which agent, or a
//! plain terminal (`/` searches them, the ones the machine has first). enter
//! on a place picks it (✓) and moves to the agent; enter on the agent starts
//! it (in the background, then selected; a terminal opens to type into). `o`
//! starts it in Focus, `i` starts it from either column. The
//! close checklist (`x` on a machine): its sessions, ticked, one confirm.
//!
//! Every start from the dashboard is a `Starting`: its row is in Fleet at once
//! with a spinner, becomes the session when it's up, and says why when it
//! isn't (`r` tries again, `x` drops it).

use super::fleet::RowId;
use super::{Bg, Dash, Handoff, Level, Mode, Pending, View};
use crate::herdr::{Herdr, NewOpts, PanesOut, Session, Workspace, Worktree};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

static GEN: AtomicU64 = AtomicU64::new(0);
static START_ID: AtomicU64 = AtomicU64::new(0);

/// How long herdr's "started" may stay unseen by a poll before it's a failure.
const LISTING_WAIT: Duration = Duration::from_secs(20);

/// A session on its way: a row in Fleet before anything is known about it.
pub(crate) struct Starting {
    pub id: u64,
    pub host: String,
    /// what it was asked to be, for another try
    pub opts: NewOpts,
    /// open it in Focus once it's up
    pub focus: bool,
    pub t0: Instant,
    /// what it's doing now, in herdr's steps
    pub step: String,
    /// its panes once they exist: the row stands in for them until it's up
    pub panes: PanesOut,
    pub state: StartState,
}

pub(crate) enum StartState {
    Running,
    /// herdr says it started: until a poll lists it (selector, since)
    Listing(String, Instant),
    Failed(String),
}

impl Starting {
    pub fn what(&self) -> String {
        name_of(&self.opts.agent_id())
    }

    /// Where it's going, as the row says it (empty: where herdr opens one).
    pub fn folder(&self, d: &Dash) -> String {
        self.opts
            .dir
            .as_deref()
            .map(|p| short(d, &self.host, &crate::path::show(p)))
            .unwrap_or_default()
    }
}

/// Where a new session goes.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Place {
    /// a new tab in this herdr workspace
    Workspace(Workspace),
    /// a folder: a new tab in the active workspace
    Folder(String),
    /// an existing worktree checkout
    Worktree(Worktree),
    Browse,
    /// the full form: name, prompt, arguments, tiles
    More,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pick {
    pub id: String,
    pub version: String,
    pub installed: bool,
    /// herdr's integration for it there: Some(false) = missing or outdated
    pub integrated: Option<bool>,
}

#[derive(Clone)]
pub(crate) struct QuickStart {
    pub host: String,
    pub gen: u64,
    /// None while they load
    workspaces: Option<Result<Vec<Workspace>, String>>,
    worktrees: Vec<Worktree>,
    folders: Vec<String>,
    agents: Vec<Pick>,
    /// 0 = where, 1 = agent
    col: usize,
    at: [usize; 2],
    /// a folder: start it in a new git worktree
    worktree: bool,
    /// the where cursor was moved (else it follows the workspaces in)
    moved: bool,
    /// the place enter picked (✓). More options keeps its folder
    picked: Option<Place>,
    /// the browser was asked for with the agent settled: start once the
    /// folder is back (Some(true): in Focus)
    after_browse: Option<bool>,
    /// `/`: what's typed to find an agent (the agent column shows matches)
    search: Option<super::forms::Field>,
}

impl QuickStart {
    pub fn new(d: &Dash, host: &str) -> QuickStart {
        let mut folders: Vec<String> = crate::history::recent_dirs(Some(host))
            .into_iter()
            .map(|r| r.dir)
            .take(6)
            .collect();
        if crate::sshcfg::is_local(host) {
            for f in crate::moshi::recent_folders() {
                if folders.len() < 8 && !folders.contains(&f) {
                    folders.push(f);
                }
            }
        }
        QuickStart {
            host: host.to_string(),
            gen: GEN.fetch_add(1, Ordering::Relaxed) + 1,
            workspaces: None,
            worktrees: vec![],
            folders,
            agents: picks(d, host),
            col: 0,
            at: [0, 0],
            worktree: false,
            moved: false,
            picked: None,
            after_browse: None,
            search: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_folders(mut self, folders: &[&str]) -> QuickStart {
        self.folders = folders.iter().map(|f| f.to_string()).collect();
        self
    }

    /// The agent column as shown: every agent (installed first, the
    /// machine's default at the top, a plain shell, then the rest), or
    /// while searching the matches — the ones the machine has first, the
    /// best match first within each. Indexes into `agents`.
    fn shown(&self) -> Vec<usize> {
        let Some(q) = self.search.as_ref().map(|f| f.text.trim().to_string()) else {
            return (0..self.agents.len()).collect();
        };
        let mut v: Vec<(u8, i32, usize)> = self
            .agents
            .iter()
            .enumerate()
            .filter_map(|(i, a)| {
                let label = name_of(&a.id);
                let score = [label.as_str(), a.id.as_str()]
                    .iter()
                    .filter_map(|c| crate::fuzzy::score(&q, c).map(|x| x.0))
                    .max()?;
                let rank = if a.installed {
                    0
                } else if a.id == crate::agents::SHELL {
                    1
                } else {
                    2
                };
                Some((rank, -score, i))
            })
            .collect();
        v.sort();
        v.into_iter().map(|x| x.2).collect()
    }

    /// Keys while searching the agents.
    fn search_key(&mut self, k: KeyEvent) -> QuickRes {
        let shown = self.shown();
        let cur = shown.get(self.at[1]).copied();
        // leaving the search: the column whole again, on the same agent
        let close = |q: &mut QuickStart, keep: Option<usize>| {
            q.search = None;
            if let Some(i) = keep {
                q.at[1] = i;
            }
        };
        match k.code {
            KeyCode::Esc => close(self, cur),
            KeyCode::Enter => {
                if cur.is_some() {
                    close(self, cur);
                    return self.go(self.opens_on_enter());
                }
            }
            KeyCode::Up => self.at[1] = self.at[1].saturating_sub(1),
            KeyCode::Down => self.at[1] = (self.at[1] + 1).min(shown.len().saturating_sub(1)),
            KeyCode::Backspace if self.search.as_ref().is_some_and(|f| f.text.is_empty()) => {
                close(self, cur)
            }
            _ => {
                if let Some(f) = self.search.as_mut() {
                    if f.key(&k) {
                        // the best match, as it's typed
                        self.at[1] = 0;
                    }
                }
            }
        }
        QuickRes::Keep
    }

    /// Start what's under the cursors. Browse first asks for the folder.
    fn go(&mut self, focus: bool) -> QuickRes {
        if self.place() == Some(Place::Browse) {
            self.after_browse = Some(focus);
            return QuickRes::Browse;
        }
        QuickRes::Start { focus }
    }

    /// A terminal is for typing into: enter opens it. An agent starts in the
    /// background, and its row is selected.
    fn opens_on_enter(&self) -> bool {
        self.agent().is_some_and(|a| a.id == crate::agents::SHELL)
    }

    /// The browser's answer can start straight away: the agent is settled and
    /// there's nothing to install first.
    pub fn starts_after_browse(&self) -> Option<bool> {
        self.after_browse.filter(|_| {
            self.agent()
                .is_some_and(|a| a.installed || a.id == crate::agents::SHELL)
        })
    }

    /// What the browser's labels call the agent.
    pub fn agent_name(&self) -> Option<String> {
        self.agent().map(|a| name_of(&a.id))
    }

    /// The folder the browser came back with: chosen, the agent next.
    pub fn choose_folder(&mut self, dir: &str) {
        self.folders.retain(|f| f != dir);
        self.folders.insert(0, dir.to_string());
        if let Some(i) = self
            .places()
            .iter()
            .position(|p| matches!(p, Place::Folder(f) if f == dir))
        {
            self.at[0] = i;
        }
        self.moved = true;
        self.picked = self.place();
        self.col = 1;
    }

    /// Start from this folder, with another agent than `avoid` (a session
    /// stopped at its limit: carry on with a different one).
    pub fn prefer(&mut self, dir: &str, avoid: &str) {
        if dir.starts_with('/') {
            self.folders.retain(|f| f != dir);
            self.folders.insert(0, dir.to_string());
            let ws = self
                .places()
                .iter()
                .take_while(|p| matches!(p, Place::Workspace(_)))
                .count();
            self.at[0] = ws;
            self.moved = true;
        }
        if let Some(i) = self
            .agents
            .iter()
            .position(|a| a.installed && a.id != avoid && a.id != crate::agents::SHELL)
        {
            self.at[1] = i;
        }
    }

    /// The where column, in order.
    pub fn places(&self) -> Vec<Place> {
        let mut v: Vec<Place> = Vec::new();
        if let Some(Ok(ws)) = &self.workspaces {
            v.extend(ws.iter().cloned().map(Place::Workspace));
        }
        v.extend(self.folders.iter().cloned().map(Place::Folder));
        v.extend(
            self.worktrees
                .iter()
                .filter(|w| w.linked && !self.folders.contains(&w.path))
                .cloned()
                .map(Place::Worktree),
        );
        v.push(Place::Browse);
        v.push(Place::More);
        v
    }

    pub fn loaded(&mut self, ws: Result<Vec<Workspace>, String>, wt: Vec<Worktree>) {
        let was = self.places().get(self.at[0]).cloned();
        self.workspaces = Some(ws);
        self.worktrees = wt;
        let places = self.places();
        if !self.moved {
            // untouched: start on the workspace that's open there (else the first)
            self.at[0] = places
                .iter()
                .position(|p| matches!(p, Place::Workspace(w) if w.focused))
                .unwrap_or(0);
        } else if let Some(i) = was.and_then(|p| places.iter().position(|x| *x == p)) {
            // keep the cursor on the same place
            self.at[0] = i;
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> QuickRes {
        if self.search.is_some() {
            return self.search_key(k);
        }
        let n = [self.places().len(), self.agents.len()];
        match k.code {
            KeyCode::Esc => return QuickRes::Cancel,
            // search the agents (from either column)
            KeyCode::Char('/') => {
                self.col = 1;
                self.search = Some(super::forms::Field::default());
                self.at[1] = 0;
            }
            // enter on a place picks it and moves on to the agent
            KeyCode::Enter if self.col == 0 => match self.place() {
                Some(Place::Browse) => {
                    self.after_browse = None;
                    return QuickRes::Browse;
                }
                Some(Place::More) => return QuickRes::Start { focus: false },
                Some(p) => {
                    self.picked = Some(p);
                    // picked: the machine's answer arriving late can't move it
                    self.moved = true;
                    self.col = 1;
                }
                None => {}
            },
            // enter on the agent starts it
            KeyCode::Enter => return self.go(self.opens_on_enter()),
            // i starts from either column (o: and opens it in Focus)
            KeyCode::Char('i') => return self.go(false),
            KeyCode::Char('o') => return self.go(true),
            KeyCode::Up | KeyCode::Char('k') => {
                self.at[self.col] = self.at[self.col].saturating_sub(1);
                self.moved_cursor();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.at[self.col] = (self.at[self.col] + 1).min(n[self.col].saturating_sub(1));
                self.moved_cursor();
            }
            KeyCode::Left | KeyCode::Char('h') => self.col = 0,
            KeyCode::Right | KeyCode::Char('l') => self.col = 1,
            KeyCode::Tab | KeyCode::BackTab => self.col = 1 - self.col,
            KeyCode::Char('w') => self.worktree = !self.worktree,
            _ => {}
        }
        QuickRes::Keep
    }

    /// The where cursor moved: another real place takes the pick's place.
    /// (Passing over Browse or More keeps it: More options carries its folder.)
    fn moved_cursor(&mut self) {
        if self.col != 0 {
            return;
        }
        self.moved = true;
        if !matches!(self.place(), Some(Place::Browse | Place::More)) {
            self.picked = None;
        }
    }

    pub(crate) fn place(&self) -> Option<Place> {
        self.places().get(self.at[0]).cloned()
    }

    /// The agent the cursor is on (tests).
    #[cfg(test)]
    pub(crate) fn agent_id(&self) -> Option<&str> {
        self.agent().map(|a| a.id.as_str())
    }

    fn agent(&self) -> Option<&Pick> {
        self.shown()
            .get(self.at[1])
            .and_then(|i| self.agents.get(*i))
    }

    pub fn draw(&self, d: &Dash, f: &mut Frame, area: Rect) {
        let w = 96u16.min(area.width.saturating_sub(2));
        let places = self.places();
        let rows = places.len().max(self.agents.len()).min(16) as u16;
        let r = ui::centered(area, w, rows + 6);
        crate::ui::clear(f, r);
        let col = crate::theme::machine(&d.cfg, &self.host);
        let block = ui::panel(
            Line::from(vec![
                Span::raw("New session on "),
                Span::styled(self.host.clone(), ui::fg(col).add_modifier(Modifier::BOLD)),
            ]),
            true,
        );
        let inner = block.inner(r);
        f.render_widget(block, r);
        let [cols, _, foot] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        let [a, b] = Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
            .spacing(2)
            .areas(cols);
        let cw = a.width as usize;
        let mut left = vec![head("Where", self.col == 0)];
        match &self.workspaces {
            None => left.push(Line::from(Span::styled(
                format!("  {} its herdr workspaces…", ui::spinner(d.t0)),
                ui::muted(),
            ))),
            Some(Err(e)) => left.push(Line::from(Span::styled(
                format!("  workspaces: {}", ui::trunc(e, cw.saturating_sub(16))),
                ui::fg(c::YELLOW()),
            ))),
            Some(Ok(_)) => {}
        }
        let skip = self.at[0].saturating_sub(left_room(a.height, &self.workspaces));
        for (i, p) in places.iter().enumerate().skip(skip) {
            let on = i == self.at[0];
            let tick = self.picked.as_ref() == Some(p);
            let (glyph, text, note) = match p {
                Place::Workspace(ws) => (
                    "▣",
                    if ws.label.is_empty() {
                        format!("workspace {}", ws.number)
                    } else {
                        ws.label.clone()
                    },
                    format!(
                        "{} tab{}{}",
                        ws.tabs,
                        if ws.tabs == 1 { "" } else { "s" },
                        if ws.focused { " · open" } else { "" }
                    ),
                ),
                Place::Folder(dir) => ("▸", short(d, &self.host, dir), "recent folder".into()),
                Place::Worktree(wt) => (
                    "⎇",
                    if wt.branch.is_empty() {
                        short(d, &self.host, &wt.path)
                    } else {
                        wt.branch.clone()
                    },
                    short(d, &self.host, &wt.path),
                ),
                Place::Browse => ("…", "Browse folders".into(), "fuzzy search".into()),
                Place::More => ("+", "More options".into(), "name, prompt, tiles".into()),
            };
            let mut l = row(
                on,
                self.col == 0,
                glyph,
                &text,
                &note,
                cw,
                matches!(p, Place::Browse | Place::More),
            );
            if tick {
                l.spans.push(Span::styled(
                    "  ✓",
                    ui::fg(c::GREEN()).add_modifier(Modifier::BOLD),
                ));
            }
            left.push(l);
        }
        let mut right = vec![match &self.search {
            // the search, in the column's heading
            Some(fl) => {
                let mut h = head("Agent", true);
                h.spans.push(Span::styled("  ⌕ ", ui::fg(c::ACCENT())));
                h.spans.extend(fl.spans(true, "type to search"));
                h
            }
            None => {
                let mut h = head("Agent", self.col == 1);
                h.spans.push(Span::styled("   / search", ui::faint()));
                h
            }
        }];
        let bw = b.width as usize;
        let shown = self.shown();
        if shown.is_empty() {
            right.push(Line::from(Span::styled(
                "  no agent matches — backspace widens it",
                ui::faint(),
            )));
        }
        let skip = self.at[1].saturating_sub((b.height as usize).saturating_sub(2));
        for (i, a_) in shown
            .iter()
            .filter_map(|j| self.agents.get(*j))
            .enumerate()
            .skip(skip)
        {
            let on = i == self.at[1];
            let label = name_of(&a_.id);
            let note = match (a_.installed, a_.integrated) {
                (false, _) if a_.id == crate::agents::SHELL => {
                    "no agent, just your shell".to_string()
                }
                (false, _) => "not installed".into(),
                (true, Some(false)) => format!("{} · no herdr integration", a_.version),
                (true, _) => a_.version.clone(),
            };
            let mut l = row(on, self.col == 1, "", &label, &note, bw, !a_.installed);
            // the agent's own mark in its colour, ahead of the name
            l.spans.insert(1, crate::icons::agent(&a_.id));
            l.spans.insert(2, Span::raw(" "));
            right.push(l);
        }
        match d.agents.status(&self.host) {
            crate::agentwatch::Status::Checking => right.push(Line::from(Span::styled(
                format!("  {} checking what's installed…", ui::spinner(d.t0)),
                ui::muted(),
            ))),
            crate::agentwatch::Status::Ready {
                refreshing: true, ..
            } => right.push(Line::from(Span::styled(
                format!("  {} looking again…", ui::spinner(d.t0)),
                ui::faint(),
            ))),
            _ => {}
        }
        f.render_widget(Paragraph::new(left), a);
        f.render_widget(Paragraph::new(right), b);
        let wt = matches!(self.place(), Some(Place::Folder(_)));
        let install = self
            .agent()
            .is_some_and(|a| !a.installed && a.id != crate::agents::SHELL);
        // what enter does on the agent column, from where the cursors are
        let start = match self.place() {
            Some(Place::Browse) => "pick a folder, then start",
            Some(Place::More) => "more options",
            _ if install => "install it there first",
            _ if self.opens_on_enter() => "open it",
            _ => "start",
        };
        let mut hints = if self.search.is_some() {
            vec![
                ("type", "search"),
                ("↑↓", "pick"),
                ("enter", start),
                ("esc", "all agents"),
            ]
        } else if let (0, Some(Place::Browse)) = (self.col, self.place()) {
            vec![("enter", "browse"), ("/", "search"), ("←→", "column")]
        } else if let (0, Some(Place::More)) = (self.col, self.place()) {
            vec![("enter", "more options"), ("/", "search"), ("←→", "column")]
        } else if self.col == 0 {
            vec![
                ("enter", "next: the agent"),
                ("i", "start"),
                ("o", "start + Focus"),
                ("/", "search"),
                ("←→", "column"),
            ]
        } else if install {
            vec![("enter", start), ("/", "search"), ("←→", "column")]
        } else {
            vec![
                ("enter", start),
                ("o", "start + Focus"),
                ("/", "search"),
                ("←→", "column"),
            ]
        };
        if wt && self.search.is_none() {
            hints.push((
                "w",
                if self.worktree {
                    "worktree: on"
                } else {
                    "worktree: off"
                },
            ));
        }
        if self.search.is_none() {
            hints.push(("esc", "cancel"));
        }
        f.render_widget(Paragraph::new(ui::hints_fit(&hints, foot.width)), foot);
    }
}

/// Rows the where column has for places (its title, plus the loading line).
fn left_room(h: u16, ws: &Option<Result<Vec<Workspace>, String>>) -> usize {
    let extra = 1 + usize::from(!matches!(ws, Some(Ok(_))));
    (h as usize).saturating_sub(extra + 1).max(1)
}

/// An entry in the agent column: a plain shell reads as a terminal.
fn name_of(id: &str) -> String {
    if id == crate::agents::SHELL {
        "Terminal".into()
    } else {
        crate::agents::label(id)
    }
}

fn short(d: &Dash, host: &str, p: &str) -> String {
    d.fleet
        .host(host)
        .map(|h| h.short_path(p))
        .unwrap_or_else(|| p.to_string())
}

fn head(t: &str, on: bool) -> Line<'static> {
    Line::from(Span::styled(
        format!(" {t}"),
        if on {
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
        } else {
            ui::muted().add_modifier(Modifier::BOLD)
        },
    ))
}

fn row(
    on: bool,
    active: bool,
    glyph: &str,
    text: &str,
    note: &str,
    w: usize,
    dim: bool,
) -> Line<'static> {
    let bar = if on && active { "┃" } else { " " };
    let base = if dim { ui::muted() } else { Style::new() };
    let st = if on && active {
        // ink of its own on the selection's background
        base.fg(if dim { c::MUTED() } else { c::TEXT() })
            .bg(c::SEL())
            .add_modifier(Modifier::BOLD)
    } else if on {
        base.add_modifier(Modifier::BOLD)
    } else {
        base
    };
    let mut spans = vec![Span::styled(bar.to_string(), ui::fg(c::ACCENT()))];
    if !glyph.is_empty() {
        spans.push(Span::styled(format!("{glyph} "), ui::faint()));
    }
    let tw = w.saturating_sub(ui::width(note) + 6).max(8);
    spans.push(Span::styled(ui::trunc(text, tw), st));
    if !note.is_empty() {
        spans.push(Span::styled(format!("  {note}"), ui::faint()));
    }
    Line::from(spans)
}

/// The agents for the agent column: installed ones (the machine's default
/// first), a plain shell, then the rest (greyed: enter installs one).
pub(crate) fn picks(d: &Dash, host: &str) -> Vec<Pick> {
    let def = crate::agents::default_agent_for(&d.cfg, host).id;
    let mut v: Vec<Pick> = Vec::new();
    if let crate::agentwatch::Status::Ready { probe, .. } = d.agents.status(host) {
        for a in &probe.agents {
            if v.iter().any(|p| p.id == a.id) {
                continue;
            }
            v.push(Pick {
                id: a.id.clone(),
                version: a.version.clone(),
                installed: true,
                integrated: a
                    .integration
                    .as_ref()
                    .map(|i| matches!(i, crate::probe::IState::Current(_))),
            });
        }
    }
    if !v.iter().any(|p| p.id == def) {
        // not probed yet (or not found): offer the default anyway
        v.insert(
            0,
            Pick {
                id: def.to_string(),
                version: String::new(),
                installed: v.is_empty(),
                integrated: None,
            },
        );
    }
    v.sort_by_key(|p| (p.id != def, !p.installed));
    v.push(Pick {
        id: crate::agents::SHELL.into(),
        version: String::new(),
        installed: false,
        integrated: None,
    });
    for id in crate::agents::IDS {
        if !v.iter().any(|p| p.id == *id) {
            v.push(Pick {
                id: id.to_string(),
                version: String::new(),
                installed: false,
                integrated: None,
            });
        }
    }
    v
}

pub(crate) enum QuickRes {
    Keep,
    Cancel,
    Start {
        focus: bool,
    },
    /// open the folder browser; the folder comes back here, chosen
    Browse,
}

/// Open Quick start for a machine; its workspaces load in the background.
pub(crate) fn open(d: &mut Dash, host: &str) {
    d.agents
        .refresh_if_older(host, std::time::Duration::from_secs(30));
    let q = QuickStart::new(d, host);
    let (gen, h) = (q.gen, host.to_string());
    let (t, tx) = (d.target_of(host), d.tx.clone());
    std::thread::spawn(move || {
        let x = Herdr::new(&t);
        let ws = x.workspaces();
        let wt = if ws.is_ok() {
            x.worktrees(None).unwrap_or_default()
        } else {
            vec![]
        };
        let _ = tx.send(Bg::Quick {
            host: h,
            gen,
            ws,
            wt,
        });
    });
    d.mode = Mode::Quick(q);
}

/// Start what Quick start has under its cursors.
pub(crate) fn start(d: &mut Dash, q: QuickStart, focus: bool) -> Option<Handoff> {
    let host = q.host.clone();
    let (Some(place), Some(agent)) = (q.place(), q.agent().cloned()) else {
        return None;
    };
    let folder = |p: &Place| match p {
        Place::Folder(p) => Some(p.clone()),
        Place::Worktree(w) => Some(w.path.clone()),
        _ => None,
    };
    // the form has its own agent list: nothing to install yet
    if place == Place::More {
        let dir = q.picked.as_ref().and_then(folder);
        let mut nf = super::forms::NewForm::new(d, &host, dir.as_deref());
        nf.set_agent(&agent.id);
        d.mode = Mode::NewForm(nf);
        return None;
    }
    let dir = folder(&place);
    if !agent.installed && agent.id != crate::agents::SHELL {
        return Some(Handoff::Install {
            agent: agent.id,
            host,
            dir,
        });
    }
    let mut o = match crate::cli::parse_new(&["--host".into(), host.clone()], &d.cfg) {
        Ok(x) => x.opts,
        Err(e) => {
            d.error(e);
            return None;
        }
    };
    o.agent = agent.id.clone();
    if let Some(p) = &dir {
        o.dir = Some(crate::path::normalize(p.as_bytes()));
    }
    if let Place::Workspace(ws) = &place {
        o.workspace = Some(ws.id.clone());
    }
    if q.worktree && matches!(place, Place::Folder(_)) {
        o.worktree = Some(String::new());
    }
    begin(d, &host, o, focus);
    None
}

/// Start `o` on `host` in the background: its row is there at once, selected.
pub(crate) fn begin(d: &mut Dash, host: &str, mut o: NewOpts, focus: bool) -> u64 {
    o.no_focus = true;
    o.live = d.fleet.live_names();
    crate::cli::fill_names(&mut o);
    let id = START_ID.fetch_add(1, Ordering::Relaxed) + 1;
    let panes = Arc::new(Mutex::new(Vec::new()));
    o.panes_out = Some(panes.clone());
    let st = Starting {
        id,
        host: host.to_string(),
        opts: o,
        focus,
        t0: Instant::now(),
        step: String::new(),
        panes,
        state: StartState::Running,
    };
    let what = st.what();
    d.starting.push(st);
    if let Some(h) = d.fleet.host_mut(host) {
        h.collapsed = false;
    }
    if d.view == View::Fleet {
        super::fleet::select(d, RowId::Starting(id));
    } else {
        d.toast(Level::Info, format!("starting {what} on {host}…"));
    }
    run_start(d, id);
    id
}

/// A launch key (`launch.<key>`): `terminal` (you pick the folder), `terminal
/// [machine:]/folder`, or a preset's name. What it starts opens in Focus.
pub(crate) fn launch_key(d: &mut Dash, what: &str) -> Option<Handoff> {
    let here = if d.view == View::Wall {
        super::wall::focused_session(d).map(|s| s.host)
    } else {
        d.fv.selected_host(d)
    }
    .or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone()));
    if what == "terminal" {
        let Some(host) = here else {
            d.note("no machine to open a terminal on");
            return None;
        };
        return Some(Handoff::Browse {
            target: d.target_of(&host),
            quick: None,
            terminal: true,
        });
    }
    if let Some(place) = what.strip_prefix("terminal ") {
        let place = place.trim();
        let (host, dir) = match place.split_once(':') {
            Some((h, p)) => (Some(h.to_string()), p),
            None => (here, place),
        };
        let host = host.filter(|h| d.fleet.host(h).is_some());
        match host {
            Some(h) if dir.starts_with('/') => shell(d, &h, Some(dir)),
            Some(_) => d.error(format!("launch key: `{dir}` isn't a full path")),
            None => d.error(format!("launch key: no such machine in `{place}`")),
        }
        return None;
    }
    match crate::cli::parse_new(&[format!("@{what}")], &d.cfg) {
        Ok(a) => match a.host.or(here) {
            Some(h) => {
                begin(d, &h, a.opts, true);
            }
            None => d.note("no machine to start it on"),
        },
        Err(e) => d.error(format!("launch key: {e}")),
    }
    None
}

/// The start itself, off the UI thread.
fn run_start(d: &Dash, id: u64) {
    if cfg!(test) {
        return;
    }
    let Some(st) = d.starting.iter().find(|s| s.id == id) else {
        return;
    };
    let (t, tx, o) = (d.target_of(&st.host), d.tx.clone(), st.opts.clone());
    std::thread::spawn(move || {
        let steps = tx.clone();
        let r = Herdr::new(&t)
            .new_session(&o, &mut |_, label| {
                let _ = steps.send(Bg::StartStep { id, label });
            })
            .and_then(|v| {
                crate::launch::after_start(&t, &o, &v);
                v.first()
                    .map(|s| (format!("{}:{}", t.ssh, s.pane), s.name.clone()))
                    .ok_or_else(|| "nothing started".to_string())
            });
        let _ = tx.send(Bg::StartDone { id, result: r });
    });
}

pub(crate) fn on_step(d: &mut Dash, id: u64, label: String) {
    if let Some(st) = d.starting.iter_mut().find(|s| s.id == id) {
        st.step = label;
    }
}

/// `new_session` answered.
pub(crate) fn on_done(d: &mut Dash, id: u64, result: Result<(String, String), String>) {
    // dropped while it ran: what it made goes too
    if let Some(i) = d.start_gone.iter().position(|g| g.0 == id) {
        let (_, host, panes) = d.start_gone.remove(i);
        if result.is_ok() {
            let panes = panes.lock().unwrap_or_else(|e| e.into_inner()).clone();
            close_panes(d, &host, panes);
        }
        return;
    }
    let in_fleet = d.view == View::Fleet;
    let Some(st) = d.starting.iter_mut().find(|s| s.id == id) else {
        return;
    };
    let host = st.host.clone();
    match result {
        Ok((sel, name)) => {
            st.state = StartState::Listing(sel, Instant::now());
            if !in_fleet {
                d.toast(Level::Ok, format!("started {name} on {host}"));
            }
            d.fleet.refresh(Some(&host));
            settle(d);
        }
        Err(e) => {
            // herdr's side closed what it made
            st.panes.lock().unwrap_or_else(|e| e.into_inner()).clear();
            st.state = StartState::Failed(e.clone());
            d.toast(Level::Err, format!("{host}: {e}"));
        }
    }
}

/// Rows whose session a poll has now seen hand over to it (the selection too,
/// if it's still on them); ones nobody can see after a while are failures.
pub(crate) fn settle(d: &mut Dash) {
    if d.starting.is_empty() {
        return;
    }
    let all: Vec<Session> = d.fleet.sessions().into_iter().cloned().collect();
    let mut up: Vec<(u64, String, bool)> = Vec::new();
    for st in &mut d.starting {
        let StartState::Listing(sel, since) = &st.state else {
            continue;
        };
        let late = since.elapsed() > LISTING_WAIT;
        match crate::herdr::pick(&all, sel) {
            Some(s) if !s.launch_pending || late => {
                up.push((st.id, s.key(), s.agent == crate::agents::SHELL))
            }
            None if late => {
                st.state =
                    StartState::Failed("herdr said it started, but doesn't list it".to_string())
            }
            _ => {}
        }
    }
    for (id, key, shell) in up {
        let focus = d.starting.iter().any(|s| s.id == id && s.focus);
        d.starting.retain(|s| s.id != id);
        if d.fv.sel == Some(RowId::Starting(id)) {
            super::fleet::select(d, RowId::Sess(key.clone()));
        }
        if focus {
            super::wall::focus_on(d, &key, View::Fleet);
            // a plain terminal is for typing into
            if shell {
                d.wall.typing = true;
            }
        }
    }
}

/// The panes the machine's starting rows stand in for.
pub(crate) fn hidden(d: &Dash, host: &str) -> Vec<String> {
    d.starting
        .iter()
        .filter(|s| s.host == host && !matches!(s.state, StartState::Failed(_)))
        .flat_map(|s| s.panes.lock().unwrap_or_else(|e| e.into_inner()).clone())
        .collect()
}

/// `r` on a row that failed.
pub(crate) fn retry(d: &mut Dash, id: u64) {
    let live = d.fleet.live_names();
    let Some(st) = d.starting.iter_mut().find(|s| s.id == id) else {
        return;
    };
    if !matches!(st.state, StartState::Failed(_)) {
        return;
    }
    st.state = StartState::Running;
    st.t0 = Instant::now();
    st.step.clear();
    st.opts.live = live;
    st.panes.lock().unwrap_or_else(|e| e.into_inner()).clear();
    run_start(d, id);
}

/// `x` on a starting row: gone, and so is anything it made (or goes on to make).
pub(crate) fn dismiss(d: &mut Dash, id: u64) {
    let Some(i) = d.starting.iter().position(|s| s.id == id) else {
        return;
    };
    let st = d.starting.remove(i);
    let panes = st.panes.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if matches!(st.state, StartState::Running) {
        d.start_gone.push((id, st.host.clone(), st.panes.clone()));
    }
    close_panes(d, &st.host, panes);
}

fn close_panes(d: &Dash, host: &str, panes: Vec<String>) {
    if cfg!(test) || panes.is_empty() {
        return;
    }
    let t = d.target_of(host);
    d.spawn(Some(host.to_string()), move || {
        let h = Herdr::new(&t);
        for p in &panes {
            let _ = h.close_pane(p);
        }
        Ok(String::new())
    });
}

/// `!`: a plain terminal (no agent) on `host`, in `dir` (else where herdr
/// opens one), started in the background, then opened in Focus to type into.
pub(crate) fn shell(d: &mut Dash, host: &str, dir: Option<&str>) {
    // one that's ready: the machine's standing terminal, or one already in that
    // folder. Straight there, nothing new made
    let ready = d.fleet.host(host).and_then(|h| {
        h.sessions
            .iter()
            .filter(|s| s.agent == crate::agents::SHELL)
            .find(|s| match dir {
                Some(p) => s.cwd == p,
                None => s.name == crate::agents::READY_SHELL,
            })
            .map(Session::key)
    });
    if let Some(key) = ready {
        let from = d.view;
        super::wall::focus_on(d, &key, from);
        return;
    }
    let mut o = match crate::cli::parse_new(&["--host".into(), host.to_string()], &d.cfg) {
        Ok(x) => x.opts,
        Err(e) => {
            d.error(e);
            return;
        }
    };
    o.agent = crate::agents::SHELL.into();
    match dir {
        Some(p) => o.dir = Some(crate::path::normalize(p.as_bytes())),
        None => o.name = Some(crate::agents::READY_SHELL.into()),
    }
    begin(d, host, o, true);
}

/// `D`: the same agent again, in the same folder on the same machine, with
/// the arguments it was started with (when the history knows them); a new
/// story name; started in the background, then selected.
pub(crate) fn duplicate(d: &mut Dash, s: &Session) {
    let host = s.host.clone();
    let mut o = match crate::cli::parse_new(&["--host".into(), host.clone()], &d.cfg) {
        Ok(x) => x.opts,
        Err(e) => {
            d.error(e);
            return;
        }
    };
    let hist = crate::history::load(500);
    let h = hist
        .iter()
        .rev()
        .find(|e| e.host == s.host && e.name == s.name);
    o.agent = s.agent.clone();
    let dir = h
        .map(|e| e.dir.clone())
        .filter(|x| !x.is_empty())
        .unwrap_or_else(|| s.cwd.clone());
    if dir.starts_with('/') {
        o.dir = Some(crate::path::normalize(dir.as_bytes()));
    }
    if let Some(e) = h {
        o.args.extend(e.args.iter().cloned());
    }
    begin(d, &host, o, false);
}

// ------------------------------------------------------------------ close

/// A machine's sessions with checkboxes (`x` on a machine).
pub(crate) struct CloseList {
    pub host: String,
    pub items: Vec<(Session, bool)>,
    at: usize,
}

impl CloseList {
    pub fn new(host: &str, sessions: Vec<Session>) -> CloseList {
        CloseList {
            host: host.to_string(),
            items: sessions.into_iter().map(|s| (s, false)).collect(),
            at: 0,
        }
    }

    /// enter: the ticked ones (or the one under the cursor when none are).
    pub fn chosen(&self) -> Vec<Session> {
        let v: Vec<Session> = self
            .items
            .iter()
            .filter(|(_, on)| *on)
            .map(|(s, _)| s.clone())
            .collect();
        if v.is_empty() {
            self.items
                .get(self.at)
                .map(|(s, _)| vec![s.clone()])
                .unwrap_or_default()
        } else {
            v
        }
    }

    pub fn key(&mut self, k: KeyEvent) -> super::forms::Res {
        use super::forms::Res;
        let n = self.items.len();
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Res::Cancel,
            KeyCode::Enter => return Res::Submit,
            KeyCode::Up | KeyCode::Char('k') => self.at = self.at.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.at = (self.at + 1).min(n.saturating_sub(1)),
            KeyCode::Char(' ') => {
                if let Some(x) = self.items.get_mut(self.at) {
                    x.1 = !x.1;
                }
                self.at = (self.at + 1).min(n.saturating_sub(1));
            }
            KeyCode::Char('a') => {
                let all = self.items.iter().all(|x| x.1);
                for x in &mut self.items {
                    x.1 = !all;
                }
            }
            _ => {}
        }
        Res::Keep
    }

    pub fn draw(&self, d: &Dash, f: &mut Frame, area: Rect) {
        let w = 78u16.min(area.width.saturating_sub(2));
        let r = ui::centered(area, w, self.items.len().min(14) as u16 + 5);
        crate::ui::clear(f, r);
        let col = crate::theme::machine(&d.cfg, &self.host);
        let block = ui::panel(
            Line::from(vec![
                Span::raw("Close sessions on "),
                Span::styled(self.host.clone(), ui::fg(col).add_modifier(Modifier::BOLD)),
            ]),
            true,
        );
        let inner = block.inner(r);
        f.render_widget(block, r);
        let iw = inner.width as usize;
        let mut lines: Vec<Line> = Vec::new();
        let skip = self.at.saturating_sub(13);
        for (i, (s, on)) in self.items.iter().enumerate().skip(skip).take(14) {
            let sel = i == self.at;
            let (st, scol) = super::fleet::status_for(d, s);
            let mut l = vec![
                Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
                Span::styled(
                    if *on { "[✕] " } else { "[ ] " },
                    if *on {
                        ui::fg(c::RED()).add_modifier(Modifier::BOLD)
                    } else {
                        ui::faint()
                    },
                ),
                Span::styled(format!("{} ", s.glyph()), ui::fg(s.color())),
                Span::styled(
                    ui::pad(&ui::trunc(s.display(), 22), 22),
                    if sel {
                        ui::bold().fg(c::TEXT()).bg(c::SEL())
                    } else {
                        Style::new()
                    },
                ),
                Span::styled(format!("  {}  ", ui::pad(&st, 17)), ui::fg(scol)),
            ];
            let used: usize = l.iter().map(|x| ui::width(&x.content)).sum();
            l.push(Span::styled(
                ui::trunc_left(&short(d, &self.host, &s.cwd), iw.saturating_sub(used + 2)),
                ui::faint(),
            ));
            lines.push(Line::from(l));
        }
        lines.push(Line::raw(""));
        let n = self.items.iter().filter(|x| x.1).count();
        let label = if n == 0 {
            "close this one".to_string()
        } else {
            format!("close {n}")
        };
        lines.push(ui::hints(&[
            ("space", "tick"),
            ("a", "all"),
            ("enter", &label),
            ("esc", "cancel"),
        ]));
        f.render_widget(Paragraph::new(lines), inner);
    }
}

/// `x` on a machine: the checklist (nothing to close → a note).
pub(crate) fn close_list(d: &mut Dash, host: &str) {
    let v = d
        .fleet
        .host(host)
        .map(|h| h.sessions.clone())
        .unwrap_or_default();
    if v.is_empty() {
        d.note(format!("nothing running on {host}"));
        return;
    }
    d.mode = Mode::CloseList(CloseList::new(host, v));
}

/// enter in the checklist: one confirm for all of them.
pub(crate) fn confirm_close(d: &mut Dash, cl: CloseList) {
    let v = cl.chosen();
    if v.is_empty() {
        return;
    }
    let names: Vec<String> = v.iter().map(|s| s.display().to_string()).collect();
    d.mode = Mode::Confirm {
        title: if v.len() == 1 {
            "Close session?".into()
        } else {
            format!("Close {} sessions on {}?", v.len(), cl.host)
        },
        text: format!("{} — {}", names.join(", "), super::fleet::close_what(d)),
        on_yes: Pending::Close(v),
    };
}
