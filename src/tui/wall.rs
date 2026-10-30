//! Focus and Tile — an overlay over the tabs: enter on a session opens it in
//! Focus (one agent fullscreen), `w` in Fleet (or `aleph wall [names…]`)
//! tiles sessions side by side, and splitting Focus turns it into a tile. It's
//! a multi-host, multi-agent terminal multiplexer: windows hold a tree of
//! split panes; each pane shows a live session from any machine (or is
//! empty, waiting for one). It speaks both dialects:
//!
//! * **tmux** — prefix `ctrl+b` (setting `mux_prefix`), then `%` split side by
//!   side, `"` split stacked, arrows/hjkl move, `o` next pane, `x` close
//!   pane, `z` zoom, `c` new window, `n`/`p`/`0`–`9` windows, `,` rename, `&`
//!   close window, `space` next layout, `{`/`}` swap, `ctrl+arrows` resize,
//!   `q` pane numbers, `[` stop typing, `d` back to Fleet. The prefix also
//!   works while typing into a pane (press it twice to send it through).
//! * **vim** — in normal mode `h j k l` move, `s`/`v` split (stacked / side
//!   by side), `ctrl+w` then `s v h j k l w W q o = < > + - x`, `[`/`]`
//!   previous/next window.
//!
//! Focus opens typing into its agent (`focus.keys`): every key as its own
//! terminal sends it, over the pane's control stream (`stream.rs`), pastes
//! as one; ctrl+q (`focus.exit`) gives the keys back to Aleph and a second
//! leaves Focus — esc is the agent's then, and interrupts it, so the first is
//! held back with a warning (`focus.esc_guard`; a second within 3 s goes) — and
//! a click outside gives the keys back too.
//! The wheel scrolls the pane under it (herdr's history, or the agent's own
//! scrolling when it asked for the mouse); clicks go to an agent that takes
//! them. A drag across any pane selects what it shows and copies it on release
//! (`focus.copy_on_select`), the screen held still meanwhile; option+drag goes
//! to the agent.
//!
//! In a session pane, `i` (or enter) opens the message bar at its bottom:
//! enter sends now, tab queues it for after the current reply (the hub sends
//! it once the agent is done), ↑ recalls, esc closes (the draft stays). `I`
//! types raw keys into the pane (`ctrl+q`, `mux_leave` or a click outside it stops; `ctrl+q` again leaves), `Q` shows that pane's
//! queue, `a` attaches in herdr (`A` straight to the agent, `O` in a new
//! terminal tab) and `X` closes the session. esc goes back to the tab it was
//! opened from.
//!
//! Streams run only for panes in the current window. Windows you make
//! yourself are saved (`mux.json`) and come back; Focus and Tile are rebuilt
//! each time they open.

use super::{forms, Dash, Handoff, Level, Mode, View};
use crate::chan::{self, Chan};
use crate::herdr::{self, Session};
use crate::stream::Stream;
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

// ------------------------------------------------------------------ the layout tree

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// panes side by side (a vertical divider) — tmux `%`, vim `:vsplit`
    Side,
    /// panes stacked (a horizontal divider) — tmux `"`, vim `:split`
    Stack,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Node {
    Leaf(u32),
    Split {
        dir: Dir,
        /// share of the first child (0.1..0.9)
        ratio: f32,
        a: Box<Node>,
        b: Box<Node>,
    },
}

impl Node {
    pub fn leaves(&self) -> Vec<u32> {
        match self {
            Node::Leaf(id) => vec![*id],
            Node::Split { a, b, .. } => {
                let mut v = a.leaves();
                v.extend(b.leaves());
                v
            }
        }
    }
    fn contains(&self, id: u32) -> bool {
        match self {
            Node::Leaf(x) => *x == id,
            Node::Split { a, b, .. } => a.contains(id) || b.contains(id),
        }
    }
    /// Replace leaf `id` with a split holding `id` and `new`.
    pub fn split(&mut self, id: u32, new: u32, dir: Dir) -> bool {
        match self {
            Node::Leaf(x) if *x == id => {
                *self = Node::Split {
                    dir,
                    ratio: 0.5,
                    a: Box::new(Node::Leaf(id)),
                    b: Box::new(Node::Leaf(new)),
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => a.split(id, new, dir) || b.split(id, new, dir),
        }
    }
    /// Remove leaf `id`; its sibling takes the parent's place. False when
    /// `id` is the only leaf.
    pub fn remove(&mut self, id: u32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { a, b, .. } => {
                if matches!(**a, Node::Leaf(x) if x == id) {
                    *self = (**b).clone();
                    true
                } else if matches!(**b, Node::Leaf(x) if x == id) {
                    *self = (**a).clone();
                    true
                } else {
                    a.remove(id) || b.remove(id)
                }
            }
        }
    }
    /// Grow (`delta` > 0) or shrink pane `id` along `dir`, moving the
    /// closest enclosing divider of that direction.
    pub fn resize(&mut self, id: u32, dir: Dir, delta: f32) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split {
                dir: d,
                ratio,
                a,
                b,
            } => {
                let in_a = a.contains(id);
                if !in_a && !b.contains(id) {
                    return false;
                }
                let deeper = if in_a {
                    a.resize(id, dir, delta)
                } else {
                    b.resize(id, dir, delta)
                };
                if deeper {
                    return true;
                }
                if *d == dir {
                    let s = if in_a { delta } else { -delta };
                    *ratio = (*ratio + s).clamp(0.1, 0.9);
                    return true;
                }
                false
            }
        }
    }
    /// Every divider proportional to the panes on each side.
    pub fn equalize(&mut self) {
        if let Node::Split { ratio, a, b, .. } = self {
            let (na, nb) = (a.leaves().len() as f32, b.leaves().len() as f32);
            *ratio = na / (na + nb);
            a.equalize();
            b.equalize();
        }
    }
    /// Rects of every leaf inside `r`, plus every divider (for mouse drags):
    /// (path to the split, the split's rect, its direction).
    pub fn layout(
        &self,
        r: Rect,
        out: &mut Vec<(u32, Rect)>,
        dividers: &mut Vec<(Vec<bool>, Rect, Dir)>,
        path: &mut Vec<bool>,
    ) {
        match self {
            Node::Leaf(id) => out.push((*id, r)),
            Node::Split { dir, ratio, a, b } => {
                let (ra, rb) = split_rect(r, *dir, *ratio);
                dividers.push((path.clone(), r, *dir));
                path.push(false);
                a.layout(ra, out, dividers, path);
                path.pop();
                path.push(true);
                b.layout(rb, out, dividers, path);
                path.pop();
            }
        }
    }
    fn at_path(&mut self, path: &[bool]) -> Option<&mut Node> {
        match (path.split_first(), self) {
            (None, n) => Some(n),
            (Some((first, rest)), Node::Split { a, b, .. }) => {
                if *first {
                    b.at_path(rest)
                } else {
                    a.at_path(rest)
                }
            }
            _ => None,
        }
    }
    fn ratio_at(&self, path: &[bool]) -> Option<f32> {
        match (path.split_first(), self) {
            (None, Node::Split { ratio, .. }) => Some(*ratio),
            (Some((f, rest)), Node::Split { a, b, .. }) => {
                if *f {
                    b.ratio_at(rest)
                } else {
                    a.ratio_at(rest)
                }
            }
            _ => None,
        }
    }
    fn to_json(&self) -> Value {
        match self {
            Node::Leaf(id) => json!({ "pane": id }),
            Node::Split { dir, ratio, a, b } => json!({
                "dir": if *dir == Dir::Side { "side" } else { "stack" },
                "ratio": ratio, "a": a.to_json(), "b": b.to_json(),
            }),
        }
    }
    fn from_json(v: &Value) -> Option<Node> {
        if let Some(p) = v.get("pane").and_then(Value::as_u64) {
            return Some(Node::Leaf(p as u32));
        }
        Some(Node::Split {
            dir: if v.get("dir")?.as_str()? == "side" {
                Dir::Side
            } else {
                Dir::Stack
            },
            ratio: (v.get("ratio")?.as_f64()? as f32).clamp(0.1, 0.9),
            a: Box::new(Node::from_json(v.get("a")?)?),
            b: Box::new(Node::from_json(v.get("b")?)?),
        })
    }
    /// The same shape as herdr's layout tree (side by side = `right`).
    pub fn to_tree(&self) -> crate::layouts::Tree<u32> {
        use crate::layouts::{Dir as H, Tree};
        match self {
            Node::Leaf(id) => Tree::Pane(*id),
            Node::Split { dir, ratio, a, b } => Tree::split(
                if *dir == Dir::Side { H::Right } else { H::Down },
                *ratio,
                a.to_tree(),
                b.to_tree(),
            ),
        }
    }
}

fn split_rect(r: Rect, dir: Dir, ratio: f32) -> (Rect, Rect) {
    match dir {
        Dir::Side => {
            let wa = ((r.width as f32 * ratio).round() as u16).min(r.width);
            (
                Rect { width: wa, ..r },
                Rect {
                    x: r.x + wa,
                    width: r.width - wa,
                    ..r
                },
            )
        }
        Dir::Stack => {
            let ha = ((r.height as f32 * ratio).round() as u16).min(r.height);
            (
                Rect { height: ha, ..r },
                Rect {
                    y: r.y + ha,
                    height: r.height - ha,
                    ..r
                },
            )
        }
    }
}

/// A tree for `ids` in one of the preset layouts (tiled, side by side,
/// stacked, main + stack).
pub fn preset(ids: &[u32], which: usize) -> Node {
    fn chain(ids: &[u32], dir: Dir) -> Node {
        let mut it = ids.iter().rev();
        let mut n = Node::Leaf(*it.next().unwrap_or(&0));
        let mut count = 1.0f32;
        for i in it {
            count += 1.0;
            n = Node::Split {
                dir,
                ratio: 1.0 / count,
                a: Box::new(Node::Leaf(*i)),
                b: Box::new(n),
            };
        }
        n
    }
    if ids.len() <= 1 {
        return Node::Leaf(*ids.first().unwrap_or(&0));
    }
    match which % LAYOUTS.len() {
        0 => {
            let cols = (ids.len() as f64).sqrt().ceil() as usize;
            let rows: Vec<Node> = ids.chunks(cols).map(|r| chain(r, Dir::Side)).collect();
            let mut it = rows.into_iter().rev();
            let mut n = it.next().unwrap_or(Node::Leaf(0));
            let mut count = 1.0f32;
            for r in it {
                count += 1.0;
                n = Node::Split {
                    dir: Dir::Stack,
                    ratio: 1.0 / count,
                    a: Box::new(r),
                    b: Box::new(n),
                };
            }
            n
        }
        1 => chain(ids, Dir::Side),
        2 => chain(ids, Dir::Stack),
        _ => Node::Split {
            dir: Dir::Side,
            ratio: 0.6,
            a: Box::new(Node::Leaf(ids[0])),
            b: Box::new(chain(&ids[1..], Dir::Stack)),
        },
    }
}

const LAYOUTS: [&str; 4] = ["tiled", "side by side", "stacked", "main + stack"];

// ------------------------------------------------------------------ state

pub struct Window {
    pub name: String,
    pub root: Node,
    pub focus: u32,
    pub zoom: bool,
    layout: usize,
}

/// A drag across a pane's screen: the text to copy.
pub(crate) struct Selection {
    pane: u32,
    /// the screen it's on, and the two ends (terminal cells, inside it)
    area: Rect,
    anchor: (u16, u16),
    cursor: (u16, u16),
    moved: bool,
    /// let go and copied: when the highlight goes
    fades: Option<Instant>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    Empty,
    /// a session (`host|pane`)
    Session(String),
    /// a session being started for this pane (what, for the label)
    Starting(String),
}

pub(crate) struct Pane {
    pub content: Content,
    /// its session has been seen in the fleet (it can end, not just start)
    seen: bool,
    stream: Option<Stream>,
    /// a tile size the stream should move to, and since when (debounce)
    want: Option<((u16, u16), Instant)>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Pending {
    None,
    /// the tmux prefix was pressed
    Prefix,
    /// vim's ctrl+w was pressed
    Win,
}

pub(crate) struct State {
    pub windows: Vec<Window>,
    pub cur: usize,
    panes: HashMap<u32, Pane>,
    pub(crate) selection: Option<Selection>,
    next: u32,
    /// keys go to the focused pane
    pub typing: bool,
    /// esc while typing (`focus.esc_guard`): when the first was held back,
    /// and when the last one went through to the agent
    esc_held: Option<Instant>,
    esc_sent: Option<Instant>,
    pending: Pending,
    /// since when a prefix waits for its key (the which-key popup after 400 ms)
    pending_since: Option<Instant>,
    pinned: Vec<String>,
    loaded: bool,
    chans: HashMap<String, Chan>,
    rects: Vec<(u32, Rect)>,
    dividers: Vec<(Vec<bool>, Rect, Dir)>,
    tabs: Vec<(Rect, usize)>,
    drag: Option<(Vec<bool>, Rect, Dir)>,
    numbers: Option<Instant>,
    pub cheats: bool,
    renaming: Option<forms::Field>,
    dirty: bool,
    /// the message bar open on a pane
    pub compose: Option<Compose>,
    /// unsent text per session key
    drafts: HashMap<String, String>,
    /// messages sent per session key, for ↑ recall
    sent: HashMap<String, Vec<String>>,
    /// the queue overlay (`Q`): selected row
    pub queue_view: Option<usize>,
    /// zoomed automatically for typing (undone after)
    auto_zoom: bool,
    /// where esc goes back to (Focus and Tile are opened from Fleet, Inbox, …)
    pub back: Option<View>,
    /// Focus's timeline (`t`): None = on when the screen is wide
    pub timeline: Option<bool>,
    /// the width the panes last had, the timeline included (0: not drawn yet)
    body_w: u16,
    /// the click that moved focus to a tile: its release isn't the agent's
    swallow_up: bool,
    /// opened from the approval board to answer: leaving goes back to it
    /// while questions still wait
    pub back_to_board: bool,
}

/// The window Focus uses (one agent, fullscreen) and the one Tile uses.
pub const FOCUS: &str = "focus";
pub const TILE: &str = "tile";

/// The message bar at the bottom of a session pane.
pub(crate) struct Compose {
    pane: u32,
    key: String,
    field: forms::Field,
    recall: Option<usize>,
}

impl State {
    pub fn new(pinned: &[String]) -> State {
        State {
            windows: vec![],
            cur: 0,
            panes: HashMap::new(),
            selection: None,
            next: 1,
            typing: false,
            esc_held: None,
            esc_sent: None,
            pending: Pending::None,
            pending_since: None,
            pinned: pinned.to_vec(),
            loaded: false,
            chans: HashMap::new(),
            rects: vec![],
            dividers: vec![],
            tabs: vec![],
            drag: None,
            numbers: None,
            cheats: false,
            renaming: None,
            dirty: false,
            compose: None,
            drafts: HashMap::new(),
            sent: HashMap::new(),
            queue_view: None,
            auto_zoom: false,
            back: None,
            timeline: None,
            body_w: 0,
            swallow_up: false,
            back_to_board: false,
        }
    }
    /// Is the current window Focus (one agent, fullscreen)?
    pub fn in_focus(&self) -> bool {
        self.win().is_some_and(|w| w.name == FOCUS)
    }
    /// Is the current window Tile (sessions side by side)?
    pub fn in_tile(&self) -> bool {
        self.win().is_some_and(|w| w.name == TILE)
    }
    /// Each session's tile as last drawn (`host|pane`, where): showcase
    /// mode's celebrations.
    pub(crate) fn places(&self) -> Vec<(String, Rect)> {
        self.rects
            .iter()
            .filter_map(|(id, r)| match &self.panes.get(id)?.content {
                Content::Session(k) => Some((k.clone(), *r)),
                _ => None,
            })
            .collect()
    }
    /// The session in the focused pane.
    pub(crate) fn focused_session(&self, d: &Dash) -> Option<Session> {
        session_of(d, self.focus()?)
    }
    /// Panes in the current window.
    pub fn window_len(&self) -> usize {
        self.win().map_or(0, |w| w.root.leaves().len())
    }
    /// Drop a named window (Focus and Tile are rebuilt each time).
    fn drop_window(&mut self, name: &str) {
        if let Some(i) = self.windows.iter().position(|w| w.name == name) {
            let w = self.windows.remove(i);
            for id in w.root.leaves() {
                self.panes.remove(&id);
            }
            if self.cur >= self.windows.len() {
                self.cur = self.windows.len().saturating_sub(1);
            }
        }
    }
    /// Fleet's `w`: these sessions in a new tiled window.
    pub fn pin(&mut self, keys: Vec<String>) {
        self.pinned = keys;
    }
    /// Stop every stream and channel (leaving the view, or before herdr runs).
    pub fn drop_all(&mut self) {
        for p in self.panes.values_mut() {
            p.stream = None;
        }
        self.chans.clear();
        self.typing = false;
        self.pending = Pending::None;
        self.close_compose();
        self.queue_view = None;
    }
    /// Close the message bar, keeping its text as the pane's draft.
    fn close_compose(&mut self) {
        if let Some(c) = self.compose.take() {
            if c.field.text.trim().is_empty() {
                self.drafts.remove(&c.key);
            } else {
                self.drafts.insert(c.key, c.field.text);
            }
        }
        self.unzoom();
    }
    /// Zoom a cramped pane while typing into it.
    fn zoom_for_typing(&mut self) {
        let small = self
            .focus()
            .and_then(|id| self.rects.iter().find(|(x, _)| *x == id))
            .is_some_and(|(_, r)| r.width < 60 || r.height < 12);
        if let Some(w) = self.win_mut() {
            if small && !w.zoom && w.root.leaves().len() > 1 {
                w.zoom = true;
                self.auto_zoom = true;
            }
        }
    }
    fn unzoom(&mut self) {
        if std::mem::take(&mut self.auto_zoom) {
            if let Some(w) = self.win_mut() {
                w.zoom = false;
            }
        }
    }
    /// Does the view want every key right now (typing, a pending prefix, …)?
    pub fn captures(&self) -> bool {
        self.typing
            || self.pending != Pending::None
            || self.renaming.is_some()
            || self.cheats
            || self.compose.is_some()
            || self.queue_view.is_some()
    }
    /// Panes holding a session (the tab badge).
    pub fn sessions(&self) -> usize {
        self.panes
            .values()
            .filter(|p| matches!(p.content, Content::Session(_)))
            .count()
    }
    pub(crate) fn win(&self) -> Option<&Window> {
        self.windows.get(self.cur)
    }
    fn win_mut(&mut self) -> Option<&mut Window> {
        self.windows.get_mut(self.cur)
    }
    fn focus(&self) -> Option<u32> {
        self.win().map(|w| w.focus)
    }
    pub(crate) fn content_of_focus(&self) -> Option<&Content> {
        self.focus()
            .and_then(|id| self.panes.get(&id))
            .map(|p| &p.content)
    }
    fn new_pane(&mut self, content: Content) -> u32 {
        let id = self.next;
        self.next += 1;
        self.panes.insert(
            id,
            Pane {
                content,
                seen: false,
                stream: None,
                want: None,
            },
        );
        id
    }
    fn new_window(&mut self, name: &str, contents: Vec<Content>, layout: usize) {
        let ids: Vec<u32> = if contents.is_empty() {
            vec![self.new_pane(Content::Empty)]
        } else {
            contents.into_iter().map(|c| self.new_pane(c)).collect()
        };
        self.windows.push(Window {
            name: name.to_string(),
            root: preset(&ids, layout),
            focus: ids[0],
            zoom: false,
            layout,
        });
        self.cur = self.windows.len() - 1;
        self.dirty = true;
    }
}

// ------------------------------------------------------------------ persistence

fn file() -> std::path::PathBuf {
    crate::config::world_state().join("mux.json")
}

fn save(st: &State) {
    if cfg!(test) {
        return;
    }
    // Focus and Tile are rebuilt every time they open: only your own
    // windows are kept
    let kept = |w: &&Window| w.name != FOCUS && w.name != TILE;
    let cur = st.windows.iter().take(st.cur).filter(kept).count();
    let wins: Vec<Value> = st
        .windows
        .iter()
        .filter(kept)
        .map(|w| {
            let panes: serde_json::Map<String, Value> = w
                .root
                .leaves()
                .iter()
                .map(|id| {
                    let c = match st.panes.get(id).map(|p| &p.content) {
                        Some(Content::Session(k)) => json!(k),
                        _ => Value::Null,
                    };
                    (id.to_string(), c)
                })
                .collect();
            json!({"name": w.name, "root": w.root.to_json(), "focus": w.focus, "layout": w.layout, "panes": panes})
        })
        .collect();
    let doc = json!({"version": 1, "cur": cur.min(wins.len().saturating_sub(1)), "windows": wins});
    let _ = crate::config::write_atomic(&file(), doc.to_string().as_bytes());
}

fn load(st: &mut State) -> bool {
    if cfg!(test) {
        return false;
    }
    let Some(v) = std::fs::read_to_string(file())
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
    else {
        return false;
    };
    let Some(ws) = v.get("windows").and_then(Value::as_array) else {
        return false;
    };
    for w in ws {
        let (Some(name), Some(root)) = (
            w.get("name").and_then(Value::as_str),
            w.get("root").and_then(Node::from_json),
        ) else {
            continue;
        };
        let leaves = root.leaves();
        for id in &leaves {
            let c = w
                .pointer(&format!("/panes/{id}"))
                .and_then(Value::as_str)
                .map(|k| Content::Session(k.to_string()))
                .unwrap_or(Content::Empty);
            st.panes.insert(
                *id,
                Pane {
                    seen: false,
                    content: c,
                    stream: None,
                    want: None,
                },
            );
            st.next = st.next.max(id + 1);
        }
        let focus = w
            .get("focus")
            .and_then(Value::as_u64)
            .map(|x| x as u32)
            .filter(|f| root.contains(*f))
            .unwrap_or(leaves[0]);
        st.windows.push(Window {
            name: name.to_string(),
            root,
            focus,
            zoom: false,
            layout: w.get("layout").and_then(Value::as_u64).unwrap_or(0) as usize,
        });
    }
    st.cur = (v.get("cur").and_then(Value::as_u64).unwrap_or(0) as usize)
        .min(st.windows.len().saturating_sub(1));
    !st.windows.is_empty()
}

// ------------------------------------------------------------------ entering

fn resolve(d: &Dash, q: &str) -> Option<String> {
    let all: Vec<Session> = d.fleet.sessions().into_iter().cloned().collect();
    if let Some(s) = all.iter().find(|s| s.key() == q) {
        return Some(s.key());
    }
    match q.split_once(':') {
        Some((h, n)) if d.fleet.host(h).is_some() => {
            let on: Vec<Session> = all.iter().filter(|s| s.host == h).cloned().collect();
            herdr::pick(&on, n).map(|s| s.key())
        }
        _ => herdr::pick(&all, q).map(|s| s.key()),
    }
}

/// Every session, needs-you first (the default first window).
fn default_contents(d: &Dash) -> Vec<Content> {
    let mut v: Vec<&Session> = d.fleet.sessions();
    v.sort_by_key(|s| match s.status.as_str() {
        "blocked" => 0,
        "working" => 1,
        _ => 2,
    });
    let max = (d.cfg.num("wall_max") as usize).clamp(1, 16);
    v.into_iter()
        .take(max)
        .map(|s| Content::Session(s.key()))
        .collect()
}

pub(crate) fn enter(d: &mut Dash) {
    if !d.wall.loaded {
        d.wall.loaded = true;
        // windows of your own from before (Focus and Tile are never saved)
        load(&mut d.wall);
    }
    // `aleph wall a b c`: those sessions side by side, once a poll has seen
    // them (until every machine has answered)
    let pinned = std::mem::take(&mut d.wall.pinned);
    if !pinned.is_empty() {
        let c: Vec<String> = pinned.iter().filter_map(|q| resolve(d, q)).collect();
        let polling = d
            .fleet
            .hosts
            .iter()
            .any(|h| h.conn == crate::fleet::Conn::Unknown);
        if c.len() == pinned.len() || (!polling && !c.is_empty()) {
            d.wall.drop_window(TILE);
            d.wall.new_window(
                TILE,
                c.into_iter().map(Content::Session).collect(),
                lay_of(d),
            );
        } else if polling {
            d.wall.pinned = pinned;
        }
    }
}

/// Nothing open yet (`aleph wall` without names): every session side by side.
fn tile_everything(d: &mut Dash) {
    if d.wall.windows.is_empty() && d.wall.pinned.is_empty() {
        let c = default_contents(d);
        if !c.is_empty() {
            d.wall.new_window(TILE, c, lay_of(d));
        }
    }
}

/// The `wall_layout` setting as a layout preset.
fn lay_of(d: &Dash) -> usize {
    match d.cfg.get_or("wall_layout").as_str() {
        "cols" => 1,
        "rows" => 2,
        "main" => 3,
        _ => 0,
    }
}

/// Focus: one session fullscreen (the window `focus`, reused), with the
/// message bar, queue, approve and keys; esc goes back to `from`.
pub(crate) fn focus_on(d: &mut Dash, key: &str, from: View) {
    enter(d);
    d.wall.drop_window(FOCUS);
    d.wall
        .new_window(FOCUS, vec![Content::Session(key.to_string())], 0);
    if let Some(w) = d.wall.win_mut() {
        w.zoom = true;
    }
    d.wall.back = Some(from);
    d.wall.back_to_board = false;
    d.switch(View::Wall);
    // keys straight to the agent, as in its own terminal (`focus.keys`)
    d.wall.typing = d.cfg.get_or("focus.keys") != "aleph";
}

/// Tile: these sessions side by side (the window `tile`, rebuilt each time).
pub(crate) fn tile(d: &mut Dash, keys: Vec<String>, from: View) {
    enter(d);
    if keys.is_empty() {
        return;
    }
    d.wall.drop_window(TILE);
    let c: Vec<Content> = keys.into_iter().map(Content::Session).collect();
    d.wall.new_window(TILE, c, lay_of(d));
    d.wall.back = Some(from);
    d.switch(View::Wall);
}

/// In Focus: show the next (or previous) session — `needy` only those
/// waiting for you.
fn focus_step(d: &mut Dash, fwd: bool, needy: bool) {
    let cur = d
        .wall
        .focus()
        .and_then(|id| session_of(d, id))
        .map(|s| s.key());
    let all: Vec<Session> = d
        .fleet
        .sessions()
        .into_iter()
        .filter(|s| !needy || s.blocked())
        .cloned()
        .collect();
    if all.is_empty() {
        d.toast(
            Level::Info,
            if needy {
                "nothing needs you"
            } else {
                "no other sessions"
            },
        );
        return;
    }
    let i = cur
        .as_ref()
        .and_then(|k| all.iter().position(|s| s.key() == *k));
    let n = all.len();
    let next = match (i, fwd) {
        (Some(i), true) => (i + 1) % n,
        (Some(i), false) => (i + n - 1) % n,
        (None, _) => 0,
    };
    let key = all[next].key();
    if Some(&key) == cur.as_ref() {
        return;
    }
    show_in_focus(d, key);
}

/// The focused pane shows this session instead.
fn show_in_focus(d: &mut Dash, key: String) {
    if let Some(id) = d.wall.focus() {
        if let Some(p) = d.wall.panes.get_mut(&id) {
            p.content = Content::Session(key);
            p.stream = None;
            p.want = None;
        }
    }
    d.wall.close_compose();
}

/// The next machine with sessions: the one waiting for you there, else its first.
fn focus_machine(d: &mut Dash, fwd: bool) {
    let cur = d
        .wall
        .focus()
        .and_then(|id| session_of(d, id))
        .map(|s| s.host.clone());
    let mut hosts: Vec<String> = Vec::new();
    for s in d.fleet.sessions() {
        if !hosts.contains(&s.host) {
            hosts.push(s.host.clone());
        }
    }
    let n = hosts.len();
    if n == 0 {
        d.toast(Level::Info, "no sessions on any machine");
        return;
    }
    let next = match cur.as_ref().and_then(|c| hosts.iter().position(|h| h == c)) {
        Some(i) if fwd => (i + 1) % n,
        Some(i) => (i + n - 1) % n,
        None => 0,
    };
    if cur.as_ref() == Some(&hosts[next]) {
        d.toast(Level::Info, "no other machine has sessions");
        return;
    }
    let on_it: Vec<&Session> = d
        .fleet
        .sessions()
        .into_iter()
        .filter(|s| s.host == hosts[next])
        .collect();
    let pick = on_it.iter().find(|s| s.blocked()).or(on_it.first());
    if let Some(key) = pick.map(|s| s.key()) {
        show_in_focus(d, key);
    }
}

/// A numbered question on the focused session's screen (`❯ 1. Yes` …), to
/// answer with the digit keys: only while herdr says the session is blocked,
/// and only a menu with a cursor on one option (a numbered list in the
/// agent's output never counts).
pub(crate) fn quick_answers(d: &Dash) -> Option<(Session, Vec<(u32, String)>)> {
    let id = d.wall.focus()?;
    let s = session_of(d, id)?;
    if !s.blocked() {
        return None;
    }
    let text = d.wall.panes.get(&id)?.stream.as_ref()?.text();
    let opts = menu(&text);
    (!opts.is_empty()).then_some((s, opts))
}

/// The menu around the last cursor line: options numbered 1, 2, … in a row
/// (up to two wrapped or description lines between them).
pub(crate) fn menu(screen: &str) -> Vec<(u32, String)> {
    fn numbered(l: &str) -> Option<(bool, u32, String)> {
        let t = l.trim_start();
        let (cur, t) = match t.strip_prefix(['❯', '›', '>', '→', '▸']) {
            Some(r) => (true, r.trim_start()),
            None => (false, t),
        };
        let (n, rest) = t.split_once(['.', ')'])?;
        let n: u32 = n.trim().parse().ok()?;
        let label = rest.trim();
        (!label.is_empty() && (1..10).contains(&n)).then(|| (cur, n, label.to_string()))
    }
    let lines: Vec<&str> = screen.lines().collect();
    let Some(c) = lines
        .iter()
        .rposition(|l| numbered(l).is_some_and(|(cur, _, _)| cur))
    else {
        return vec![];
    };
    let Some((_, n0, l0)) = numbered(lines[c]) else {
        return vec![];
    };
    let mut v = vec![(n0, l0)];
    let (mut want, mut gap, mut i) = (n0, 0, c);
    while i > 0 && want > 1 {
        i -= 1;
        match numbered(lines[i]) {
            Some((_, n, l)) if n + 1 == want => {
                v.push((n, l));
                want = n;
                gap = 0;
            }
            None if gap < 2 => gap += 1,
            _ => break,
        }
    }
    let (mut want, mut gap) = (n0, 0);
    for l in &lines[c + 1..] {
        match numbered(l) {
            Some((_, n, lab)) if n == want + 1 => {
                v.push((n, lab));
                want = n;
                gap = 0;
            }
            None if gap < 2 => gap += 1,
            _ => break,
        }
    }
    v.sort_by_key(|x| x.0);
    if v.len() >= 2 && v[0].0 == 1 {
        v
    } else {
        vec![]
    }
}

/// The footer while the focused session asks: its options, one key each
/// (drawn in place of the hints, so the panes never change size).
pub(crate) fn answer_line(d: &Dash, width: u16) -> Option<Line<'static>> {
    let (s, opts) = quick_answers(d)?;
    let mut l = vec![
        Span::styled(" ▲ ", ui::fg(c::NEEDS()).add_modifier(Modifier::BOLD)),
        Span::styled(format!("{} asks  ", s.display()), ui::muted()),
    ];
    let room = (width as usize).saturating_sub(ui::width(s.display()) + 34);
    let each = (room / opts.len()).clamp(8, 48);
    for (n, label) in &opts {
        l.push(ui::keycap(&n.to_string()));
        l.push(Span::styled(
            format!(" {}  ", ui::trunc(label, each.saturating_sub(5))),
            ui::bold(),
        ));
    }
    l.push(Span::styled("press a number · i reply", ui::faint()));
    Some(Line::from(l))
}

/// A digit while the focused session shows a menu: pick that option.
pub(crate) fn answer_key(d: &mut Dash, digit: char) -> bool {
    let Some((s, opts)) = quick_answers(d) else {
        return false;
    };
    let n = digit.to_digit(10).unwrap_or(0);
    let Some((_, label)) = opts.into_iter().find(|(m, _)| *m == n) else {
        return false;
    };
    let t = d.target_of(&s.host);
    let h = s.host.clone();
    let key = n.to_string();
    d.spawn(Some(h), move || {
        herdr::Herdr::new(&t)
            .send_keys(&s.pane_id, &[key.as_str()])
            .map(|_| format!("answered {}: {n} · {}", s.display(), ui::trunc(&label, 40)))
    });
    true
}

/// The focused pane if it's empty, else a new empty split beside it (for a
/// session about to start there).
pub(crate) fn empty_pane(d: &mut Dash) -> Option<u32> {
    enter(d);
    let focus = d.wall.focus()?;
    if matches!(d.wall.content_of_focus(), Some(Content::Empty)) {
        return Some(focus);
    }
    let id = d.wall.new_pane(Content::Empty);
    let dir = best_dir(&d.wall, focus);
    if let Some(w) = d.wall.win_mut() {
        w.root.split(focus, id, dir);
        w.focus = id;
    }
    d.wall.dirty = true;
    Some(id)
}

/// A session into the focused pane (if it's empty) or a new split beside it.
pub(crate) fn add(d: &mut Dash, key: String) {
    enter(d);
    let st = &mut d.wall;
    let Some(f) = st.focus() else { return };
    if st
        .panes
        .get(&f)
        .is_some_and(|p| p.content == Content::Empty)
    {
        if let Some(p) = st.panes.get_mut(&f) {
            p.content = Content::Session(key);
        }
    } else {
        let id = st.new_pane(Content::Session(key));
        let dir = best_dir(st, f);
        if let Some(w) = st.win_mut() {
            w.root.split(f, id, dir);
            w.focus = id;
        }
    }
    st.dirty = true;
}

/// `:split <session>`: it goes beside the focused pane (Focus becomes a tile).
pub(crate) fn split_with(d: &mut Dash, key: String) {
    add(d, key);
    if let Some(w) = d.wall.win_mut() {
        w.zoom = false;
    }
}

/// Split along the pane's longer side (what you'd pick by eye).
fn best_dir(st: &State, id: u32) -> Dir {
    match st.rects.iter().find(|(x, _)| *x == id) {
        Some((_, r)) if r.height * 2 > r.width => Dir::Stack,
        _ => Dir::Side,
    }
}

// ------------------------------------------------------------------ background

/// Streams only for the current window's panes; save the layout on change.
pub(crate) fn pump(d: &mut Dash) {
    if d.view != View::Wall {
        return;
    }
    let vis: Vec<u32> = d
        .wall
        .win()
        .map(|w| {
            if w.zoom {
                vec![w.focus]
            } else {
                w.root.leaves()
            }
        })
        .unwrap_or_default();
    let ids: Vec<u32> = d.wall.panes.keys().copied().collect();
    // fit each shown session to its tile (unless it's shown twice: two
    // tiles of one session would fight over its size)
    let fit = d.cfg.get_or("wall_fit") != "off";
    let mut shown: HashMap<String, usize> = HashMap::new();
    for id in &vis {
        if let Some(Content::Session(k)) = d.wall.panes.get(id).map(|p| &p.content) {
            *shown.entry(k.clone()).or_default() += 1;
        }
    }
    for id in ids {
        let key = match d.wall.panes.get(&id).map(|p| p.content.clone()) {
            Some(Content::Session(k)) => k,
            _ => {
                if let Some(p) = d.wall.panes.get_mut(&id) {
                    p.stream = None;
                }
                continue;
            }
        };
        let (h, pane) = key.split_once('|').unwrap_or_default();
        let target = vis.contains(&id).then(|| d.target_of(h));
        let pane = pane.to_string();
        let tile = d
            .wall
            .rects
            .iter()
            .find(|(x, _)| *x == id)
            .map(|(_, r)| (r.width.saturating_sub(2), r.height.saturating_sub(2)));
        let size = if fit && shown.get(&key) == Some(&1) {
            tile
        } else {
            None
        };
        // not drawn yet: wait one frame for the tile's size
        let wait = fit && size.is_none() && tile.is_none() && shown.get(&key) == Some(&1);
        if let Some(p) = d.wall.panes.get_mut(&id) {
            match target {
                Some(_) if wait => {}
                Some(t) if p.stream.is_none() => {
                    p.stream = Some(Stream::start_sized(&t, &pane, size));
                    p.want = None;
                }
                Some(t) => {
                    let have = p.stream.as_ref().and_then(|s| s.size);
                    if have != size {
                        match p.want {
                            Some((w, since)) if w == size.unwrap_or((0, 0)) => {
                                if since.elapsed() >= Duration::from_millis(400) {
                                    p.stream = Some(Stream::start_sized(&t, &pane, size));
                                    p.want = None;
                                }
                            }
                            _ => p.want = Some((size.unwrap_or((0, 0)), Instant::now())),
                        }
                    } else {
                        p.want = None;
                    }
                }
                None => p.stream = None,
            }
        }
    }
    let errs: Vec<String> = d
        .wall
        .chans
        .values()
        .flat_map(|c| c.take_errors().into_iter().map(|(_, e)| e))
        .collect();
    for e in errs {
        d.toast(Level::Err, e);
    }
    if d.wall.dirty {
        d.wall.dirty = false;
        save(&d.wall);
    }
}

/// Mark pane `id` as waiting for a session being started (`what`).
pub(crate) fn starting(d: &mut Dash, id: u32, what: String) {
    if let Some(p) = d.wall.panes.get_mut(&id) {
        p.content = Content::Starting(what);
    }
}

/// The session started for pane `id` (`host|pane`), or why it didn't.
pub(crate) fn started(d: &mut Dash, id: u32, res: Result<String, String>) {
    match res {
        Ok(key) => {
            if let Some(p) = d.wall.panes.get_mut(&id) {
                p.content = Content::Session(key.clone());
                p.stream = None;
            }
            d.wall.dirty = true;
            if let Some((h, _)) = key.split_once('|') {
                d.fleet.refresh(Some(h));
            }
        }
        Err(e) => {
            if let Some(p) = d.wall.panes.get_mut(&id) {
                p.content = Content::Empty;
            }
            d.error(format!("couldn't start it: {e}"));
        }
    }
}

fn session_of(d: &Dash, id: u32) -> Option<Session> {
    match &d.wall.panes.get(&id)?.content {
        Content::Session(k) => super::fleet::session(d, k),
        _ => None,
    }
}

fn chan<'a>(d: &'a mut Dash, host: &str) -> Option<&'a mut Chan> {
    // a stalled channel (commands unanswered): reopen it, and say what may
    // not have arrived
    if let Some(n) = d.wall.chans.get(host).and_then(|c| c.stalled()) {
        d.wall.chans.remove(host);
        d.toast(
            Level::Warn,
            format!(
                "the link to {host} stalled — reconnected; {n} command{} may not have arrived",
                if n == 1 { "" } else { "s" }
            ),
        );
    }
    let alive = d.wall.chans.get_mut(host).is_some_and(|c| c.alive());
    if !alive {
        match Chan::open(host) {
            Ok(c) => {
                d.wall.chans.insert(host.to_string(), c);
            }
            Err(e) => {
                d.toast(Level::Err, format!("{host}: {e}"));
                return None;
            }
        }
    }
    d.wall.chans.get_mut(host)
}

/// The sessions in the window on screen (Focus's one, Tile's), for the
/// notifier.
pub(crate) fn shown(d: &Dash) -> Vec<Session> {
    d.wall
        .win()
        .map(|w| w.root.leaves())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| session_of(d, id))
        .collect()
}

/// The focused pane's live stream.
fn focused_stream(d: &Dash) -> Option<&Stream> {
    d.wall
        .focus()
        .and_then(|id| d.wall.panes.get(&id))
        .and_then(|p| p.stream.as_ref())
}

/// How long a held-back esc waits for the second that sends it.
const ESC_AGAIN: Duration = Duration::from_secs(3);

/// The first esc held back: the status bar's warning is up.
pub(crate) fn esc_waits(d: &Dash) -> bool {
    d.wall.typing && d.wall.esc_held.is_some_and(|t| t.elapsed() < ESC_AGAIN)
}

/// Esc while typing into an agent: does this one go? The first is held back
/// with a toast and the status bar's warning (esc stops most agents' work);
/// one more within `ESC_AGAIN` goes, and so do the ones right after it
/// (Claude Code's esc esc). Any other key starts over; `focus.esc_guard =
/// off` sends every one.
fn esc_goes(d: &mut Dash) -> bool {
    let recent = |t: Option<Instant>| t.is_some_and(|t| t.elapsed() < ESC_AGAIN);
    if !d.cfg.flag("focus.esc_guard") || esc_waits(d) || recent(d.wall.esc_sent) {
        d.wall.esc_held = None;
        d.wall.esc_sent = Some(Instant::now());
        return true;
    }
    d.wall.esc_held = Some(Instant::now());
    d.toast(
        Level::Warn,
        format!(
            "native input: esc again stops the agent's work · {} or a click outside gives the keys back",
            keys_back(d)
        ),
    );
    false
}

/// A key into the focused pane as a terminal sends it, over its control
/// stream (in order, no process per key); false when there's none to take
/// it (the caller sends it through herdr's CLI instead).
fn type_key(d: &Dash, k: &KeyEvent) -> bool {
    let Some(st) = focused_stream(d) else {
        return false;
    };
    crate::stream::key_bytes(k, st.app_cursor()).is_some_and(|b| st.input(&b))
}

/// A paste while typing into a pane: to the agent as one paste.
pub(crate) fn paste(d: &mut Dash, text: &str) {
    if focused_stream(d).is_some_and(|st| st.paste(text)) {
        return;
    }
    if let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) {
        let pane = s.pane_id.clone();
        if let Some(c) = chan(d, &s.host) {
            let _ = c.send_text(&pane, text);
        }
    }
}

/// A pane's screen inside its frame.
fn inner_of(r: Rect) -> Rect {
    Rect::new(
        r.x + 1,
        r.y + 1,
        r.width.saturating_sub(2),
        r.height.saturating_sub(2),
    )
}

fn send_keys(d: &mut Dash, s: &Session, keys: &[&str]) {
    let pane = s.pane_id.clone();
    if let Some(c) = chan(d, &s.host) {
        if let Err(e) = c.send_keys(&pane, keys) {
            d.toast(Level::Err, format!("{}: {e}", s.host));
        }
    }
}

// ------------------------------------------------------------------ commands

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cmd {
    SplitSide,
    SplitStack,
    Move(i8, i8),
    Next,
    Prev,
    Close,
    Zoom,
    NewWindow,
    NextWindow,
    PrevWindow,
    Window(usize),
    CloseWindow,
    Rename,
    NextLayout,
    SwapNext,
    SwapPrev,
    Grow(Dir, f32),
    Equalize,
    Numbers,
    Type,
    StopTyping,
    Detach,
    Cheats,
}

fn run(d: &mut Dash, cmd: Cmd) {
    let Some(focus) = d.wall.focus() else { return };
    let st = &mut d.wall;
    match cmd {
        Cmd::SplitSide | Cmd::SplitStack => {
            let id = st.new_pane(Content::Empty);
            let dir = if cmd == Cmd::SplitSide {
                Dir::Side
            } else {
                Dir::Stack
            };
            if let Some(w) = st.win_mut() {
                w.zoom = false;
                w.root.split(focus, id, dir);
                w.focus = id;
            }
        }
        Cmd::Move(dx, dy) => {
            if let Some(n) = neighbour(&st.rects, focus, dx, dy) {
                if let Some(w) = st.win_mut() {
                    w.focus = n;
                }
            }
        }
        Cmd::Next | Cmd::Prev => {
            if let Some(w) = st.win_mut() {
                let l = w.root.leaves();
                let i = l.iter().position(|x| *x == w.focus).unwrap_or(0);
                let n = l.len();
                w.focus = l[if cmd == Cmd::Next {
                    (i + 1) % n
                } else {
                    (i + n - 1) % n
                }];
            }
        }
        Cmd::Close => {
            let alone = st.win().is_some_and(|w| w.root.leaves().len() == 1);
            if alone {
                if st.windows.len() > 1 {
                    st.panes.remove(&focus);
                    st.windows.remove(st.cur);
                    st.cur = st.cur.min(st.windows.len() - 1);
                } else if let Some(p) = st.panes.get_mut(&focus) {
                    p.content = Content::Empty;
                    p.stream = None;
                }
            } else {
                if let Some(w) = st.win_mut() {
                    w.root.remove(focus);
                    w.focus = w.root.leaves()[0];
                    w.zoom = false;
                }
                st.panes.remove(&focus);
            }
        }
        Cmd::Zoom => {
            if let Some(w) = st.win_mut() {
                w.zoom = !w.zoom;
            }
        }
        Cmd::NewWindow => {
            let n = st.windows.len();
            st.new_window(&n.to_string(), vec![], 0);
        }
        Cmd::NextWindow => st.cur = (st.cur + 1) % st.windows.len().max(1),
        Cmd::PrevWindow => {
            let n = st.windows.len().max(1);
            st.cur = (st.cur + n - 1) % n;
        }
        Cmd::Window(i) => {
            if i < st.windows.len() {
                st.cur = i;
            }
        }
        Cmd::CloseWindow => {
            if st.windows.len() > 1 {
                for id in st.windows[st.cur].root.leaves() {
                    st.panes.remove(&id);
                }
                st.windows.remove(st.cur);
                st.cur = st.cur.min(st.windows.len() - 1);
            }
        }
        Cmd::Rename => {
            let n = st.win().map(|w| w.name.clone()).unwrap_or_default();
            st.renaming = Some(forms::Field::new(&n));
        }
        Cmd::NextLayout => {
            let mut name = "";
            if let Some(w) = st.win_mut() {
                w.layout = (w.layout + 1) % LAYOUTS.len();
                let l = w.root.leaves();
                w.root = preset(&l, w.layout);
                w.zoom = false;
                name = LAYOUTS[w.layout];
            }
            d.toast(Level::Info, format!("layout: {name}"));
        }
        Cmd::SwapNext | Cmd::SwapPrev => {
            let l = st.win().map(|w| w.root.leaves()).unwrap_or_default();
            if l.len() > 1 {
                let i = l.iter().position(|x| *x == focus).unwrap_or(0);
                let j = if cmd == Cmd::SwapNext {
                    (i + 1) % l.len()
                } else {
                    (i + l.len() - 1) % l.len()
                };
                let (a, b) = (l[i], l[j]);
                let pa = st.panes.remove(&a);
                let pb = st.panes.remove(&b);
                if let (Some(pa), Some(pb)) = (pa, pb) {
                    st.panes.insert(a, pb);
                    st.panes.insert(b, pa);
                }
                if let Some(w) = st.win_mut() {
                    w.focus = b;
                }
            }
        }
        Cmd::Grow(dir, delta) => {
            if let Some(w) = st.win_mut() {
                w.root.resize(focus, dir, delta);
            }
        }
        Cmd::Equalize => {
            if let Some(w) = st.win_mut() {
                w.root.equalize();
            }
        }
        Cmd::Numbers => st.numbers = Some(Instant::now()),
        Cmd::Type => {
            if matches!(
                st.panes.get(&focus).map(|p| &p.content),
                Some(Content::Session(_))
            ) {
                st.typing = true;
                st.zoom_for_typing();
            } else {
                d.toast(
                    Level::Info,
                    "nothing to type into — n starts a session here, a adds one",
                );
            }
        }
        Cmd::StopTyping => {
            st.typing = false;
            st.unzoom();
        }
        Cmd::Detach => {
            st.typing = false;
            let back = st.back.take().unwrap_or(View::Fleet);
            d.switch(back);
            return;
        }
        Cmd::Cheats => st.cheats = !st.cheats,
    }
    d.wall.dirty = true;
}

/// The pane next to `id` in direction (dx, dy), by geometry.
fn neighbour(rects: &[(u32, Rect)], id: u32, dx: i8, dy: i8) -> Option<u32> {
    let (_, r) = rects.iter().find(|(x, _)| *x == id)?;
    let (cx, cy) = (
        r.x as i32 + r.width as i32 / 2,
        r.y as i32 + r.height as i32 / 2,
    );
    rects
        .iter()
        .filter(|(x, _)| *x != id)
        .filter(|(_, o)| match (dx, dy) {
            (-1, _) => o.x + o.width <= r.x,
            (1, _) => o.x >= r.x + r.width,
            (_, -1) => o.y + o.height <= r.y,
            _ => o.y >= r.y + r.height,
        })
        .min_by_key(|(_, o)| {
            let (ox, oy) = (
                o.x as i32 + o.width as i32 / 2,
                o.y as i32 + o.height as i32 / 2,
            );
            // the nearest in that direction, preferring panes level with this one
            let along = if dx != 0 {
                (ox - cx).abs()
            } else {
                (oy - cy).abs()
            };
            let across = if dx != 0 {
                (oy - cy).abs()
            } else {
                (ox - cx).abs()
            };
            along + across * 3
        })
        .map(|(x, _)| *x)
}

/// tmux keys after the prefix.
fn tmux(k: &KeyEvent) -> Option<Cmd> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    Some(match k.code {
        KeyCode::Char('%') => Cmd::SplitSide,
        KeyCode::Char('"') => Cmd::SplitStack,
        KeyCode::Left if ctrl => Cmd::Grow(Dir::Side, -0.05),
        KeyCode::Right if ctrl => Cmd::Grow(Dir::Side, 0.05),
        KeyCode::Up if ctrl => Cmd::Grow(Dir::Stack, -0.05),
        KeyCode::Down if ctrl => Cmd::Grow(Dir::Stack, 0.05),
        KeyCode::Left | KeyCode::Char('h') => Cmd::Move(-1, 0),
        KeyCode::Right | KeyCode::Char('l') => Cmd::Move(1, 0),
        KeyCode::Up | KeyCode::Char('k') => Cmd::Move(0, -1),
        KeyCode::Down | KeyCode::Char('j') => Cmd::Move(0, 1),
        KeyCode::Char('o') => Cmd::Next,
        KeyCode::Char(';') => Cmd::Prev,
        KeyCode::Char('x') => Cmd::Close,
        KeyCode::Char('z') => Cmd::Zoom,
        KeyCode::Char('c') => Cmd::NewWindow,
        KeyCode::Char('n') => Cmd::NextWindow,
        KeyCode::Char('p') => Cmd::PrevWindow,
        KeyCode::Char(d @ '0'..='9') => Cmd::Window(d as usize - '0' as usize),
        KeyCode::Char('&') => Cmd::CloseWindow,
        KeyCode::Char(',') => Cmd::Rename,
        KeyCode::Char(' ') => Cmd::NextLayout,
        KeyCode::Char('}') => Cmd::SwapNext,
        KeyCode::Char('{') => Cmd::SwapPrev,
        KeyCode::Char('=') | KeyCode::Char('E') => Cmd::Equalize,
        KeyCode::Char('q') => Cmd::Numbers,
        KeyCode::Esc => Cmd::StopTyping,
        KeyCode::Char('i') => Cmd::Type,
        KeyCode::Char('d') => Cmd::Detach,
        KeyCode::Char('?') => Cmd::Cheats,
        _ => return None,
    })
}

/// vim keys after `ctrl+w`.
fn vim_win(k: &KeyEvent) -> Option<Cmd> {
    Some(match k.code {
        KeyCode::Char('s') | KeyCode::Char('S') => Cmd::SplitStack,
        KeyCode::Char('v') => Cmd::SplitSide,
        KeyCode::Char('h') | KeyCode::Left => Cmd::Move(-1, 0),
        KeyCode::Char('l') | KeyCode::Right => Cmd::Move(1, 0),
        KeyCode::Char('k') | KeyCode::Up => Cmd::Move(0, -1),
        KeyCode::Char('j') | KeyCode::Down => Cmd::Move(0, 1),
        KeyCode::Char('w') => Cmd::Next,
        KeyCode::Char('W') | KeyCode::Char('p') => Cmd::Prev,
        KeyCode::Char('q') | KeyCode::Char('c') => Cmd::Close,
        KeyCode::Char('o') => Cmd::Zoom,
        KeyCode::Char('=') => Cmd::Equalize,
        KeyCode::Char('<') => Cmd::Grow(Dir::Side, -0.05),
        KeyCode::Char('>') => Cmd::Grow(Dir::Side, 0.05),
        KeyCode::Char('-') => Cmd::Grow(Dir::Stack, -0.05),
        KeyCode::Char('+') => Cmd::Grow(Dir::Stack, 0.05),
        KeyCode::Char('x') | KeyCode::Char('r') => Cmd::SwapNext,
        KeyCode::Char('T') | KeyCode::Char('n') => Cmd::NewWindow,
        _ => return None,
    })
}

/// The prefix key: `mux_prefix` if set; else, with `mux_keys = herdr` (the
/// default), herdr's own `[keys] prefix` from its config.toml, so the same
/// muscle memory works here; else ctrl+b.
/// More keys that stop typing, besides `focus.exit` (`mux_leave`).
fn leave_keys(d: &Dash) -> Vec<String> {
    d.cfg
        .get_or("mux_leave")
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The key that gives the keys back, for hints.
pub(crate) fn keys_back(d: &Dash) -> String {
    exit_keys(d)
        .into_iter()
        .chain(leave_keys(d))
        .next()
        .unwrap_or_else(|| "a click outside".into())
}

/// The keys out, typing or not (`focus.exit`): typing, the first gives the keys
/// back to Aleph; then the next goes from Focus (and Tile) to Fleet.
fn exit_keys(d: &Dash) -> Vec<String> {
    d.cfg
        .get_or("focus.exit")
        .split(',')
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// The first of them, for hints (None: no such key).
pub(crate) fn exit_key(d: &Dash) -> Option<String> {
    exit_keys(d).into_iter().next()
}

/// Keys back to Aleph from the agent (ctrl+q, a click outside, the palette); a
/// pane zoomed for typing goes back and a held esc is forgotten.
pub(crate) fn stop_typing(d: &mut Dash) {
    d.wall.typing = false;
    d.wall.unzoom();
    d.wall.dirty = true;
    (d.wall.esc_held, d.wall.esc_sent) = (None, None);
}

/// Leave the panes view: typing stops, the view you came from comes back.
pub(crate) fn exit(d: &mut Dash) {
    stop_typing(d);
    if let Some(st) = focused_stream(d) {
        st.live();
    }
    let back = d.wall.back.take().unwrap_or(View::Fleet);
    d.switch(back);
    // answered from the approval board: back to it while others still wait
    if std::mem::take(&mut d.wall.back_to_board) && d.fleet.sessions().iter().any(|s| s.blocked()) {
        super::approvals::open(d);
    }
}

/// `I` in Fleet: Focus on a session, typing into it at once.
pub(crate) fn type_into(d: &mut Dash, key: &str, back: View) {
    focus_on(d, key, back);
    run(d, Cmd::Type);
}

/// Is `ks` the panes' prefix (ctrl+b, or herdr's own)?
pub(crate) fn is_prefix(d: &Dash, ks: &str) -> bool {
    prefix_key(d) == ks
}

fn prefix_key(d: &Dash) -> String {
    if let Some(p) = d.cfg.get("mux_prefix").filter(|p| !p.is_empty()) {
        return p;
    }
    if d.cfg.get_or("mux_keys") == "herdr" {
        static P: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let p = P.get_or_init(|| {
            let t = std::fs::read_to_string(crate::herdrtheme::herdr_config()).ok()?;
            herdr_prefix(&t)
        });
        if let Some(p) = p {
            return p.clone();
        }
    }
    "ctrl+b".into()
}

/// `[keys] prefix = "ctrl+a"` in herdr's config.toml (`C-a` style too).
pub(crate) fn herdr_prefix(toml: &str) -> Option<String> {
    let doc: toml_edit::DocumentMut = toml.parse().ok()?;
    let p = doc
        .get("keys")?
        .get("prefix")?
        .as_str()?
        .trim()
        .to_lowercase();
    let p = match p.strip_prefix("c-") {
        Some(k) => format!("ctrl+{k}"),
        None => p.replace("control+", "ctrl+"),
    };
    (!p.is_empty()).then_some(p)
}

/// Sessions in the current window.
fn window_sessions(d: &Dash) -> Vec<Session> {
    d.wall
        .win()
        .map(|w| w.root.leaves())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|id| session_of(d, id))
        .collect()
}

/// The current window's shape with only `host`'s sessions in it (the
/// others, and empty panes, give their room to their neighbours) — what
/// "push to herdr" recreates there. None below two sessions.
pub(crate) fn push_shape(d: &Dash, host: &str) -> Option<crate::layouts::Tree<Session>> {
    let w = d.wall.win()?;
    let t = w
        .root
        .to_tree()
        .prune(&mut |id| session_of(d, *id).is_some_and(|s| s.host == host))?;
    let t = t.map(&mut |id| session_of(d, *id).unwrap_or_default());
    (t.len() >= 2).then_some(t)
}

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    enter(d);
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let ks = super::key_str(&k);
    if d.wall.compose.is_some() {
        compose_key(d, k);
        return None;
    }
    if d.wall.queue_view.is_some() {
        queue_key(d, k);
        return None;
    }
    // renaming a window
    if let Some(mut f) = d.wall.renaming.take() {
        match k.code {
            KeyCode::Enter => {
                let n = f.text.trim().to_string();
                if let (Some(w), false) = (d.wall.win_mut(), n.is_empty()) {
                    w.name = n;
                }
                d.wall.dirty = true;
            }
            KeyCode::Esc => {}
            _ => {
                f.key(&k);
                d.wall.renaming = Some(f);
            }
        }
        return None;
    }
    // a pending prefix / ctrl+w
    match d.wall.pending {
        Pending::Prefix => {
            d.wall.pending = Pending::None;
            if ks == prefix_key(d) || exit_keys(d).contains(&ks) {
                // the prefix twice, or the prefix and ctrl+q (the way to wake a
                // shell frozen by ctrl+s): send it through to the pane
                if d.wall.typing && type_key(d, &k) {
                    return None;
                }
                if let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) {
                    if let Some(name) = chan::key_name(&k) {
                        send_keys(d, &s, &[&name]);
                    }
                }
                return None;
            }
            // the prefix and [ ]: the previous / next session, keys and all
            match k.code {
                KeyCode::Char('[') => focus_step(d, false, false),
                KeyCode::Char(']') => focus_step(d, true, false),
                _ => {
                    if let Some(c) = tmux(&k) {
                        run(d, c);
                    }
                }
            }
            return None;
        }
        Pending::Win => {
            d.wall.pending = Pending::None;
            if let Some(c) = vim_win(&k) {
                run(d, c);
            }
            return None;
        }
        Pending::None => {}
    }
    if ks == prefix_key(d) {
        d.wall.pending = Pending::Prefix;
        return None;
    }
    // ctrl+q: typing, the keys come back to Aleph (esc is the agent's then);
    // then it goes to Fleet, wherever Focus was opened from (esc goes back there)
    if exit_keys(d).contains(&ks) {
        if d.wall.typing {
            stop_typing(d);
        } else {
            d.wall.back = Some(View::Fleet);
            exit(d);
        }
        return None;
    }
    // typing into the focused pane
    if d.wall.typing {
        if leave_keys(d).contains(&ks) {
            stop_typing(d);
            return None;
        }
        // alt+, alt+.: the previous / next session (every bracket is the agent's)
        if ks == "alt+," || ks == "alt+." {
            focus_step(d, ks == "alt+.", false);
            return None;
        }
        // ctrl+k: Aleph's palette, typing or not (the agent never gets it;
        // closing the palette goes back to typing)
        if ks == "ctrl+k" {
            d.mode = Mode::Palette(super::palette::Palette::new(d));
            return None;
        }
        // ctrl+a: the approval board while anything waits on you (else it's
        // the agent's: the start of its line)
        if ks == d.keys.key_for(super::Act::Approvals)
            && d.fleet.sessions().iter().any(|s| s.blocked())
        {
            super::approvals::open(d);
            return None;
        }
        let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) else {
            d.wall.typing = false;
            return None;
        };
        if k.code == KeyCode::Esc && k.modifiers.is_empty() {
            if !esc_goes(d) {
                return None;
            }
        } else {
            (d.wall.esc_held, d.wall.esc_sent) = (None, None);
        }
        if type_key(d, &k) {
            return None;
        }
        // no control stream (observed, polled, still connecting): herdr's CLI
        if let Some(st) = focused_stream(d) {
            st.live();
        }
        match k.code {
            KeyCode::Char(ch) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => {
                let pane = s.pane_id.clone();
                if let Some(c) = chan(d, &s.host) {
                    let _ = c.send_text(&pane, &ch.to_string());
                }
            }
            _ => {
                if let Some(name) = chan::key_name(&k) {
                    send_keys(d, &s, &[&name]);
                }
            }
        }
        return None;
    }
    if d.wall.cheats {
        d.wall.cheats = false;
        return None;
    }
    if ctrl && k.code == KeyCode::Char('w') {
        d.wall.pending = Pending::Win;
        return None;
    }
    let sess = d.wall.focus().and_then(|id| session_of(d, id));
    let empty = matches!(d.wall.content_of_focus(), Some(Content::Empty) | None);
    match k.code {
        KeyCode::Char('h') | KeyCode::Left => run(d, Cmd::Move(-1, 0)),
        KeyCode::Char('l') | KeyCode::Right => run(d, Cmd::Move(1, 0)),
        KeyCode::Char('k') | KeyCode::Up => run(d, Cmd::Move(0, -1)),
        KeyCode::Char('j') | KeyCode::Down => run(d, Cmd::Move(0, 1)),
        // split, then pick what goes in the new pane (esc leaves it empty: n starts one)
        KeyCode::Char('v') | KeyCode::Char('%') | KeyCode::Char('|') => {
            run(d, Cmd::SplitSide);
            d.mode = Mode::Palette(super::palette::Palette::sessions_only(d));
        }
        KeyCode::Char('s') | KeyCode::Char('"') | KeyCode::Char('_') => {
            run(d, Cmd::SplitStack);
            d.mode = Mode::Palette(super::palette::Palette::sessions_only(d));
        }
        // the machines: ctrl+[ ctrl+], ( ) where a terminal can't tell them
        _ if super::machine_step(&k).is_some() => {
            focus_machine(d, super::machine_step(&k).unwrap_or(true))
        }
        // step through sessions; in Focus the ones waiting for you too
        KeyCode::Char(']') => focus_step(d, true, false),
        KeyCode::Char('[') => focus_step(d, false, false),
        KeyCode::Char('}') if d.wall.in_focus() => focus_step(d, true, true),
        KeyCode::Char('{') if d.wall.in_focus() => focus_step(d, false, true),
        KeyCode::Char('o') => run(d, Cmd::Next),
        KeyCode::Char('x') => run(d, Cmd::Close),
        KeyCode::Char('z') => run(d, Cmd::Zoom),
        KeyCode::Char('c') => run(d, Cmd::NewWindow),
        KeyCode::Char('w') => run(d, Cmd::NextWindow),
        KeyCode::Char('W') => run(d, Cmd::PrevWindow),
        KeyCode::Char(',') => run(d, Cmd::Rename),
        KeyCode::Char(' ') => run(d, Cmd::NextLayout),
        KeyCode::Char('}') => run(d, Cmd::SwapNext),
        KeyCode::Char('{') => run(d, Cmd::SwapPrev),
        KeyCode::Char('=') => run(d, Cmd::Equalize),
        KeyCode::Char('<') => run(d, Cmd::Grow(Dir::Side, -0.05)),
        KeyCode::Char('>') => run(d, Cmd::Grow(Dir::Side, 0.05)),
        KeyCode::Char('-') => run(d, Cmd::Grow(Dir::Stack, -0.05)),
        KeyCode::Char('+') => run(d, Cmd::Grow(Dir::Stack, 0.05)),
        KeyCode::Char('#') => run(d, Cmd::Numbers),
        KeyCode::Char('?') => run(d, Cmd::Cheats),
        KeyCode::Char('i') | KeyCode::Enter => {
            if empty {
                open_new(d, None);
            } else {
                open_compose(d);
            }
        }
        KeyCode::Char('I') => run(d, Cmd::Type),
        KeyCode::Char('Q') => {
            let s = sess?;
            if crate::queue::for_session(&s.host, &s.name).is_empty() {
                d.toast(
                    Level::Info,
                    format!(
                        "nothing queued for {} — i, then tab, queues a message",
                        s.display()
                    ),
                );
            } else {
                d.wall.queue_view = Some(0);
            }
        }
        KeyCode::Char('N') => {
            let s = sess?;
            super::fleet::note_box(d, &s);
        }
        KeyCode::Char('t') if d.wall.in_focus() => {
            // it flips what's on screen (a wide screen shows it before any t)
            let w = d.wall.body_w;
            let showing = if w > 0 {
                super::timeline::shown(d, w)
            } else {
                d.wall.timeline.unwrap_or(false)
            };
            let on = !showing;
            d.wall.timeline = Some(on);
            d.toast(
                Level::Info,
                if !on {
                    "timeline hidden"
                } else if w > 0 && !super::timeline::shown(d, w) {
                    "the timeline needs a wider window (100 columns)"
                } else {
                    "timeline shown (t hides it)"
                },
            );
        }
        KeyCode::Char('X') => {
            let s = sess?;
            d.mode = Mode::Confirm {
                title: "Close session?".into(),
                text: format!(
                    "{} on {} — {}",
                    s.display(),
                    s.host,
                    super::fleet::close_what(d)
                ),
                on_yes: super::Pending::Close(vec![s]),
            };
        }
        KeyCode::Char('n') => open_new(d, None),
        KeyCode::Char('S') => {
            let h = sess.as_ref().map(|s| s.host.clone());
            open_new(d, Some(h));
        }
        // the same keys as Fleet: a attaches in herdr here, A attaches to the
        // agent directly, O opens herdr in a new terminal tab (Aleph stays);
        // in an empty pane a adds an existing session
        KeyCode::Char('a') => {
            let Some(s) = sess else {
                d.mode = Mode::Palette(super::palette::Palette::sessions_only(d));
                return None;
            };
            return Some(Handoff::Attach {
                target: d.target_of(&s.host),
                pane: s.pane_id.clone(),
                label: s.display().to_string(),
            });
        }
        KeyCode::Char('A') => {
            let s = sess?;
            return Some(Handoff::Direct {
                target: d.target_of(&s.host),
                pane: s.pane_id.clone(),
                takeover: false,
            });
        }
        KeyCode::Char('O') => {
            let s = sess?;
            super::open_in_herdr_tab(d, &s);
        }
        KeyCode::Char('y') => {
            let s = sess?;
            let t = d.target_of(&s.host);
            let h = s.host.clone();
            d.spawn(Some(h), move || {
                herdr::Herdr::new(&t)
                    .approve(&s.pane_id)
                    .map(|k| format!("approved {} ({})", s.display(), k.join(" ")))
            });
        }
        KeyCode::Char('K') => {
            let s = sess?;
            d.mode = Mode::Input(forms::Input::new(
                format!("Keys for {}", s.display()),
                "space-separated: 2 · enter · esc · up · ctrl+c",
                String::new(),
                forms::For::Keys(s),
            ));
        }
        KeyCode::Char('m') => {
            let s = sess?;
            d.mode = Mode::Input(forms::Input::new(
                format!("Prompt {}", s.display()),
                "enter sends it as if you typed it · tab queues it for when it's free",
                String::new(),
                forms::For::Send(vec![s]),
            ));
        }
        KeyCode::Char('b') => {
            let v = window_sessions(d);
            if v.is_empty() {
                return None;
            }
            d.mode = Mode::Input(forms::Input::new(
                format!(
                    "Broadcast to this window's {} session{}",
                    v.len(),
                    if v.len() == 1 { "" } else { "s" }
                ),
                "every pane gets the same prompt",
                String::new(),
                forms::For::Send(v),
            ));
        }
        KeyCode::Char('T') => {
            // push to herdr: this window's sessions on the focused one's
            // machine become real herdr panes, in the window's shape
            let host = sess?.host;
            let Some(tree) = push_shape(d, &host) else {
                d.toast(
                    Level::Info,
                    format!("only one pane on {host} — herdr tiles within a machine"),
                );
                return None;
            };
            let t = d.target_of(&host);
            let n = tree.len();
            d.spawn(Some(host.clone()), move || {
                crate::cli::push_into(&t, &tree)
                    .map(|l| format!("pushed {n} sessions into herdr as {l} on {host}"))
            });
        }
        KeyCode::Esc | KeyCode::Char('q') => {
            let back = d.wall.back.take().unwrap_or(View::Fleet);
            d.switch(back);
        }
        _ => {}
    }
    None
}

/// Open the message bar on the focused session pane (with its draft).
fn open_compose(d: &mut Dash) {
    let Some(id) = d.wall.focus() else { return };
    let Some(Content::Session(key)) = d.wall.panes.get(&id).map(|p| p.content.clone()) else {
        return;
    };
    let text = d.wall.drafts.get(&key).cloned().unwrap_or_default();
    d.wall.compose = Some(Compose {
        pane: id,
        key,
        field: forms::Field::new(&text),
        recall: None,
    });
    d.wall.zoom_for_typing();
}

fn compose_key(d: &mut Dash, k: KeyEvent) {
    let Some(mut c) = d.wall.compose.take() else {
        return;
    };
    let s = super::fleet::session(d, &c.key);
    let text = c.field.text.trim().to_string();
    match k.code {
        KeyCode::Esc => {
            d.wall.compose = Some(c);
            d.wall.close_compose();
            return;
        }
        KeyCode::Enter | KeyCode::Tab if text.is_empty() => {}
        KeyCode::Enter | KeyCode::Tab => {
            let Some(s) = s else {
                d.toast(Level::Err, "that session isn't running any more");
                d.wall.compose = Some(c);
                return;
            };
            // out of reach, enter queues it too (it goes once you're back); a
            // machine on the local network still answers without the internet
            let offline = d.cut_off(&s.host);
            if k.code == KeyCode::Enter && offline {
                d.toast(
                    Level::Info,
                    "offline — queued instead; it goes once you're back",
                );
            }
            if k.code == KeyCode::Enter && !offline {
                let t = d.target_of(&s.host);
                let body = text.clone();
                d.spawn(Some(s.host.clone()), move || {
                    crate::herdr::Herdr::new(&t)
                        .prompt(&s.pane_id, &body)
                        .map(|_| format!("sent to {}", s.display()))
                        .map_err(|e| format!("{}: {e}", s.display()))
                });
            } else {
                let n = crate::queue::for_session(&s.host, &s.name).len();
                // queued because we're offline: for the moment on screen now
                let now_seen = (k.code == KeyCode::Enter && offline).then(|| {
                    (
                        s.seq,
                        if s.blocked() {
                            "asking".to_string()
                        } else {
                            s.status.clone()
                        },
                    )
                });
                crate::queue::add(crate::queue::Task {
                    host: s.host.clone(),
                    prompt: text.clone(),
                    agent: None,
                    to: Some(s.name.clone()),
                    seq: now_seen.as_ref().map(|x| x.0),
                    seen: now_seen.map(|x| x.1),
                    ..Default::default()
                });
                let when = if n > 0 {
                    format!("after the {n} before it")
                } else if s.blocked() {
                    "after you answer its question".to_string()
                } else if matches!(s.status.as_str(), "idle" | "done") {
                    "now — it's free".to_string()
                } else {
                    "once it's done".to_string()
                };
                d.note(format!("queued for {} — sent {when}", s.display()));
            }
            let v = d.wall.sent.entry(c.key.clone()).or_default();
            v.retain(|x| *x != text);
            v.push(text);
            c.field = forms::Field::new("");
            c.recall = None;
            d.wall.drafts.remove(&c.key);
        }
        KeyCode::Up | KeyCode::Down => {
            let v = d.wall.sent.get(&c.key).cloned().unwrap_or_default();
            if !v.is_empty() && (c.field.text.is_empty() || c.recall.is_some()) {
                let i = match (c.recall, k.code) {
                    (None, KeyCode::Up) => Some(v.len() - 1),
                    (Some(i), KeyCode::Up) => Some(i.saturating_sub(1)),
                    (Some(i), _) if i + 1 < v.len() => Some(i + 1),
                    _ => None,
                };
                c.recall = i;
                c.field = forms::Field::new(i.map_or("", |i| v[i].as_str()));
            }
        }
        _ => {
            c.recall = None;
            c.field.key(&k);
        }
    }
    d.wall.compose = Some(c);
}

/// Keys in the queue overlay: ↑↓ pick, enter send now, d drop, esc close.
fn queue_key(d: &mut Dash, k: KeyEvent) {
    let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) else {
        d.wall.queue_view = None;
        return;
    };
    let q = crate::queue::for_session(&s.host, &s.name);
    let mut i = d
        .wall
        .queue_view
        .unwrap_or(0)
        .min(q.len().saturating_sub(1));
    match k.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
            d.wall.queue_view = None;
            return;
        }
        KeyCode::Up | KeyCode::Char('k') => i = i.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => i = (i + 1).min(q.len().saturating_sub(1)),
        KeyCode::Enter => {
            if let Some(t) = q.get(i) {
                let (id, tgt, sc) = (t.id, d.target_of(&s.host), s.clone());
                d.spawn(Some(s.host.clone()), move || {
                    crate::queue::send_now(id, &sc, &tgt)
                });
            }
        }
        KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => {
            if let Some(t) = q.get(i) {
                crate::queue::remove(t.id);
            }
        }
        _ => {}
    }
    let left = crate::queue::for_session(&s.host, &s.name).len();
    d.wall.queue_view = (left > 0).then_some(i.min(left.saturating_sub(1)));
}

/// The new-session form for the focused pane (if empty) or a new split;
/// `shell`: Some(host) presets a plain shell on that machine.
fn open_new(d: &mut Dash, shell: Option<Option<String>>) {
    let Some(id) = empty_pane(d) else { return };
    let host = shell
        .clone()
        .flatten()
        .or_else(|| d.fv.selected_host(d))
        .or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone()))
        .unwrap_or_default();
    let mut nf = forms::NewForm::new(d, &host, None);
    nf.into_pane = Some(id);
    if shell.is_some() {
        nf.set_agent(crate::agents::SHELL);
    }
    d.mode = Mode::NewForm(nf);
    d.wall.dirty = true;
}

/// A session that ended by itself leaves Focus and Tile (`close.exited`): its
/// tile goes, and with the last one you're back where you came from. Only a
/// session seen alive, on a machine that's answering: not one still starting,
/// nor one behind a dropped link.
pub(crate) fn drop_ended(d: &mut Dash) {
    if d.view != View::Wall {
        return;
    }
    let live: Vec<(u32, bool, bool)> = d
        .wall
        .panes
        .iter()
        .filter_map(|(id, p)| match &p.content {
            Content::Session(k) => {
                let up = k.split_once('|').is_some_and(|(host, _)| {
                    d.fleet
                        .host(host)
                        .is_some_and(|h| h.conn == crate::fleet::Conn::Up && !h.stale)
                });
                Some((*id, super::fleet::session(d, k).is_some(), up))
            }
            _ => None,
        })
        .collect();
    let keep = d.cfg.get_or("close.exited") == "keep";
    for (id, alive, up) in live {
        let seen = d.wall.panes.get(&id).is_some_and(|p| p.seen);
        if alive {
            if let Some(p) = d.wall.panes.get_mut(&id) {
                p.seen = true;
            }
        } else if seen && up && !keep {
            drop_tile(d, id);
        }
    }
}

/// Take one tile away, whichever window it's in.
fn drop_tile(d: &mut Dash, id: u32) {
    let Some(i) = d
        .wall
        .windows
        .iter()
        .position(|w| w.root.leaves().contains(&id))
    else {
        return;
    };
    let alone = d.wall.windows[i].root.leaves().len() == 1;
    if alone && d.wall.windows.len() == 1 {
        d.toast(Level::Info, "the session ended");
        exit(d);
        if let Some(p) = d.wall.panes.get_mut(&id) {
            p.content = Content::Empty;
            p.stream = None;
            p.seen = false;
        }
        return;
    }
    d.wall.typing = d.wall.typing && d.wall.focus() != Some(id);
    d.wall.panes.remove(&id);
    if alone {
        d.wall.windows.remove(i);
        d.wall.cur = d.wall.cur.min(d.wall.windows.len() - 1);
    } else {
        let w = &mut d.wall.windows[i];
        w.root.remove(id);
        if w.focus == id {
            w.focus = w.root.leaves()[0];
        }
        w.zoom = false;
    }
    d.wall.dirty = true;
}

/// A tile showing this output, and its screen (render tests).
#[cfg(test)]
pub(crate) fn fix_screen(d: &mut Dash, key: &str, output: &[u8]) -> Rect {
    let (id, r) = d
        .wall
        .rects
        .iter()
        .copied()
        .find(|(id, _)| {
            matches!(d.wall.panes.get(id).map(|p| &p.content),
                Some(Content::Session(k)) if k == key)
        })
        .expect("no such tile");
    let inner = inner_of(r);
    if let Some(p) = d.wall.panes.get_mut(&id) {
        p.stream = Some(Stream::fixed(key, inner.width, inner.height, output));
    }
    inner
}

/// The stream of the pane a selection is on.
fn selected_stream(d: &Dash) -> Option<&Stream> {
    let s = d.wall.selection.as_ref()?;
    d.wall.panes.get(&s.pane)?.stream.as_ref()
}

/// No selection any more, and its pane live again.
pub(crate) fn clear_selection(d: &mut Dash) {
    if let Some(st) = selected_stream(d) {
        st.release();
    }
    d.wall.selection = None;
}

/// A press: where a selection would start, on a screen with something on it.
fn select_down(d: &mut Dash, m: &MouseEvent) {
    clear_selection(d);
    if m.modifiers.contains(KeyModifiers::ALT) {
        return;
    }
    let inside = |r: &Rect| {
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
    };
    let Some((pane, area)) = d
        .wall
        .rects
        .iter()
        .map(|(id, r)| (*id, inner_of(*r)))
        .find(|(_, r)| inside(r))
    else {
        return;
    };
    if d.wall.panes.get(&pane).is_some_and(|p| p.stream.is_some()) {
        d.wall.selection = Some(Selection {
            pane,
            area,
            anchor: (m.column, m.row),
            cursor: (m.column, m.row),
            moved: false,
            fades: None,
        });
    }
}

/// A drag: the selection grows, kept inside its pane; false when there's none.
fn select_drag(d: &mut Dash, m: &MouseEvent) -> bool {
    if d.wall.drag.is_some() {
        return false;
    }
    let Some(s) = d.wall.selection.as_mut() else {
        return false;
    };
    let cursor = (
        m.column
            .clamp(s.area.x, s.area.x + s.area.width.saturating_sub(1)),
        m.row
            .clamp(s.area.y, s.area.y + s.area.height.saturating_sub(1)),
    );
    if !s.moved && cursor == s.anchor {
        return true;
    }
    s.cursor = cursor;
    let first = !std::mem::replace(&mut s.moved, true);
    if first {
        if let Some(st) = selected_stream(d) {
            st.hold();
        }
    }
    d.wall.dirty = true;
    true
}

/// The release: a drag's text is copied (`focus.copy_on_select`). False when it
/// was a click.
fn select_up(d: &mut Dash) -> bool {
    let moved = d.wall.selection.as_ref().is_some_and(|s| s.moved);
    if !moved {
        d.wall.selection = None;
        return false;
    }
    d.wall.swallow_up = false;
    if d.cfg.flag("focus.copy_on_select") {
        copy_selection(d);
        if let Some(s) = d.wall.selection.as_mut() {
            s.fades = Some(Instant::now() + Duration::from_millis(700));
        }
    }
    true
}

/// The selection to the clipboard, and a word on how much; false when there's
/// nothing selected.
pub(crate) fn copy_selection(d: &mut Dash) -> bool {
    let (Some(s), Some(st)) = (d.wall.selection.as_ref(), selected_stream(d)) else {
        return false;
    };
    if !s.moved {
        return false;
    }
    let text = st.text_between(s.area, s.anchor, s.cursor);
    if text.trim().is_empty() {
        return false;
    }
    if super::copy_screen(&text) {
        let lines = text.lines().count();
        d.toast(
            Level::Ok,
            if lines > 1 {
                format!("copied {lines} lines")
            } else {
                format!("copied {} characters", text.chars().count())
            },
        );
    } else {
        d.toast(Level::Warn, super::copy_failed());
    }
    true
}

/// The selection's cells lit, on top of the panes; its time up, it goes.
fn draw_selection(d: &mut Dash, f: &mut Frame) {
    if let Some(at) = d.wall.selection.as_ref().and_then(|s| s.fades) {
        if Instant::now() >= at {
            clear_selection(d);
        }
    }
    // the tile it's on moved or went: so does the selection
    let gone = d.wall.selection.as_ref().is_some_and(|s| {
        !d.wall
            .rects
            .iter()
            .any(|(id, r)| *id == s.pane && inner_of(*r) == s.area)
    });
    if gone {
        clear_selection(d);
    }
    let Some(s) = d.wall.selection.as_ref().filter(|s| s.moved) else {
        return;
    };
    let (a, b) = (s.anchor, s.cursor);
    let ((c1, r1), (c2, r2)) = if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    };
    let (left, right) = (s.area.x, s.area.x + s.area.width.saturating_sub(1));
    let style = Style::new().fg(c::TEXT()).bg(c::SEL());
    let buf = f.buffer_mut();
    for row in r1..=r2 {
        let from = if row == r1 { c1 } else { left };
        let to = if row == r2 { c2 } else { right };
        for col in from..=to {
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.set_style(style);
            }
        }
    }
}

pub(crate) fn mouse(d: &mut Dash, m: MouseEvent) {
    let at = |r: &Rect| {
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height
    };
    // the pane you're typing into, if the mouse is over its screen: clicks
    // go to the agent (herdr drops them unless it asked for the mouse)
    let typing_in = d
        .wall
        .typing
        .then(|| d.wall.focus())
        .flatten()
        .and_then(|f| d.wall.rects.iter().find(|(id, _)| *id == f).copied())
        .map(|(_, r)| inner_of(r))
        .filter(|r| at(r));
    let to_agent = |d: &Dash, action: &str| {
        if let (Some(r), Some(st)) = (typing_in, focused_stream(d)) {
            st.mouse(r, action, m.column, m.row);
        }
    };
    // a press on a pane's screen may start a selection. Whether it did shows when
    // it moves, so a click waits for its release to reach the agent; option goes
    // straight through, for an agent's own dragging
    let clicked = d.wall.selection.as_ref().is_some_and(|s| !s.moved);
    if m.kind == MouseEventKind::Down(MouseButton::Left) {
        select_down(d, &m);
    }
    match m.kind {
        MouseEventKind::Drag(MouseButton::Left) if select_drag(d, &m) => {}
        MouseEventKind::Up(MouseButton::Left) if select_up(d) => {}
        // the end of the click that moved focus here: never the agent's
        MouseEventKind::Drag(MouseButton::Left) if d.wall.swallow_up => {}
        MouseEventKind::Up(_) if d.wall.swallow_up => {
            d.wall.swallow_up = false;
            d.wall.drag = None;
        }
        // the wheel scrolls the pane under it: its history (herdr's), or
        // the agent's own scrolling when it asked for the mouse
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = m.kind == MouseEventKind::ScrollUp;
            if let Some((id, r)) = d.wall.rects.iter().find(|(_, r)| at(r)).copied() {
                if let Some(st) = d.wall.panes.get(&id).and_then(|p| p.stream.as_ref()) {
                    st.wheel(inner_of(r), up, m.column, m.row);
                }
            }
        }
        MouseEventKind::Down(MouseButton::Left) if typing_in.is_some() => {
            if d.wall.selection.is_none() {
                to_agent(d, "down")
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if typing_in.is_some() && d.wall.drag.is_none() => {
            to_agent(d, "drag")
        }
        MouseEventKind::Up(MouseButton::Left) if typing_in.is_some() && d.wall.drag.is_none() => {
            if clicked {
                to_agent(d, "down");
            }
            to_agent(d, "up")
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // a click outside the pane you're typing into stops the typing
            let was_typing = d.wall.typing;
            if d.wall.typing {
                let inside = d
                    .wall
                    .focus()
                    .and_then(|f| d.wall.rects.iter().find(|(id, _)| *id == f))
                    .is_some_and(|(_, r)| at(r));
                if !inside {
                    stop_typing(d);
                }
            }
            if let Some((_, i)) = d.wall.tabs.iter().find(|(r, _)| at(r)) {
                d.wall.cur = *i;
                return;
            }
            // grab a divider: the boundary between a split's two halves, from
            // the borders (on a pane's screen a press is the start of a selection)
            let root = d.wall.win().map(|w| w.root.clone());
            if let Some(root) = root.filter(|_| d.wall.selection.is_none()) {
                for (path, r, dir) in d.wall.dividers.clone() {
                    let ratio = root.ratio_at(&path).unwrap_or(0.5);
                    let hit = match dir {
                        Dir::Side => {
                            let x = r.x + (r.width as f32 * ratio).round() as u16;
                            m.row >= r.y
                                && m.row < r.y + r.height
                                && (m.column as i32 - x as i32).abs() <= 1
                        }
                        Dir::Stack => {
                            let y = r.y + (r.height as f32 * ratio).round() as u16;
                            m.column >= r.x
                                && m.column < r.x + r.width
                                && (m.row as i32 - y as i32).abs() <= 1
                        }
                    };
                    if hit {
                        d.wall.drag = Some((path, r, dir));
                        return;
                    }
                }
            }
            if let Some((id, _)) = d.wall.rects.iter().find(|(_, r)| at(r)).copied() {
                let again = d.wall.focus() == Some(id);
                if let Some(w) = d.wall.win_mut() {
                    w.focus = id;
                }
                // a click focuses the tile under it, and the keys follow: left
                // on Aleph's they'd run its commands (x closes, y approves) on
                // a session you meant to type into. With focus.keys = aleph,
                // and not typing, a second click starts the typing.
                let follow = was_typing || d.cfg.get_or("focus.keys") != "aleph";
                let session = matches!(
                    d.wall.panes.get(&id).map(|p| &p.content),
                    Some(Content::Session(_))
                );
                if again && !d.wall.typing {
                    run(d, Cmd::Type);
                } else if !again && follow && session {
                    d.wall.typing = true;
                    d.wall.swallow_up = true;
                }
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some((path, r, dir)) = d.wall.drag.clone() {
                let ratio = match dir {
                    Dir::Side => m.column.saturating_sub(r.x) as f32 / r.width.max(1) as f32,
                    Dir::Stack => m.row.saturating_sub(r.y) as f32 / r.height.max(1) as f32,
                };
                if let Some(w) = d.wall.win_mut() {
                    if let Some(Node::Split { ratio: rt, .. }) = w.root.at_path(&path) {
                        *rt = ratio.clamp(0.1, 0.9);
                    }
                }
                d.wall.dirty = true;
            }
        }
        MouseEventKind::Up(_) => d.wall.drag = None,
        _ => {}
    }
}

/// A key's name for a hint (one of the usual ones; any other as "exit key").
fn static_key(k: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "ctrl+q", "ctrl+x", "ctrl+g", "ctrl+o", "ctrl+\\", "alt+q", "f10", "f12",
    ];
    KNOWN
        .iter()
        .find(|x| **x == k)
        .copied()
        .unwrap_or("focus.exit")
}

pub(crate) fn hints(d: &Dash) -> Vec<(&'static str, &'static str)> {
    if d.wall.compose.is_some() {
        return vec![
            ("enter", "send now"),
            ("tab", "after the reply"),
            ("↑", "recall"),
            ("esc", "close (keeps the draft)"),
        ];
    }
    if d.wall.queue_view.is_some() {
        return vec![("enter", "send now"), ("d", "drop"), ("esc", "close")];
    }
    if d.wall.typing {
        return vec![
            (static_key(&keys_back(d)), "Aleph's keys (or click outside)"),
            ("wheel", "scroll back"),
            ("drag", "copy"),
            ("ctrl+b", "prefix"),
        ];
    }
    match d.wall.content_of_focus() {
        Some(Content::Empty) | None => vec![
            ("n", "new session"),
            ("a", "add existing"),
            ("S", "shell"),
            ("v s", "split"),
            ("hjkl", "move"),
            ("x", "close pane"),
            ("?", "keys"),
        ],
        _ if d.wall.in_focus() => vec![
            ("i", "message"),
            ("y", "approve"),
            ("t", "timeline"),
            ("[ ]", "session"),
            ("{ }", "needs you"),
            ("O", "new tab"),
            ("a", "attach"),
            ("| _", "split"),
            ("Q", "queue"),
            ("esc", "back"),
            ("?", "keys"),
        ],
        _ => vec![
            ("i", "message"),
            ("I", "raw keys"),
            ("y", "approve"),
            ("Q", "queue"),
            ("a", "attach"),
            ("O", "new tab"),
            ("X", "close"),
            ("v s", "split"),
            ("hjkl", "move"),
            ("?", "keys"),
        ],
    }
}

// ------------------------------------------------------------------ drawing

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    enter(d);
    tile_everything(d);
    let [body, bar] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(r);
    let Some(w) = d.wall.win() else {
        let polling = d
            .fleet
            .hosts
            .iter()
            .any(|h| h.conn == crate::fleet::Conn::Unknown);
        let msg = if polling {
            format!("{} looking for sessions…", ui::spinner(d.t0))
        } else {
            "no sessions to show — n starts one · esc back".to_string()
        };
        let y = body.y + body.height / 3;
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(msg, ui::muted()))).alignment(Alignment::Center),
            Rect {
                y,
                height: 1,
                ..body
            },
        );
        if !polling {
            // showcase mode: stars around it
            crate::showcase::starfield(&d.cfg, f.buffer_mut(), body);
        }
        return;
    };
    let (focus, zoom, leaves) = (w.focus, w.zoom, w.root.leaves());
    // (the window's tree, owned: the side panel below may change the dashboard)
    let root = w.root.clone();
    // Focus: the session's timeline at the side, when there's room
    d.wall.body_w = body.width;
    let body = match d.wall.focused_session(d) {
        Some(s) if d.wall.in_focus() && super::timeline::shown(d, body.width) => {
            let [main, side] = Layout::horizontal([
                Constraint::Min(40),
                Constraint::Length(super::timeline::width(body.width)),
            ])
            .areas(body);
            super::timeline::draw(d, f, side, &s);
            main
        }
        _ => body,
    };
    let mut rects = Vec::new();
    let mut divs = Vec::new();
    if zoom {
        rects.push((focus, body));
    } else {
        root.layout(body, &mut rects, &mut divs, &mut vec![]);
    }
    d.wall.rects = rects.clone();
    d.wall.dividers = divs;
    let numbers = d
        .wall
        .numbers
        .is_some_and(|t| t.elapsed() < Duration::from_secs(2));
    let mut cursor = None;
    for (id, rect) in rects {
        if rect.width < 3 || rect.height < 3 {
            continue;
        }
        let is_focus = id == focus;
        let n = leaves.iter().position(|x| *x == id).unwrap_or(0);
        let content = d
            .wall
            .panes
            .get(&id)
            .map(|p| p.content.clone())
            .unwrap_or(Content::Empty);
        let plain_border = |title: String| {
            Block::bordered()
                .border_set(crate::ui::border_set(is_focus))
                .border_style(ui::fg(if is_focus { c::ACCENT() } else { c::LINE() }))
                .title(Line::from(Span::styled(title, ui::faint())))
        };
        match content {
            Content::Session(key) => {
                if let Some(cur) = draw_session(d, f, rect, id, &key, n, is_focus) {
                    cursor = Some(cur);
                }
            }
            Content::Starting(what) => {
                let block = plain_border(format!(" {n} "));
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                center(
                    f,
                    inner,
                    vec![Line::from(vec![
                        Span::styled(format!("{} ", ui::spinner(d.t0)), ui::fg(c::ACCENT())),
                        Span::styled(format!("starting {what}…"), ui::muted()),
                    ])],
                );
            }
            Content::Empty => {
                let block = plain_border(format!(" {n} empty "));
                let inner = block.inner(rect);
                f.render_widget(block, rect);
                center(
                    f,
                    inner,
                    vec![
                        Line::from(Span::styled("an empty pane", ui::muted())),
                        Line::raw(""),
                        ui::hints(&[("n", "new session"), ("a", "add existing"), ("S", "shell")])
                            .centered(),
                    ],
                );
            }
        }
        if numbers {
            let label = format!(" {n} ");
            let nr = Rect {
                x: rect.x + rect.width.saturating_sub(label.len() as u16) / 2,
                y: rect.y + rect.height / 2,
                width: (label.len() as u16).min(rect.width),
                height: 1,
            };
            crate::ui::clear(f, nr);
            f.render_widget(
                Paragraph::new(Span::styled(
                    label,
                    Style::new()
                        .fg(c::BASE())
                        .bg(if is_focus { c::ACCENT() } else { c::BLUE() })
                        .add_modifier(Modifier::BOLD),
                )),
                nr,
            );
        }
    }
    if let Some((x, y)) = cursor {
        f.set_cursor_position((x, y));
    }
    draw_selection(d, f);
    if d.wall.queue_view.is_some() {
        draw_queue(d, f, body);
    }
    status_bar(d, f, bar);
    if d.wall.cheats {
        cheatsheet(d, f, r);
    }
    // which-key: a prefix waiting for its key shows what comes next
    match (d.wall.pending, d.wall.pending_since) {
        (Pending::None, _) => d.wall.pending_since = None,
        (_, None) => d.wall.pending_since = Some(Instant::now()),
        (p, Some(t)) if t.elapsed() >= Duration::from_millis(400) => which_key(d, f, body, p),
        _ => {}
    }
}

/// The keys a pending prefix (tmux) or ctrl+w (vim) takes, bottom right.
fn which_key(d: &Dash, f: &mut Frame, r: Rect, p: Pending) {
    let items: &[(&str, &str)] = match p {
        Pending::Prefix => &[
            ("% \"", "split side · stacked"),
            ("arrows", "move"),
            ("o ;", "next · previous pane"),
            ("x z", "close · zoom"),
            ("c n p", "new · next · prev window"),
            ("0–9", "window"),
            (", &", "rename · close window"),
            ("space { }", "layout · swap"),
            ("= q", "equalise · numbers"),
            ("[ ]", "session"),
            ("esc i", "stop · start typing"),
            ("d", "back"),
            ("?", "every key"),
        ],
        Pending::Win => &[
            ("s v", "split stacked · side"),
            ("h j k l", "move"),
            ("w W", "next · previous"),
            ("q c", "close"),
            ("o =", "zoom · equalise"),
            ("< > - +", "resize"),
            ("x r", "swap"),
            ("T n", "new window"),
        ],
        Pending::None => return,
    };
    let title = match p {
        Pending::Prefix => format!("{} then", prefix_key(d)),
        _ => "ctrl+w then".into(),
    };
    let w = 40u16.min(r.width);
    let h = (items.len() as u16 + 2).min(r.height);
    let area = Rect::new(
        r.x + r.width.saturating_sub(w + 1),
        r.y + r.height.saturating_sub(h),
        w,
        h,
    );
    f.render_widget(ratatui::widgets::Clear, area);
    let lines: Vec<Line> = items
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(
                    format!(" {k:<10}"),
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(v.to_string(), ui::muted()),
            ])
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines).block(ui::panel(title.as_str(), true)),
        area,
    );
}

/// One session pane; returns the cursor when typing into it.
fn draw_session(
    d: &Dash,
    f: &mut Frame,
    rect: Rect,
    id: u32,
    key: &str,
    n: usize,
    is_focus: bool,
) -> Option<(u16, u16)> {
    let s = super::fleet::session(d, key);
    let (h, pane_id) = key.split_once('|').unwrap_or_default();
    let host = d.fleet.host(h);
    let hcol = host.map(|x| x.color).unwrap_or(c::MUTED());
    let border = if is_focus && d.wall.typing {
        ui::fg(c::GREEN())
    } else if is_focus {
        ui::fg(c::ACCENT())
    } else {
        ui::fg(hcol)
    };
    let mut block = Block::bordered()
        .border_set(crate::ui::border_set(is_focus))
        .border_style(border);
    let stream = d.wall.panes.get(&id).and_then(|p| p.stream.as_ref());
    let sstate = stream.map(Stream::state);
    let (live, lcol) = match &sstate {
        None => ("○ paused".to_string(), c::FAINT()),
        Some(x) if x.closed => ("■ ended".to_string(), c::FAINT()),
        Some(x) if x.polling => ("◌ polling".to_string(), c::YELLOW()),
        Some(x) if x.connected => ("● live".to_string(), c::GREEN()),
        // it streamed before: the last frame stays up, frozen, while it reconnects
        Some(x) if x.frames > 0 => (
            format!(
                "{} · frozen {}",
                if d.cut_off(key.split('|').next().unwrap_or_default()) {
                    "○ offline"
                } else {
                    "⟳ reconnecting"
                },
                ui::dur(x.last.map_or(0, |t| t.elapsed().as_secs()))
            ),
            c::YELLOW(),
        ),
        Some(x) if x.err.is_some() => (format!("{} reconnecting", ui::spinner(d.t0)), c::YELLOW()),
        Some(_) => (format!("{} connecting", ui::spinner(d.t0)), c::MUTED()),
    };
    match &s {
        Some(s) => {
            let (st, scol) = super::fleet::status_for(d, s);
            let folder = host.map(|x| x.short_path(&s.cwd)).unwrap_or_default();
            let mut title = vec![
                Span::styled(format!(" {n} "), ui::faint()),
                Span::styled(format!("{} ", s.glyph()), ui::fg(s.color())),
                Span::styled(
                    s.display().to_string(),
                    Style::new().fg(s.color()).add_modifier(Modifier::BOLD),
                ),
                Span::styled(" · ", ui::faint()),
                Span::styled(h.to_string(), ui::fg(hcol)),
                Span::styled(" · ", ui::faint()),
                crate::logo::glyph(crate::logo::Kind::Agent, &s.agent),
                Span::styled(format!(" {} ", crate::agents::label(&s.agent)), ui::faint()),
            ];
            // its model, effort and usage (from its own logs), where it fits
            if let Some(l) = super::usage::live_for(d, s) {
                let used: usize = title.iter().map(|x| ui::width(&x.content)).sum();
                let room = (rect.width as usize).saturating_sub(used + ui::width(&st) + 8);
                let mut model = crate::timeline::short_model(&l.model);
                if l.models.len() > 1 {
                    // it switched: how many it has been on
                    model = format!("{model} ({} models)", l.models.len());
                }
                if !l.effort.is_empty() {
                    model = format!("{model} {}", l.effort);
                }
                if room > ui::width(&model) + 16 {
                    title.push(Span::styled(format!("· {model} · "), ui::muted()));
                    title.extend(super::usage::chip(l, room - ui::width(&model) - 6));
                    title.push(Span::raw(" "));
                } else if room > 14 {
                    title.push(Span::styled("· ", ui::faint()));
                    title.extend(super::usage::chip(l, room - 2));
                    title.push(Span::raw(" "));
                }
            }
            block = block
                .title(Line::from(title))
                .title(Line::from(Span::styled(format!(" {st} "), ui::fg(scol))).right_aligned())
                .title_bottom(Line::from(Span::styled(
                    format!(
                        " {} ",
                        ui::trunc_left(&folder, (rect.width as usize).saturating_sub(16).max(4))
                    ),
                    ui::faint(),
                )))
                .title_bottom(
                    Line::from(Span::styled(format!(" {live} "), ui::fg(lcol))).right_aligned(),
                );
            let q = crate::queue::for_session(&s.host, &s.name).len();
            if q > 0 {
                let wait = if s.blocked() {
                    " · after you answer"
                } else {
                    ""
                };
                block = block.title_bottom(
                    Line::from(Span::styled(
                        format!(" ≡ {q} queued{wait} "),
                        ui::fg(c::BLUE()).add_modifier(Modifier::BOLD),
                    ))
                    .centered(),
                );
            }
        }
        None => {
            block = block.title(Line::from(Span::styled(
                format!(" {n} {h} · {pane_id} "),
                ui::faint(),
            )));
        }
    }
    let inner = block.inner(rect);
    f.render_widget(block, rect);
    let up = host.is_some_and(|x| x.conn == crate::fleet::Conn::Up);
    if s.is_none() && up {
        center(
            f,
            inner,
            vec![
                Line::from(Span::styled("■ this session ended", ui::muted())),
                Line::raw(""),
                ui::hints(&[
                    ("n", "new one here"),
                    ("a", "add existing"),
                    ("x", "close pane"),
                ])
                .centered(),
            ],
        );
        return None;
    }
    let cur = stream.and_then(|stream| stream.render(f.buffer_mut(), inner));
    // scrolled back with the wheel: how far, and the way back
    if let Some((up, loading)) = stream.map(Stream::scrolled).filter(|x| x.0 > 0 || x.1) {
        let text = if loading {
            format!(" {} reading its history… ", ui::spinner(d.t0))
        } else {
            format!(" ↑ {up} lines back · scroll down or type to return ")
        };
        let w = (ui::width(&text) as u16).min(inner.width);
        let at = Rect::new(inner.x + inner.width - w, inner.y, w, 1);
        f.render_widget(
            Paragraph::new(Span::styled(
                text,
                Style::new()
                    .fg(c::BASE())
                    .bg(c::YELLOW())
                    .add_modifier(Modifier::BOLD),
            )),
            at,
        );
    }
    if sstate.as_ref().is_some_and(|x| x.frames == 0) {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{} connecting to {h}…", ui::spinner(d.t0)),
                ui::muted(),
            )),
            inner,
        );
    }
    if let Some(c) = d.wall.compose.as_ref().filter(|c| c.pane == id) {
        draw_compose(d, f, inner, c, s.as_ref());
        return None;
    }
    (is_focus && d.wall.typing).then_some(cur).flatten()
}

/// The message bar over the bottom of a session pane.
fn draw_compose(d: &Dash, f: &mut Frame, inner: Rect, c: &Compose, s: Option<&Session>) {
    let h = 3.min(inner.height);
    if h < 3 || inner.width < 12 {
        return;
    }
    let r = Rect {
        y: inner.y + inner.height - h,
        height: h,
        ..inner
    };
    let name = s.map(|s| s.display().to_string()).unwrap_or_default();
    let w = r.width as usize;
    let keys = "enter send · tab after reply · ↑ recall · esc";
    let short = "enter · tab queue · esc";
    let mut block = Block::bordered()
        .border_set(crate::ui::border_set(false))
        .border_style(ui::fg(c::ACCENT()))
        .title(Line::from(vec![
            Span::styled(" ✎ ", ui::fg(c::ACCENT())),
            Span::styled(format!("to {name} "), ui::bold()),
        ]));
    let room = w.saturating_sub(ui::width(&name) + 10);
    let k = if ui::width(keys) + 2 <= room {
        keys
    } else if ui::width(short) + 2 <= room {
        short
    } else {
        ""
    };
    if !k.is_empty() {
        block =
            block.title(Line::from(Span::styled(format!(" {k} "), ui::faint())).right_aligned());
    }
    if let Some(s) = s {
        let q = crate::queue::for_session(&s.host, &s.name);
        if !q.is_empty() {
            let mut t = format!(" ≡ {} queued", q.len());
            for x in &q {
                t.push_str(&format!(" · “{}”", ui::trunc(&x.prompt, 24)));
            }
            block = block.title_bottom(Line::from(vec![
                Span::styled(ui::trunc(&t, w.saturating_sub(14)), ui::fg(c::BLUE())),
                Span::styled(" · Q manage ", ui::faint()),
            ]));
        } else if !matches!(s.status.as_str(), "idle" | "done") {
            let (st, col) = super::fleet::status_for(d, s);
            block = block.title_bottom(Line::from(vec![
                Span::styled(format!(" {st}"), ui::fg(col)),
                Span::styled(" — tab sends it once it's done ", ui::faint()),
            ]));
        }
    }
    let ir = block.inner(r);
    crate::ui::clear(f, r);
    f.render_widget(block, r);
    let mut l = vec![Span::styled("❯ ", ui::fg(c::ACCENT()))];
    l.extend(field_window(
        &c.field,
        (ir.width as usize).saturating_sub(2),
        "message — runs like you typed it",
    ));
    f.render_widget(Paragraph::new(Line::from(l)), ir);
}

/// A one-line view of `fl` that keeps the cursor visible within `w` columns.
fn field_window(fl: &forms::Field, w: usize, placeholder: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = fl.text.chars().collect();
    let cur = fl.cur.min(chars.len());
    if chars.is_empty() {
        return vec![
            Span::styled(" ", Style::new().fg(c::BASE()).bg(c::ACCENT())),
            Span::styled(format!(" {placeholder}"), ui::faint()),
        ];
    }
    let w = w.max(2);
    let start = (cur + 1).saturating_sub(w);
    let end = (start + w).min(chars.len() + 1);
    let mut out = Vec::new();
    let before: String = chars[start..cur].iter().collect();
    out.push(Span::raw(before));
    let at = chars.get(cur).map_or(" ".to_string(), |c| c.to_string());
    out.push(Span::styled(at, Style::new().fg(c::BASE()).bg(c::ACCENT())));
    if cur + 1 < end {
        let after: String = chars[cur + 1..end.min(chars.len())].iter().collect();
        out.push(Span::raw(after));
    }
    out
}

/// The focused session's queue (`Q`).
fn draw_queue(d: &mut Dash, f: &mut Frame, area: Rect) {
    let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) else {
        d.wall.queue_view = None;
        return;
    };
    let q = crate::queue::for_session(&s.host, &s.name);
    if q.is_empty() {
        d.wall.queue_view = None;
        return;
    }
    let sel = d.wall.queue_view.unwrap_or(0).min(q.len() - 1);
    let w = 76.min(area.width.saturating_sub(4));
    let h = (q.len() as u16 + 6).min(area.height.saturating_sub(2));
    let r = ui::centered(area, w, h);
    let (st, col) = super::fleet::status_for(d, &s);
    let block = Block::bordered()
        .border_set(crate::ui::border_set(false))
        .border_style(ui::fg(c::BLUE()))
        .title(Line::from(vec![
            Span::styled(" ≡ queued for ", ui::bold()),
            Span::styled(
                s.display().to_string(),
                ui::fg(s.color()).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ]))
        .title(Line::from(Span::styled(format!(" {st} "), ui::fg(col))).right_aligned());
    let inner = block.inner(r);
    crate::ui::clear(f, r);
    f.render_widget(block, r);
    let iw = inner.width as usize;
    let mut lines = vec![Line::from(Span::styled(
        ui::trunc(
            if s.blocked() {
                " waits for your answer, then goes one by one as each reply finishes"
            } else {
                " goes one by one, each once the previous reply is done"
            },
            iw,
        ),
        ui::faint(),
    ))];
    lines.push(Line::raw(""));
    let now = crate::cache::now();
    for (i, t) in q.iter().enumerate() {
        let on = i == sel;
        let mut l = Line::from(vec![
            Span::styled(if on { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(format!(" {}. ", i + 1), ui::faint()),
            Span::raw(ui::trunc(&t.prompt, iw.saturating_sub(16))),
            Span::styled(
                format!("  {}", ui::ago(now.saturating_sub(t.added))),
                ui::faint(),
            ),
        ]);
        if on {
            l = l.style(ui::selected());
        }
        lines.push(l);
    }
    let hint = ui::hints(&[("enter", "send now"), ("d", "drop"), ("esc", "close")]);
    let room = inner.height as usize;
    while lines.len() + 1 < room {
        lines.push(Line::raw(""));
    }
    lines.truncate(room.saturating_sub(1));
    lines.push(hint);
    f.render_widget(Paragraph::new(lines), inner);
}

fn center(f: &mut Frame, r: Rect, lines: Vec<Line<'static>>) {
    let h = lines.len() as u16;
    let rr = Rect {
        y: r.y + r.height.saturating_sub(h) / 2,
        height: h.min(r.height),
        ..r
    };
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), rr);
}

/// tmux-style: mode · windows · the focused pane.
fn status_bar(d: &mut Dash, f: &mut Frame, r: Rect) {
    let (mode, col) = match (d.wall.pending, d.wall.typing, d.wall.renaming.is_some()) {
        _ if d.wall.compose.is_some() => ("MESSAGE", c::GREEN()),
        _ if d.wall.queue_view.is_some() => ("QUEUE", c::BLUE()),
        (_, _, true) => ("RENAME", c::BLUE()),
        (Pending::Prefix, _, _) => ("PREFIX", c::YELLOW()),
        (Pending::Win, _, _) => ("CTRL-W", c::YELLOW()),
        (_, true, _) => ("TYPING", c::GREEN()),
        _ if d.wall.in_focus() => ("FOCUS", c::ACCENT()),
        _ if d.wall.win().is_some_and(|w| w.name == TILE) => ("TILE", c::ACCENT()),
        _ => ("NORMAL", c::ACCENT()),
    };
    let tag = format!(" {mode} ");
    let mut l = vec![
        Span::styled(
            tag.clone(),
            Style::new()
                .fg(c::BASE())
                .bg(col)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    // an esc held back: what the next one does, and the ways out
    if esc_waits(d) {
        l.push(Span::styled(
            format!("esc again sends it to the agent and stops its work · {} gives the keys back · or click outside the pane", keys_back(d)),
            ui::fg(c::YELLOW()).add_modifier(Modifier::BOLD),
        ));
        d.wall.tabs = Vec::new();
        f.render_widget(Paragraph::new(Line::from(l)), r);
        return;
    }
    let mut x = r.x + ui::width(&tag) as u16 + 1;
    let mut tabs = Vec::new();
    for (i, w) in d.wall.windows.iter().enumerate() {
        let on = i == d.wall.cur;
        let label = match (on, &d.wall.renaming) {
            (true, Some(f)) => format!(" {i}:{}▏ ", f.text),
            _ => format!(" {i}:{}{} ", w.name, if on { "*" } else { "" }),
        };
        let wdt = ui::width(&label) as u16;
        tabs.push((Rect::new(x, r.y, wdt, 1), i));
        x += wdt;
        l.push(Span::styled(
            label,
            if on {
                Style::new()
                    .fg(c::TEXT())
                    .bg(c::SEL())
                    .add_modifier(Modifier::BOLD)
            } else {
                ui::muted()
            },
        ));
    }
    d.wall.tabs = tabs;
    if let Some(s) = d.wall.focus().and_then(|id| session_of(d, id)) {
        l.push(Span::styled("   ", ui::faint()));
        l.push(Span::styled(
            format!("{} {}", s.glyph(), s.display()),
            ui::fg(s.color()),
        ));
        l.push(Span::styled(format!(" on {}", s.host), ui::muted()));
        if let Some(n) = crate::notes::get(&s.host, &s.name) {
            l.push(Span::styled(
                format!("  ✎ {}", ui::trunc(&n.text, 32)),
                ui::fg(c::ACCENT()),
            ));
        }
    }
    if d.wall.win().is_some_and(|w| w.zoom) && !d.wall.in_focus() {
        l.push(Span::styled(
            "  ZOOM",
            ui::fg(c::YELLOW()).add_modifier(Modifier::BOLD),
        ));
    }
    // how to get into / out of the focused session, here and from a shell
    let normal = !d.wall.captures();
    if let Some(s) = d
        .wall
        .focus()
        .and_then(|id| session_of(d, id))
        .filter(|_| normal)
    {
        let used: usize = l.iter().map(|x| ui::width(&x.content)).sum();
        let me = crate::doctor::me();
        let full = format!(
            "a attach · O new tab · X close · {me} resume {h}:{n} · {me} close {h}:{n} ",
            h = s.host,
            n = s.name
        );
        let short = "a attach · X close ".to_string();
        let room = (r.width as usize).saturating_sub(used + 2);
        let tail = if ui::width(&full) <= room {
            full
        } else if ui::width(&short) <= room {
            short
        } else {
            String::new()
        };
        if !tail.is_empty() {
            l.push(Span::raw(" ".repeat(room - ui::width(&tail) + 2)));
            l.push(Span::styled(tail, ui::faint()));
        }
    }
    f.render_widget(Paragraph::new(Line::from(l)), r);
}

pub(crate) fn cheatsheet(d: &Dash, f: &mut Frame, area: Rect) {
    let p = prefix_key(d);
    let head = |t: &str| {
        Line::from(Span::styled(
            t.to_string(),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ))
    };
    let row = |k: &str, w: &str| {
        Line::from(vec![
            Span::styled(format!("  {k:<16}"), ui::bold()),
            Span::styled(w.to_string(), ui::muted()),
        ])
    };
    let left = vec![
        head("Normal mode"),
        row("h j k l  ←↓↑→", "move between panes"),
        row("v · s", "split side by side · stacked"),
        row("o", "next pane"),
        row("x", "close pane (session lives on)"),
        row("z", "zoom the pane"),
        row("< > - +  =", "resize · equalise"),
        row("space", "next layout"),
        row("{ }", "swap panes"),
        row("[ ]", "the previous / next session, on any machine"),
        row(
            "ctrl+[ ] · ( )",
            "the previous / next machine (( ) where a terminal can't tell ctrl+[ from esc)",
        ),
        row(
            "alt+, alt+.",
            "typing: the previous / next session (or the prefix and [ ])",
        ),
        row("c · w W", "new window · next / previous"),
        row("{ }", "Focus: next · previous that needs you"),
        row(", · #", "rename window · pane numbers"),
        row("digits · tab", "other views"),
        Line::raw(""),
        head("Panes"),
        row("i / enter", "message: enter send · tab after reply"),
        row(
            "I",
            "keys straight into it, as its own terminal (Focus opens so)",
        ),
        row(
            "drag",
            "select across a pane, copied when you let go (focus.copy_on_select; option+drag goes to the agent)",
        ),
        row(
            "ctrl+q",
            "keys back to Aleph (or a click outside), then to Fleet (focus.exit; esc is the agent's)",
        ),
        row(
            "ctrl+a",
            "the approval board, typing too, while anything waits on you (else the agent's)",
        ),
        row(
            "ctrl+k",
            "the palette, typing too (the agent never gets ctrl+k)",
        ),
        row(
            "other keys",
            if crate::term::keys_enhanced() {
                "to the agent as pressed — ctrl+enter, ctrl+tab, alt+… too"
            } else {
                "to the agent (this terminal sends ctrl+enter as enter · ui.keyboard)"
            },
        ),
        row(
            "esc esc",
            "esc to the agent: it stops its work (the first is held back · focus.esc_guard)",
        ),
        row("Q · X", "its queue · close the session"),
        row("n · S", "new session here · new shell"),
        row("a", "add an existing session (in an empty pane)"),
        row("y · K · m", "approve · keys · prompt"),
        row("b", "broadcast to this window"),
        row("a · A", "attach in herdr · straight to the agent"),
        row("O", "herdr in a new terminal tab (Aleph stays)"),
        row("T", "push to herdr: real panes, as here"),
    ];
    let right = vec![
        head(&format!("tmux — {p} then")),
        row("% · \"", "split side by side · stacked"),
        row("arrows / hjkl", "move"),
        row("ctrl+arrows", "resize"),
        row("o · ;", "next · previous pane"),
        row("x · z", "close · zoom"),
        row("c n p 0–9", "new · next · prev · pick window"),
        row(", · &", "rename · close window"),
        row("space · { }", "layout · swap"),
        row("q", "pane numbers"),
        row("[ · ] · d", "stop typing · type · back"),
        row(&format!("{p} {p}"), "send the prefix to the pane"),
        Line::raw(""),
        head("vim — ctrl+w then"),
        row("s · v", "split stacked · side by side"),
        row("hjkl · w · W", "move · next · previous"),
        row("q · o · =", "close · zoom · equalise"),
        row("< > - +", "resize"),
        row("x · T", "swap · new window"),
        Line::raw(""),
        row("mouse", "click · click again types · drag"),
        row(
            "wheel",
            "scroll a pane's history (the agent's own, if it asks)",
        ),
    ];
    let w = 112u16.min(area.width.saturating_sub(2));
    let h = (left.len().max(right.len()) as u16 + 3).min(area.height.saturating_sub(2));
    let r = ui::centered(area, w, h);
    crate::ui::clear(f, r);
    let block = ui::panel("Focus · Tile — keys", true)
        .title_bottom(Line::from(Span::styled(" any key closes ", ui::faint())).right_aligned());
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [a, b] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(inner);
    f.render_widget(Paragraph::new(left), a);
    f.render_widget(Paragraph::new(right), b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn herdr_prefix_from_its_config() {
        assert_eq!(
            herdr_prefix("[keys]\nprefix = \"ctrl+a\"\n").as_deref(),
            Some("ctrl+a")
        );
        assert_eq!(
            herdr_prefix("[keys]\nprefix = \"C-a\"\n").as_deref(),
            Some("ctrl+a")
        );
        assert_eq!(herdr_prefix("[ui]\ntheme = \"x\"\n"), None);
        assert_eq!(herdr_prefix("not toml ["), None);
    }

    #[test]
    fn quick_answer_menus() {
        let claude = "⏺ Bash(pytest -x)\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for pytest commands\n   3. No, and tell Claude what to do differently (esc)\n";
        let m = menu(claude);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0], (1, "Yes".to_string()));
        assert!(m[2].1.starts_with("No, and tell"));
        // the cursor on another option, Codex style
        let codex = "  1. Yes, proceed (y)\n› 2. Yes, and don't ask again (a)\n  3. No (esc)\n";
        assert_eq!(menu(codex).len(), 3);
        // a numbered plan above the menu never leaks into it
        let both = "Plan:\n1. Install deps\n2. Run tests\n3. Ship\n4. Tag\n\nProceed?\n❯ 1. Yes\n  2. No\n";
        assert_eq!(menu(both), vec![(1, "Yes".into()), (2, "No".into())]);
        // a list without a cursor isn't a question; neither is one option
        assert!(menu("1. Install deps\n2. Run tests\n").is_empty());
        assert!(menu("❯ 1. Yes\n").is_empty());
        // wrapped labels between options are fine
        let wrapped = "❯ 1. Yes\n   2. Yes, and don't ask again for commands like\n      pytest in this folder\n   3. No\n";
        assert_eq!(menu(wrapped).len(), 3);
    }

    #[test]
    fn split_remove_resize() {
        let mut n = Node::Leaf(1);
        assert!(n.split(1, 2, Dir::Side));
        assert!(n.split(2, 3, Dir::Stack));
        assert_eq!(n.leaves(), vec![1, 2, 3]);
        assert!(n.resize(3, Dir::Stack, 0.1));
        assert!(n.resize(1, Dir::Side, 0.1));
        if let Node::Split { ratio, .. } = &n {
            assert!((*ratio - 0.6).abs() < 1e-6);
        }
        assert!(n.remove(2));
        assert_eq!(n.leaves(), vec![1, 3]);
        assert!(n.remove(1));
        assert_eq!(n, Node::Leaf(3));
        assert!(!n.remove(3));
    }

    #[test]
    fn layouts_cover_the_area_exactly() {
        let r = Rect::new(0, 0, 120, 40);
        for which in 0..LAYOUTS.len() {
            for count in 1..=9u32 {
                let ids: Vec<u32> = (1..=count).collect();
                let n = preset(&ids, which);
                let (mut out, mut dv) = (vec![], vec![]);
                n.layout(r, &mut out, &mut dv, &mut vec![]);
                assert_eq!(out.len(), count as usize);
                let area: u32 = out
                    .iter()
                    .map(|(_, r)| r.width as u32 * r.height as u32)
                    .sum();
                assert_eq!(area, 120 * 40, "layout {which} with {count} panes");
            }
        }
    }

    #[test]
    fn neighbours_by_geometry() {
        let mut n = Node::Leaf(1);
        n.split(1, 2, Dir::Side);
        n.split(2, 3, Dir::Stack);
        let (mut out, mut dv) = (vec![], vec![]);
        n.layout(Rect::new(0, 0, 100, 40), &mut out, &mut dv, &mut vec![]);
        assert_eq!(neighbour(&out, 1, 1, 0), Some(2));
        assert_eq!(neighbour(&out, 2, 0, 1), Some(3));
        assert_eq!(neighbour(&out, 3, -1, 0), Some(1));
        assert_eq!(neighbour(&out, 1, -1, 0), None);
    }

    #[test]
    fn json_roundtrip() {
        let mut n = Node::Leaf(4);
        n.split(4, 7, Dir::Stack);
        assert_eq!(Node::from_json(&n.to_json()), Some(n));
    }

    #[test]
    fn the_window_as_herdrs_tree() {
        use crate::layouts::{Dir as H, Tree};
        // 1 | (2 / 3), then 3 split side by side with 4
        let mut n = Node::Leaf(1);
        n.split(1, 2, Dir::Side);
        n.split(2, 3, Dir::Stack);
        n.split(3, 4, Dir::Side);
        let t = n.to_tree();
        assert_eq!(
            t.leaves().into_iter().copied().collect::<Vec<_>>(),
            n.leaves()
        );
        assert!(matches!(&t, Tree::Split { dir: H::Right, .. }));
        // pushing only some panes (one machine's): the rest give way
        let p = t.prune(&mut |id| *id != 2 && *id != 4).unwrap();
        assert_eq!(p, Tree::split(H::Right, 0.5, Tree::Pane(1), Tree::Pane(3)));
    }

    #[test]
    fn key_dialects() {
        let k = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(tmux(&k(KeyCode::Char('%'))), Some(Cmd::SplitSide));
        assert_eq!(tmux(&k(KeyCode::Char('"'))), Some(Cmd::SplitStack));
        assert_eq!(tmux(&k(KeyCode::Char('3'))), Some(Cmd::Window(3)));
        assert_eq!(vim_win(&k(KeyCode::Char('v'))), Some(Cmd::SplitSide));
        assert_eq!(vim_win(&k(KeyCode::Char('s'))), Some(Cmd::SplitStack));
        assert_eq!(vim_win(&k(KeyCode::Char('l'))), Some(Cmd::Move(1, 0)));
    }
}
