//! Fleet view: one grouped list of every machine and its sessions, the live
//! machine panel under it, and the selected row's details on the right.

use super::{forms, Act, Bg, Dash, Handoff, Level, Mode, Pending, View};
use crate::fleet::{Conn, Host};
use crate::herdr::{Herdr, Session};
use crate::ui::{self, c};
use crate::{ansi, fuzzy, icons, telemetry};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::thread;
use std::time::{Duration, Instant};

const PREVIEW_REFRESH: Duration = Duration::from_secs(1);
const PREVIEW_DELAY: Duration = Duration::from_millis(180);
const PREVIEW_LINES: usize = 400;

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub enum RowId {
    Host(String),
    Sess(String),
    /// "Not in fleet": the header of machines you could add
    Others,
    /// a machine in ~/.ssh/config that isn't in the fleet
    Avail(String),
    /// an agent running outside herdr on a machine (`host|pid`)
    Outside(String),
    /// a session on its way (`quick::Starting`)
    Starting(u64),
}

pub(crate) struct Preview {
    pub lines: Result<Vec<Line<'static>>, String>,
    pub text: String,
    pub detect: Option<String>,
    pub at: Instant,
}

/// How long a first `Z` waits for the second.
const PAUSE_CONFIRM: Duration = Duration::from_secs(4);

pub struct State {
    pub sel: Option<RowId>,
    idx: usize,
    scroll: usize,
    pub marked: HashSet<String>,
    pub filter: String,
    pub filtering: bool,
    pub(crate) previews: HashMap<String, Preview>,
    want: Option<(String, Instant)>,
    busy: bool,
    select_first: Option<String>,
    /// the machine panel's mode (v cycles): resources · GPU · processes
    pub stats_mode: u8,
    /// list screen lines → row index (None: a spacer)
    pub(crate) linemap: Vec<Option<usize>>,
    /// the "Not in fleet" group is open
    pub others_open: bool,
    /// the right panel shows the session's timeline, not its recent output (`G`)
    pub timeline_panel: bool,
    /// where the last draw left room for it
    pub(crate) timeline_area: Option<Rect>,
    /// `Z` once on a machine: it pauses on a second `Z` (a slip of shift on
    /// `z` mustn't stop watching it)
    pub(crate) pause_armed: Option<(String, Instant)>,
}

impl State {
    pub fn new(select: Option<String>) -> State {
        State {
            sel: None,
            idx: 0,
            scroll: 0,
            marked: HashSet::new(),
            filter: String::new(),
            filtering: false,
            previews: HashMap::new(),
            want: None,
            busy: false,
            select_first: select,
            timeline_panel: false,
            timeline_area: None,
            stats_mode: 0,
            linemap: Vec::new(),
            others_open: false,
            pause_armed: None,
        }
    }

    pub(crate) fn on_preview(
        &mut self,
        key: String,
        recent: Result<String, String>,
        detect: Option<String>,
    ) {
        self.busy = false;
        let text = recent.as_deref().map(strip_ansi).unwrap_or_default();
        self.previews.insert(
            key,
            Preview {
                lines: recent.map(|t| ansi::lines(&t)),
                text,
                detect,
                at: Instant::now(),
            },
        );
    }

    pub(crate) fn invalidate_preview(&mut self) {
        for p in self.previews.values_mut() {
            p.at = Instant::now() - PREVIEW_REFRESH * 2;
        }
    }

    /// Host of the selected row.
    pub fn selected_host(&self, d: &Dash) -> Option<String> {
        match selected(d)? {
            RowId::Host(h) | RowId::Avail(h) => Some(h),
            RowId::Sess(k) | RowId::Outside(k) => k.split_once('|').map(|x| x.0.to_string()),
            RowId::Starting(id) => d
                .starting
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.host.clone()),
            RowId::Others => None,
        }
    }
}

fn strip_ansi(s: &str) -> String {
    ansi::lines(s)
        .iter()
        .map(ansi::plain)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Does `s` match the filter (fuzzy on name, agent, folder, title, machine)?
pub(crate) fn matches(q: &str, s: &Session) -> bool {
    if q.trim().is_empty() {
        return true;
    }
    let hay = format!(
        "{} {} {} {} {}",
        s.display(),
        crate::agents::label(&s.agent),
        s.cwd,
        s.title,
        s.host
    );
    fuzzy::score(q, &hay).is_some()
}

/// A machine's group under `fleet.group = tag`: its first tag (None: the
/// grouping is off, or nothing in the fleet has tags).
pub(crate) fn tag_of(d: &Dash, alias: &str) -> Option<String> {
    if d.cfg.get_or("fleet.group") != "tag" {
        return None;
    }
    let first = |a: &str| {
        d.cfg
            .host_get(a, "tags")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .find(|t| !t.is_empty())
            .map(str::to_string)
    };
    if !d.fleet.hosts.iter().any(|h| first(&h.alias).is_some()) {
        return None;
    }
    Some(first(alias).unwrap_or_default())
}

/// The rows of the grouped list, in order (machines grouped by their first
/// tag when `fleet.group = tag`: tags in fleet order, untagged ones last).
pub(crate) fn rows(d: &Dash) -> Vec<RowId> {
    let q = &d.fv.filter;
    let mut v = Vec::new();
    let mut hosts: Vec<&crate::fleet::Host> = d.fleet.hosts.iter().collect();
    let tags: Vec<Option<String>> = hosts.iter().map(|h| tag_of(d, &h.alias)).collect();
    if tags.iter().any(Option::is_some) {
        let mut order: Vec<String> = Vec::new();
        for t in tags.iter().flatten().filter(|t| !t.is_empty()) {
            if !order.contains(t) {
                order.push(t.clone());
            }
        }
        let rank = |a: &str| {
            let t = tag_of(d, a).unwrap_or_default();
            order.iter().position(|x| *x == t).unwrap_or(order.len())
        };
        hosts.sort_by_key(|h| rank(&h.alias));
    }
    for h in hosts {
        let starting = super::quick::hidden(d, &h.alias);
        let ss: Vec<&Session> = h
            .sessions
            .iter()
            .filter(|s| matches(q, s) && !starting.contains(&s.pane_id))
            .collect();
        // agents running outside herdr there, in the same tree
        let out: Vec<super::outside::Outside> = super::outside::list(d, &h.alias)
            .into_iter()
            .filter(|o| {
                q.trim().is_empty()
                    || fuzzy::score(
                        q,
                        &format!(
                            "{} {} {}",
                            o.p.agent,
                            o.cwd.as_deref().unwrap_or(""),
                            h.alias
                        ),
                    )
                    .is_some()
            })
            .collect();
        if !q.is_empty() && ss.is_empty() && out.is_empty() {
            continue;
        }
        v.push(RowId::Host(h.alias.clone()));
        if h.collapsed && q.is_empty() {
            continue;
        }
        v.extend(ss.into_iter().map(|s| RowId::Sess(s.key())));
        v.extend(
            d.starting
                .iter()
                .filter(|s| s.host == h.alias)
                .map(|s| RowId::Starting(s.id)),
        );
        v.extend(out.iter().map(|o| RowId::Outside(o.key())));
    }
    // machines you could add: ~/.ssh/config aliases outside the fleet
    if q.is_empty() {
        let others = available(d);
        if !others.is_empty() {
            v.push(RowId::Others);
            if d.fv.others_open {
                v.extend(others.into_iter().map(RowId::Avail));
            }
        }
    }
    v
}

/// Machines known to ssh (or this computer) that aren't in the fleet.
pub(crate) fn available(d: &Dash) -> Vec<String> {
    let fleet: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
    d.hv.rows
        .iter()
        .map(|r| r.alias.clone())
        .filter(|a| !fleet.contains(a))
        .collect()
}

/// The selected row (falls back to the nearest one when it disappears).
pub(crate) fn selected(d: &Dash) -> Option<RowId> {
    let r = rows(d);
    if let Some(s) = &d.fv.sel {
        if r.contains(s) {
            return Some(s.clone());
        }
    }
    if r.is_empty() {
        return None;
    }
    // prefer the first session over a header when nothing was chosen yet
    if d.fv.sel.is_none() {
        if let Some(s) = r.iter().find(|x| matches!(x, RowId::Sess(_))) {
            return Some(s.clone());
        }
    }
    r.get(d.fv.idx.min(r.len() - 1)).cloned()
}

fn sel_index(d: &Dash) -> usize {
    let r = rows(d);
    selected(d)
        .and_then(|s| r.iter().position(|x| *x == s))
        .unwrap_or(0)
}

pub(crate) fn session(d: &Dash, key: &str) -> Option<Session> {
    let (h, _) = key.split_once('|')?;
    d.fleet
        .host(h)?
        .sessions
        .iter()
        .find(|s| s.key() == key)
        .cloned()
}

pub(crate) fn selected_session(d: &Dash) -> Option<Session> {
    match selected(d)? {
        RowId::Sess(k) => session(d, &k),
        _ => None,
    }
}

/// Marked sessions, or the selected one.
pub(crate) fn targets(d: &Dash) -> Vec<Session> {
    let mut v: Vec<Session> = d
        .fleet
        .sessions()
        .into_iter()
        .filter(|s| d.fv.marked.contains(&s.key()))
        .cloned()
        .collect();
    if v.is_empty() {
        v.extend(selected_session(d));
    }
    v
}

pub(crate) fn select(d: &mut Dash, r: RowId) {
    let all = rows(d);
    if let Some(i) = all.iter().position(|x| *x == r) {
        d.fv.idx = i;
    }
    d.fv.sel = Some(r);
}

fn move_by(d: &mut Dash, delta: isize) {
    let r = rows(d);
    if r.is_empty() {
        return;
    }
    let i = sel_index(d) as isize + delta;
    let i = i.clamp(0, r.len() as isize - 1) as usize;
    select(d, r[i].clone());
}

/// The next row of a kind, round and round, in the order on screen.
fn step_to(d: &mut Dash, forward: bool, kind: fn(&RowId) -> bool) {
    let r = if d.fb.on {
        super::board::rows(d)
    } else {
        rows(d)
    };
    let n = r.len();
    if n == 0 {
        return;
    }
    let at = selected(d)
        .and_then(|s| r.iter().position(|x| *x == s))
        .unwrap_or(if forward { n - 1 } else { 0 });
    let found = (1..=n)
        .map(|i| {
            if forward {
                (at + i) % n
            } else {
                (at + n - i) % n
            }
        })
        .map(|i| &r[i])
        .find(|x| kind(x));
    if let Some(x) = found.cloned() {
        select(d, x);
    }
}

/// `ctrl+[` `ctrl+]`: the previous / next machine.
fn jump_host(d: &mut Dash, forward: bool) {
    step_to(d, forward, |x| matches!(x, RowId::Host(_)));
}

/// `[` `]`: the previous / next session, whichever machine it's on.
fn step_session(d: &mut Dash, forward: bool) {
    step_to(d, forward, |x| matches!(x, RowId::Sess(_)));
}

fn set_collapsed(d: &mut Dash, host: &str, v: Option<bool>) {
    if let Some(h) = d.fleet.host_mut(host) {
        h.collapsed = v.unwrap_or(!h.collapsed);
    }
    select(d, RowId::Host(host.to_string()));
}

// ------------------------------------------------------------------ background

/// Keep the selected session's preview fresh (debounced, one fetch at a time),
/// and apply a pending initial selection.
pub(crate) fn pump(d: &mut Dash) {
    super::board::pump(d);
    if let Some(q) = d.fv.select_first.clone() {
        let all: Vec<Session> = d.fleet.sessions().into_iter().cloned().collect();
        if let Some(s) = crate::herdr::pick(&all, &q) {
            let k = s.key();
            select(d, RowId::Sess(k));
            d.fv.select_first = None;
        } else if !d.fleet.polling() && d.t0.elapsed() > Duration::from_secs(3) {
            d.fv.select_first = None;
        }
    }
    if d.view != View::Fleet {
        return;
    }
    let Some(s) = selected_session(d) else {
        return;
    };
    let key = s.key();
    match &d.fv.want {
        Some((k, _)) if *k == key => {}
        _ => d.fv.want = Some((key.clone(), Instant::now())),
    }
    let since = d.fv.want.as_ref().map(|x| x.1).unwrap_or(d.t0);
    let fresh =
        d.fv.previews
            .get(&key)
            .is_some_and(|p| p.at.elapsed() < PREVIEW_REFRESH);
    if d.fv.busy || fresh || since.elapsed() < PREVIEW_DELAY {
        return;
    }
    let host = d.fleet.host(&s.host);
    if host.is_some_and(|h| matches!(h.conn, Conn::Down(_))) {
        return;
    }
    d.fv.busy = true;
    let t = d.target_of(&s.host);
    let tx = d.tx.clone();
    thread::spawn(move || {
        let h = Herdr::new(&t);
        // unwrapped lines (wrapped to the panel here), or the screen's own
        // wrapping on a herdr without that source
        let recent = h
            .read(&s.pane_id, "recent-unwrapped", PREVIEW_LINES, true)
            .or_else(|_| h.read(&s.pane_id, "recent", PREVIEW_LINES, true));
        let detect = s
            .blocked()
            .then(|| h.read(&s.pane_id, "detection", 0, false).ok())
            .flatten();
        let _ = tx.send(Bg::Preview {
            key,
            recent,
            detect,
        });
    });
}

// ------------------------------------------------------------------ keys

pub(crate) fn filter_key(d: &mut Dash, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            d.fv.filter.clear();
            d.fv.filtering = false;
        }
        KeyCode::Enter => d.fv.filtering = false,
        KeyCode::Backspace => {
            d.fv.filter.pop();
        }
        KeyCode::Up => move_by(d, -1),
        KeyCode::Down => move_by(d, 1),
        KeyCode::Char(ch) => {
            d.fv.filter.push(ch);
            // keep a matching session selected while typing
            if let Some(s) = rows(d).into_iter().find(|r| matches!(r, RowId::Sess(_))) {
                select(d, s);
            }
        }
        _ => {}
    }
}

pub(crate) fn click(d: &mut Dash, i: usize, double: bool) -> Option<Handoff> {
    // screen lines → rows (machines take two lines, blank lines none)
    let i = d.fv.linemap.get(i).copied().flatten()?;
    let r = rows(d);
    let row = r.get(i)?.clone();
    select(d, row.clone());
    if !double {
        return None;
    }
    match row {
        RowId::Host(h) => {
            set_collapsed(d, &h, None);
            None
        }
        RowId::Sess(_) => open(d),
        RowId::Starting(_) => None,
        RowId::Outside(k) => {
            super::outside::ask_reopen(d, &k);
            None
        }
        RowId::Others => {
            d.fv.others_open = !d.fv.others_open;
            None
        }
        RowId::Avail(a) => {
            super::hosts::toggle_fleet(d, &a);
            None
        }
    }
}

fn attach(d: &mut Dash) -> Option<Handoff> {
    let s = selected_session(d)?;
    Some(Handoff::Attach {
        target: d.target_of(&s.host),
        pane: s.pane_id.clone(),
        label: s.display().to_string(),
    })
}

fn direct(d: &mut Dash) -> Option<Handoff> {
    let s = selected_session(d)?;
    Some(Handoff::Direct {
        target: d.target_of(&s.host),
        pane: s.pane_id.clone(),
        takeover: false,
    })
}

/// enter (or a double click) on a session: what `enter_opens` says —
/// Focus by default.
fn open(d: &mut Dash) -> Option<Handoff> {
    match d.cfg.get_or("enter_opens").as_str() {
        "attach" => attach(d),
        "direct" => direct(d),
        _ => {
            let s = selected_session(d)?;
            super::wall::focus_on(d, &s.key(), View::Fleet);
            None
        }
    }
}

/// `N`: the note box for a session (kept after it ends; empty clears it).
pub(crate) fn note_box(d: &mut Dash, s: &Session) {
    let cur = crate::notes::get(&s.host, &s.name).map(|n| n.text);
    d.mode = Mode::Input(forms::Input::new(
        format!("Note on {}", s.display()),
        "kept after it ends · #tags are searchable in Inbox · empty clears it",
        cur.unwrap_or_default(),
        forms::For::Note(s.host.clone(), s.name.clone()),
    ));
}

/// `w`: the marked sessions side by side, else the selected machine's,
/// else every session.
fn tile_here(d: &mut Dash) {
    if d.fv.marked.is_empty() && super::wall::reopen_tile(d, View::Fleet) {
        return;
    }
    let keys: Vec<String> = if !d.fv.marked.is_empty() {
        targets(d).iter().map(|s| s.key()).collect()
    } else if let Some(h) = d.fv.selected_host(d) {
        d.fleet
            .host(&h)
            .map(|x| x.sessions.iter().map(|s| s.key()).collect())
            .unwrap_or_default()
    } else {
        d.fleet.sessions().iter().map(|s| s.key()).collect()
    };
    if keys.is_empty() {
        d.error("nothing to tile — start a session with n");
        return;
    }
    super::wall::tile(d, keys, View::Fleet);
}

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    let sess = selected_session(d);
    // custom actions: action.<key>=<label>|<command>
    let ks = super::key_str(&k);
    if let Some((_, label, cmd)) = d.keys.custom.iter().find(|(key, _, _)| *key == ks).cloned() {
        let Some(s) = sess else {
            d.error(format!("{label}: select a session first"));
            return None;
        };
        let t = d.target_of(&s.host);
        let host = s.host.clone();
        d.toast(Level::Info, format!("{label} in {}…", s.display()));
        d.spawn(Some(host), move || {
            Herdr::new(&t)
                .run_in_new_tab(&s.cwd, &label, &cmd)
                .map(|_| format!("{label}: opened a tab next to {}", s.display()))
        });
        return None;
    }
    // the board: , orders each machine's sessions
    if d.fb.on && k.code == KeyCode::Char(',') {
        super::board::next_order(d);
        return None;
    }
    if k.code == KeyCode::Esc {
        if !d.fv.filter.is_empty() {
            d.fv.filter.clear();
            return None;
        }
        if !d.fv.marked.is_empty() {
            d.fv.marked.clear();
            return None;
        }
        return Some(Handoff::Quit);
    }
    let act = d.keys.act(&k)?;
    run_act(d, act)
}

/// One Fleet action: a key, or a click on a button.
pub(crate) fn run_act(d: &mut Dash, act: Act) -> Option<Handoff> {
    let sel = selected(d);
    let host = d.fv.selected_host(d);
    let sess = selected_session(d);
    match act {
        // the board: each machine's sessions in its order
        Act::Up if d.fb.on => super::board::move_by(d, -1),
        Act::Down if d.fb.on => super::board::move_by(d, 1),
        Act::Up => move_by(d, -1),
        Act::Down => move_by(d, 1),
        Act::PrevSession => step_session(d, false),
        Act::NextSession => step_session(d, true),
        Act::PrevHost => jump_host(d, false),
        Act::NextHost => jump_host(d, true),
        Act::Mark => match sel {
            Some(RowId::Sess(k)) => {
                if !d.fv.marked.remove(&k) {
                    d.fv.marked.insert(k);
                }
                move_by(d, 1);
            }
            Some(RowId::Host(h)) => set_collapsed(d, &h, None),
            Some(RowId::Others) => d.fv.others_open = !d.fv.others_open,
            Some(RowId::Avail(_) | RowId::Outside(_) | RowId::Starting(_)) | None => {}
        },
        Act::Collapse => {
            if let Some(h) = host {
                set_collapsed(d, &h, Some(true));
            }
        }
        Act::Expand => {
            if let Some(h) = host {
                set_collapsed(d, &h, Some(false));
            }
        }
        Act::Filter => d.fv.filtering = true,
        Act::Prompts => super::promptlib::open(d),
        Act::Refresh => {
            // a start that failed: another go
            if let Some(RowId::Starting(id)) = &sel {
                super::quick::retry(d, *id);
                return None;
            }
            // on a machine: a fresh connection to it; elsewhere: poll them all
            if let Some(RowId::Host(h)) = &sel {
                let (h, tx) = (h.clone(), d.tx.clone());
                d.toast(Level::Info, format!("reconnecting to {h}…"));
                std::thread::spawn(move || {
                    crate::sshcfg::drop_master(&h);
                    let _ = tx.send(super::Bg::Done {
                        ok: true,
                        msg: String::new(),
                        refresh: Some(h),
                    });
                });
                return None;
            }
            d.fleet.refresh(None);
            d.fv.invalidate_preview();
            d.toast(Level::Info, "refreshing every machine…");
        }
        Act::Open => match sel {
            Some(RowId::Host(h)) => set_collapsed(d, &h, None),
            Some(RowId::Sess(_)) => return open(d),
            Some(RowId::Starting(_)) => {
                d.note("not up yet: enter opens it once it has started");
            }
            Some(RowId::Outside(k)) => super::outside::ask_reopen(d, &k),
            Some(RowId::Others) => d.fv.others_open = !d.fv.others_open,
            Some(RowId::Avail(a)) => super::hosts::toggle_fleet(d, &a),
            None => d.note("nothing here yet — n starts a session"),
        },
        Act::Attach => {
            if sess.is_none() {
                d.note("select a session to attach to");
                return None;
            }
            return attach(d);
        }
        Act::Direct => {
            sess?;
            return direct(d);
        }
        Act::TypeInto => {
            let Some(s) = selected_session(d) else {
                d.note("select a session to type into");
                return None;
            };
            super::wall::type_into(d, &s.key(), View::Fleet);
        }
        Act::OpenTab => {
            let s = sess?;
            super::open_in_herdr_tab(d, &s);
        }
        Act::New => {
            let Some(h) = host.or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone())) else {
                d.error("no machines yet — + adds one");
                return None;
            };
            super::quick::open(d, &h);
        }
        Act::Note => {
            let s = sess?;
            note_box(d, &s);
        }
        Act::Mute => {
            let s = sess?;
            let on = !crate::router::muted(&s.host, &s.name);
            crate::router::mute(&s.host, &s.name, if on { 3600 } else { 0 });
            d.note(if on {
                format!(
                    "{} muted for an hour (problems still come through)",
                    s.display()
                )
            } else {
                format!("{} unmuted", s.display())
            });
        }
        Act::Duplicate => {
            let s = sess?;
            super::quick::duplicate(d, &s);
        }
        Act::Shell => {
            let h = host?;
            let dir = sess
                .as_ref()
                .map(|s| s.cwd.clone())
                .filter(|c| c.starts_with('/'));
            super::quick::shell(d, &h, dir.as_deref());
        }
        Act::Presets => {
            // marked sessions: save them as one
            if !d.fv.marked.is_empty() {
                let v = targets(d);
                d.mode = Mode::Input(forms::Input::new(
                    format!(
                        "Save {} session{} as a preset",
                        v.len(),
                        if v.len() == 1 { "" } else { "s" }
                    ),
                    "its name — then P (or aleph preset launch NAME) starts them all again",
                    String::new(),
                    forms::For::SavePresetFrom(v),
                ));
                return None;
            }
            let p = super::palette::Palette::presets(d);
            if p.is_empty() {
                d.note("no presets yet — aleph preset add <name> --host H --dir D");
                return None;
            }
            d.mode = Mode::Palette(p);
        }
        Act::Browse => {
            let h = host?;
            return Some(Handoff::Browse {
                target: d.target_of(&h),
                quick: None,
                terminal: false,
            });
        }
        Act::Wall => tile_here(d),
        // marked: the same prompt to each (what b does with marks)
        Act::Send => {
            let marked = super::batch::marked(d);
            let v = if marked.is_empty() {
                vec![sess?]
            } else {
                marked
            };
            super::batch::send_form(d, v);
        }
        Act::Broadcast => {
            let marked = super::batch::marked(d);
            let v: Vec<Session> = if !marked.is_empty() {
                marked
            } else if let Some(h) = &host {
                d.fleet
                    .host(h)
                    .map(|x| x.sessions.clone())
                    .unwrap_or_default()
            } else {
                vec![]
            };
            if v.is_empty() {
                d.error("nothing to broadcast to — mark sessions with space");
                return None;
            }
            super::batch::send_form(d, v);
        }
        Act::Approve => {
            // marked: every one that asks, each with its own yes option
            let marked = super::batch::marked(d);
            if !marked.is_empty() {
                super::batch::approve(d, marked);
                return None;
            }
            let s = sess?;
            if !s.blocked() {
                d.toast(
                    Level::Info,
                    format!("{} isn't asking anything", s.display()),
                );
                return None;
            }
            let t = d.target_of(&s.host);
            let h = s.host.clone();
            d.spawn(Some(h), move || {
                Herdr::new(&t)
                    .approve(&s.pane_id)
                    .map(|k| format!("approved {} ({})", s.display(), k.join(" ")))
                    .map_err(|e| format!("{}: {e}", s.display()))
            });
        }
        Act::Keys => {
            let s = sess?;
            d.mode = Mode::Input(forms::Input::new(
                format!("Keys for {}", s.display()),
                "space-separated: 2 · enter · esc · up · ctrl+c",
                String::new(),
                forms::For::Keys(s),
            ));
        }
        Act::Rename => {
            let s = sess?;
            d.mode = Mode::Input(forms::Input::new(
                format!("Rename {}", s.display()),
                "lowercase letters, digits, - and _",
                s.name.clone(),
                forms::For::Rename(s),
            ));
        }
        Act::Close => {
            if let Some(RowId::Starting(id)) = &sel {
                super::quick::dismiss(d, *id);
                return None;
            }
            // a machine with nothing marked: tick which of its sessions go
            if let (true, Some(RowId::Host(h))) = (d.fv.marked.is_empty(), &sel) {
                super::quick::close_list(d, h);
                return None;
            }
            // an agent outside herdr: its process, after a yes
            if let (true, Some(RowId::Outside(k))) = (d.fv.marked.is_empty(), &sel) {
                let o = super::outside::find(d, k)?;
                let wher = o
                    .cwd
                    .clone()
                    .map(|c| format!(", in {c}"))
                    .unwrap_or_default();
                d.mode = Mode::Confirm {
                    title: "Stop this agent?".into(),
                    text: format!(
                        "{} (pid {}) on {}{wher} runs outside herdr, in a terminal of its own. This stops the agent (SIGTERM, like ctrl+c); its terminal stays open, and there's no undo.",
                        crate::agents::label(&o.p.agent),
                        o.p.pid,
                        o.host
                    ),
                    on_yes: Pending::Stop(o),
                };
                return None;
            }
            let v = targets(d);
            if v.is_empty() {
                return None;
            }
            let names: Vec<String> = v
                .iter()
                .map(|s| format!("{} ({})", s.display(), s.host))
                .collect();
            d.mode = Mode::Confirm {
                title: if v.len() == 1 {
                    "Close session?".into()
                } else {
                    format!("Close {} sessions?", v.len())
                },
                text: format!("{} — {}", names.join(", "), close_what(d)),
                on_yes: Pending::Close(v),
            };
        }
        Act::Undo => super::batch::undo(d),
        Act::Copy => {
            let s = sess?;
            match d.fv.previews.get(&s.key()) {
                Some(p) if !p.text.is_empty() => {
                    if super::copy_screen(&p.text) {
                        d.note(format!("copied {}'s recent output", s.display()));
                    } else {
                        d.error(super::copy_failed());
                    }
                }
                _ => d.toast(Level::Info, "nothing to copy yet"),
            }
        }
        Act::Tile => {
            let v = targets(d);
            if v.len() < 2 {
                d.toast(
                    Level::Info,
                    "mark two or more sessions on one machine with space, then t",
                );
                return None;
            }
            if v.iter().any(|s| s.host != v[0].host) {
                d.error("herdr tiles within one machine — the wall (w) tiles across machines");
                return None;
            }
            let t = d.target_of(&v[0].host);
            let h = v[0].host.clone();
            d.fv.marked.clear();
            d.spawn(Some(h), move || {
                crate::cli::tile_into(&t, &v)
                    .map(|l| format!("tiled {} sessions into {l}", v.len()))
            });
        }
        Act::Hub => return Some(Handoff::Hub),
        Act::Editor => {
            let s = sess?;
            match crate::editor::open(&d.cfg, &s.host, &s.cwd) {
                Ok(m) => d.toast(Level::Ok, m),
                Err(e) => d.error(e),
            }
        }
        // which agent takes over (the note and the new session run in the background)
        Act::Handoff => {
            let s = sess?;
            if s.agent == crate::agents::SHELL {
                d.toast(Level::Info, "a plain shell has no task to hand over");
                return None;
            }
            d.mode = super::Mode::Palette(super::palette::Palette::handoff(d, &s));
        }
        Act::Timeline => super::timechart::toggle_rows(d),
        Act::TimelinePanel => {
            // on, then the whole turn view (leaving that: its recent output again)
            if d.fv.timeline_panel {
                if let Some(s) = sess.clone() {
                    d.fv.timeline_panel = false;
                    super::tree_view::open_session(d, &s);
                    return None;
                }
            }
            d.fv.timeline_panel = !d.fv.timeline_panel;
            d.toast(
                Level::Info,
                if d.fv.timeline_panel {
                    "the session's timeline (G again: its recent output) · the wheel scrolls it, a click folds"
                } else {
                    "the session's recent output"
                },
            );
        }
        Act::Alerts => super::alerts::open(d),
        Act::Approvals => super::approvals::open(d),
        Act::StatsMode => d.fv.stats_mode = (d.fv.stats_mode + 1) % 3,
        Act::AddHost => match sel {
            // on a machine outside the fleet: bring it in
            Some(RowId::Avail(a)) => super::hosts::toggle_fleet(d, &a),
            _ => d.mode = Mode::HostForm(forms::HostForm::new()),
        },
        Act::EditHost => {
            let h = host?;
            if crate::sshcfg::is_local(&h) {
                d.toast(Level::Info, "this computer has nothing to edit");
                return None;
            }
            d.mode = Mode::HostForm(forms::HostForm::edit(&h));
        }
        Act::TestHost => {
            let h = host?;
            d.toast(Level::Info, format!("testing key login to {h}…"));
            if !d.hv.rows.iter().any(|r| r.alias == h) {
                super::hosts::reload(d);
            }
            super::hosts::test(d, &h);
        }
        Act::KeySetup => {
            let h = host?;
            if crate::sshcfg::is_local(&h) {
                return None;
            }
            return Some(Handoff::KeySetup(h));
        }
        Act::MakeDefault => {
            let h = host?;
            super::hosts::set_default(d, &h);
        }
        Act::PauseHost => {
            let h = host?;
            let paused = d.fleet.host(&h).is_some_and(|x| x.paused);
            // pausing stops every alert, state and question from it: asked twice
            let armed =
                d.fv.pause_armed
                    .take()
                    .is_some_and(|(a, t)| a == h && t.elapsed() < PAUSE_CONFIRM);
            if !paused && !armed {
                d.fv.pause_armed = Some((h.clone(), Instant::now()));
                d.toast(
                    super::Level::Warn,
                    format!("Z again to pause {h}: it stops being watched (no states, questions or alerts) until you resume it"),
                );
                return None;
            }
            let key = format!("host.{h}.paused");
            let r = if paused {
                d.cfg.unset(&key)
            } else {
                d.cfg.set(&key, "true")
            };
            if let Err(e) = r {
                d.error(format!("couldn't save: {e}"));
                return None;
            }
            if let Some(x) = d.fleet.host_mut(&h) {
                x.paused = !paused;
            }
            if paused {
                d.fleet.refresh(Some(&h));
                d.note(format!("{h} resumed"));
            } else {
                d.toast(
                    super::Level::Warn,
                    format!("{h} paused — not watched until Z again (its sessions keep running)"),
                );
            }
        }
        Act::HostColor => {
            let h = host?;
            super::hosts::cycle_color(d, &h);
        }
        Act::HerdrSetup => {
            // herdr, then its agents: `aleph host setup` in a new tab
            let h = host?;
            super::hosts::open_setup(d, &h, None);
        }
        Act::Wake => {
            let h = host?;
            if d.fleet.host(&h).is_some_and(|x| x.conn == Conn::Up) {
                d.note(format!("{h} is awake"));
                return None;
            }
            // this computer first (it may share the network), then the
            // machines that answer
            let mut senders = vec![crate::sshcfg::LOCAL.to_string()];
            senders.extend(
                d.fleet
                    .hosts
                    .iter()
                    .filter(|x| {
                        x.alias != h && x.conn == Conn::Up && !crate::sshcfg::is_local(&x.alias)
                    })
                    .map(|x| x.alias.clone()),
            );
            let cfg = d.cfg.clone();
            d.toast(Level::Info, format!("waking {h}…"));
            let hh = h.clone();
            d.spawn(Some(h), move || {
                crate::wol::wake(&cfg, &hh, &senders)
                    .map(|(from, tool)| format!("sent {hh} a wake-up packet from {from} ({tool})"))
            });
        }
        Act::RemoveHost => {
            let h = host?;
            let n = d.fleet.host(&h).map_or(0, |x| x.sessions.len());
            d.mode = Mode::Confirm {
                title: format!("Remove {h} from the fleet?"),
                text: format!(
                    "{} Its ssh entry stays: \"Not in fleet\" at the bottom brings it back.",
                    if n > 0 {
                        if n == 1 {
                            "Its session keeps running there.".to_string()
                        } else {
                            format!("Its {n} sessions keep running there.")
                        }
                    } else {
                        "Nothing running there is touched.".to_string()
                    }
                ),
                on_yes: Pending::Leave(h),
            };
        }
        Act::Queue => {
            // marked: the task queued for each, sent once it's free
            let marked = super::batch::marked(d);
            if !marked.is_empty() {
                super::batch::queue_form(d, marked);
                return None;
            }
            let h = host?;
            let dir = sess
                .as_ref()
                .map(|s| s.cwd.clone())
                .filter(|c| !c.is_empty());
            let hint = match &dir {
                Some(dd) => format!("the next idle agent in {dd} takes it"),
                None => "the next idle agent on this machine takes it".into(),
            };
            d.mode = Mode::Input(forms::Input::new(
                format!("Queue a task on {h}"),
                &hint,
                String::new(),
                forms::For::Queue(h, dir),
            ));
        }
        Act::Heal => {
            let h = host?;
            d.toast(
                Level::Info,
                format!("starting herdr on {h} and restoring its sessions…"),
            );
            let t = d.target_of(&h);
            d.spawn(Some(h.clone()), move || {
                if crate::sshcfg::is_local(&h) {
                    Herdr::new(&t)
                        .start_server()
                        .map(|_| "herdr is running on this computer".into())
                } else {
                    crate::auto::heal_now(&h)
                }
            });
        }
    }
    None
}

/// What a close does, for its confirm: the grace and its undo (`close.grace`).
pub(crate) fn close_what(d: &Dash) -> String {
    let g = crate::closing::grace(&d.cfg);
    if g.is_zero() {
        return "the agent stops and its pane closes.".into();
    }
    format!(
        "the pane closes in {} (the agent runs until then; {} keeps it).",
        crate::closing::grace_text(g),
        d.keys.key_for(Act::Undo)
    )
}

/// What `i` says on a machine: it starts that machine's default agent, by name.
/// Hints are static, so each wording is kept once (there are only so many agents).
pub(crate) fn folder_hint(d: &Dash, host: &str) -> &'static str {
    static SAID: std::sync::Mutex<BTreeMap<&'static str, &'static str>> =
        std::sync::Mutex::new(BTreeMap::new());
    let name = crate::agents::default_agent_for(&d.cfg, host).name;
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    said.entry(name)
        .or_insert_with(|| Box::leak(format!("start {name} in a folder").into_boxed_str()))
}

pub(crate) fn hints(d: &Dash) -> Vec<(&'static str, &'static str)> {
    if d.fv.filtering {
        return vec![("enter", "keep filter"), ("esc", "clear"), ("↑↓", "move")];
    }
    // the selected session is closing: keep it
    if selected_session(d).is_some_and(|s| crate::closing::pending(&s.host, &s.pane_id).is_some()) {
        return vec![
            ("u", "keep it (undo the close)"),
            ("enter", "focus"),
            ("↑↓", "move"),
            ("ctrl+k", "palette"),
            ("q", "quit"),
        ];
    }
    match selected(d) {
        Some(RowId::Host(h)) => vec![
            ("n", "new here"),
            ("i", folder_hint(d, &h)),
            ("e", "edit"),
            ("T", "test"),
            ("Z", "pause"),
            ("*", "default"),
            ("+", "add machine"),
            ("-", "remove"),
            ("ctrl+k", "palette"),
        ],
        Some(RowId::Starting(id)) => {
            let failed = d
                .starting
                .iter()
                .any(|s| s.id == id && matches!(s.state, super::quick::StartState::Failed(_)));
            if failed {
                vec![("r", "try again"), ("x", "drop it"), ("↑↓", "move")]
            } else {
                vec![("x", "cancel"), ("↑↓", "move"), ("q", "quit")]
            }
        }
        Some(RowId::Others) => vec![
            ("enter", "show machines you could add"),
            ("+", "add a new one"),
        ],
        Some(RowId::Avail(_)) => vec![
            ("enter", "add to the fleet"),
            ("T", "test login"),
            ("l", "key setup"),
            ("e", "edit"),
            ("M", "save in herdr"),
        ],
        Some(RowId::Sess(_)) if selected_session(d).is_some_and(|s| s.blocked()) => vec![
            ("y", "approve"),
            ("K", "answer…"),
            ("s", "reply"),
            ("ctrl+a", "all waiting"),
            ("enter", "focus"),
            ("a", "attach"),
            ("O", "new tab"),
            ("x", "close"),
            ("ctrl+k", "palette"),
            ("q", "quit"),
        ],
        Some(RowId::Sess(_)) => vec![
            ("enter", "focus"),
            ("a", "attach"),
            ("O", "new tab"),
            ("x", "close"),
            ("s", "prompt"),
            ("n", "new"),
            ("!", "terminal"),
            ("/", "search"),
            ("space", "mark"),
            ("w", "tile"),
            (
                "g",
                if super::timechart::fleet_rows(d) {
                    "details"
                } else {
                    "timeline"
                },
            ),
            ("/", "filter"),
            ("ctrl+k", "palette"),
            ("q", "quit"),
        ],
        Some(RowId::Outside(_)) => vec![
            ("enter", "reopen in herdr"),
            ("x", "stop it"),
            ("n", "new here (in herdr)"),
            ("↑↓", "move"),
            ("/", "filter"),
            ("ctrl+k", "palette"),
            ("q", "quit"),
        ],
        None => vec![("n", "new session"), ("+", "add a machine"), ("q", "quit")],
    }
}

/// enter on an agent outside herdr: say what Aleph can and can't do with it.
fn outside_note(d: &mut Dash) {
    d.note("it runs outside herdr: enter reopens it in herdr (same folder, same conversation), x stops it");
}

// ------------------------------------------------------------------ drawing

fn dur_since(since: Option<u64>) -> String {
    since
        .map(|t| ui::dur(crate::cache::now().saturating_sub(t)))
        .unwrap_or_default()
}

/// [`status_spans`], unless its machine is paused: then what it showed last
/// isn't news (`⏸ paused`).
pub(crate) fn status_for(d: &Dash, s: &Session) -> (String, Color) {
    if d.fleet.host(&s.host).is_some_and(|h| h.paused) {
        return ("⏸ paused · not watched".into(), c::YELLOW());
    }
    status_spans(s, d.t0)
}

/// `▲ needs you · 4m` etc.
pub(crate) fn status_spans(s: &Session, t0: Instant) -> (String, Color) {
    if let Some(r) = &s.limit {
        return (limit_text(r), c::LIMIT());
    }
    if let Some(e) = &s.restore_error {
        return (format!("✗ didn't come back: {e}"), c::RED());
    }
    let (g, col, mut label) = ui::status(&s.status, t0);
    if s.compacting {
        label = "compacting".into();
    }
    let d = dur_since(s.since);
    let text = if d.is_empty() || s.status == "idle" {
        format!("{g} {label}")
    } else {
        format!("{g} {label} · {d}")
    };
    (text, col)
}

/// `◷ limit · resets 2:40am` (see `limits::label`).
pub(crate) fn limit_text(reset: &str) -> String {
    crate::limits::label(reset)
}

/// Counts of the states the fleet cares about, over `sessions`.
pub(crate) struct Tally {
    pub working: usize,
    pub blocked: usize,
    pub done: usize,
    pub idle: usize,
    pub limit: usize,
    /// (agent id, count), most first
    pub agents: Vec<(String, usize)>,
}

pub(crate) fn tally<'a>(sessions: impl IntoIterator<Item = &'a Session>) -> Tally {
    let mut t = Tally {
        working: 0,
        blocked: 0,
        done: 0,
        idle: 0,
        limit: 0,
        agents: Vec::new(),
    };
    for s in sessions {
        if s.limit.is_some() {
            t.limit += 1;
        } else {
            match s.status.as_str() {
                "working" => t.working += 1,
                "blocked" => t.blocked += 1,
                "done" => t.done += 1,
                _ => t.idle += 1,
            }
        }
        match t.agents.iter_mut().find(|(a, _)| *a == s.agent) {
            Some(x) => x.1 += 1,
            None => t.agents.push((s.agent.clone(), 1)),
        }
    }
    t.agents.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    t
}

/// The fleet-wide strip: states, agents, systems.
pub(crate) fn summary_line(d: &Dash, w: usize) -> Line<'static> {
    let t = tally(d.fleet.sessions());
    let mut v: Vec<Span> = vec![Span::raw(" ")];
    let state = |v: &mut Vec<Span>, g: &str, n: usize, label: &str, col: Color| {
        let st = if n == 0 {
            ui::faint()
        } else {
            ui::fg(col).add_modifier(Modifier::BOLD)
        };
        v.push(Span::styled(format!("{g} {n} "), st));
        v.push(Span::styled(
            format!("{label}   "),
            if n == 0 { ui::faint() } else { ui::muted() },
        ));
    };
    state(
        &mut v,
        ui::spinner(d.t0),
        t.working,
        "working",
        c::WORKING(),
    );
    state(
        &mut v,
        crate::look::needs(),
        t.blocked,
        "needs you",
        c::NEEDS(),
    );
    state(&mut v, "✓", t.done, "done", c::GREEN());
    state(&mut v, "●", t.idle, "idle", c::MUTED());
    state(&mut v, "◷", t.limit, "at limit", c::LIMIT());
    if !t.agents.is_empty() {
        v.push(Span::styled("│  ", ui::fg(c::LINE())));
        let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
        let named: usize = t
            .agents
            .iter()
            .map(|(a, n)| ui::width(&crate::agents::label(a)) + n.to_string().len() + 5)
            .sum();
        let names = used + named + 2 <= w;
        for (a, n) in &t.agents {
            v.push(icons::agent(a));
            if names {
                v.push(Span::styled(
                    format!(" {} ", crate::agents::label(a)),
                    ui::muted(),
                ));
            } else {
                v.push(Span::raw(" "));
            }
            v.push(Span::styled(format!("{n}  "), ui::bold()));
        }
    }
    // machines by OS, and GPUs
    let mut oses: Vec<(String, Color, usize)> = Vec::new();
    let mut gpus = 0;
    for h in &d.fleet.hosts {
        let (name, col) = icons::distro(&host_os(d, h))
            .map_or(("other".to_string(), c::MUTED()), |x| {
                (x.name.to_string(), x.color)
            });
        match oses.iter_mut().find(|(x, ..)| *x == name) {
            Some(x) => x.2 += 1,
            None => oses.push((name, col, 1)),
        }
        gpus += d.stats(&h.alias).map_or(0, |s| s.gpus.len());
    }
    let mut tail: Vec<Span> = vec![Span::styled("│  ", ui::fg(c::LINE()))];
    for (name, col, n) in oses {
        tail.push(Span::styled(name, ui::fg(col)));
        tail.push(Span::styled(format!(" {n}   "), ui::muted()));
    }
    if gpus > 0 {
        tail.push(Span::styled(
            format!("{gpus} GPU{}", if gpus == 1 { "" } else { "s" }),
            ui::muted(),
        ));
    }
    let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
    let tw: usize = tail.iter().map(|x| ui::width(&x.content)).sum();
    if used + tw <= w {
        v.extend(tail);
    }
    Line::from(v)
}

/// `Linux · Ubuntu 24.04.3 LTS` → `Ubuntu 24.04.3 LTS` (the kernel name goes
/// when a distribution follows it).
pub(crate) fn os_pretty(os: &str) -> String {
    os.split_once(" · ")
        .map_or(os, |(_, rest)| rest)
        .trim()
        .to_string()
}

/// "Linux · Ubuntu 24.04" (from herdr info) or the stats' platform.
pub(crate) fn host_os(d: &Dash, h: &Host) -> String {
    let o = h.os();
    if !o.is_empty() {
        return o.to_string();
    }
    d.stats(&h.alias)
        .map(|s| format!("{} {}", s.platform, s.os).trim().to_string())
        .unwrap_or_default()
}

/// `NVIDIA RTX 4090`, `2× RTX 4090`, `AMD MI300X` — the machine's GPUs.
pub(crate) fn gpu_badge(st: &telemetry::Stats) -> Vec<Span<'static>> {
    let Some(g) = st.gpus.first() else {
        return vec![];
    };
    let model = g
        .name
        .replace("NVIDIA ", "")
        .replace("GeForce ", "")
        .replace("AMD ", "")
        .replace("Radeon ", "");
    let n = st.gpus.len();
    let mut v = vec![icons::gpu(&g.vendor), Span::raw(" ")];
    v.push(Span::styled(
        if n > 1 {
            format!("{n}× {model}")
        } else {
            model
        },
        ui::bold(),
    ));
    v
}

pub(crate) fn host_state(h: &Host, t0: Instant) -> (String, Color) {
    if h.paused {
        return ("⏸ paused".into(), c::MUTED());
    }
    match (&h.conn, h.polling.is_some()) {
        (Conn::Unknown, _) if h.stale => (
            format!("{} connecting · cached", ui::spinner(t0)),
            c::YELLOW(),
        ),
        (Conn::Unknown, _) => (format!("{} connecting", ui::spinner(t0)), c::YELLOW()),
        (Conn::Up, _) => ("● herdr".into(), c::GREEN()),
        (Conn::Down(e), _) if not_running(e) => ("○ herdr not running".into(), c::MUTED()),
        (Conn::Down(_), _) => ("✕ offline".into(), c::RED()),
    }
}

use crate::fleet::not_running;

/// A machine as a card: name · state · its sessions, with GPU and load on
/// the right; then OS · address · latency · installed agents. Comfortable
/// and large density draw the OS logo (6×3) beside three lines, the third
/// today's usage there; compact keeps two lines and an OS chip.
fn host_card(d: &Dash, h: &Host, w: usize, selected: bool) -> Vec<Line<'static>> {
    let fold = if h.collapsed && d.fv.filter.is_empty() {
        "▸"
    } else {
        "▾"
    };
    let wid = |v: &[Span]| v.iter().map(|s| ui::width(&s.content)).sum::<usize>();
    let (st, col) = host_state(h, d.t0);
    let os = host_os(d, h);
    // the OS logo beside the card when it's a picture here (an image, or
    // thumbnails asked for); else the chip
    let logo: Vec<Line<'static>> = if ui::scale() >= 1 && crate::logo::pictures(&d.cfg, 3) {
        crate::logo::lines(crate::logo::Kind::Os, &os, 6, 3, &d.cfg)
    } else {
        Vec::new()
    };
    let logo_row = |i: usize| -> Vec<Span<'static>> {
        logo.get(i).map(|l| l.spans.clone()).unwrap_or_default()
    };
    // ---- line 1
    let mut l: Vec<Span> = vec![Span::styled(format!("{fold} "), ui::faint())];
    if logo.is_empty() {
        // the OS as a badge in its own colours, padded so machines line up
        l.extend(icons::os_badge(&os, 11));
    } else {
        l.extend(logo_row(0));
    }
    l.extend([
        Span::raw(" "),
        Span::styled(
            crate::showcase::shown(&h.alias).into_owned(),
            Style::new().fg(h.color).add_modifier(Modifier::BOLD),
        ),
    ]);
    // unread inbox events from this machine: a dot (4 opens the inbox)
    let unread = super::inbox::unread_for(d, &h.alias);
    if unread > 0 {
        l.push(Span::styled(
            format!(" ●{unread}"),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ));
    }
    l.extend([Span::raw("   "), Span::styled(st, ui::fg(col))]);
    // its setup runs in another tab (`aleph host setup`): until it reports back
    if let Some(t0) = d.hv.setting_up.get(&h.alias) {
        l.push(Span::styled(
            format!("  {} setting up in a tab", ui::spinner(*t0)),
            ui::fg(c::ACCENT()),
        ));
    }
    if matches!(h.conn, Conn::Down(_)) && crate::wol::known(&d.cfg, &h.alias) {
        l.push(Span::styled("  W wakes it", ui::faint()));
    }
    let n = h.sessions.len();
    if n > 0 {
        l.push(Span::styled(
            format!("   {n} session{}", if n == 1 { "" } else { "s" }),
            ui::muted(),
        ));
    }
    let out = super::outside::list(d, &h.alias).len();
    if out > 0 {
        l.push(Span::styled(
            format!("{}{out} outside herdr", if n > 0 { " · " } else { "   " }),
            ui::faint(),
        ));
    }
    let t = tally(h.sessions.iter());
    if t.blocked > 0 {
        l.push(Span::styled(
            format!("  {} {}", crate::look::needs(), t.blocked),
            ui::fg(c::NEEDS()).add_modifier(Modifier::BOLD),
        ));
    }
    if t.limit > 0 {
        l.push(Span::styled(format!("  ◷ {}", t.limit), ui::fg(c::LIMIT())));
    }
    if t.working > 0 {
        l.push(Span::styled(
            format!("  {} {}", ui::spinner(d.t0), t.working),
            ui::fg(c::ACCENT()),
        ));
    }
    let right: Vec<Span> = d
        .stats(&h.alias)
        .map(|st| {
            let mut v = gpu_badge(&st);
            let sum = telemetry::summary(&st);
            if !v.is_empty() && !sum.is_empty() {
                v.push(Span::raw("   "));
            }
            v.extend(sum);
            v
        })
        .unwrap_or_default();
    let (used, rw) = (wid(&l), wid(&right));
    if rw > 0 && used + rw + 3 <= w {
        l.push(Span::raw(" ".repeat(w - used - rw)));
        l.extend(right);
    }
    // ---- line 2
    let mut m: Vec<Span> = Vec::new();
    let add = |m: &mut Vec<Span<'static>>, s: Span<'static>| {
        if !m.is_empty() {
            m.push(Span::styled(" · ", ui::faint()));
        }
        m.push(s);
    };
    if !os.is_empty() {
        add(&mut m, Span::styled(os_pretty(&os), ui::muted()));
    }
    if !h.addr.is_empty() {
        add(&mut m, Span::styled(h.addr.clone(), ui::faint()));
    }
    match (&h.conn, h.latency) {
        (Conn::Up, Some(lat)) => {
            // link bars: ▂▄▆ good · ▂▄· slow or lossy
            let (bars, col) = match h.link() {
                crate::fleet::Link::Degraded => ("▂▄·", c::YELLOW()),
                _ => ("▂▄▆", c::GREEN()),
            };
            add(
                &mut m,
                Span::styled(format!("{bars} {} ms", lat.as_millis()), ui::fg(col)),
            )
        }
        (Conn::Down(_), _) | (Conn::Unknown, _) if h.stale => {
            let ago = h
                .fetched
                .map(|f| ui::ago(crate::cache::now().saturating_sub(f)))
                .unwrap_or_default();
            add(
                &mut m,
                Span::styled(format!("cached {ago}"), ui::fg(c::YELLOW())),
            );
        }
        _ => {}
    }
    if let Some((theirs, _)) = h.drift() {
        add(
            &mut m,
            Span::styled(format!("≠ herdr {theirs}"), ui::fg(c::YELLOW())),
        );
    } else if h.herdr_restart() {
        add(&mut m, Span::styled("↻ herdr restart", ui::fg(c::YELLOW())));
    }
    let q = crate::queue::count(&h.alias);
    if q > 0 {
        add(
            &mut m,
            Span::styled(format!("≡ {q} queued"), ui::fg(c::BLUE())),
        );
    }
    // installed agents (from discovery), at the end
    if let Some(found) = d.agents.snapshot(&h.alias) {
        let b = crate::agentwatch::badges(&found);
        // (the line's lead: the logo column, or the badge's width)
        if !b.is_empty() && 15 + wid(&m) + wid(&b) + 6 <= w {
            m.push(Span::styled("   has ", ui::faint()));
            m.extend(b);
        }
    }
    let lead = |i: usize| -> Vec<Span<'static>> {
        if logo.is_empty() {
            vec![Span::raw(" ".repeat(15))]
        } else {
            let mut v = vec![Span::raw("  ")];
            v.extend(logo_row(i));
            v.push(Span::raw(" "));
            v
        }
    };
    let mut m2 = lead(1);
    m2.extend(m);
    let mut out = vec![Line::from(l), Line::from(m2)];
    let today = super::usage::today_for(d, &h.alias).filter(|r| r.requests > 0);
    if !logo.is_empty() || (ui::scale() >= 1 && today.is_some()) {
        let mut t = lead(2);
        if let Some(r) = today {
            t.extend(super::usage::today_line(r));
        }
        out.push(Line::from(t));
    }
    if selected {
        for x in out.iter_mut() {
            *x = std::mem::take(x).style(ui::selected());
        }
    }
    out
}

/// What every session row shares in a frame: the name and status columns'
/// widths, the room for the tiny bars (`ui.row_bars`; 0: none), and the
/// timeline's width when the rows show it (`g`; 0: they don't).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Cols {
    pub nw: usize,
    /// the model's column (0: no room for it)
    pub mw: usize,
    pub sw: usize,
    pub bars: usize,
    pub tl: usize,
}

/// A session row's columns up to its status, `lead_w` wide: the tree, the
/// mark, its glyph and agent, its name, its status (breathing while it needs
/// you, a tick when it has just finished: `ui.motion`).
pub(crate) fn lead(
    d: &Dash,
    s: &Session,
    (nw, mw, sw): (usize, usize, usize),
    guide: &str,
) -> Vec<Span<'static>> {
    let marked = d.fv.marked.contains(&s.key());
    let (st, col) = status_for(d, s);
    let base = ui::fg(col).add_modifier(if s.blocked() {
        Modifier::BOLD
    } else {
        Modifier::empty()
    });
    let mut v: Vec<Span> = vec![
        Span::styled(format!("  {guide}"), ui::fg(c::LINE())),
        Span::styled(
            if marked { "◉ " } else { "  " },
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{} ", s.glyph()), ui::fg(s.color())),
        icons::agent(&s.agent),
        Span::raw(" "),
        Span::styled(
            ui::pad(&ui::trunc(s.display(), nw), nw),
            Style::new().fg(s.color()).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
    ];
    if mw > 0 {
        v.push(Span::styled(
            ui::pad(&ui::trunc(&model_of(d, s), mw), mw),
            ui::fg(s.color()),
        ));
        v.push(Span::raw("  "));
    }
    v.extend(super::motion::status(d, s, &st, base, sw));
    v
}

/// The model a session last used, short (`opus-5-5`), from its own logs: known
/// after its first request is read.
fn model_of(d: &Dash, s: &Session) -> String {
    super::usage::live_for(d, s)
        .map(|l| crate::timeline::short_model(&l.model))
        .unwrap_or_default()
}

/// The model column's width for these sessions (0: none has one yet).
fn model_w(d: &Dash, sessions: &[&Session]) -> usize {
    sessions
        .iter()
        .map(|s| ui::width(&model_of(d, s)))
        .max()
        .unwrap_or(0)
        .min(MODEL_W)
}

/// A long model name is cut here.
const MODEL_W: usize = 16;
/// Rows narrower than this leave the model to the preview.
const MODEL_MIN_ROW: usize = 96;

/// How wide `lead` is (the same for every row of a frame).
pub(crate) fn lead_w((nw, mw, sw): (usize, usize, usize)) -> usize {
    2 + 2 + 2 + 2 + 1 + 1 + nw + 2 + if mw > 0 { mw + 2 } else { 0 } + sw
}

fn sess_line(
    d: &Dash,
    h: &Host,
    s: &Session,
    w: usize,
    cols: &Cols,
    selected: bool,
    guide: &str,
) -> Line<'static> {
    let mut v = lead(d, s, (cols.nw, cols.mw, cols.sw), guide);
    // closing (`close.grace`): greyed out, and how long until then
    if let Some(e) = crate::closing::pending(&s.host, &s.pane_id) {
        for sp in v.iter_mut() {
            sp.style = ui::faint();
        }
        v.push(Span::raw("  "));
        v.extend(super::batch::closing_note(d, &e));
        let line = Line::from(v);
        return if selected {
            line.style(ui::selected())
        } else {
            line
        };
    }
    // tokens a minute and the context's fill, in a column of their own
    v.extend(super::rowbars::slot(d, s, cols.bars));
    let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
    let room = w.saturating_sub(used + 1);
    if cols.tl > 0 {
        // the timeline instead of folder, title and usage (`g`)
        if cols.bars == 0 {
            v.push(Span::raw("  "));
        }
        v.extend(super::timechart::session_bar(d, s, cols.tl));
    } else if room > 6 {
        let folder = h.short_path(&s.cwd);
        let fw = ui::width(&folder).min(room.min(28));
        v.push(Span::styled(ui::trunc_left(&folder, fw), ui::muted()));
        // a note: its tags (or ✎) at the end, ahead of the title
        let note = crate::notes::get(&s.host, &s.name).map(|n| {
            let tags: Vec<String> = n.tags.iter().take(2).map(|t| format!("#{t}")).collect();
            if tags.is_empty() {
                "✎".to_string()
            } else {
                format!("✎ {}", tags.join(" "))
            }
        });
        let nw_ = note.as_ref().map_or(0, |n| ui::width(n) + 2);
        let room2 = room.saturating_sub(fw + 2 + nw_);
        if let Some(n) = note.filter(|_| room > fw + 2 + nw_) {
            v.push(Span::styled(format!("  {n}"), ui::fg(c::ACCENT())));
        }
        // tokens · context · $ from its own logs, at the right
        // (ahead of the title: the preview and Focus show that)
        let chip: Vec<Span> = super::usage::live_for(d, s)
            .map(|l| super::usage::chip(l, room2.saturating_sub(2).min(34)))
            .unwrap_or_default();
        let cw: usize = chip.iter().map(|x| ui::width(&x.content)).sum();
        let room3 = room2.saturating_sub(if cw > 0 { cw + 2 } else { 0 });
        if room3 > 8 && !s.title.is_empty() {
            v.push(Span::raw("  "));
            v.push(Span::styled(ui::trunc(&s.title, room3), ui::faint()));
        }
        if cw > 0 {
            let used: usize = v.iter().map(|x| ui::width(&x.content)).sum();
            if used + cw + 2 <= w {
                v.push(Span::raw(" ".repeat(w - used - cw)));
                v.extend(chip);
            }
        }
    }
    let dim = h.stale && h.conn != Conn::Up;
    let mut line = Line::from(v);
    if dim {
        line = line.style(ui::faint());
    }
    if selected {
        line = line.style(ui::selected());
    }
    line
}

fn empty(d: &Dash, f: &mut Frame, r: Rect) {
    let mut v = vec![
        Line::from(vec![
            Span::styled(
                format!("{}  ", crate::brand::MARK),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("Welcome to Aleph", ui::bold()),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "Agents run inside herdr on your ssh machines. Add a machine to get started.",
            ui::muted(),
        )),
        Line::raw(""),
    ];
    v.push(ui::hints(&[("+", "add a machine"), ("ctrl+k", "palette"), ("q", "quit")]).centered());
    v.push(Line::raw(""));
    v.push(
        Line::from(Span::styled(
            "or run  aleph setup  for a guided tour",
            ui::faint(),
        ))
        .centered(),
    );
    let h = v.len() as u16;
    let rr = Rect {
        y: r.y + r.height.saturating_sub(h) / 2,
        height: h.min(r.height),
        ..r
    };
    f.render_widget(Paragraph::new(v).alignment(Alignment::Center), rr);
    // showcase mode: stars around it
    crate::showcase::starfield(&d.cfg, f.buffer_mut(), r);
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    if d.fleet.hosts.is_empty() {
        empty(d, f, r);
        return;
    }
    // maximised (z), with the room for it: the board
    if super::board::wanted(d) && r.width >= 100 && r.height >= 20 {
        let [top, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(r);
        f.render_widget(Paragraph::new(summary_line(d, top.width as usize)), top);
        super::board::draw(d, f, rest);
        return;
    }
    d.fb.on = false;
    let all = rows(d);
    let sel = sel_index(d);
    d.fv.idx = sel;
    // (agents outside herdr show their agent's name in the same column)
    let outside_names = all.iter().filter_map(|r| match r {
        RowId::Outside(k) => {
            super::outside::find(d, k).map(|o| ui::width(&crate::agents::label(&o.p.agent)))
        }
        _ => None,
    });
    let nw = d
        .fleet
        .sessions()
        .iter()
        .map(|s| ui::width(s.display()))
        .chain(outside_names)
        .max()
        .unwrap_or(8)
        .clamp(8, 22);
    // the status column fits the widest status ("◷ limit · back in 2h 13m"),
    // with a gap before the folder
    let sw = d
        .fleet
        .sessions()
        .iter()
        .map(|s| ui::width(&status_for(d, s).0) + 2)
        .max()
        .unwrap_or(18)
        .clamp(18, 30);
    // the fleet-wide strip
    let r = if r.height >= 14 && r.width >= 50 {
        let [top, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(r);
        f.render_widget(Paragraph::new(summary_line(d, top.width as usize)), top);
        rest
    } else {
        r
    };
    let stats_on = d.cfg.get_or("stats") != "off";
    let sel_host = d.fv.selected_host(d);
    let stats = sel_host.as_deref().and_then(|h| d.stats(h));
    let gap = ui::scale() >= 1;

    // the list as display lines: a machine is a two-line card (and a blank
    // line before every machine but the first, when there's room to breathe)
    let mut lines: Vec<Line> = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    // the panels' layout (p, z, { }, =, L): the list's width follows it
    let shape = super::panes::shape(d, "fleet");
    let own_lw = (r.width * 58 / 100).clamp(70, 150);
    let lw_guess = if r.width >= 110 {
        let (main, _) = shape.columns(r, r.width.saturating_sub(own_lw + 1), 1);
        if main.width == 0 {
            own_lw
        } else {
            main.width
        }
    } else {
        r.width
    };
    let w = lw_guess.saturating_sub(2 + 2 * ui::padding().left) as usize;
    // the session rows' columns: the tiny bars when there's room, the
    // timeline across what's left (`g`; the bars give way when it's tight)
    let mw = if w >= MODEL_MIN_ROW {
        let all = d.fleet.sessions();
        model_w(d, &all)
    } else {
        0
    };
    let mut cols = Cols {
        nw,
        mw,
        sw,
        bars: super::rowbars::room(&d.cfg, w),
        tl: 0,
    };
    if super::timechart::fleet_rows(d) {
        let tl_w = |bars: usize| w.saturating_sub(lead_w((nw, mw, sw)) + bars.max(2) + 1);
        if tl_w(cols.bars) < 40 {
            cols.bars = 0;
        }
        if tl_w(cols.bars) >= 12 {
            cols.tl = tl_w(cols.bars);
        }
    }
    // the tree under each machine: is the next row still one of its children?
    let host_of = |r: &RowId| match r {
        RowId::Sess(k) | RowId::Outside(k) => k.split_once('|').map(|x| x.0.to_string()),
        RowId::Starting(id) => d
            .starting
            .iter()
            .find(|s| s.id == *id)
            .map(|s| s.host.clone()),
        _ => None,
    };
    let last_child = |i: usize| {
        let me = all.get(i).and_then(host_of);
        all.get(i + 1).and_then(host_of) != me
    };
    for (i, row) in all.iter().enumerate() {
        let is_sel = i == sel;
        match row {
            RowId::Host(a) => {
                let Some(hh) = d.fleet.host(a) else { continue };
                if gap && i > 0 {
                    lines.push(Line::raw(""));
                    map.push(None);
                }
                // a tag's heading where its machines begin
                if let Some(t) = tag_of(d, a) {
                    let before = all[..i].iter().rev().find_map(|r| match r {
                        RowId::Host(b) => Some(tag_of(d, b)),
                        _ => None,
                    });
                    if before != Some(Some(t.clone())) {
                        let label = if t.is_empty() {
                            "untagged".to_string()
                        } else {
                            format!("#{t}")
                        };
                        let fill = w.saturating_sub(ui::width(&label) + 3);
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!(" {label} "),
                                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                            ),
                            Span::styled("─".repeat(fill), ui::fg(c::LINE())),
                        ]));
                        map.push(None);
                    }
                }
                for l in host_card(d, hh, w, is_sel) {
                    lines.push(l);
                    map.push(Some(i));
                }
            }
            RowId::Sess(k) => {
                let (ha, _) = k.split_once('|').unwrap_or_default();
                if let Some((hh, s)) = d
                    .fleet
                    .host(ha)
                    .and_then(|hh| hh.sessions.iter().find(|s| s.key() == *k).map(|s| (hh, s)))
                {
                    let g = super::outside::guide(last_child(i));
                    lines.push(sess_line(d, hh, s, w, &cols, is_sel, g));
                    map.push(Some(i));
                }
            }
            RowId::Outside(k) => {
                if let Some(o) = super::outside::find(d, k) {
                    let g = super::outside::guide(last_child(i));
                    lines.push(super::outside::line(d, &o, g, w, (nw, mw, sw), is_sel));
                    map.push(Some(i));
                }
            }
            RowId::Starting(id) => {
                if let Some(st) = d.starting.iter().find(|s| s.id == *id) {
                    let g = format!("  {}  ", super::outside::guide(last_child(i)));
                    lines.push(starting_line(d, st, &g, w, is_sel));
                    map.push(Some(i));
                }
            }
            RowId::Others => {
                lines.push(Line::raw(""));
                map.push(None);
                let n = available(d).len();
                lines.push(Line::from(vec![
                    Span::styled(
                        if d.fv.others_open { "  ▾ " } else { "  ▸ " },
                        ui::fg(c::MUTED()),
                    ),
                    Span::styled(
                        "Not in fleet",
                        if is_sel { ui::bold() } else { ui::muted() },
                    ),
                    Span::styled(
                        format!(
                            "  {n} machine{} in ~/.ssh/config{}",
                            if n == 1 { "" } else { "s" },
                            if d.fv.others_open {
                                ""
                            } else {
                                " · enter shows them"
                            }
                        ),
                        ui::faint(),
                    ),
                ]));
                map.push(Some(i));
            }
            RowId::Avail(a) => {
                lines.push(avail_line(d, a, w, is_sel));
                map.push(Some(i));
            }
        }
    }
    if all.is_empty() && !d.fv.filter.is_empty() {
        lines.push(Line::from(Span::styled(
            format!("nothing matches “{}”", d.fv.filter),
            ui::muted(),
        )));
        map.push(None);
    }
    let pad_v = ui::padding().top + ui::padding().bottom + 2;
    let need = lines.len() as u16 + pad_v + u16::from(cols.tl > 0);

    // layout: list (as tall as it needs) and resources on the left, the
    // selected session or machine on the right
    let (list_r, tele_r, detail_r) = if r.width >= 110 {
        let (left, right) = shape.columns(r, r.width.saturating_sub(own_lw + 1), 1);
        let res_min = 12;
        let want = need.max(6).min(
            left.height
                .saturating_sub(if stats_on { res_min } else { 0 })
                .max(6),
        );
        // the list as tall as it needs, the resources under it — or as set
        let own = if stats_on && left.height.saturating_sub(want) >= 8 {
            want
        } else {
            left.height
        };
        let (l, t) = shape.rows(left, own);
        let t = if stats_on { t } else { Rect { height: 0, ..t } };
        (l, t, right)
    } else if r.width >= 60 {
        let want = need.max(6).min((r.height / 2).max(4));
        let tele_h = if stats_on && r.height > 32 { 12 } else { 0 };
        let [l, t, de] = Layout::vertical([
            Constraint::Length(want),
            Constraint::Length(tele_h),
            Constraint::Min(5),
        ])
        .areas(r);
        (l, t, de)
    } else {
        let want = need.max(5).min((r.height * 3 / 5).max(3));
        let [l, de] = Layout::vertical([Constraint::Length(want), Constraint::Min(4)]).areas(r);
        (l, Rect::default(), de)
    };
    // narrower, everything's stacked: the layout keys still hide or maximise
    let (list_r, tele_r, detail_r) = if r.width >= 110 {
        (list_r, tele_r, detail_r)
    } else {
        use super::panes::Max;
        let none = Rect { height: 0, ..r };
        match shape.max {
            Max::Side => (none, none, r),
            Max::Top => (r, none, none),
            Max::Bottom if tele_r.height > 0 => (none, r, none),
            _ if shape.hidden || shape.max == Max::Main => {
                let th = tele_r.height;
                let lh = r.height.saturating_sub(th);
                (
                    Rect { height: lh, ..r },
                    Rect {
                        y: r.y + lh,
                        height: th,
                        ..r
                    },
                    none,
                )
            }
            _ => (list_r, tele_r, detail_r),
        }
    };

    // the list
    let mut title = vec![Span::raw("Machines & sessions")];
    if d.fv.filtering || !d.fv.filter.is_empty() {
        title.push(Span::styled("  / ", ui::fg(c::ACCENT())));
        title.push(Span::styled(d.fv.filter.clone(), ui::bold()));
        if d.fv.filtering {
            title.push(Span::styled("▏", ui::fg(c::ACCENT())));
        }
    }
    if cols.tl > 0 {
        title.push(Span::styled(
            format!("  · timeline {}h", super::timechart::hours(&d.cfg)),
            ui::faint(),
        ));
    }
    let block = ui::panel(Line::from(title), true);
    let inner = block.inner(list_r);
    f.render_widget(block, list_r);
    // the timeline's hour axis, pinned over the rows
    let inner = if cols.tl > 0 && inner.height > 2 {
        let x = lead_w((nw, mw, sw)) + cols.bars.max(2);
        f.render_widget(
            Paragraph::new(super::timechart::fleet_axis(d, x, cols.tl)),
            Rect { height: 1, ..inner },
        );
        Rect {
            y: inner.y + 1,
            height: inner.height - 1,
            ..inner
        }
    } else {
        inner
    };
    let h = inner.height as usize;
    // keep every line of the selected row in view
    let first = map.iter().position(|x| *x == Some(sel)).unwrap_or(0);
    let last = map.iter().rposition(|x| *x == Some(sel)).unwrap_or(first);
    if first < d.fv.scroll {
        d.fv.scroll = first.saturating_sub(usize::from(gap));
    } else if h > 0 && last >= d.fv.scroll + h {
        d.fv.scroll = last + 1 - h;
    }
    d.fv.scroll = d.fv.scroll.min(lines.len().saturating_sub(h.max(1)));
    let shown: Vec<Line> = lines.into_iter().skip(d.fv.scroll).take(h).collect();
    f.render_widget(Paragraph::new(shown), inner);
    // the accent bar, one column left of the row (in the panel's padding)
    for (j, m) in map.iter().enumerate().skip(d.fv.scroll).take(h) {
        if *m == Some(sel) {
            let y = inner.y + (j - d.fv.scroll) as u16;
            if let Some(cell) = f.buffer_mut().cell_mut((inner.x.saturating_sub(1), y)) {
                cell.set_symbol("┃").set_fg(c::ACCENT());
            }
        }
    }
    d.hits.list = inner;
    d.hits.list_first = inner.y;
    d.hits.list_offset = d.fv.scroll;
    d.hits.list_len = map.len();
    d.fv.linemap = map;

    // resources of the selected machine (v: gpu · processes)
    if tele_r.height > 0 {
        if let Some(hh) = sel_host.as_deref().and_then(|a| d.fleet.host(a)) {
            match (&stats, d.fv.stats_mode) {
                (Some(st), m) if m > 0 => {
                    super::monitor_draw::compact(f, tele_r, &hh.alias, hh.color, st, m, d.t0)
                }
                _ => {
                    super::machine::resources(f, tele_r, &hh.alias, hh.color, stats.as_ref(), d.t0)
                }
            }
        }
    }

    // details: the session, or the whole machine
    if detail_r.width == 0 || detail_r.height == 0 {
        return;
    }
    match selected(d) {
        Some(RowId::Sess(k)) => {
            if let Some(s) = session(d, &k) {
                draw_session(d, f, detail_r, &s);
                // its timeline, in the room the recent output had
                if let Some(r) = d.fv.timeline_area.take() {
                    super::tree_view::show(
                        d,
                        &s.host,
                        &s.agent,
                        s.native_id.as_deref().unwrap_or(""),
                        s.status == "working",
                    );
                    super::tree_view::draw(d, f, r, false);
                }
            }
        }
        Some(RowId::Host(a)) => {
            let b = super::machine::view(d, f, detail_r, &a);
            d.hits.buttons.extend(b);
        }
        Some(RowId::Avail(a)) => draw_avail(d, f, detail_r, &a),
        Some(RowId::Starting(id)) => draw_starting(d, f, detail_r, id),
        Some(RowId::Outside(k)) => {
            if let Some(o) = super::outside::find(d, &k) {
                let block = ui::panel(
                    Line::from(vec![
                        Span::styled(" ", ui::faint()),
                        Span::styled(o.host.clone(), ui::bold()),
                        Span::styled(" · outside herdr ", ui::faint()),
                    ]),
                    false,
                );
                let inner = block.inner(detail_r);
                f.render_widget(block, detail_r);
                let lines = super::outside::info(d, &o, inner.width as usize);
                f.render_widget(Paragraph::new(lines), inner.inner(Margin::new(1, 1)));
            }
        }
        Some(RowId::Others) | None => {}
    }
}

/// A machine outside the fleet, one line: alias, where it points, login state.
/// A session on its way: a spinner and what it's doing, or why it didn't start.
pub(crate) fn starting_line(
    d: &Dash,
    st: &super::quick::Starting,
    guide: &str,
    w: usize,
    sel: bool,
) -> Line<'static> {
    use super::quick::StartState;
    let (mark, col, state) = match &st.state {
        StartState::Failed(e) => ("✗".to_string(), c::RED(), format!("couldn't start: {e}")),
        StartState::Listing(..) => (
            ui::spinner(d.t0).to_string(),
            c::ACCENT(),
            "almost up…".to_string(),
        ),
        StartState::Running => (
            ui::spinner(d.t0).to_string(),
            c::ACCENT(),
            if st.step.is_empty() {
                "starting…".to_string()
            } else {
                st.step.clone()
            },
        ),
    };
    let mut v = vec![
        Span::styled(guide.to_string(), ui::fg(c::LINE())),
        Span::styled(format!("{mark} "), ui::fg(col)),
        crate::icons::agent(&st.opts.agent_id()),
        Span::raw(" "),
        Span::styled(st.what(), ui::bold()),
        Span::styled(format!("  {state}"), ui::fg(col)),
    ];
    let folder = st.folder(d);
    if !folder.is_empty() {
        v.push(Span::styled(format!("  {folder}"), ui::faint()));
    }
    let line = fit_line(&Line::from(v), w)
        .into_iter()
        .next()
        .unwrap_or_default();
    if sel {
        line.style(ui::selected())
    } else {
        line
    }
}

/// The right panel for a session on its way.
fn draw_starting(d: &Dash, f: &mut Frame, r: Rect, id: u64) {
    use super::quick::StartState;
    let Some(st) = d.starting.iter().find(|s| s.id == id) else {
        return;
    };
    let block = ui::panel(
        Line::from(vec![
            Span::styled(st.what(), ui::bold()),
            Span::styled(format!(" on {}", st.host), ui::faint()),
        ]),
        false,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    let folder = st.folder(d);
    let mut lines = vec![Line::from(vec![
        Span::styled("in  ", ui::faint()),
        Span::raw(if folder.is_empty() {
            "where herdr opens a new tab".to_string()
        } else {
            folder
        }),
    ])];
    lines.push(Line::raw(""));
    let w = inner.width.saturating_sub(2).max(10) as usize;
    match &st.state {
        StartState::Failed(e) => {
            lines.push(Line::from(Span::styled(
                "✗ It didn't start",
                ui::fg(c::RED()).add_modifier(Modifier::BOLD),
            )));
            for l in ui::wrap(e, w) {
                lines.push(Line::from(Span::styled(l, ui::muted())));
            }
            lines.push(Line::raw(""));
            lines.push(ui::hints(&[("r", "try again"), ("x", "drop it")]));
        }
        _ => {
            let step = match &st.state {
                StartState::Listing(..) => "herdr has it, waiting to see it listed".to_string(),
                _ if st.step.is_empty() => "starting…".to_string(),
                _ => st.step.clone(),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{} ", ui::spinner(d.t0)), ui::fg(c::ACCENT())),
                Span::raw(step),
                Span::styled(format!("  {}s", st.t0.elapsed().as_secs()), ui::faint()),
            ]));
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Enter opens it once it's up.",
                ui::muted(),
            )));
            lines.push(ui::hints(&[("x", "cancel")]));
        }
    }
    f.render_widget(Paragraph::new(lines), inner.inner(Margin::new(1, 1)));
}

pub(crate) fn avail_line(d: &Dash, alias: &str, w: usize, sel: bool) -> Line<'static> {
    let row = d.hv.rows.iter().find(|r| r.alias == alias);
    let target = row
        .and_then(|r| r.res.as_ref())
        .map(|r| r.short())
        .unwrap_or_else(|| "…".into());
    let (st, col) = match row {
        Some(r) if r.testing.is_some() => ("testing…".to_string(), c::MUTED()),
        Some(r) => match &r.test {
            Some(p) if p.ok() => ("✓ key login works".to_string(), c::GREEN()),
            Some(p) => (format!("✕ {}", p.explain()), c::RED()),
            None => (String::new(), c::MUTED()),
        },
        None => (String::new(), c::MUTED()),
    };
    let name = ui::pad(alias, 14);
    let mut l = vec![
        Span::styled("    ○ ", ui::fg(c::FAINT())),
        Span::styled(name, if sel { ui::bold() } else { Style::new() }),
        Span::styled(format!(" {}", ui::trunc(&target, 34)), ui::muted()),
    ];
    if !st.is_empty() {
        l.push(Span::styled(format!("   {st}"), ui::fg(col)));
    }
    fit_line(&Line::from(l), w)
        .into_iter()
        .next()
        .unwrap_or_default()
}

/// Details for a machine outside the fleet.
fn draw_avail(d: &Dash, f: &mut Frame, r: Rect, alias: &str) {
    let row = d.hv.rows.iter().find(|x| x.alias == alias);
    let mut lines: Vec<Line> = Vec::new();
    let kv = |k: &str, v: String| {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), ui::muted()),
            Span::raw(v),
        ])
    };
    if let Some(res) = row.and_then(|x| x.res.as_ref()) {
        lines.push(kv("connects", res.short()));
        if !res.identities.is_empty() {
            lines.push(kv("key", res.identities.join(", ")));
        }
        if let Some(p) = res.proxy.as_ref().filter(|p| !p.is_empty() && *p != "none") {
            lines.push(kv("through", p.clone()));
        }
    } else {
        lines.push(Line::from(Span::styled(
            "reading ~/.ssh/config…",
            ui::muted(),
        )));
    }
    if let Some(p) = row.and_then(|x| x.test.as_ref()) {
        lines.push(kv(
            "login",
            if p.ok() {
                "key login works".into()
            } else {
                p.explain()
            },
        ));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "Not in the fleet: Aleph doesn't poll it or show its sessions.",
        ui::muted(),
    )));
    lines.push(Line::raw(""));
    lines.push(ui::hints(&[
        ("enter", "add to the fleet"),
        ("T", "test login"),
        ("l", "key setup"),
        ("e", "edit"),
    ]));
    let block = ui::panel(
        Line::from(vec![Span::raw(format!(
            " ○ {} ",
            crate::showcase::shown(alias)
        ))]),
        false,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    f.render_widget(
        Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }),
        inner,
    );
}

fn chip(text: &str, col: Color) -> Span<'static> {
    if c::mono() {
        return Span::styled(
            format!(" {text} "),
            Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
        );
    }
    Span::styled(
        format!(" {text} "),
        Style::new()
            .fg(c::BASE())
            .bg(col)
            .add_modifier(Modifier::BOLD),
    )
}

fn kv(k: &str, v: Vec<Span<'static>>) -> Line<'static> {
    let mut l = vec![Span::styled(ui::pad(k, 9), ui::faint())];
    l.extend(v);
    Line::from(l)
}

fn draw_session(d: &mut Dash, f: &mut Frame, r: Rect, s: &Session) {
    let timeline_room = std::cell::Cell::new(None);
    draw_session_in(d, f, r, s, &timeline_room);
    d.fv.timeline_area = timeline_room.get();
}

fn draw_session_in(
    d: &Dash,
    f: &mut Frame,
    r: Rect,
    s: &Session,
    timeline_room: &std::cell::Cell<Option<Rect>>,
) {
    let host = d.fleet.host(&s.host);
    let hcol = host.map(|h| h.color).unwrap_or(c::MUTED());
    let block = ui::panel(
        Line::from(vec![
            chip(&format!("{} {}", s.glyph(), s.display()), s.color()),
            Span::raw(" "),
            chip(&crate::showcase::shown(&s.host), hcol),
        ]),
        false,
    )
    .title_bottom(
        Line::from(vec![
            ui::keycap("enter"),
            Span::styled(" focus  ", ui::muted()),
            ui::keycap("a"),
            Span::styled(" attach  ", ui::muted()),
            ui::keycap("O"),
            Span::styled(" new tab  ", ui::muted()),
            ui::keycap(&d.keys.key_for(Act::Close)),
            Span::styled(" close ", ui::muted()),
        ])
        .right_aligned(),
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let (st, col) = status_for(d, s);
    let mut lines: Vec<Line> = Vec::new();
    if !s.title.is_empty() {
        for l in ui::wrap(&s.title, w).into_iter().take(2) {
            lines.push(Line::from(Span::styled(l, ui::bold())));
        }
    }
    lines.push(kv(
        "status",
        vec![
            Span::styled(st, ui::fg(col).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("   {}", crate::agents::label(&s.agent)),
                ui::muted(),
            ),
        ],
    ));
    let folder = host
        .map(|h| h.short_path(&s.cwd))
        .unwrap_or_else(|| s.cwd.clone());
    lines.push(kv(
        "folder",
        vec![Span::raw(ui::trunc_left(&folder, w.saturating_sub(8)))],
    ));
    lines.push(kv(
        "tab",
        vec![
            Span::raw(ui::trunc(&s.label, w.saturating_sub(24))),
            Span::styled(format!("  · pane {}", s.pane_id), ui::faint()),
        ],
    ));
    // the model it's on (its latest request), and the ones it was on before
    if let Some(l) = super::usage::live_for(d, s).filter(|l| !l.model.is_empty()) {
        let mut v = vec![Span::styled(
            crate::timeline::short_model(&l.model),
            ui::bold(),
        )];
        if !l.effort.is_empty() {
            v.push(Span::styled(format!(" {}", l.effort), ui::muted()));
        }
        let before: Vec<String> = l
            .models
            .iter()
            .filter(|m| **m != l.model)
            .map(|m| crate::timeline::short_model(m))
            .collect();
        if !before.is_empty() {
            v.push(Span::styled(
                ui::trunc(
                    &format!("  · before: {}", before.join(", ")),
                    w.saturating_sub(24),
                ),
                ui::faint(),
            ));
        }
        lines.push(kv("model", v));
    }
    // what it runs on the GPU (telemetry::owner: processes in its folder)
    {
        let mine = d.gpu_of(s);
        if let Some(p) = mine.first() {
            let more = if mine.len() > 1 {
                format!("  +{} more", mine.len() - 1)
            } else {
                String::new()
            };
            lines.push(kv(
                "gpu",
                vec![
                    Span::styled(ui::trunc(&p.name, 20), ui::bold()),
                    Span::styled(
                        format!(" · {} on GPU {}{more}", ui::bytes(p.mem), p.gpu),
                        ui::fg(c::GREEN()),
                    ),
                ],
            ));
        }
    }
    // what its last finished turn took (turncost.rs)
    if let Some((t, turn)) = super::inbox::last_turn(d, s) {
        let mut v = super::inbox::turn_spans(&turn);
        v.push(Span::styled(
            format!("  {}", ui::ago(crate::cache::now().saturating_sub(t))),
            ui::faint(),
        ));
        lines.push(kv("turn", v));
    }
    // its states over the last hours, newest at the right
    if w >= 9 + 12 && super::timechart::has_any(d, s) {
        lines.push(kv(
            &format!("last {}h", super::timechart::hours(&d.cfg)),
            super::timechart::session_bar(d, s, w - 9),
        ));
    }
    if let Some(n) = crate::notes::get(&s.host, &s.name) {
        lines.push(kv(
            "note",
            vec![Span::styled(
                ui::trunc(&format!("✎ {}", n.text), w.saturating_sub(8)),
                ui::fg(c::ACCENT()),
            )],
        ));
    }
    if host.is_some_and(|h| h.stale && h.conn != Conn::Up) {
        lines.push(Line::from(Span::styled(
            "cached — the machine isn't answering right now",
            ui::fg(c::YELLOW()),
        )));
    }
    let top = lines.len() as u16;
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            height: top.min(inner.height),
            ..inner
        },
    );
    let mut rest = Rect {
        y: inner.y + top + 1,
        height: inner.height.saturating_sub(top + 1),
        ..inner
    };
    let pv = d.fv.previews.get(&s.key());
    // Claude is asking
    if s.blocked() && rest.height > 6 {
        let text = pv.and_then(|p| p.detect.clone()).unwrap_or_default();
        let body: Vec<Line> = if text.trim().is_empty() {
            vec![Line::from(Span::styled(
                format!("{} reading the question…", ui::spinner(d.t0)),
                ui::muted(),
            ))]
        } else {
            text.lines()
                .map(str::trim_end)
                .filter(|l| !l.trim().is_empty())
                .take(8)
                .map(|l| {
                    let st = if l.trim_start().starts_with('❯') {
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::new()
                    };
                    Line::from(Span::styled(ui::trunc(l, w.saturating_sub(4)), st))
                })
                .collect()
        };
        let bh = (body.len() as u16 + 3).min(rest.height.saturating_sub(2));
        let br = Rect { height: bh, ..rest };
        let bb = Block::bordered()
            .border_set(crate::ui::border_set(false))
            .border_style(ui::fg(c::RED()))
            .title(Span::styled(
                format!(" ▲ {} is asking ", agent_title(&s.agent)),
                ui::fg(c::RED()).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(
                Line::from(vec![
                    ui::keycap("y"),
                    Span::styled(" approve  ", ui::muted()),
                    ui::keycap("K"),
                    Span::styled(" answer  ", ui::muted()),
                    ui::keycap("s"),
                    Span::styled(" reply ", ui::muted()),
                ])
                .right_aligned(),
            );
        let bi = bb.inner(br);
        f.render_widget(bb, br);
        f.render_widget(Paragraph::new(body), bi);
        ui::agent_screen(bi);
        // showcase: the question as the screen shows it (`showcase.screens`)
        crate::showcase::screen_area(f.buffer_mut(), bi);
        rest = Rect {
            y: rest.y + bh + 1,
            height: rest.height.saturating_sub(bh + 1),
            ..rest
        };
    }
    if rest.height < 2 {
        return;
    }
    let when = pv
        .map(|p| format!(" · {}", ui::ago(p.at.elapsed().as_secs())))
        .unwrap_or_default();
    let rule = if d.fv.timeline_panel {
        "── timeline · G recent output ".to_string()
    } else {
        format!("── recent output{when} · G timeline ")
    };
    let fill = w.saturating_sub(ui::width(&rule));
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(rule, ui::faint()),
            Span::styled("─".repeat(fill), ui::fg(c::LINE())),
        ])),
        Rect { height: 1, ..rest },
    );
    let out = Rect {
        y: rest.y + 1,
        height: rest.height - 1,
        ..rest
    };
    // the timeline instead: drawn by the caller, which may change the dashboard
    if d.fv.timeline_panel {
        timeline_room.set(Some(out));
        return;
    }
    let lines: Vec<Line> = match pv.map(|p| &p.lines) {
        None => vec![Line::from(vec![
            Span::styled(format!("{} ", ui::spinner(d.t0)), ui::fg(c::ACCENT())),
            Span::styled("reading the session…", ui::muted()),
        ])],
        Some(Err(e)) => vec![Line::from(Span::styled(
            format!(
                "preview unavailable: {}",
                ui::trunc(e, w.saturating_sub(22))
            ),
            ui::muted(),
        ))],
        Some(Ok(v)) => {
            let end = v
                .iter()
                .rposition(|l| !ansi::plain(l).trim().is_empty())
                .map_or(0, |i| i + 1);
            if end == 0 {
                vec![Line::from(Span::styled("(empty screen)", ui::faint()))]
            } else {
                // wrap to the panel, newest at the bottom, like a terminal —
                // a full-screen app's screen (OpenCode's) is fitted instead
                let h = out.height as usize;
                let screen = screen_like(&v[..end]);
                let mut shown: Vec<Line> = Vec::new();
                for l in v[..end].iter().rev() {
                    let mut parts = if screen {
                        vec![fit_screen_line(l, out.width as usize)]
                    } else {
                        fit_line(l, out.width as usize)
                    };
                    while let Some(p) = parts.pop() {
                        shown.push(p);
                    }
                    if shown.len() >= h {
                        break;
                    }
                }
                shown.truncate(h);
                shown.reverse();
                let pad = h.saturating_sub(shown.len());
                let mut all = vec![Line::raw(""); pad];
                all.extend(shown);
                all
            }
        }
    };
    f.render_widget(Paragraph::new(lines), out);
    ui::agent_screen(out);
    // showcase: the preview redacted or hidden (`showcase.screens`)
    if pv.is_some_and(|p| p.lines.is_ok()) {
        crate::showcase::screen_area(f.buffer_mut(), out);
    }
}

/// A line of agent output for a `w`-wide panel: tables and rules (box
/// drawing) are cut, not wrapped — wrapping would shred them; prose wraps,
/// without the trailing padding that carries a highlight's background.
pub(crate) fn fit_line(l: &Line<'static>, w: usize) -> Vec<Line<'static>> {
    let mut l = l.clone();
    while let Some(last) = l.spans.last_mut() {
        let t = last.content.trim_end().to_string();
        if t.is_empty() {
            l.spans.pop();
        } else {
            last.content = t.into();
            break;
        }
    }
    let boxy = l.spans.iter().any(|s| {
        s.content
            .chars()
            .any(|c| ('\u{2500}'..='\u{257f}').contains(&c))
    });
    if boxy {
        let mut out: Vec<Span> = Vec::new();
        let mut left = w;
        for sp in &l.spans {
            if left == 0 {
                break;
            }
            let t = ui::trunc(&sp.content, left);
            left = left.saturating_sub(ui::width(&t));
            out.push(Span::styled(t, sp.style));
        }
        return vec![Line::from(out).style(l.style)];
    }
    wrap_line(&l, w)
}

/// Is this a full-screen app's screen (a layout: centred blocks, columns
/// apart) rather than a stream of output? Wrapping one shreds it.
pub(crate) fn screen_like(lines: &[Line<'static>]) -> bool {
    let laid_out = |t: &str| {
        let lead = t.len() - t.trim_start().len();
        let inner = t.trim();
        lead >= 12 || inner.contains("        ")
    };
    let texts: Vec<String> = lines
        .iter()
        .map(ansi::plain)
        .filter(|t| !t.trim().is_empty())
        .collect();
    let n = texts.iter().filter(|t| laid_out(t)).count();
    n >= 3 && n * 4 >= texts.len()
}

/// One line of a full-screen app's screen in a `w`-wide panel, never
/// wrapped: indentation cut so what fits stays where it was relative to
/// its neighbours (a centred block stays one block), wide gaps between
/// columns closed, the rest cut at the edge.
pub(crate) fn fit_screen_line(l: &Line<'static>, w: usize) -> Line<'static> {
    // the line as (char, style) cells
    let mut cells: Vec<(char, Style)> = l
        .spans
        .iter()
        .flat_map(|sp| sp.content.chars().map(move |c| (c, sp.style)))
        .collect();
    while cells.last().is_some_and(|c| c.0 == ' ') {
        cells.pop();
    }
    let width =
        |cs: &[(char, Style)]| -> usize { cs.iter().map(|c| ui::width(&c.0.to_string())).sum() };
    if width(&cells) > w {
        let lead = cells.iter().take_while(|c| c.0 == ' ').count();
        let content = width(&cells[lead..]);
        let keep = if content <= w { (w - content) / 2 } else { 0 };
        cells.drain(..lead.saturating_sub(keep));
    }
    if width(&cells) > w {
        // close the gaps between columns: runs of 4+ spaces become 2
        let mut out: Vec<(char, Style)> = Vec::with_capacity(cells.len());
        let mut run = 0;
        for c in cells {
            run = if c.0 == ' ' { run + 1 } else { 0 };
            if run <= 2 || c.0 != ' ' {
                out.push(c);
            }
        }
        cells = out;
    }
    // back to spans, cut at the edge
    let mut spans: Vec<Span<'static>> = Vec::new();
    let (mut buf, mut cur, mut used) = (String::new(), None::<Style>, 0);
    for (ch, st) in cells {
        let cw = ui::width(&ch.to_string());
        if used + cw > w {
            break;
        }
        used += cw;
        if cur != Some(st) {
            if let Some(s0) = cur {
                spans.push(Span::styled(std::mem::take(&mut buf), s0));
            }
            cur = Some(st);
        }
        buf.push(ch);
    }
    if let Some(s0) = cur {
        spans.push(Span::styled(buf, s0));
    }
    Line::from(spans).style(l.style)
}

/// Hard-wrap a styled line at `w` columns (terminal output: no word breaks).
pub(crate) fn wrap_line(l: &Line<'static>, w: usize) -> Vec<Line<'static>> {
    let w = w.max(1);
    let mut out: Vec<Line> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut cw = 0;
    for sp in &l.spans {
        let mut buf = String::new();
        for ch in sp.content.chars() {
            let chw = ui::width(&ch.to_string());
            if cw + chw > w && cw > 0 {
                if !buf.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut buf), sp.style));
                }
                out.push(Line::from(std::mem::take(&mut cur)).style(l.style));
                cw = 0;
            }
            buf.push(ch);
            cw += chw;
        }
        if !buf.is_empty() {
            cur.push(Span::styled(buf, sp.style));
        }
    }
    out.push(Line::from(cur).style(l.style));
    out
}

fn agent_title(a: &str) -> String {
    crate::agents::label(a)
}

/// One-line advice for a herdr/ssh error.
pub(crate) fn hint_for(e: &str) -> String {
    let l = e.to_lowercase();
    if l.contains("unknown_machine")
        || l.contains("unknown machine")
        || l.contains("no saved machine")
    {
        "save it in herdr: M on the machine".into()
    } else if l.contains("permission denied") {
        "key login isn't set up: l on the machine".into()
    } else if l.contains("resolve") {
        "the hostname doesn't resolve — VPN? typo in ~/.ssh/config?".into()
    } else if l.contains("unavailable")
        || l.contains("no herdr server")
        || l.contains("not running")
    {
        "herdr isn't running there — S starts it (and brings its sessions back)".into()
    } else if l.contains("timed out") || l.contains("timeout") {
        "the machine is slow or unreachable — it retries every refresh".into()
    } else {
        "the Doctor tab (8) explains what's wrong".into()
    }
}
