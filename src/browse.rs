//! Remote folder browser (`aleph -i`, and "new in folder…" from the dashboard).
//!
//! Left: folders (project tags / symlink / no-access / "an agent runs here"
//! badges), ★ recent folders on top, and — while searching — matches from a
//! deep index a few levels down. Right: a live preview of the highlighted
//! folder — git branch + last commit, project type, counts, the agent sessions
//! in it, and its sub-folders — fetched lazily and cached.
//!
//! Two ssh connections, both closed before the folder is handed back:
//! * the main one answers what the user is looking at (listings, stats, the
//!   hovered folder's facts);
//! * a background one builds the deep index and prefetches facts (tags) for
//!   visible rows, so slow `find`s never hold up the interactive one.
//!
//! A query starting with `/` or `~/` is a path: the browser follows it to the
//! last `/` as you type and filters by the rest; Tab completes the top match.

use crate::cache::{self, Cache, Deep, Dir, Entry};
use crate::config::Target;
use crate::herdr::{self, Session};
use crate::remote::{Ev, Info, Op, Remote};
use crate::term::Tui;
use crate::ui::{self, c};
use crate::{fuzzy, ignore, path};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::prelude::*;
use ratatui::widgets::{
    List, ListItem, ListState, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

const HOVER_DELAY: Duration = Duration::from_millis(140);
/// Rest this long in a folder before its deep index is built (typing starts it at once).
const DEEP_DELAY: Duration = Duration::from_millis(400);
/// A failed deep index is retried after this long.
const DEEP_RETRY: Duration = Duration::from_secs(60);
/// Background Info requests in flight at once (project tags for visible rows).
const PREFETCH: usize = 3;
const MAX_RECENT: usize = 5;
const MAX_DEEP_ROWS: usize = 300;
/// Search boost for the best recent folder (less for later ones).
const RECENT_BOOST: i32 = 20;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// How long `i` waits for the session list before deciding "new vs attach".
const SETTLE: Duration = Duration::from_secs(3);
/// The folder on screen is listed again this often (folders made elsewhere show up).
const AUTO_REFRESH: Duration = Duration::from_secs(5);

enum Conn {
    Connecting,
    Online,
    Offline { reason: String, retry_at: Instant },
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    /// ★ a recent folder (absolute path in `name`)
    Recent,
    Here,
    Up,
    Dir,
    /// a deep-index match (path relative to cwd in `name`)
    Deep,
}

#[derive(Clone, Debug)]
struct Row {
    kind: Kind,
    name: Vec<u8>,
    link: bool,
    denied: bool,
    git: bool,
    /// matched char positions in the displayed name (fuzzy search)
    hits: Vec<usize>,
}

type Rows = (Rc<Vec<Row>>, usize, usize);
type MemoKey = (u64, String, bool, bool);

impl Row {
    fn special(kind: Kind) -> Row {
        Row {
            kind,
            name: Vec::new(),
            link: false,
            denied: false,
            git: false,
            hits: Vec::new(),
        }
    }
}

/// A query that starts with `/` or `~/` (or is just `~`) is a path: returns
/// the folder up to its last `/` (with `.`, `..` and `//` resolved) and the
/// filter text after it. `~` needs the remote home.
fn parse_jump(q: &str, home: Option<&[u8]>) -> Option<(Vec<u8>, String)> {
    let abs: Vec<u8> = if q == "~" || q.starts_with("~/") {
        let mut h = path::normalize(home?);
        if h != b"/" {
            h.push(b'/');
        }
        h.extend_from_slice(q.strip_prefix("~/").unwrap_or("").as_bytes());
        h
    } else if q.starts_with('/') {
        q.as_bytes().to_vec()
    } else {
        return None;
    };
    let cut = abs.iter().rposition(|&b| b == b'/')?;
    let rest = String::from_utf8_lossy(&abs[cut + 1..]).into_owned();
    let mut parts: Vec<&[u8]> = Vec::new();
    for c in abs[..cut].split(|&b| b == b'/') {
        match c {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut dir = Vec::new();
    for p in parts {
        dir.push(b'/');
        dir.extend_from_slice(p);
    }
    if dir.is_empty() {
        dir.push(b'/');
    }
    Some((dir, rest))
}

/// Tab in a path query: replace the text after the last `/` with `name/`.
fn complete(q: &str, name: &str) -> String {
    let head = match q.rfind('/') {
        Some(i) => &q[..=i],
        None if q == "~" => "~/",
        None => "",
    };
    let head = if head == "~" { "~/" } else { head };
    format!("{head}{name}/")
}

/// Project tags from a folder's marker files (and git), shortest first.
fn tags(markers: &[String], git: bool) -> Vec<(&'static str, Color)> {
    let has = |m: &str| markers.iter().any(|x| x == m);
    let mut v = Vec::new();
    // agent instruction files: Claude Code's, and the shared AGENTS.md
    if has("CLAUDE.md") {
        v.push(("claude", c::ACCENT()));
    }
    if has("AGENTS.md") {
        v.push(("agents", c::ACCENT()));
    }
    if has("Cargo.toml") {
        v.push(("rs", c::PEACH()));
    }
    if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        v.push(("py", c::YELLOW()));
    }
    if has("package.json") {
        v.push(("node", c::GREEN()));
    }
    if has("go.mod") {
        v.push(("go", c::SKY()));
    }
    if has("flake.nix") {
        v.push(("nix", c::BLUE()));
    }
    if has("Dockerfile") || has("docker-compose.yml") {
        v.push(("docker", c::BLUE()));
    }
    if git {
        v.push(("git", c::GREEN()));
    }
    v
}

pub struct App {
    host: String,
    /// ssh binary for the background connection ("" = none, e.g. in tests)
    ssh: String,
    color: Color,
    /// what enter starts (`Claude Code`, `Codex`, …)
    agent: String,
    /// Quick start asked for a folder: `i` hands one back, nothing starts here
    choosing: bool,
    /// `t` was the key that picked: a terminal, not an agent
    want_terminal: bool,
    /// a terminal is what this browse is for (ctrl+t): `i` opens one
    terminal_only: bool,
    ttl: u64,
    pub cache: Cache,
    rules: ignore::Rules,
    globs: Vec<Vec<u8>>,
    deep_levels: u64,
    recent: Vec<Vec<u8>>,
    start: Option<Vec<u8>>,
    /// the first folder shown (★ recent rows appear there)
    first: Option<Vec<u8>>,
    cwd: Option<Vec<u8>>,
    prev: Option<Vec<u8>>,
    arrived: Instant,
    dirs: Option<Vec<Dir>>,
    fetched: Option<u64>,
    loading: bool,
    validating: bool,
    err: Option<String>,
    sel: usize,
    page: usize,
    /// the search query (fuzzy, or a path jump): `/` starts one
    query: String,
    /// typing goes to the query (else letters are commands: i n r j k …)
    searching: bool,
    /// a new folder's name being typed (`n`)
    naming: Option<String>,
    /// a folder being made: (request, its path)
    mkdir_req: Option<(u64, Vec<u8>)>,
    /// when the folder on screen was last listed again by itself
    auto_at: Instant,
    gen: u64,
    memo: RefCell<Option<(MemoKey, Rows)>>,
    show_hidden: bool,
    show_ignored: bool,
    conn: Conn,
    since: Instant,
    msg: Option<(String, Instant, bool)>,
    list_id: Option<u64>,
    stat_id: Option<u64>,
    choose: Option<(u64, Vec<u8>)>,
    /// picked + verified (target, realpath, when): waiting for the session list
    settle: Option<(Vec<u8>, Vec<u8>, Instant)>,
    /// "already running here" prompt: (target, sessions there)
    ask: Option<(Vec<u8>, Vec<Session>)>,
    reselect: Option<Vec<u8>>,
    // path jump
    jump_req: Option<(u64, Vec<u8>)>,
    jump_err: Option<String>,
    // preview
    hover: Option<(Vec<u8>, Instant)>,
    infos: HashMap<Vec<u8>, (Info, Instant)>,
    info_err: HashMap<Vec<u8>, String>,
    info_req: Option<(u64, Vec<u8>)>,
    peek_req: Option<(u64, Vec<u8>)>,
    sessions: Vec<Session>,
    sessions_rx: Option<Receiver<Vec<Session>>>,
    // background connection: deep index + tag prefetch
    bg: Option<Remote>,
    bg_online: bool,
    deep_req: Option<(u64, Vec<u8>)>,
    deep_fail: HashMap<Vec<u8>, Instant>,
    pre_req: HashMap<u64, Vec<u8>>,
    // list geometry (for mouse + prefetch)
    list_inner: Rect,
    list_off: usize,
    last_click: Option<(Instant, usize)>,
    pub done: Option<Option<Picked>>,
}

impl App {
    fn new(t: &Target, o: &BrowseOpts, cache: Cache) -> App {
        // sessions for "an agent runs here" badges — fetched in the background
        // unless the caller already knows them
        let (sessions, sessions_rx) = match &o.sessions {
            Some(s) => (s.clone(), None),
            None => {
                let (tx, rx) = mpsc::channel();
                let tt = t.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(herdr::Herdr::new(&tt).sessions().unwrap_or_default());
                });
                (Vec::new(), Some(rx))
            }
        };
        let rules = ignore::Rules::parse(&o.ignore_edits);
        let mut recent: Vec<Vec<u8>> = Vec::new();
        for r in &o.recent {
            let p = path::normalize(r.as_bytes());
            if p.starts_with(b"/") && !recent.contains(&p) {
                recent.push(p);
            }
        }
        App {
            host: t.ssh.clone(),
            ssh: String::new(),
            color: o.color,
            agent: if o.agent.is_empty() {
                "the agent".into()
            } else {
                o.agent.clone()
            },
            choosing: o.choosing,
            want_terminal: false,
            terminal_only: o.terminal,
            ttl: o.ttl,
            cache,
            globs: rules.prune_globs(),
            rules,
            deep_levels: o.deep,
            recent,
            start: o.start.clone().map(|s| path::normalize(&s)),
            first: None,
            cwd: None,
            prev: None,
            arrived: Instant::now(),
            dirs: None,
            fetched: None,
            loading: false,
            validating: false,
            err: None,
            sel: 0,
            page: 10,
            query: String::new(),
            searching: false,
            naming: None,
            mkdir_req: None,
            auto_at: Instant::now(),
            gen: 0,
            memo: RefCell::new(None),
            show_hidden: o.show_hidden,
            show_ignored: o.show_ignored,
            conn: Conn::Connecting,
            since: Instant::now(),
            msg: None,
            list_id: None,
            stat_id: None,
            choose: None,
            settle: None,
            ask: None,
            reselect: None,
            jump_req: None,
            jump_err: None,
            hover: None,
            infos: HashMap::new(),
            info_err: HashMap::new(),
            info_req: None,
            peek_req: None,
            sessions,
            sessions_rx,
            bg: None,
            bg_online: false,
            deep_req: None,
            deep_fail: HashMap::new(),
            pre_req: HashMap::new(),
            list_inner: Rect::default(),
            list_off: 0,
            last_click: None,
            done: None,
        }
    }

    /// Start immediately from cache when possible; otherwise wait for the handshake ($HOME).
    fn boot(&mut self, r: &mut Remote) {
        if let Some(s) = self.start.clone().or_else(|| self.cache.home.clone()) {
            self.goto(r, s, None);
        }
    }

    fn note(&mut self, m: impl Into<String>) {
        self.msg = Some((m.into(), Instant::now(), false));
    }
    fn error(&mut self, m: impl Into<String>) {
        self.msg = Some((m.into(), Instant::now(), true));
    }

    // ---------------------------------------------------------------- navigation

    fn goto(&mut self, r: &mut Remote, p: Vec<u8>, reselect: Option<Vec<u8>>) {
        self.go(r, p, reselect, false)
    }

    /// Open folder `p`. `keep_query` is for path jumps (the query is the path).
    fn go(&mut self, r: &mut Remote, p: Vec<u8>, reselect: Option<Vec<u8>>, keep_query: bool) {
        let p = path::normalize(&p);
        self.prev = self.cwd.replace(p.clone());
        if self.first.is_none() {
            self.first = Some(p.clone());
        }
        if !keep_query {
            self.query.clear();
            self.jump_req = None;
            self.jump_err = None;
        }
        self.arrived = Instant::now();
        self.sel = 0;
        self.list_off = 0;
        self.err = None;
        self.reselect = reselect;
        self.list_id = None;
        self.stat_id = None;
        if let Some((dirs, fetched)) = self.cache.get(&p).map(|e| (e.dirs.clone(), e.fetched)) {
            // show the cached listing instantly, but verify the folder still
            // exists (and hasn't changed) before trusting it
            self.set_dirs(Some(dirs));
            self.fetched = Some(fetched);
            self.loading = false;
            self.validating = true;
            self.stat_id = Some(r.send(Op::Stat, &p));
        } else {
            self.set_dirs(None);
            self.fetched = None;
            self.loading = true;
            self.validating = false;
            self.list_id = Some(r.send(Op::List, &p));
        }
        self.apply_reselect();
    }

    fn up(&mut self, r: &mut Remote) {
        let Some(cwd) = self.cwd.clone() else { return };
        if cwd != b"/" {
            let child = path::basename(&cwd).to_vec();
            self.goto(r, path::parent(&cwd), Some(child));
        }
    }

    fn open(&mut self, r: &mut Remote) {
        let rows = self.rows().0;
        let Some(row) = rows.get(self.sel) else {
            return;
        };
        match row.kind {
            // picking a folder isn't starting anything: enter takes it
            Kind::Here if self.choosing || self.terminal_only => {
                self.want_terminal = self.terminal_only;
                self.pick(r);
            }
            Kind::Here => self.note(format!("i starts {} in this folder", self.agent)),
            Kind::Up => self.up(r),
            Kind::Dir if row.denied => self.error(format!("{}: no access", path::show(&row.name))),
            Kind::Dir | Kind::Deep | Kind::Recent => {
                if let Some(p) = self.row_path(row) {
                    self.goto(r, p, None);
                }
            }
        }
    }

    fn row_path(&self, row: &Row) -> Option<Vec<u8>> {
        if row.kind == Kind::Recent {
            return Some(row.name.clone());
        }
        let cwd = self.cwd.clone()?;
        Some(match row.kind {
            Kind::Here => cwd,
            Kind::Up => path::parent(&cwd),
            _ => path::join(&cwd, &row.name),
        })
    }

    /// Enter: start the agent in the highlighted folder (the top hit while searching).
    fn enter(&mut self, r: &mut Remote) {
        if self.jump_pending() {
            match self.jump_err.clone() {
                Some(e) => self.error(e),
                None => self.note("still opening that folder…"),
            }
        } else if self.rows().0.is_empty() {
            self.error(format!("no folder matches '{}'", self.query));
        } else {
            self.pick(r)
        }
    }

    fn pick(&mut self, r: &mut Remote) {
        let rows = self.rows().0;
        let Some(row) = rows.get(self.sel) else {
            return;
        };
        if row.kind == Kind::Dir && row.denied {
            return self.error(format!("{}: no access", path::show(&row.name)));
        }
        // `..` is for moving: i on it starts here, in the folder on screen
        let target = if row.kind == Kind::Up {
            self.cwd.clone()
        } else {
            self.row_path(row)
        };
        let Some(target) = target else {
            return;
        };
        // final existence/permission check on the remote before handing it back
        let id = r.send(Op::Stat, &target);
        self.choose = Some((id, target));
    }

    /// The folder is verified: hand it back, unless an agent already runs there
    /// (then ask). Waits (bounded) for the session list if it's still coming.
    fn resolve_pick(&mut self) {
        let Some((target, real, at)) = self.settle.clone() else {
            return;
        };
        if self.sessions_rx.is_some() && at.elapsed() < SETTLE {
            return;
        }
        self.settle = None;
        // a terminal is a new thing to type in, whoever else runs there
        if self.want_terminal {
            self.done = Some(Some(Picked::Terminal(target)));
            return;
        }
        let mut here: Vec<Session> = self.sessions_in(&target).into_iter().cloned().collect();
        if here.is_empty() && real != target {
            here = self.sessions_in(&real).into_iter().cloned().collect();
        }
        if here.is_empty() {
            self.done = Some(Some(Picked::New(target)));
        } else {
            self.ask = Some((target, here));
        }
    }

    fn refresh(&mut self, r: &mut Remote) {
        if let Some(cwd) = self.cwd.clone() {
            self.loading = true;
            self.list_id = Some(r.send(Op::List, &cwd));
            self.infos.remove(&cwd);
            self.deep_fail.remove(&cwd);
            if let Some(d) = self.cache.deep(&cwd).cloned() {
                // re-index on the next pump
                self.cache.put_deep(&cwd, Deep { fetched: 0, ..d });
            }
        }
    }

    /// The folder being shown is gone (or unreadable): forget it and step back.
    fn vanished(&mut self, r: &mut Remote, cwd: &[u8], e: String) {
        let gone = e == "missing" || e == "not a folder";
        if gone || e == "permission denied" {
            self.cache.remove_tree(cwd);
        }
        self.error(if gone {
            format!("{} no longer exists", self.pretty(cwd))
        } else {
            format!("{}: {e}", self.pretty(cwd))
        });
        let back = match self.prev.clone() {
            Some(p) if !path::is_within(&p, cwd) => p,
            _ => path::parent(cwd),
        };
        if back == cwd {
            self.set_dirs(None);
            self.err = Some(e);
            return;
        }
        let reselect = (!gone && path::parent(cwd) == back).then(|| path::basename(cwd).to_vec());
        self.goto(r, back, reselect);
    }

    // ---------------------------------------------------------------- query / path jump

    /// (jump folder, filter text) for the current query.
    fn filter(&self) -> (Option<Vec<u8>>, String) {
        match parse_jump(&self.query, self.cache.home.as_deref()) {
            Some((d, rest)) => (Some(d), rest),
            None => (None, self.query.clone()),
        }
    }

    /// A path query points somewhere we haven't opened (yet).
    fn jump_pending(&self) -> bool {
        self.filter()
            .0
            .is_some_and(|d| Some(&d) != self.cwd.as_ref())
    }

    /// After every query edit: re-rank, and follow a path query to its folder.
    fn on_query(&mut self, r: &mut Remote) {
        self.sel = 0;
        self.list_off = 0;
        self.jump_err = None;
        let Some(dir) = self.filter().0 else {
            self.jump_req = None;
            return;
        };
        if Some(&dir) == self.cwd.as_ref() {
            self.jump_req = None;
            return;
        }
        if self.jump_req.as_ref().is_some_and(|j| j.1 == dir) {
            return;
        }
        if matches!(self.conn, Conn::Offline { .. }) && self.cache.get(&dir).is_some() {
            // offline: browse the cached copy (validated once we're back)
            return self.go(r, dir, None, true);
        }
        // stat first so a typo shows inline instead of navigating away
        self.jump_req = Some((r.send(Op::Stat, &dir), dir));
    }

    fn set_query(&mut self, q: String, r: &mut Remote) {
        self.query = q;
        self.on_query(r);
    }

    /// Tab while typing a path: complete the highlighted match + `/`.
    fn tab(&mut self, r: &mut Remote) {
        if self.filter().0.is_none() {
            return self.open(r);
        }
        let rows = self.rows().0;
        match rows.get(self.sel) {
            Some(row) if matches!(row.kind, Kind::Dir | Kind::Deep) => {
                let q = complete(&self.query, &path::show(&row.name));
                self.set_query(q, r);
            }
            _ if self.query == "~" => self.set_query("~/".into(), r),
            _ => {}
        }
    }

    // ---------------------------------------------------------------- rows / selection

    fn set_dirs(&mut self, d: Option<Vec<Dir>>) {
        self.dirs = d;
        self.gen += 1;
    }

    fn recent_bonus(&self, p: &[u8]) -> i32 {
        self.recent
            .iter()
            .position(|x| x == p)
            .map_or(0, |i| (RECENT_BOOST - 2 * i as i32).max(4))
    }

    fn excluded(&self, name: &[u8], incl_hidden: bool) -> (bool, bool) {
        (
            !incl_hidden && ignore::hidden(name),
            !self.show_ignored && self.rules.ignored(name),
        )
    }

    /// Visible rows plus how many folders were hidden / ignored. With a query,
    /// rows are fuzzy matches ranked best-first (the top hit is row 0), then
    /// deep-index matches. Memoised: recomputed only when the listing, deep
    /// index, query or toggles change.
    fn rows(&self) -> Rows {
        let key = (
            self.gen,
            self.query.clone(),
            self.show_hidden,
            self.show_ignored,
        );
        if let Some((k, v)) = self.memo.borrow().as_ref() {
            if *k == key {
                return v.clone();
            }
        }
        let v = self.compute_rows();
        *self.memo.borrow_mut() = Some((key, v.clone()));
        v
    }

    fn compute_rows(&self) -> Rows {
        let (jump, q) = self.filter();
        if jump.as_ref().is_some_and(|d| Some(d) != self.cwd.as_ref()) {
            return (Rc::new(Vec::new()), 0, 0); // still on the way there
        }
        let q = q.trim().to_string();
        let mut out = Vec::new();
        if q.is_empty() {
            if jump.is_none() && self.cwd.is_some() && self.cwd == self.first {
                for p in self
                    .recent
                    .iter()
                    .filter(|p| Some(*p) != self.cwd.as_ref())
                    .take(MAX_RECENT)
                {
                    out.push(Row {
                        name: p.clone(),
                        ..Row::special(Kind::Recent)
                    });
                }
            }
            out.push(Row::special(Kind::Here));
            if self.cwd.as_deref() != Some(&b"/"[..]) {
                out.push(Row::special(Kind::Up));
            }
        }
        // a query starting with '.' is looking for hidden folders
        let incl_hidden = self.show_hidden || q.starts_with('.');
        let cwd = self.cwd.clone().unwrap_or_default();
        let (mut hid, mut ign) = (0, 0);
        let mut scored: Vec<(i32, Row)> = Vec::new();
        for d in self.dirs.iter().flatten() {
            match self.excluded(&d.name, incl_hidden) {
                (true, _) => {
                    hid += 1;
                    continue;
                }
                (_, true) => {
                    ign += 1;
                    continue;
                }
                _ => {}
            }
            let row = Row {
                kind: Kind::Dir,
                name: d.name.clone(),
                link: d.link,
                denied: d.denied,
                git: d.git,
                hits: Vec::new(),
            };
            if q.is_empty() {
                out.push(row);
            } else if let Some((sc, pos)) = fuzzy::score(&q, &path::show(&d.name)) {
                let sc = sc + self.recent_bonus(&path::join(&cwd, &d.name));
                scored.push((sc, Row { hits: pos, ..row }));
            }
        }
        let by_score = |a: &(i32, Row), b: &(i32, Row)| {
            b.0.cmp(&a.0)
                .then(a.1.name.len().cmp(&b.1.name.len()))
                .then(a.1.name.cmp(&b.1.name))
        };
        if !q.is_empty() {
            scored.sort_by(by_score);
            out.extend(scored.into_iter().map(|x| x.1));
            // deeper matches from the index, after the direct children
            if let Some(dp) = self.cwd.as_ref().and_then(|c| self.cache.deep(c)) {
                let mut deep: Vec<(i32, Row)> = Vec::new();
                for rel in &dp.paths {
                    if !rel.contains(&b'/') {
                        continue; // direct children are listed above
                    }
                    let skip = rel.split(|&b| b == b'/').any(|n| {
                        let (h, i) = self.excluded(n, incl_hidden);
                        h || i
                    });
                    if skip {
                        continue;
                    }
                    if let Some((sc, pos)) = fuzzy::score_path(&q, &path::show(rel)) {
                        let sc = sc + self.recent_bonus(&path::join(&cwd, rel));
                        deep.push((
                            sc,
                            Row {
                                name: rel.clone(),
                                hits: pos,
                                ..Row::special(Kind::Deep)
                            },
                        ));
                    }
                }
                deep.sort_by(by_score);
                deep.truncate(MAX_DEEP_ROWS);
                out.extend(deep.into_iter().map(|x| x.1));
            }
        }
        (Rc::new(out), hid, ign)
    }

    fn apply_reselect(&mut self) {
        if self.dirs.is_none() {
            return;
        }
        let Some(want) = self.reselect.take() else {
            return;
        };
        if let Some(i) = self
            .rows()
            .0
            .iter()
            .position(|r| r.kind == Kind::Dir && r.name == want)
        {
            self.sel = i;
        }
    }

    fn selected_name(&self) -> Option<Vec<u8>> {
        self.rows()
            .0
            .get(self.sel)
            .filter(|r| r.kind == Kind::Dir)
            .map(|r| r.name.clone())
    }

    fn nav(&mut self, code: KeyCode) {
        let n = self.rows().0.len().max(1);
        let page = self.page.max(1);
        self.sel = match code {
            KeyCode::Up => self.sel.saturating_sub(1),
            KeyCode::Down => (self.sel + 1).min(n - 1),
            KeyCode::PageUp => self.sel.saturating_sub(page),
            KeyCode::PageDown => (self.sel + page).min(n - 1),
            KeyCode::Home => 0,
            KeyCode::End => n - 1,
            _ => self.sel,
        };
    }

    fn toggle(&mut self, f: impl FnOnce(&mut App)) {
        let keep = self.selected_name();
        f(self);
        self.sel = 0;
        self.reselect = keep;
        self.apply_reselect();
    }

    // ---------------------------------------------------------------- background work

    /// Debounced: once the cursor rests on a folder, fetch its facts and
    /// sub-folders; keep the deep index and visible rows' tags coming.
    fn pump(&mut self, r: &mut Remote) {
        if let Some(rx) = &self.sessions_rx {
            if let Ok(s) = rx.try_recv() {
                self.sessions = s;
                self.sessions_rx = None;
            }
        }
        self.resolve_pick();
        // the folder on screen, listed again now and then (quietly)
        if matches!(self.conn, Conn::Online)
            && self.list_id.is_none()
            && self.naming.is_none()
            && self.cwd.is_some()
            && self.auto_at.elapsed() >= AUTO_REFRESH
        {
            self.auto_at = Instant::now();
            if let Some(cwd) = self.cwd.clone() {
                self.list_id = Some(r.send(Op::List, &cwd));
            }
        }
        if matches!(self.conn, Conn::Online) && self.bg.is_none() && !self.ssh.is_empty() {
            self.bg = Some(Remote::start_with(self.host.clone(), self.ssh.clone()));
        }
        self.want_deep();
        self.prefetch();

        let rows = self.rows().0;
        let target = rows
            .get(self.sel)
            .filter(|row| !(row.kind == Kind::Dir && row.denied))
            .and_then(|row| self.row_path(row));
        match (&self.hover, &target) {
            (Some((p, _)), Some(t)) if p == t => {}
            (_, Some(t)) => self.hover = Some((t.clone(), Instant::now())),
            (_, None) => self.hover = None,
        }
        let Some((p, since)) = self.hover.clone() else {
            return;
        };
        if since.elapsed() < HOVER_DELAY || !matches!(self.conn, Conn::Online) {
            return;
        }
        let stale = |t: u64| cache::now().saturating_sub(t) > self.ttl;
        if self.info_req.is_none()
            && !self.info_err.contains_key(&p)
            && !self.pre_req.values().any(|x| *x == p)
            && self
                .infos
                .get(&p)
                .is_none_or(|(_, at)| at.elapsed().as_secs() > self.ttl)
        {
            self.info_req = Some((r.send(Op::Info, &p), p.clone()));
        }
        if self.peek_req.is_none()
            && Some(&p) != self.cwd.as_ref()
            && self.cache.get(&p).is_none_or(|e| stale(e.fetched))
        {
            self.peek_req = Some((r.send(Op::List, &p), p.clone()));
        }
    }

    /// Index the current folder's sub-folders in the background (cached per
    /// root, re-fetched after the TTL like listings).
    fn want_deep(&mut self) {
        if self.deep_levels == 0 || !self.bg_online || self.deep_req.is_some() {
            return;
        }
        let Some(cwd) = self.cwd.clone() else { return };
        if self.dirs.is_none() || self.jump_pending() {
            return;
        }
        let fresh = self.cache.deep(&cwd).is_some_and(|d| {
            d.depth == self.deep_levels && cache::now().saturating_sub(d.fetched) <= self.ttl
        });
        if fresh
            || self
                .deep_fail
                .get(&cwd)
                .is_some_and(|t| t.elapsed() < DEEP_RETRY)
        {
            return;
        }
        if self.query.is_empty() && self.arrived.elapsed() < DEEP_DELAY {
            return;
        }
        if let Some(bg) = self.bg.as_mut() {
            let id = bg.send_find(&cwd, self.deep_levels, &self.globs);
            self.deep_req = Some((id, cwd));
        }
    }

    /// Project tags for the rows on screen: a few Info requests at a time.
    fn prefetch(&mut self) {
        if !self.bg_online || self.pre_req.len() >= PREFETCH {
            return;
        }
        let rows = self.rows().0;
        let h = self.list_inner.height as usize;
        for row in rows.iter().skip(self.list_off).take(h) {
            if self.pre_req.len() >= PREFETCH {
                break;
            }
            if !matches!(row.kind, Kind::Dir | Kind::Deep | Kind::Recent) || row.denied {
                continue;
            }
            let Some(p) = self.row_path(row) else {
                continue;
            };
            let known = self
                .infos
                .get(&p)
                .is_some_and(|(_, at)| at.elapsed().as_secs() <= self.ttl);
            if known
                || self.info_err.contains_key(&p)
                || self.pre_req.values().any(|x| *x == p)
                || self.info_req.as_ref().is_some_and(|x| x.1 == p)
            {
                continue;
            }
            if let Some(bg) = self.bg.as_mut() {
                let id = bg.send(Op::Info, &p);
                self.pre_req.insert(id, p);
            }
        }
    }

    fn got_info(&mut self, p: Vec<u8>, res: Result<Info, String>) {
        match res {
            Ok(i) => {
                // folder changed since its listing was cached → re-list it
                if i.mtime != 0
                    && self.cache.get(&p).is_some_and(|e| e.mtime != i.mtime)
                    && Some(&p) != self.cwd.as_ref()
                {
                    self.cache.forget(&p);
                }
                self.infos.insert(p, (i, Instant::now()));
            }
            Err(e) if e != "remote not responding" => {
                self.info_err.insert(p, e);
            }
            Err(_) => {}
        }
    }

    /// Replies on the background connection.
    fn on_bg(&mut self, ev: Ev) {
        match ev {
            Ev::Online { .. } => self.bg_online = true,
            Ev::Offline { .. } => self.bg_online = false,
            Ev::Info { id, res } => {
                if let Some(p) = self.pre_req.remove(&id) {
                    self.got_info(p, res);
                }
            }
            Ev::Find { id, res } if self.deep_req.as_ref().map(|x| x.0) == Some(id) => {
                let (_, root) = self.deep_req.take().unwrap_or_default();
                match res {
                    Ok((l, truncated)) => {
                        self.cache.put_deep(
                            &root,
                            Deep {
                                fetched: cache::now(),
                                depth: self.deep_levels,
                                truncated,
                                paths: l.dirs.into_iter().map(|d| d.name).collect(),
                            },
                        );
                        if Some(&root) == self.cwd.as_ref() {
                            self.gen += 1; // new matches for the current query
                        }
                    }
                    Err(_) => {
                        self.deep_fail.insert(root, Instant::now());
                    }
                }
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- events

    fn on_key(&mut self, k: KeyEvent, r: &mut Remote) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
            self.done = Some(None);
            return;
        }
        if let Some((target, here)) = &self.ask {
            match k.code {
                KeyCode::Char('a') | KeyCode::Enter => {
                    self.done = Some(Some(Picked::Attach(here[0].clone())))
                }
                KeyCode::Char('n') => self.done = Some(Some(Picked::New(target.clone()))),
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.ask = None;
                    self.note("cancelled");
                }
                _ => {}
            }
            return;
        }
        if self.choose.is_some() || self.settle.is_some() {
            if k.code == KeyCode::Esc {
                self.choose = None;
                self.settle = None;
                self.note("cancelled");
            }
            return;
        }
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if ctrl {
            match k.code {
                KeyCode::Char('q') => self.done = Some(None),
                KeyCode::Char('r') => self.refresh(r),
                KeyCode::Char('o') => self.toggle(|a| {
                    let all = !(a.show_hidden && a.show_ignored);
                    a.show_hidden = all;
                    a.show_ignored = all;
                }),
                KeyCode::Char('n') | KeyCode::Char('j') => self.nav(KeyCode::Down),
                KeyCode::Char('p') | KeyCode::Char('k') => self.nav(KeyCode::Up),
                KeyCode::Char('u') | KeyCode::Char('w') => self.set_query(String::new(), r),
                _ => {}
            }
            return;
        }
        // a new folder's name (`n`)
        if let Some(name) = self.naming.as_mut() {
            match k.code {
                KeyCode::Esc => {
                    self.naming = None;
                    self.note("cancelled");
                }
                KeyCode::Backspace => {
                    name.pop();
                }
                KeyCode::Char(ch) if !alt && !ch.is_control() && ch != '/' => name.push(ch),
                KeyCode::Enter => {
                    let n = name.trim().to_string();
                    self.naming = None;
                    self.mkdir(r, &n);
                }
                _ => {}
            }
            return;
        }
        match k.code {
            KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Home
            | KeyCode::End => return self.nav(k.code),
            _ => {}
        }
        // searching: typing filters, enter opens what's picked
        if self.searching {
            match k.code {
                KeyCode::Esc => {
                    self.searching = false;
                    self.set_query(String::new(), r);
                }
                KeyCode::Enter => {
                    self.searching = false;
                    if self.jump_pending() {
                        self.enter(r);
                    } else if !self.query.is_empty() {
                        self.open(r);
                        if !self.query.is_empty() {
                            self.set_query(String::new(), r);
                        }
                    }
                }
                KeyCode::Tab => self.tab(r),
                KeyCode::Backspace => {
                    self.query.pop();
                    self.on_query(r);
                }
                KeyCode::Char(ch) if !alt => {
                    // every keystroke re-ranks, top hit selected
                    self.query.push(ch);
                    self.on_query(r);
                }
                _ => {}
            }
            return;
        }
        // browsing: enter moves (into a folder, up from ..), i starts
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => self.done = Some(None),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open(r),
            KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => self.up(r),
            KeyCode::Char('i') => {
                self.want_terminal = self.terminal_only;
                self.enter(r);
            }
            KeyCode::Char('t') => {
                self.want_terminal = true;
                self.enter(r);
            }
            KeyCode::Char('n') => {
                if self.cwd.is_some() {
                    self.naming = Some(String::new());
                }
            }
            KeyCode::Char('r') => {
                self.refresh(r);
                self.note("listing it again…");
            }
            KeyCode::Char('j') => self.nav(KeyCode::Down),
            KeyCode::Char('k') => self.nav(KeyCode::Up),
            KeyCode::Char('g') => self.nav(KeyCode::Home),
            KeyCode::Char('G') => self.nav(KeyCode::End),
            KeyCode::Char('.') => self.toggle(|a| {
                let all = !(a.show_hidden && a.show_ignored);
                a.show_hidden = all;
                a.show_ignored = all;
            }),
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('~') => {
                self.searching = true;
                self.set_query("~/".into(), r);
            }
            _ => {}
        }
    }

    /// Make `name` in the folder on screen (`n`), then select it.
    fn mkdir(&mut self, r: &mut Remote, name: &str) {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return self.error("a folder name, without /");
        }
        let Some(cwd) = self.cwd.clone() else { return };
        let p = path::join(&cwd, name.as_bytes());
        let id = r.send(Op::Mkdir, &p);
        self.mkdir_req = Some((id, p));
        self.note(format!("making {name}…"));
    }

    /// Click selects, double-click acts like enter, the wheel scrolls.
    fn on_mouse(&mut self, m: MouseEvent, r: &mut Remote) {
        if self.ask.is_some() || self.choose.is_some() || self.settle.is_some() {
            return;
        }
        let n = self.rows().0.len();
        match m.kind {
            MouseEventKind::ScrollDown => self.sel = (self.sel + 3).min(n.saturating_sub(1)),
            MouseEventKind::ScrollUp => self.sel = self.sel.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left) => {
                let a = self.list_inner;
                if !a.contains(Position::new(m.column, m.row)) {
                    return;
                }
                let idx = self.list_off + (m.row - a.y) as usize;
                if idx >= n {
                    return;
                }
                let double = self
                    .last_click
                    .is_some_and(|(t, i)| i == idx && t.elapsed() < DOUBLE_CLICK);
                self.sel = idx;
                if double {
                    self.last_click = None;
                    self.open(r);
                } else {
                    self.last_click = Some((Instant::now(), idx));
                }
            }
            _ => {}
        }
    }

    fn on_remote(&mut self, ev: Ev, r: &mut Remote) {
        match ev {
            Ev::Online { home } => {
                self.conn = Conn::Online;
                let home = path::normalize(&home);
                self.cache.home = Some(home.clone());
                self.gen += 1; // `~` queries can resolve now
                if self.cwd.is_none() {
                    let s = self.start.clone().unwrap_or(home);
                    self.goto(r, s, None);
                }
            }
            Ev::Offline { reason, retry_in } => {
                self.conn = Conn::Offline {
                    reason,
                    retry_at: Instant::now() + Duration::from_secs(retry_in),
                };
                self.info_req = None;
                self.peek_req = None;
            }
            Ev::Info { id, res } if self.info_req.as_ref().map(|x| x.0) == Some(id) => {
                let (_, p) = self.info_req.take().unwrap_or_default();
                self.got_info(p, res);
            }
            Ev::List { id, res } if self.peek_req.as_ref().map(|x| x.0) == Some(id) => {
                let (_, p) = self.peek_req.take().unwrap_or_default();
                if let Ok(l) = res {
                    self.cache.put(
                        &p,
                        Entry {
                            mtime: l.mtime,
                            fetched: cache::now(),
                            dirs: l.dirs,
                        },
                    );
                }
            }
            Ev::Stat { id, res } if self.jump_req.as_ref().map(|j| j.0) == Some(id) => {
                let (_, dir) = self.jump_req.take().unwrap_or_default();
                if self.filter().0.as_ref() != Some(&dir) {
                    return; // the query moved on
                }
                match res {
                    Ok(_) => self.go(r, dir, None, true),
                    Err(e) => self.jump_err = Some(format!("{}: {e}", self.pretty(&dir))),
                }
            }
            Ev::Stat { id, res } if self.choose.as_ref().map(|c| c.0) == Some(id) => {
                let (_, target) = self.choose.take().unwrap_or_default();
                match res {
                    Ok((_, real)) => {
                        self.settle = Some((target, real, Instant::now()));
                        self.resolve_pick();
                    }
                    Err(e) => {
                        if e == "missing" || e == "not a folder" {
                            self.cache.remove_tree(&target);
                        }
                        self.error(format!("can't start in {}: {e}", self.pretty(&target)));
                    }
                }
            }
            Ev::Stat { id, res } if self.mkdir_req.as_ref().map(|m| m.0) == Some(id) => {
                let (_, p) = self.mkdir_req.take().unwrap_or_default();
                match res {
                    Ok(_) => {
                        self.note(format!("made {}", self.pretty(&p)));
                        // list it again, the new folder selected
                        self.reselect = Some(path::basename(&p).to_vec());
                        self.refresh(r);
                    }
                    Err(e) => self.error(format!("{}: {e}", self.pretty(&p))),
                }
            }
            Ev::Stat { id, res } if self.stat_id == Some(id) => {
                self.stat_id = None;
                self.validating = false;
                let cwd = self.cwd.clone().unwrap_or_default();
                match res {
                    Ok((mtime, _)) => {
                        let stale = self.cache.get(&cwd).is_none_or(|e| {
                            e.mtime != mtime || cache::now().saturating_sub(e.fetched) > self.ttl
                        });
                        if stale {
                            self.loading = true;
                            self.list_id = Some(r.send(Op::List, &cwd));
                        }
                    }
                    Err(e) if e == "remote not responding" => {
                        self.note("couldn't verify folder — showing cached")
                    }
                    Err(e) => self.vanished(r, &cwd, e),
                }
            }
            Ev::List { id, res } if self.list_id == Some(id) => {
                self.list_id = None;
                self.loading = false;
                let cwd = self.cwd.clone().unwrap_or_default();
                match res {
                    Ok(l) => {
                        // background refresh: keep the cursor on the same folder name
                        if self.reselect.is_none() {
                            self.reselect = self.selected_name();
                        }
                        self.cache.put(
                            &cwd,
                            Entry {
                                mtime: l.mtime,
                                fetched: cache::now(),
                                dirs: l.dirs,
                            },
                        );
                        if let Some((dirs, fetched)) =
                            self.cache.get(&cwd).map(|e| (e.dirs.clone(), e.fetched))
                        {
                            self.set_dirs(Some(dirs));
                            self.fetched = Some(fetched);
                        }
                        self.apply_reselect();
                    }
                    Err(e) if self.dirs.is_some() && e == "remote not responding" => {
                        self.note("refresh timed out — showing cached")
                    }
                    Err(e) => self.vanished(r, &cwd, e),
                }
            }
            _ => {} // replies for folders we've already left
        }
    }

    // ---------------------------------------------------------------- drawing

    fn pretty(&self, p: &[u8]) -> String {
        match &self.cache.home {
            Some(h) if h.as_slice() != b"/" && path::is_within(p, h) => {
                format!("~{}", path::show(&p[h.len()..]))
            }
            _ => path::show(p),
        }
    }

    fn sessions_in(&self, p: &[u8]) -> Vec<&Session> {
        let ps = path::show(&path::normalize(p));
        self.sessions
            .iter()
            .filter(|s| {
                let c = s.cwd.trim_end_matches('/');
                c == ps || (c.is_empty() && ps == "/" && !s.cwd.is_empty())
            })
            .collect()
    }

    /// What enter would start in, for the search line.
    fn enter_label(&self, row: &Row) -> String {
        match row.kind {
            Kind::Recent => self.pretty(&row.name),
            Kind::Up => "..".into(),
            Kind::Here => self
                .cwd
                .as_deref()
                .map(|p| path::show(path::basename(p)))
                .filter(|s| !s.is_empty())
                .unwrap_or("/".into()),
            Kind::Dir | Kind::Deep => path::show(&row.name),
        }
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let [head, body, foot] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, head);
        let wide = body.width >= 96;
        let [left, right] = if wide {
            Layout::horizontal([Constraint::Percentage(46), Constraint::Percentage(54)]).areas(body)
        } else {
            [body, Rect::default()]
        };
        self.draw_list(f, left);
        if wide {
            self.draw_preview(f, right);
        }
        self.draw_footer(f, foot);
        if self.ask.is_some() {
            self.draw_ask(f, body);
        }
    }

    fn draw_header(&self, f: &mut Frame, r: Rect) {
        let mut spans = vec![
            // machine identity stripe
            Span::styled("▌", ui::fg(self.color)),
            Span::styled(
                format!("{} ", crate::brand::MARK),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.host.clone(), ui::bold().fg(self.color)),
            Span::styled("  ", Style::new()),
        ];
        let loc = self
            .cwd
            .as_deref()
            .map(|p| self.pretty(p))
            .unwrap_or_else(|| "…".into());
        let segs: Vec<&str> = loc.split('/').filter(|s| !s.is_empty()).collect();
        let absolute = loc.starts_with('/');
        if absolute {
            spans.push(Span::styled("/", ui::muted()));
        }
        let budget = (r.width as usize).saturating_sub(52).max(12);
        let (mut take, mut used) = (0, 0);
        for seg in segs.iter().rev() {
            let w = ui::width(seg) + 3;
            if used + w > budget && take > 0 {
                break;
            }
            used += w;
            take += 1;
        }
        let first = segs.len() - take;
        if first > 0 {
            spans.push(Span::styled("… › ", ui::faint()));
        }
        for (i, seg) in segs[first..].iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" › ", ui::faint()));
            }
            let last = first + i + 1 == segs.len();
            spans.push(Span::styled(
                seg.to_string(),
                if last {
                    ui::bold().fg(c::ACCENT())
                } else {
                    ui::muted()
                },
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), r);

        let (st, col) = match &self.conn {
            Conn::Online => ("● connected".to_string(), c::GREEN()),
            Conn::Connecting => (
                format!(
                    "{} connecting · {}s",
                    ui::spinner(self.since),
                    self.since.elapsed().as_secs()
                ),
                c::YELLOW(),
            ),
            Conn::Offline { retry_at, .. } => (
                format!(
                    "✕ offline · retrying in {}s",
                    retry_at.saturating_duration_since(Instant::now()).as_secs() + 1
                ),
                c::RED(),
            ),
        };
        let mut right = vec![Span::styled(st, ui::fg(col))];
        if let Some(t) = self.fetched {
            right.push(Span::styled(
                format!("  ·  listed {}", ui::ago(cache::now().saturating_sub(t))),
                ui::faint(),
            ));
        }
        right.push(Span::raw(" "));
        f.render_widget(Paragraph::new(Line::from(right)).right_aligned(), r);
    }

    /// Badges for a row: sessions here, project tags (or plain git), link, no access.
    fn badges(&self, row: &Row) -> Vec<Span<'static>> {
        let mut badges: Vec<Span> = Vec::new();
        if !matches!(row.kind, Kind::Dir | Kind::Deep | Kind::Recent) {
            return badges;
        }
        let p = self.row_path(row);
        if let Some(p) = &p {
            let here = self.sessions_in(p);
            if !here.is_empty() {
                badges.push(Span::styled(
                    format!("✻ {} ", here.len()),
                    ui::fg(c::ACCENT()),
                ));
            }
        }
        match p.as_ref().and_then(|p| self.infos.get(p)) {
            Some((info, _)) => {
                let t = tags(&info.markers, row.git || info.branch.is_some());
                // at most three, the most specific first
                let n = t.len();
                for (label, col) in t.into_iter().take(3) {
                    badges.push(Span::styled(format!("{label} "), ui::fg(col)));
                }
                if n > 3 {
                    badges.push(Span::styled(format!("+{} ", n - 3), ui::faint()));
                }
            }
            None if row.git => badges.push(Span::styled("git ", ui::fg(c::GREEN()))),
            None => {}
        }
        if row.link {
            badges.push(Span::styled("↗ ", ui::fg(c::BLUE())));
        }
        if row.denied {
            badges.push(Span::styled("⊘ ", ui::fg(c::RED())));
        }
        badges
    }

    fn row_item(&self, row: &Row, iw: usize) -> ListItem<'static> {
        let (left, lstyle): (String, Style) = match row.kind {
            Kind::Recent => (format!("★ {}", self.pretty(&row.name)), ui::fg(c::YELLOW())),
            Kind::Here if self.terminal_only => (
                format!("$ Terminal in {}", self.enter_label(row)),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Kind::Here if self.choosing => (
                format!("✓ Use {}", self.enter_label(row)),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Kind::Here => (
                format!("✻ Start {} in {}", self.agent, self.enter_label(row)),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Kind::Up => ("↰ ..".into(), ui::muted()),
            Kind::Dir | Kind::Deep => (
                format!("  {}/", path::show(&row.name)),
                if row.denied {
                    ui::faint()
                } else if ignore::hidden(&row.name) || self.rules.ignored(&row.name) {
                    ui::muted()
                } else {
                    Style::new()
                },
            ),
        };
        let badges = self.badges(row);
        let bw: usize = badges.iter().map(|s| ui::width(&s.content)).sum();
        let lw = iw.saturating_sub(bw + 1);
        let mut spans: Vec<Span> = if row.hits.is_empty() && row.kind != Kind::Deep {
            vec![Span::styled(ui::pad(&ui::trunc(&left, lw), lw), lstyle)]
        } else {
            // fuzzy hits: matched letters in the accent colour, underlined;
            // a deep match's parent folders are muted
            let name = path::show(&row.name);
            let dir_end = if row.kind == Kind::Deep {
                name.rfind('/').map_or(0, |i| name[..=i].chars().count())
            } else {
                0
            };
            let hit_style = lstyle
                .fg(c::ACCENT())
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            let mut v = vec![Span::styled("  ", lstyle)];
            let mut used = 2;
            // a long deep path loses its leading folders, not its name
            let chars: Vec<char> = name.chars().collect();
            let mut skip = 0;
            if row.kind == Kind::Deep {
                let room = lw.saturating_sub(5); // "  " + "…" … "/" + margin
                let mut w: usize = chars.iter().map(|ch| ui::width(&ch.to_string())).sum();
                while w > room && skip < chars.len() {
                    w -= ui::width(&chars[skip].to_string());
                    skip += 1;
                }
                if skip > 0 {
                    v.push(Span::styled("…", ui::muted()));
                    used += 1;
                }
            }
            for (i, ch) in chars.into_iter().enumerate().skip(skip) {
                let cw = ui::width(&ch.to_string());
                if used + cw > lw.saturating_sub(2) {
                    v.push(Span::styled("…", lstyle));
                    used += 1;
                    break;
                }
                let st = if row.hits.contains(&i) {
                    hit_style
                } else if i < dir_end {
                    ui::muted()
                } else {
                    lstyle
                };
                v.push(Span::styled(ch.to_string(), st));
                used += cw;
            }
            if used < lw {
                v.push(Span::styled("/", lstyle));
                used += 1;
            }
            v.push(Span::raw(" ".repeat(lw.saturating_sub(used))));
            v
        };
        spans.push(Span::raw(" "));
        spans.extend(badges);
        ListItem::new(Line::from(spans))
    }

    fn search_line(&self, rows: &[Row], n: usize) -> Line<'static> {
        let prompt = Span::styled(" ❯ ", ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD));
        if let Some(name) = &self.naming {
            return Line::from(vec![
                Span::styled(
                    " + new folder: ",
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{name}▏"), ui::bold()),
                Span::styled("  enter makes it · esc cancels", ui::faint()),
            ]);
        }
        if self.query.is_empty() {
            return Line::from(vec![
                prompt,
                Span::styled(
                    if self.searching {
                        "type to search · /path or ~/ to jump ▏"
                    } else {
                        if self.terminal_only {
                            "/ search · enter opens · i terminal here · n new folder "
                        } else if self.choosing {
                            "/ search · enter opens · i uses it · t terminal · n new folder "
                        } else {
                            "/ search · enter opens · i starts here · t terminal · n new folder "
                        }
                    },
                    ui::faint(),
                ),
            ]);
        }
        let mut v = vec![prompt];
        let jump = self.filter().0.is_some();
        match (jump, self.query.rfind('/')) {
            (true, Some(i)) => {
                v.push(Span::styled(self.query[..=i].to_string(), ui::muted()));
                v.push(Span::styled(self.query[i + 1..].to_string(), ui::bold()));
            }
            _ => v.push(Span::styled(self.query.clone(), ui::bold())),
        }
        v.push(Span::styled("▏", ui::fg(c::ACCENT())));
        if let Some(e) = &self.jump_err {
            v.push(Span::styled(format!(" ✗ {e} "), ui::fg(c::RED())));
            return Line::from(v);
        }
        if self.jump_pending() {
            v.push(Span::styled(
                format!(" {} opening… ", ui::spinner(self.since)),
                ui::fg(c::ACCENT()),
            ));
            return Line::from(v);
        }
        v.push(Span::styled(
            format!(" {n} match{} ", if n == 1 { "" } else { "es" }),
            if n == 0 {
                ui::fg(c::RED())
            } else {
                ui::muted()
            },
        ));
        // what enter will open
        if let Some(x) = rows.get(self.sel) {
            v.push(Span::styled(
                format!("· ↵ opens {} ", self.enter_label(x)),
                ui::fg(c::ACCENT()),
            ));
        }
        Line::from(v)
    }

    fn draw_list(&mut self, f: &mut Frame, r: Rect) {
        let (rows, hid, ign) = self.rows();
        if self.sel >= rows.len() {
            self.sel = rows.len().saturating_sub(1);
        }
        let n_dir = rows.iter().filter(|x| x.kind == Kind::Dir).count();
        let n_deep = rows.iter().filter(|x| x.kind == Kind::Deep).count();
        let mut title = vec![
            Span::raw("Folders "),
            Span::styled(n_dir.to_string(), ui::muted()),
        ];
        if n_deep > 0 {
            title.push(Span::styled(format!(" · {n_deep} deeper"), ui::muted()));
        }
        let mut block = ui::panel(Line::from(title), true);
        let mut tags = Vec::new();
        if hid > 0 {
            tags.push(format!("{hid} hidden"));
        }
        if ign > 0 {
            tags.push(format!("{ign} ignored"));
        }
        if self.validating {
            tags.push("checking…".into());
        } else if self.loading && self.dirs.is_some() {
            tags.push("refreshing…".into());
        }
        let cwd = self.cwd.clone().unwrap_or_default();
        if !self.query.is_empty() {
            if self.bg_online && self.deep_req.as_ref().is_some_and(|d| d.1 == cwd) {
                tags.push(format!("{} indexing", ui::spinner(self.since)));
            } else if self.cache.deep(&cwd).is_some_and(|d| d.truncated) {
                tags.push("index capped".into());
            }
        }
        if matches!(self.conn, Conn::Offline { .. }) && self.dirs.is_some() {
            tags.push("offline · cached".into());
        }
        if !tags.is_empty() {
            block = block.title_top(
                Line::from(Span::styled(format!(" {} ", tags.join(" · ")), ui::muted()))
                    .right_aligned(),
            );
        }
        // always-on search box in the bottom border
        block = block.title_bottom(self.search_line(&rows, n_dir + n_deep));
        let inner = block.inner(r);
        self.page = inner.height as usize;
        self.list_inner = inner;

        if self.dirs.is_none() || (self.jump_pending() && rows.is_empty()) {
            f.render_widget(block, r);
            let jumping = self.jump_pending();
            let (m, text, sub) = match (&self.err, &self.conn) {
                _ if jumping => match (&self.jump_err, self.filter().0) {
                    (Some(e), _) => (
                        "✗".to_string(),
                        e.clone(),
                        "backspace to edit · esc to clear".to_string(),
                    ),
                    (None, d) => (
                        ui::spinner(self.since).into(),
                        format!("Opening {}", self.pretty(&d.unwrap_or_default())),
                        String::new(),
                    ),
                },
                (Some(e), _) => (
                    "✗".to_string(),
                    format!("Can't open this folder: {e}"),
                    "← to go back".to_string(),
                ),
                (None, Conn::Offline { reason, .. }) => (
                    "✕".into(),
                    format!("Can't reach {}", self.host),
                    reason.clone(),
                ),
                (None, Conn::Connecting) => (
                    ui::spinner(self.since).into(),
                    format!("Connecting to {}", self.host),
                    format!("ssh {} · {}s", self.host, self.since.elapsed().as_secs()),
                ),
                (None, Conn::Online) => (
                    ui::spinner(self.since).into(),
                    "Reading folder".into(),
                    String::new(),
                ),
            };
            let bad = (jumping && self.jump_err.is_some())
                || (!jumping && (self.err.is_some() || matches!(self.conn, Conn::Offline { .. })));
            let col = if bad { c::RED() } else { c::ACCENT() };
            let mut lines = vec![Line::from(vec![
                Span::styled(format!("{m}  "), ui::fg(col)),
                Span::styled(text, ui::bold()),
            ])];
            for l in ui::wrap(&sub, inner.width.saturating_sub(4) as usize) {
                lines.push(Line::from(Span::styled(format!("   {l}"), ui::muted())));
            }
            let h = lines.len() as u16;
            let y = inner.y + inner.height.saturating_sub(h) / 2;
            f.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                Rect {
                    y,
                    height: h.min(inner.height),
                    ..inner
                },
            );
            return;
        }

        if rows.is_empty() && !self.query.is_empty() {
            f.render_widget(block, r);
            let lines = vec![
                Line::from(vec![
                    Span::styled("∅  ", ui::fg(c::MUTED())),
                    Span::styled(format!("No folder matches “{}”", self.query), ui::bold()),
                ]),
                Line::from(Span::styled(
                    "backspace to edit · esc to clear · ctrl+o to include hidden & ignored",
                    ui::muted(),
                )),
            ];
            let y = inner.y + inner.height.saturating_sub(2) / 2;
            f.render_widget(
                Paragraph::new(lines).alignment(Alignment::Center),
                Rect {
                    y,
                    height: 2.min(inner.height),
                    ..inner
                },
            );
            return;
        }
        let iw = inner.width.saturating_sub(2) as usize;
        // only the rows that can be on screen are built
        let h = inner.height as usize;
        let off = if self.sel < self.list_off {
            self.sel
        } else if h > 0 && self.sel >= self.list_off + h {
            self.sel + 1 - h
        } else {
            self.list_off
        };
        self.list_off = off.min(rows.len().saturating_sub(1));
        let items: Vec<ListItem> = rows
            .iter()
            .enumerate()
            .map(|(i, row)| {
                if i >= self.list_off && i < self.list_off + h {
                    self.row_item(row, iw)
                } else {
                    ListItem::new("")
                }
            })
            .collect();
        let list = List::new(items)
            .block(block)
            .highlight_style(ui::selected())
            .highlight_symbol("┃ ");
        let mut st = ListState::default()
            .with_selected(Some(self.sel))
            .with_offset(self.list_off);
        f.render_stateful_widget(list, r, &mut st);
        self.list_off = st.offset();
        // accent-coloured selection bar (List styles its symbol like the row)
        ui::accent_bar(f, inner, inner.y, st.offset(), self.sel);
        if rows.len() > inner.height as usize {
            let mut ss = ScrollbarState::new(rows.len()).position(self.sel);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .begin_symbol(None)
                    .end_symbol(None)
                    .track_symbol(Some("│"))
                    .track_style(ui::fg(c::LINE()))
                    .thumb_style(ui::fg(c::MUTED())),
                r.inner(Margin {
                    vertical: 1,
                    horizontal: 0,
                }),
                &mut ss,
            );
        }
    }

    fn draw_preview(&self, f: &mut Frame, r: Rect) {
        let rows = self.rows().0;
        let row = rows.get(self.sel);
        let p = row.and_then(|x| self.row_path(x));
        let title = match (row.map(|x| x.kind), &p) {
            (Some(Kind::Up), _) => "..".to_string(),
            (_, Some(p)) => {
                let b = path::show(path::basename(p));
                if b.is_empty() {
                    "/".into()
                } else {
                    format!("{b}/")
                }
            }
            _ => String::new(),
        };
        let block = ui::panel(Line::from(title), false);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let Some(p) = p else { return };
        let w = inner.width as usize;
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                ui::trunc_left(&self.pretty(&p), w),
                ui::muted(),
            )),
            Line::raw(""),
        ];

        if row.is_some_and(|x| x.kind == Kind::Dir && x.denied) {
            lines.push(Line::from(vec![
                Span::styled("⊘  ", ui::fg(c::RED())),
                Span::styled("No access", ui::bold()),
            ]));
            lines.push(Line::from(Span::styled(
                "You can't open this folder or start an agent in it.",
                ui::muted(),
            )));
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        if let Some(e) = self.info_err.get(&p) {
            lines.push(Line::from(vec![
                Span::styled("✗  ", ui::fg(c::RED())),
                Span::raw(e.clone()),
            ]));
            f.render_widget(Paragraph::new(lines), inner);
            return;
        }
        match self.infos.get(&p) {
            None => {
                let s = self.hover.as_ref().map_or(self.since, |h| h.1);
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", ui::spinner(s)), ui::fg(c::ACCENT())),
                    Span::styled("reading…", ui::muted()),
                ]));
            }
            Some((info, _)) => {
                if let Some(b) = &info.branch {
                    let mut l = vec![
                        Span::styled("git ", ui::fg(c::GREEN())),
                        Span::styled(b.clone(), ui::fg(c::GREEN()).add_modifier(Modifier::BOLD)),
                    ];
                    if let Some((h, age, _)) = &info.commit {
                        l.push(Span::styled(format!("  {h} · {age}"), ui::muted()));
                    }
                    lines.push(Line::from(l));
                    if let Some((_, _, subj)) = &info.commit {
                        for s in ui::wrap(subj, w.saturating_sub(2)).into_iter().take(2) {
                            lines.push(Line::from(Span::styled(format!("  {s}"), Style::new())));
                        }
                    }
                    lines.push(Line::raw(""));
                }
                let chips = chips(&info.markers);
                if !chips.is_empty() {
                    let mut l: Vec<Span> = Vec::new();
                    let mut used = 0;
                    for (label, col) in chips {
                        let cw = ui::width(label) + 3;
                        if used + cw > w {
                            lines.push(Line::from(std::mem::take(&mut l)));
                            used = 0;
                        }
                        l.push(Span::styled(
                            format!(" {label} "),
                            Style::new().fg(col).bg(c::SEL()),
                        ));
                        l.push(Span::raw(" "));
                        used += cw;
                    }
                    lines.push(Line::from(l));
                    lines.push(Line::raw(""));
                }
                let changed = if info.mtime > 0 {
                    format!(
                        " · changed {}",
                        ui::ago(cache::now().saturating_sub(info.mtime as u64))
                    )
                } else {
                    String::new()
                };
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} folder{} · {} file{}{changed}",
                        info.dirs,
                        if info.dirs == 1 { "" } else { "s" },
                        info.files,
                        if info.files == 1 { "" } else { "s" }
                    ),
                    ui::muted(),
                )));
            }
        }

        let here = self.sessions_in(&p);
        if !here.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                "Agents here",
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            )));
            for s in here {
                let (g, col, label) = ui::status(&s.status, self.since);
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", s.mark), ui::fg(ui::glyph_color(&s.mark))),
                    Span::styled(format!("{}  ", herdr::base_label(&s.label)), ui::bold()),
                    Span::styled(format!("{g} {label}"), ui::fg(col)),
                ]));
            }
        }

        // sub-folders (from the listing cache; fetched on hover)
        let subs: Option<Vec<&Dir>> = self.cache.get(&p).map(|e| {
            e.dirs
                .iter()
                .filter(|d| {
                    (self.show_hidden || !ignore::hidden(&d.name))
                        && (self.show_ignored || !self.rules.ignored(&d.name))
                })
                .collect()
        });
        let room = (inner.height as usize).saturating_sub(lines.len() + 2);
        if room >= 2 {
            lines.push(Line::raw(""));
            match subs {
                None => lines.push(Line::from(Span::styled("…", ui::faint()))),
                Some(v) if v.is_empty() => {
                    lines.push(Line::from(Span::styled("no sub-folders", ui::faint())))
                }
                Some(v) => {
                    let show = v.len().min(room.saturating_sub(1));
                    for (i, d) in v.iter().take(show).enumerate() {
                        let last = i + 1 == show && show == v.len();
                        let mut l = vec![
                            Span::styled(if last { "└ " } else { "├ " }, ui::faint()),
                            Span::styled(
                                format!("{}/", path::show(&d.name)),
                                if d.denied { ui::faint() } else { Style::new() },
                            ),
                        ];
                        if d.git {
                            l.push(Span::styled(" git", ui::faint()));
                        }
                        lines.push(Line::from(l));
                    }
                    if v.len() > show {
                        lines.push(Line::from(Span::styled(
                            format!("└ … {} more", v.len() - show),
                            ui::faint(),
                        )));
                    }
                }
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
    }

    /// "already running here" card over the list.
    fn draw_ask(&self, f: &mut Frame, body: Rect) {
        let Some((target, here)) = &self.ask else {
            return;
        };
        let s = &here[0];
        let more = if here.len() > 1 {
            format!(" (+{} more)", here.len() - 1)
        } else {
            String::new()
        };
        let name = s.display().to_string();
        let lines = vec![
            Line::from(vec![
                Span::styled(format!("{} ", s.glyph()), ui::fg(s.color())),
                Span::styled(name, ui::bold().fg(s.color())),
                Span::styled(more, ui::muted()),
                Span::raw(" is already running here"),
            ]),
            Line::from(Span::styled(
                ui::trunc_left(&self.pretty(target), 60),
                ui::muted(),
            )),
            Line::raw(""),
            Line::from(vec![
                ui::keycap("a"),
                Span::styled(" attach   ", ui::muted()),
                ui::keycap("n"),
                Span::styled(" new anyway   ", ui::muted()),
                ui::keycap("esc"),
                Span::styled(" back", ui::muted()),
            ]),
        ];
        let w = lines.iter().map(ui::line_width).max().unwrap_or(40) as u16 + 4;
        let area = ui::centered(body, w.max(44), lines.len() as u16 + 2);
        crate::ui::clear(f, area);
        let block = ui::panel(Line::from("Already running here"), true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines), inner);
    }

    fn draw_footer(&self, f: &mut Frame, r: Rect) {
        let checking = self
            .choose
            .as_ref()
            .map(|c| c.1.clone())
            .or_else(|| self.settle.as_ref().map(|s| s.0.clone()));
        let line = if let Some((_, here)) = &self.ask {
            Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    format!("{} is already running here", here[0].display()),
                    ui::fg(c::ACCENT()),
                ),
                Span::styled(" — a attach · n new anyway · esc", ui::muted()),
            ])
        } else if let Some(t) = &checking {
            Line::from(vec![
                Span::styled(
                    format!(" {} ", ui::spinner(self.since)),
                    ui::fg(c::ACCENT()),
                ),
                Span::styled(format!("checking {}…", self.pretty(t)), ui::muted()),
                Span::styled("  esc cancel", ui::faint()),
            ])
        } else {
            let jump = self.filter().0.is_some();
            ui::hints_fit(
                &[
                    (
                        "i",
                        if self.terminal_only {
                            "terminal here"
                        } else if self.choosing {
                            "use this folder"
                        } else {
                            "start agent"
                        },
                    ),
                    ("t", "terminal"),
                    ("enter →", "open"),
                    ("←", "back"),
                    ("/", "search"),
                    ("n", "new folder"),
                    ("r", "refresh"),
                    if jump {
                        ("tab", "complete")
                    } else {
                        ("~ /", "jump")
                    },
                    (
                        "ctrl+o",
                        if self.show_hidden && self.show_ignored {
                            "hide extras"
                        } else {
                            "show all"
                        },
                    ),
                    (
                        "esc",
                        if !self.query.is_empty() {
                            "clear"
                        } else if self.choosing {
                            "back"
                        } else {
                            "quit"
                        },
                    ),
                ]
                .into_iter()
                // browsing for a terminal, `i` already is the terminal key
                .filter(|h| !(self.terminal_only && h.0 == "t"))
                .collect::<Vec<_>>(),
                r.width,
            )
        };
        let busy = checking.is_some() || self.ask.is_some();
        let toast = self
            .msg
            .as_ref()
            .filter(|m| m.1.elapsed() < Duration::from_secs(5));
        let line = match (toast, &self.conn) {
            (Some((m, _, err)), _) if !busy => {
                let col = if *err { c::RED() } else { c::GREEN() };
                Line::from(vec![
                    Span::raw(" "),
                    Span::styled(
                        format!(" {} ", if *err { "✗" } else { "✓" }),
                        Style::new()
                            .fg(c::BASE())
                            .bg(col)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!(" {m}"), ui::fg(col)),
                ])
            }
            (_, Conn::Offline { reason, .. }) if !busy => Line::from(vec![
                Span::raw(" "),
                Span::styled(
                    " ✕ offline ",
                    Style::new()
                        .fg(c::BASE())
                        .bg(c::RED())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {} — browsing cached folders", ui::trunc(reason, 70)),
                    ui::fg(c::RED()),
                ),
            ]),
            _ => line,
        };
        f.render_widget(Paragraph::new(line), r);
    }
}

/// Marker files → project-type chips.
fn chips(markers: &[String]) -> Vec<(&'static str, Color)> {
    let has = |m: &str| markers.iter().any(|x| x == m);
    let mut v = Vec::new();
    if has("CLAUDE.md") {
        v.push(("CLAUDE.md", c::ACCENT()));
    }
    if has("AGENTS.md") {
        v.push(("AGENTS.md", c::ACCENT()));
    }
    if has("Cargo.toml") {
        v.push(("Rust", c::PEACH()));
    }
    if has("pyproject.toml") || has("requirements.txt") || has("setup.py") {
        v.push(("Python", c::YELLOW()));
    }
    if has(".venv") {
        v.push(("venv", c::YELLOW()));
    }
    if has("package.json") {
        v.push(("Node", c::GREEN()));
    }
    if has("go.mod") {
        v.push(("Go", c::SKY()));
    }
    if has("CMakeLists.txt") {
        v.push(("CMake", c::BLUE()));
    }
    if has("Makefile") {
        v.push(("Make", c::MUTED()));
    }
    if has("Dockerfile") || has("docker-compose.yml") {
        v.push(("Docker", c::BLUE()));
    }
    if has("flake.nix") {
        v.push(("Nix", c::BLUE()));
    }
    if has("README.md") {
        v.push(("README", c::MUTED()));
    }
    v
}

/// What the browser needs to know (built from config + the fleet).
#[derive(Clone, Debug, Default)]
pub struct BrowseOpts {
    /// folder to open first (absolute, remote)
    pub start: Option<Vec<u8>>,
    /// seconds before a cached listing is re-fetched
    pub ttl: u64,
    pub show_hidden: bool,
    pub show_ignored: bool,
    /// folder levels indexed for search below the current folder (0 = off)
    pub deep: u64,
    /// `ignore=` edits, e.g. "+data,-build"
    pub ignore_edits: String,
    /// recent folders on this host, best first (frecency)
    pub recent: Vec<String>,
    /// sessions already known for this host (skips a fetch); None = fetch
    pub sessions: Option<Vec<Session>>,
    /// the machine's identity colour
    pub color: ratatui::style::Color,
    /// what enter starts, for the labels (`Claude Code`, `Codex`, …)
    pub agent: String,
    /// picking a folder for Quick start: `i` hands it back, nothing starts
    pub choosing: bool,
    /// picking a folder for a terminal (ctrl+t): `i` opens one there
    pub terminal: bool,
}

impl BrowseOpts {
    pub fn from_config(cfg: &crate::config::Config, t: &Target) -> BrowseOpts {
        let start = crate::brand::env("DIR")
            .filter(|d| d.starts_with('/'))
            .or_else(|| cfg.host_get(&t.ssh, "dir").filter(|d| d.starts_with('/')))
            .map(|d| path::normalize(d.as_bytes()));
        BrowseOpts {
            start,
            ttl: cfg.ttl(),
            show_hidden: cfg.flag("show_hidden"),
            show_ignored: cfg.flag("show_ignored"),
            deep: cfg.num("deep_search"),
            ignore_edits: cfg.get_or("ignore"),
            recent: crate::history::recent_dirs(Some(&t.ssh))
                .into_iter()
                .map(|r| r.dir)
                .collect(),
            sessions: None,
            color: crate::theme::machine(cfg, &t.ssh),
            agent: crate::agents::default_agent_for(cfg, &t.ssh)
                .name
                .to_string(),
            choosing: false,
            terminal: false,
        }
    }
}

/// The browser's answer.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)] // returned once per browse
pub enum Picked {
    /// start a new session in this folder
    New(Vec<u8>),
    /// a session already runs there and the user chose to attach to it
    Attach(Session),
    /// `t`: a terminal in this folder
    Terminal(Vec<u8>),
}

/// Run the browser on an already-open screen. Returns the chosen remote
/// folder (or the session to attach to), or None if cancelled. Both ssh
/// connections are closed before this returns.
pub fn run(tui: &mut Tui, t: &Target, o: BrowseOpts) -> io::Result<Option<Picked>> {
    let ssh = crate::sshcfg::ssh_bin();
    let mut remote = Remote::start_with(t.ssh.clone(), ssh.clone());
    let mut app = App::new(t, &o, Cache::load(&t.ssh));
    app.ssh = ssh;
    app.boot(&mut remote);

    let res = (|| -> io::Result<()> {
        loop {
            while let Ok(ev) = remote.rx.try_recv() {
                app.on_remote(ev, &mut remote);
            }
            let bg: Vec<Ev> = app
                .bg
                .as_ref()
                .map(|b| b.rx.try_iter().collect())
                .unwrap_or_default();
            for ev in bg {
                app.on_bg(ev);
            }
            app.pump(&mut remote);
            if app.done.is_some() {
                return Ok(());
            }
            tui.term.draw(|f| {
                app.draw(f);
                crate::ui::finish(f.buffer_mut());
                crate::showcase::buffer(f.buffer_mut());
            })?;
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    // showcase: the panic key (and, while paused, every key) is its
                    Event::Key(k)
                        if k.kind != KeyEventKind::Release
                            && crate::showcase::panic_key(&crate::tui::key_str(&k)) => {}
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.on_key(k, &mut remote),
                    Event::Mouse(_) if crate::showcase::paused() => {}
                    Event::Mouse(m) => app.on_mouse(m, &mut remote),
                    _ => {}
                }
            }
        }
    })();

    // ssh is gone before the folder is handed back
    remote.shutdown();
    if let Some(bg) = app.bg.take() {
        bg.shutdown();
    }
    let _ = app.cache.save();
    res?;
    Ok(app.done.flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Listing;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn dir(n: &str) -> Dir {
        Dir {
            name: n.as_bytes().to_vec(),
            link: false,
            denied: false,
            git: n == "proj",
        }
    }

    fn app(sessions: Vec<Session>) -> App {
        let t = Target {
            ssh: "h".into(),
            machine: "h".into(),
            source: "test",
        };
        let o = BrowseOpts {
            ttl: 600,
            deep: 3,
            ignore_edits: "+data,-build".into(),
            recent: vec![
                "/home/u/proj".into(),
                "/srv/web/".into(),
                "relative/ignored".into(),
                "/home/u".into(),
            ],
            sessions: Some(sessions),
            color: Color::Cyan,
            agent: "Codex".into(),
            ..Default::default()
        };
        let mut a = App::new(&t, &o, Cache::default());
        a.cache.home = Some(b"/home/u".to_vec());
        a.conn = Conn::Online;
        a
    }

    /// A Remote whose ssh can't start: requests just queue, ids are real.
    fn dead() -> Remote {
        Remote::start_with("h".into(), "/nonexistent/ssh".into())
    }

    /// Open `p` and answer its listing (a cached folder shows at once).
    fn open(a: &mut App, r: &mut Remote, p: &str, names: &[&str]) {
        a.goto(r, p.as_bytes().to_vec(), None);
        if a.list_id.is_some() {
            listed(a, r, names);
        }
    }

    fn listed(a: &mut App, r: &mut Remote, names: &[&str]) {
        let id = a.list_id.expect("listing requested");
        a.on_remote(
            Ev::List {
                id,
                res: Ok(Listing {
                    mtime: 1,
                    dirs: names.iter().map(|n| dir(n)).collect(),
                }),
            },
            r,
        );
    }

    fn key(a: &mut App, r: &mut Remote, code: KeyCode) {
        a.on_key(KeyEvent::new(code, KeyModifiers::NONE), r);
    }

    fn typ(a: &mut App, r: &mut Remote, s: &str) {
        if !a.searching {
            key(a, r, KeyCode::Char('/'));
        }
        for ch in s.chars() {
            key(a, r, KeyCode::Char(ch));
        }
    }

    fn names(a: &App) -> Vec<(Kind, String)> {
        a.rows()
            .0
            .iter()
            .map(|r| (r.kind, path::show(&r.name)))
            .collect()
    }

    fn screen(a: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| a.draw(f)).unwrap();
        let buf = t.backend().buffer().clone();
        let mut s = String::new();
        for y in 0..h {
            for x in 0..w {
                s.push_str(buf[(x, y)].symbol());
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn path_jump_parsing() {
        let h = Some(&b"/home/u"[..]);
        let j = |q: &str| parse_jump(q, h).map(|(d, r)| (path::show(&d), r));
        let p = |d: &str, r: &str| Some((d.to_string(), r.to_string()));
        assert_eq!(j("/"), p("/", ""));
        assert_eq!(j("/ho"), p("/", "ho"));
        assert_eq!(j("/home/u/"), p("/home/u", ""));
        assert_eq!(j("/home//u/pr"), p("/home/u", "pr"));
        assert_eq!(j("~"), p("/home/u", ""));
        assert_eq!(j("~/"), p("/home/u", ""));
        assert_eq!(j("~/pro"), p("/home/u", "pro"));
        assert_eq!(j("~/a/./../b/x"), p("/home/u/b", "x"));
        assert_eq!(j("/../.."), p("/", ".."));
        assert_eq!(j("~x"), None);
        assert_eq!(j("abc/def"), None);
        assert_eq!(j(""), None);
        assert_eq!(parse_jump("~/x", None), None);
        assert_eq!(
            parse_jump("~/x", Some(b"/")).map(|x| x.0),
            Some(b"/".to_vec())
        );
        assert_eq!(complete("~/pr", "proj"), "~/proj/");
        assert_eq!(complete("~", "proj"), "~/proj/");
        assert_eq!(complete("/", "etc"), "/etc/");
        assert_eq!(complete("/srv/w", "web"), "/srv/web/");
    }

    #[test]
    fn project_tags() {
        let m = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let l = |v: Vec<(&'static str, Color)>| v.into_iter().map(|x| x.0).collect::<Vec<_>>();
        assert_eq!(
            l(tags(&m(&["CLAUDE.md", "Cargo.toml", "README.md"]), true)),
            vec!["claude", "rs", "git"]
        );
        assert_eq!(l(tags(&m(&["AGENTS.md", "go.mod"]), false))[0], "agents");
        assert_eq!(
            l(tags(
                &m(&["requirements.txt", "Dockerfile", "flake.nix"]),
                false
            )),
            vec!["py", "nix", "docker"]
        );
        assert_eq!(
            l(tags(&m(&["package.json", "go.mod"]), false)),
            vec!["node", "go"]
        );
        assert!(tags(&[], false).is_empty());
    }

    #[test]
    fn recent_rows_only_in_start_folder() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(
            &mut a,
            &mut r,
            "/home/u",
            &["proj", "notes", "data", "build"],
        );
        let rows = names(&a);
        // ★ rows first (relative + current folder dropped), then here / up / folders
        assert_eq!(rows[0], (Kind::Recent, "/home/u/proj".into()));
        assert_eq!(rows[1], (Kind::Recent, "/srv/web".into()));
        assert_eq!(rows[2].0, Kind::Here);
        assert_eq!(rows[3].0, Kind::Up);
        // ignore edits: +data hides data, -build shows build
        let dirs: Vec<&str> = rows[4..].iter().map(|x| x.1.as_str()).collect();
        assert_eq!(dirs, vec!["build", "notes", "proj"]);
        assert_eq!(a.rows().2, 1, "one ignored");
        // → on a ★ row navigates there
        key(&mut a, &mut r, KeyCode::Down);
        key(&mut a, &mut r, KeyCode::Right);
        assert_eq!(a.cwd.as_deref(), Some(&b"/srv/web"[..]));
        listed(&mut a, &mut r, &["site"]);
        assert!(names(&a).iter().all(|x| x.0 != Kind::Recent));
        // a query hides them in the start folder too
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        typ(&mut a, &mut r, "no");
        assert!(names(&a).iter().all(|x| x.0 != Kind::Recent));
    }

    #[test]
    fn deep_matches_follow_direct_ones_with_recent_boost() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(&mut a, &mut r, "/home/u", &["src", "proj", "x"]);
        a.cache.put_deep(
            b"/home/u",
            Deep {
                fetched: cache::now(),
                depth: 3,
                truncated: false,
                paths: [
                    "src",
                    "proj",
                    "proj/src",
                    "x/y/src",
                    "proj/data/src",
                    "x/srcs",
                ]
                .iter()
                .map(|s| s.as_bytes().to_vec())
                .collect(),
            },
        );
        a.gen += 1;
        typ(&mut a, &mut r, "src");
        let rows = names(&a);
        assert_eq!(rows[0], (Kind::Dir, "src".into()));
        let deep: Vec<&str> = rows
            .iter()
            .filter(|x| x.0 == Kind::Deep)
            .map(|x| x.1.as_str())
            .collect();
        // depth-1 entries are the direct rows; data/ is ignored (edit)
        assert_eq!(deep, vec!["proj/src", "x/y/src", "x/srcs"]);
        // recent folders rank up
        a.recent = vec![b"/home/u/x/y/src".to_vec()];
        a.gen += 1;
        let deep: Vec<String> = names(&a)
            .into_iter()
            .filter(|x| x.0 == Kind::Deep)
            .map(|x| x.1)
            .collect();
        assert_eq!(deep[0], "x/y/src");
        // a deep row opens (and starts) relative to the current folder
        let i = a
            .rows()
            .0
            .iter()
            .position(|x| x.kind == Kind::Deep)
            .unwrap();
        let row = a.rows().0[i].clone();
        assert_eq!(a.row_path(&row).unwrap(), b"/home/u/x/y/src");
    }

    #[test]
    fn path_jump_navigates_and_errors_inline() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(&mut a, &mut r, "/home/u", &["proj"]);
        typ(&mut a, &mut r, "/srv/w");
        // stat-checked first: still here, rows empty while on the way
        let (id, d) = a.jump_req.clone().unwrap();
        assert_eq!(d, b"/srv");
        assert!(a.jump_pending() && a.rows().0.is_empty());
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, b"/srv".to_vec())),
            },
            &mut r,
        );
        assert_eq!(a.cwd.as_deref(), Some(&b"/srv"[..]));
        assert_eq!(a.query, "/srv/w", "the query survives the jump");
        listed(&mut a, &mut r, &["web", "www-old", "db"]);
        assert_eq!(names(&a)[0], (Kind::Dir, "web".into()));
        // Tab completes the top match + '/', which jumps again
        key(&mut a, &mut r, KeyCode::Tab);
        assert_eq!(a.query, "/srv/web/");
        let (id, d) = a.jump_req.clone().unwrap();
        assert_eq!(d, b"/srv/web");
        a.on_remote(
            Ev::Stat {
                id,
                res: Err("missing".into()),
            },
            &mut r,
        );
        assert_eq!(a.cwd.as_deref(), Some(&b"/srv"[..]), "a bad path stays put");
        assert_eq!(a.jump_err.as_deref(), Some("/srv/web: missing"));
        let s = screen(&mut a, 120, 16);
        assert!(s.contains("✗ /srv/web: missing"), "{s}");
        // backspace back into the current folder clears it
        key(&mut a, &mut r, KeyCode::Backspace);
        assert_eq!(a.jump_err, None);
        assert!(!a.jump_pending());
        key(&mut a, &mut r, KeyCode::Esc);
        assert_eq!(a.query, "");
        // `~` jumps home (stat-checked like any path)
        typ(&mut a, &mut r, "~");
        assert_eq!(a.jump_req.as_ref().unwrap().1, b"/home/u");
    }

    #[test]
    fn enter_moves_i_starts_n_makes_a_folder() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(&mut a, &mut r, "/home/u/proj", &["src", "docs"]);
        // enter on `..` goes up (it never starts a session)
        let up = names(&a).iter().position(|(k, _)| *k == Kind::Up).unwrap();
        a.sel = up;
        key(&mut a, &mut r, KeyCode::Enter);
        assert!(a.choose.is_none());
        assert_eq!(a.cwd.as_deref(), Some(&b"/home/u"[..]));
        listed(&mut a, &mut r, &["proj", "notes"]);
        // i on `..` starts in the folder on screen
        let up = names(&a).iter().position(|(k, _)| *k == Kind::Up).unwrap();
        a.sel = up;
        key(&mut a, &mut r, KeyCode::Char('i'));
        assert_eq!(a.choose.as_ref().unwrap().1, b"/home/u");
        a.choose = None;
        // n: a new folder, made there and selected
        key(&mut a, &mut r, KeyCode::Char('n'));
        typ_name(&mut a, &mut r, "fresh");
        key(&mut a, &mut r, KeyCode::Enter);
        let (id, p) = a.mkdir_req.clone().unwrap();
        assert_eq!(p, b"/home/u/fresh");
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((2, p.clone())),
            },
            &mut r,
        );
        listed(&mut a, &mut r, &["fresh", "notes", "proj"]);
        let sel = names(&a)[a.sel].1.clone();
        assert!(sel.contains("fresh"), "{sel}");
        // letters are commands until / starts a search
        key(&mut a, &mut r, KeyCode::Char('j'));
        assert_eq!(a.query, "");
        typ(&mut a, &mut r, "not");
        assert_eq!(a.query, "not");
    }

    fn typ_name(a: &mut App, r: &mut Remote, s: &str) {
        for ch in s.chars() {
            key(a, r, KeyCode::Char(ch));
        }
    }

    /// ctrl+t: the browser is for a terminal, so `i` opens one, whoever runs there.
    #[test]
    fn a_browse_for_a_terminal_opens_one() {
        let mut r = dead();
        let s = Session {
            name: "ignite".into(),
            label: "claude_ignite:proj".into(),
            cwd: "/home/u/proj/".into(),
            ..Default::default()
        };
        let mut a = app(vec![s]);
        a.terminal_only = true;
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        let sc = screen(&mut a, 120, 20);
        assert!(
            sc.contains("Terminal in u") && sc.contains("terminal here"),
            "{sc}"
        );
        key(&mut a, &mut r, KeyCode::Char('i'));
        let (id, t) = a.choose.clone().unwrap();
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, t.clone())),
            },
            &mut r,
        );
        assert!(a.ask.is_none());
        assert!(matches!(&a.done, Some(Some(Picked::Terminal(_)))));
    }

    /// `t`: a terminal in the folder, even beside a running agent. Picking for
    /// Quick start says so: nothing starts here, and enter takes the folder.
    #[test]
    fn t_opens_a_terminal_and_choosing_only_hands_a_folder_back() {
        let mut r = dead();
        let s = Session {
            name: "ignite".into(),
            label: "claude_ignite:proj".into(),
            cwd: "/home/u/proj/".into(),
            ..Default::default()
        };
        let mut a = app(vec![s]);
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        key(&mut a, &mut r, KeyCode::Char('t'));
        let (id, t) = a.choose.clone().unwrap();
        assert_eq!(t, b"/home/u/proj");
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, t.clone())),
            },
            &mut r,
        );
        assert!(a.ask.is_none());
        assert!(matches!(&a.done, Some(Some(Picked::Terminal(p))) if p == b"/home/u/proj"));

        let mut a = app(vec![]);
        a.choosing = true;
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        a.sel = a
            .rows()
            .0
            .iter()
            .position(|x| x.kind == Kind::Here)
            .unwrap();
        let sc = screen(&mut a, 120, 20);
        assert!(sc.contains("Use u") && !sc.contains("Start Codex"), "{sc}");
        assert!(sc.contains("use this folder"), "{sc}");
        key(&mut a, &mut r, KeyCode::Enter);
        let (id, t) = a.choose.clone().unwrap();
        assert_eq!(t, b"/home/u");
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, t.clone())),
            },
            &mut r,
        );
        assert!(matches!(&a.done, Some(Some(Picked::New(p))) if p == b"/home/u"));
    }

    #[test]
    fn existing_session_prompt() {
        let mut r = dead();
        let s = Session {
            name: "ignite".into(),
            label: "claude_ignite:proj".into(),
            cwd: "/home/u/proj/".into(),
            ..Default::default()
        };
        let mut a = app(vec![s.clone()]);
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        // row 0 is the ★ /home/u/proj recent; i starts there
        key(&mut a, &mut r, KeyCode::Char('i'));
        let (id, t) = a.choose.clone().unwrap();
        assert_eq!(t, b"/home/u/proj");
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, t.clone())),
            },
            &mut r,
        );
        assert!(a.done.is_none() && a.ask.is_some());
        let sc = screen(&mut a, 120, 20);
        assert!(sc.contains("is already running here"), "{sc}");
        assert!(sc.contains("new anyway"), "{sc}");
        // n = new anyway
        key(&mut a, &mut r, KeyCode::Char('n'));
        assert!(matches!(&a.done, Some(Some(Picked::New(p))) if p == b"/home/u/proj"));
        // a = attach
        a.done = None;
        a.ask = Some((t.clone(), vec![s.clone()]));
        key(&mut a, &mut r, KeyCode::Char('a'));
        assert!(matches!(&a.done, Some(Some(Picked::Attach(x))) if x.name == "ignite"));
        // esc goes back to browsing
        a.done = None;
        a.ask = Some((t, vec![s]));
        key(&mut a, &mut r, KeyCode::Esc);
        assert!(a.done.is_none() && a.ask.is_none());
        // a folder without a session is handed straight back
        key(&mut a, &mut r, KeyCode::Down);
        key(&mut a, &mut r, KeyCode::Down);
        key(&mut a, &mut r, KeyCode::Char('i')); // "Start Codex in u"
        let (id, t) = a.choose.clone().unwrap();
        a.on_remote(
            Ev::Stat {
                id,
                res: Ok((1, t)),
            },
            &mut r,
        );
        assert!(matches!(&a.done, Some(Some(Picked::New(p))) if p == b"/home/u"));
    }

    #[test]
    fn mouse_click_double_click_and_wheel() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(
            &mut a,
            &mut r,
            "/home/u",
            &["a1", "a2", "a3", "a4", "a5", "a6"],
        );
        let _ = screen(&mut a, 120, 12);
        let inner = a.list_inner;
        let m = |kind, row| MouseEvent {
            kind,
            column: inner.x + 2,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let y0 = inner.y;
        let click = m(MouseEventKind::Down(MouseButton::Left), y0 + 4);
        a.on_mouse(click, &mut r);
        assert_eq!(a.sel, 4);
        assert!(a.choose.is_none());
        a.on_mouse(click, &mut r); // double-click = open (moves into it)
        assert!(a.choose.is_none(), "a double-click never starts a session");
        assert_eq!(a.cwd.as_deref(), Some(&b"/home/u/a1"[..]));
        open(
            &mut a,
            &mut r,
            "/home/u",
            &["a1", "a2", "a3", "a4", "a5", "a6"],
        );
        let _ = screen(&mut a, 120, 12);
        a.sel = 4;
        a.on_mouse(m(MouseEventKind::ScrollDown, y0), &mut r);
        assert_eq!(a.sel, 7);
        a.on_mouse(m(MouseEventKind::ScrollDown, y0), &mut r);
        assert_eq!(a.sel, 9, "clamped to the last row");
        a.on_mouse(m(MouseEventKind::ScrollUp, y0), &mut r);
        assert_eq!(a.sel, 6);
        // the list scrolled; clicks map through the offset
        a.sel = 9;
        let _ = screen(&mut a, 120, 12);
        let off = a.list_off;
        assert!(off > 0);
        a.on_mouse(m(MouseEventKind::Down(MouseButton::Left), y0), &mut r);
        assert_eq!(a.sel, off);
        // outside the list: ignored
        let before = a.sel;
        a.on_mouse(
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 110,
                row: y0 + 1,
                modifiers: KeyModifiers::NONE,
            },
            &mut r,
        );
        assert_eq!(a.sel, before);
    }

    #[test]
    fn renders_stripe_recent_tags_and_deep_rows() {
        let mut r = dead();
        let mut a = app(vec![]);
        open(&mut a, &mut r, "/home/u", &["proj", "notes"]);
        a.infos.insert(
            b"/home/u/proj".to_vec(),
            (
                Info {
                    markers: vec!["Cargo.toml".into(), "CLAUDE.md".into()],
                    branch: Some("main".into()),
                    ..Default::default()
                },
                Instant::now(),
            ),
        );
        let s = screen(&mut a, 120, 16);
        let first = s.lines().next().unwrap();
        assert!(first.starts_with("▌ℵ h"), "{first}");
        assert!(s.contains("★ ~/proj"), "{s}");
        assert!(s.contains("★ /srv/web"), "{s}");
        assert!(s.contains("claude rs git"), "{s}");
        assert!(s.contains("Start Codex in u"), "{s}");

        a.cache.put_deep(
            b"/home/u",
            Deep {
                fetched: cache::now(),
                depth: 3,
                truncated: true,
                paths: vec![b"notes/2024/talks".to_vec()],
            },
        );
        a.gen += 1;
        typ(&mut a, &mut r, "talk");
        let s = screen(&mut a, 120, 16);
        assert!(s.contains("notes/2024/talks/"), "{s}");
        assert!(s.contains("1 deeper"), "{s}");
        assert!(s.contains("index capped"), "{s}");
        // narrow terminals still render; a long deep path keeps its name
        a.cache.put_deep(
            b"/home/u",
            Deep {
                fetched: cache::now(),
                depth: 3,
                truncated: false,
                paths: vec![b"notes/some_rather_long_folder/another_long_one/talks".to_vec()],
            },
        );
        a.gen += 1;
        let s = screen(&mut a, 40, 10);
        assert!(s.contains("…") && s.contains("talks/"), "{s}");
    }
}
