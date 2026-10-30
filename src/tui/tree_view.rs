//! The timeline drawn: one agent conversation's tree (`crate::timeline`) in a
//! panel — Fleet's right panel (`T`), Focus's side, Monitor › Timeline.
//! Loaded off the UI thread, again every few seconds while it's on screen
//! (and its machine's logs asked to be read then too), folds kept per
//! session. The wheel scrolls it, a click folds or opens a row, and its top line
//! has fold all, expand all and the turn view to click.
//!
//! The turn view (`Mode::Turns`: `t` in Monitor › Timeline, `G` in Focus, `G`
//! again in Fleet) is the same tree taking the whole screen, with its keys:
//! j/k, PgUp/PgDn, enter/space fold one, ← → close / open, z fold all, Z expand
//! all, v by session › by prompt › every step (what ran under each, counted),
//! ( ) prompts, e the next error, f follows the end, w words (read live from
//! the machine, never kept).

use super::Dash;
use crate::timeline::{self, Fold, Row, RowKind, Tree};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// How often a shown timeline is read again.
const AGAIN: Duration = Duration::from_secs(10);

/// (host, agent, the agent's session id)
pub(crate) type Key = (String, String, String);

#[derive(Default)]
pub(crate) struct State {
    pub key: Option<Key>,
    pub tree: Option<Tree>,
    /// row id → open
    pub toggled: HashMap<String, bool>,
    pub sel: usize,
    pub scroll: usize,
    /// keep the end in view (it grows while the agent works)
    pub follow: bool,
    /// the words (prompts' first lines, tools' targets), read live; shown or not
    pub words: HashMap<String, String>,
    pub show_words: bool,
    words_for: Option<Key>,
    /// the last level `v` folded to (0 sessions, 1 prompts, 2 every step)
    pub level: u8,
    /// z or Z last asked for this (true: folded): the next conversation opens so too
    wish: Option<bool>,
    /// the session is working now (its last prompt is still open)
    pub running: bool,
    pub mode: Option<Fold>,
    gen: u64,
    at: Option<Instant>,
    rx: Option<Receiver<(u64, Key, Tree)>>,
    wrx: Option<Receiver<(Key, HashMap<String, String>)>>,
    /// where the last draw put it (for the mouse): the rows' rect, the first row shown
    pub area: Option<Rect>,
    pub rows_n: usize,
    /// asked for its machine's logs (now and then while it's shown)
    nudged: Option<Instant>,
    /// last drawn: so a frame that doesn't draw it stops the reloads
    pub drawn: Option<Instant>,
    /// what the turn view calls it
    pub title: String,
    /// its top line, to click
    buttons: Vec<(Rect, Button)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Button {
    FoldAll,
    ExpandAll,
    TurnView,
}

impl State {
    pub fn mode(&self, d: &crate::config::Config) -> Fold {
        self.mode
            .unwrap_or_else(|| Fold::parse(&d.get_or("timeline.fold")))
    }
}

/// Tests: this tree, as if just read (no reload for a while).
#[cfg(test)]
pub(crate) fn set_tree(d: &mut Dash, t: Tree) {
    d.tv.tree = Some(t);
    d.tv.at = Some(Instant::now());
}

/// Show this conversation (the same one: nothing changes; another: from the top).
pub(crate) fn show(d: &mut Dash, host: &str, agent: &str, session: &str, running: bool) {
    let k: Key = (host.to_string(), agent.to_string(), session.to_string());
    d.tv.running = running;
    if d.tv.key.as_ref() == Some(&k) {
        return;
    }
    d.tv.key = Some(k);
    d.tv.tree = None;
    d.tv.toggled.clear();
    d.tv.sel = 0;
    d.tv.scroll = 0;
    d.tv.follow = running;
    d.tv.words.clear();
    d.tv.words_for = None;
    d.tv.at = None;
}

/// The whole turn view of this conversation, with its keys.
pub(crate) fn open_full(
    d: &mut Dash,
    host: &str,
    agent: &str,
    session: &str,
    running: bool,
    title: String,
) {
    show(d, host, agent, session, running);
    d.tv.title = title;
    d.mode = super::Mode::Turns;
}

/// The turn view of a live session. Not every one has a conversation to show:
/// it says so, and false.
pub(crate) fn open_session(d: &mut Dash, s: &crate::herdr::Session) -> bool {
    let Some(id) = s.native_id.clone() else {
        d.note(if s.agent == crate::agents::SHELL {
            "a terminal has no turns: t shows the commands it ran"
        } else {
            "no turns to show yet: its agent hasn't said which conversation it is"
        });
        return false;
    };
    let title = format!("{} · {}", s.display(), crate::agents::label(&s.agent));
    open_full(d, &s.host, &s.agent, &id, s.status == "working", title);
    true
}

/// Every frame: take what was loaded; load again when due and it's on screen.
pub(crate) fn pump(d: &mut Dash) {
    if let Some(rx) = &d.tv.rx {
        if let Ok((g, k, t)) = rx.try_recv() {
            if g == d.tv.gen && d.tv.key.as_ref() == Some(&k) {
                // the first time it's read: as you last asked. After that a row
                // you opened or closed stays that way
                let first = d.tv.tree.is_none();
                d.tv.tree = Some(t);
                if first {
                    apply_wish(d);
                }
            }
        }
    }
    if let Some(rx) = &d.tv.wrx {
        if let Ok((k, w)) = rx.try_recv() {
            if d.tv.key.as_ref() == Some(&k) {
                d.tv.words = w;
                d.tv.words_for = Some(k);
            }
        }
    }
    let Some(k) = d.tv.key.clone() else { return };
    // only while it's drawn
    if d.tv
        .drawn
        .is_none_or(|t| t.elapsed() > Duration::from_secs(2))
    {
        return;
    }
    // the words, once a session, when they're wanted
    if d.tv.show_words
        && d.tv.words_for.as_ref() != Some(&k)
        && d.cfg.get_or("timeline.words") != "off"
    {
        d.tv.words_for = Some(k.clone());
        let k = k.clone();
        let (tx, rx) = channel();
        d.tv.wrx = Some(rx);
        std::thread::spawn(move || {
            let w = crate::usage::steps::words(&k.0, &k.1, &k.2).unwrap_or_default();
            let _ = tx.send((k, w));
        });
    }
    if d.tv.at.is_some_and(|t| t.elapsed() < AGAIN) {
        return;
    }
    d.tv.gen += 1;
    d.tv.at = Some(Instant::now());
    let (tx, rx) = channel();
    d.tv.rx = Some(rx);
    let (cfg, g, k2) = (d.cfg.clone(), d.tv.gen, k.clone());
    std::thread::spawn(move || {
        let t = timeline::load(&cfg, &k2.0, &k2.1, &k2.2);
        let _ = tx.send((g, k2, t));
    });
    // while it's working: its machine's logs read more often than the daemon's minute
    if d.tv.running
        && d.tv
            .nudged
            .is_none_or(|t| t.elapsed() > Duration::from_secs(15))
    {
        d.tv.nudged = Some(Instant::now());
        let (cfg, host) = (d.cfg.clone(), k.0.clone());
        std::thread::spawn(move || {
            let _ = crate::usage::sync(&cfg, &[host], false, Duration::from_millis(200), |_| {});
        });
    }
}

fn rows(d: &Dash) -> Vec<Row> {
    let mut v = match &d.tv.tree {
        Some(t) => timeline::rows(t, d.tv.mode(&d.cfg), &d.tv.toggled, d.tv.running),
        None => Vec::new(),
    };
    // compacting right now, by its screen: the log says so once it's over
    if let Some((host, _, id)) = &d.tv.key {
        let now = d.fleet.sessions().iter().any(|s| {
            s.compacting && s.host == *host && s.native_id.as_deref() == Some(id.as_str())
        });
        if now {
            let ms = crate::cache::now() * 1000;
            v.push(Row {
                id: "compacting-now".into(),
                depth: 0,
                kind: RowKind::Compact,
                label: "compacting now…".into(),
                start: ms,
                end: ms,
                sums: Default::default(),
                foldable: false,
                folded: false,
                bad: false,
                lane: String::new(),
                key: String::new(),
            });
        }
    }
    v
}

fn glyph(r: &Row) -> &'static str {
    match r.kind {
        RowKind::Prompt if r.folded => "▸ ",
        RowKind::Prompt => "┌ ",
        RowKind::PromptEnd => "└ ",
        RowKind::Lane => {
            if r.folded {
                "⑂▸"
            } else {
                "⑂ "
            }
        }
        RowKind::Turn if r.folded => "◆▸",
        RowKind::Turn => "◆ ",
        RowKind::Group if r.folded => "▸ ",
        RowKind::Group => "▾ ",
        RowKind::Tool if r.foldable && r.folded => "›▸",
        RowKind::Tool => "› ",
        RowKind::Compact => "≋ ",
        RowKind::Error => "✗ ",
    }
}

/// Draw it into `r` (a panel's inside). `focused`: the keys are its.
pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect, focused: bool) {
    d.tv.drawn = Some(Instant::now());
    if r.height == 0 || r.width < 12 {
        return;
    }
    let Some(k) = d.tv.key.clone() else {
        f.render_widget(
            Paragraph::new(Span::styled("no conversation picked", ui::faint())),
            r,
        );
        return;
    };
    let Some(tree) = &d.tv.tree else {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{} reading its timeline…", ui::spinner(d.t0)),
                ui::muted(),
            )),
            r,
        );
        return;
    };
    if tree.lanes.iter().all(|l| l.prompts.is_empty()) {
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("nothing read for it yet", ui::muted())),
                Line::from(Span::styled(
                    "its machine's logs are read about once a minute (S in Usage reads now)",
                    ui::faint(),
                )),
            ]),
            r,
        );
        return;
    }
    // its top line, when there's room: what the keys do, to click
    let r = if r.height >= 5 {
        top_line(d, f, r, focused);
        Rect {
            y: r.y + 1,
            height: r.height - 1,
            ..r
        }
    } else {
        d.tv.buttons.clear();
        r
    };
    let Some(tree) = &d.tv.tree else { return };
    let all = rows(d);
    let n = all.len();
    let h = r.height as usize;
    if d.tv.follow {
        d.tv.sel = n.saturating_sub(1);
    }
    d.tv.sel = d.tv.sel.min(n.saturating_sub(1));
    // keep the selection in view
    if d.tv.sel < d.tv.scroll {
        d.tv.scroll = d.tv.sel;
    } else if d.tv.sel >= d.tv.scroll + h {
        d.tv.scroll = d.tv.sel + 1 - h;
    }
    d.tv.scroll = d.tv.scroll.min(n.saturating_sub(h));
    d.tv.area = Some(r);
    d.tv.rows_n = n;
    let w = r.width as usize;
    let tz = ui::utc_offset();
    let lanes: Vec<String> = tree.lanes.iter().map(|l| l.id.clone()).collect();
    let mut out = Vec::new();
    for (i, row) in all.iter().enumerate().skip(d.tv.scroll).take(h) {
        let sel = focused && i == d.tv.sel;
        let lane_i = lanes.iter().position(|l| *l == row.lane).unwrap_or(0);
        let gutter = if row.lane.is_empty() {
            Span::raw(" ")
        } else {
            Span::styled("▎", Style::new().fg(super::charts::series(lane_i)))
        };
        let clock = {
            let l = (row.start / 1000) as i64 + tz;
            let dd = l.rem_euclid(86_400);
            format!("{:02}:{:02} ", dd / 3600, dd % 3600 / 60)
        };
        let figs_full = timeline::figures(row);
        let st = if row.bad {
            ui::fg(c::RED())
        } else {
            match row.kind {
                RowKind::Prompt | RowKind::Lane => ui::bold(),
                RowKind::PromptEnd => ui::muted(),
                RowKind::Compact => ui::fg(c::MAUVE()),
                RowKind::Group => ui::fg(c::ACCENT()),
                _ => Style::new(),
            }
        };
        let indent = "  ".repeat(row.depth as usize);
        let mut left = vec![
            gutter,
            Span::styled(if w >= 40 { clock } else { String::new() }, ui::faint()),
            Span::raw(indent),
            Span::styled(
                glyph(row),
                ui::fg(if row.kind == RowKind::Compact {
                    c::MAUVE()
                } else {
                    c::ACCENT()
                }),
            ),
            Span::styled(row.label.clone(), st),
        ];
        // the figures in what the label leaves (the counts give way first)
        let used: usize = left.iter().map(|s| ui::width(&s.content)).sum();
        let figs = if ui::width(&figs_full) + used + 2 <= w {
            figs_full
        } else {
            timeline::figures_in(row, w.saturating_sub(used + 2))
        };
        // the words in what's left after the figures (they give way first)
        if d.tv.show_words {
            if let Some(word) = d.tv.words.get(&row.key) {
                let used: usize = left.iter().map(|s| ui::width(&s.content)).sum();
                let room = w.saturating_sub(used + ui::width(&figs) + 4);
                if room >= 6 {
                    let text = crate::showcase::text(word);
                    left.push(Span::styled(
                        format!("  {}", ui::trunc(&text, room)),
                        ui::faint(),
                    ));
                }
            }
        }
        let lw: usize = left.iter().map(|s| ui::width(&s.content)).sum();
        let room = w.saturating_sub(lw + 1);
        let figs = if ui::width(&figs) <= room {
            figs
        } else {
            ui::trunc(&figs, room)
        };
        // the figures right-aligned
        let pad = w.saturating_sub(lw + ui::width(&figs));
        let mut spans = left;
        if lw > w {
            // too long: cut the label
            let keep = w.saturating_sub(2);
            let text: String = spans.iter().map(|s| s.content.to_string()).collect();
            spans = vec![Span::styled(ui::trunc(&text, keep), st)];
        } else {
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(Span::styled(figs, ui::muted()));
        }
        let mut l = Line::from(spans);
        if sel {
            l = l.style(ui::selected());
        }
        out.push(l);
    }
    f.render_widget(Paragraph::new(out), r);
    let _ = k;
}

/// The tree's top line: fold all, expand all, and from a panel the turn view.
fn top_line(d: &mut Dash, f: &mut Frame, r: Rect, focused: bool) {
    let mut items = vec![
        (Button::FoldAll, "▸ fold all"),
        (Button::ExpandAll, "▾ expand all"),
    ];
    if !focused {
        items.push((Button::TurnView, "↗ turn view"));
    }
    let mut x = r.x + 1;
    let mut spans = vec![Span::raw(" ")];
    d.tv.buttons.clear();
    for (b, label) in items {
        let w = ui::width(label) as u16;
        if x + w > r.x + r.width {
            break;
        }
        d.tv.buttons.push((
            Rect {
                x,
                y: r.y,
                width: w,
                height: 1,
            },
            b,
        ));
        spans.push(Span::styled(label, ui::muted()));
        spans.push(Span::raw("   "));
        x += w + 3;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), Rect { height: 1, ..r });
}

/// The turn view: the tree on the whole screen, with its keys.
pub(crate) fn draw_full(d: &mut Dash, f: &mut Frame, area: Rect) {
    ui::clear(f, area);
    let mode = match d.tv.mode(&d.cfg) {
        Fold::Smart => "smart",
        Fold::Open => "open",
        Fold::Folded => "folded",
    };
    let block = ui::panel(
        Line::from(vec![
            Span::raw(" Turns "),
            Span::styled(format!("· {} ", d.tv.title), ui::muted()),
            Span::styled(format!("· {mode} "), ui::faint()),
        ]),
        true,
    );
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let body = Rect {
        height: inner.height - 1,
        ..inner
    };
    draw(d, f, body, true);
    let foot = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    let mut h = hints();
    h.push(("esc", "back"));
    f.render_widget(Paragraph::new(ui::hints_fit(&h, foot.width)), foot);
}

fn apply_wish(d: &mut Dash) {
    match d.tv.wish {
        Some(true) => fold_all(d),
        Some(false) => expand_all(d),
        None => {}
    }
}

/// z or Z from outside the tree (Monitor › Timeline): this conversation, and the
/// ones after it. A tree still being read is folded when it arrives.
pub(crate) fn fold_every(d: &mut Dash, fold: bool) {
    d.tv.wish = Some(fold);
    if d.tv.tree.is_some() {
        apply_wish(d);
    }
}

/// Everything down to a line per prompt, with any compaction still showing.
pub(crate) fn fold_all(d: &mut Dash) {
    d.tv.wish = Some(true);
    d.tv.follow = false;
    d.tv.level = 1;
    fold_to(d, 1);
}

/// Everything open, level by level.
pub(crate) fn expand_all(d: &mut Dash) {
    d.tv.wish = Some(false);
    d.tv.level = 2;
    d.tv.toggled.insert("L".into(), true);
    for _ in 0..8 {
        let now = rows(d);
        let mut more = false;
        for r in now.iter().filter(|r| r.foldable && r.folded) {
            d.tv.toggled.insert(r.id.clone(), true);
            more = true;
        }
        if !more {
            break;
        }
    }
}

/// Fold the tree to a level: 0 each session (and subagent) on a line, 1 each
/// prompt, 2 every step open.
fn fold_to(d: &mut Dash, level: u8) {
    let all = rows(d);
    match level {
        0 => {
            d.tv.toggled.insert("L".into(), false);
            for r in all.iter().filter(|r| r.kind == RowKind::Lane) {
                d.tv.toggled.insert(r.id.clone(), false);
            }
        }
        _ => {
            d.tv.toggled.insert("L".into(), true);
            // open level by level, nested subagents too
            for _ in 0..8 {
                let now = rows(d);
                let mut more = false;
                for r in now
                    .iter()
                    .filter(|r| r.folded && (r.kind == RowKind::Lane || (level == 2 && r.foldable)))
                {
                    d.tv.toggled.insert(r.id.clone(), true);
                    more = true;
                }
                if !more {
                    break;
                }
            }
            if level == 1 {
                for r in rows(d).iter().filter(|r| r.kind == RowKind::Prompt) {
                    d.tv.toggled.insert(r.id.clone(), false);
                }
            }
        }
    }
    d.tv.sel = 0;
    d.tv.scroll = 0;
}

fn toggle(d: &mut Dash, i: usize) {
    let all = rows(d);
    if let Some(row) = all.get(i).filter(|r| r.foldable) {
        d.tv.toggled.insert(row.id.clone(), row.folded);
    }
}

/// The mouse over the panel (true: it was there).
pub(crate) fn mouse(d: &mut Dash, m: &MouseEvent) -> bool {
    let fresh =
        d.tv.drawn
            .is_some_and(|t| t.elapsed() <= Duration::from_secs(1));
    if fresh && matches!(m.kind, MouseEventKind::Down(_)) {
        let hit =
            d.tv.buttons
                .iter()
                .find(|(r, _)| m.column >= r.x && m.column < r.x + r.width && m.row == r.y);
        if let Some((_, b)) = hit {
            match *b {
                Button::FoldAll => fold_all(d),
                Button::ExpandAll => expand_all(d),
                Button::TurnView => {
                    if let Some((host, agent, id)) = d.tv.key.clone() {
                        let running = d.tv.running;
                        let title = crate::agents::label(&agent);
                        open_full(d, &host, &agent, &id, running, title);
                    }
                }
            }
            return true;
        }
    }
    let Some(r) = d.tv.area else { return false };
    let inside =
        m.column >= r.x && m.column < r.x + r.width && m.row >= r.y && m.row < r.y + r.height;
    if !inside
        || d.tv
            .drawn
            .is_none_or(|t| t.elapsed() > Duration::from_secs(1))
    {
        return false;
    }
    match m.kind {
        MouseEventKind::ScrollUp => {
            d.tv.follow = false;
            d.tv.scroll = d.tv.scroll.saturating_sub(3);
            d.tv.sel = d.tv.sel.min(d.tv.scroll + r.height as usize - 1);
            if d.tv.sel >= d.tv.scroll + r.height as usize {
                d.tv.sel = d.tv.scroll;
            }
            d.tv.sel =
                d.tv.sel
                    .max(d.tv.scroll)
                    .min(d.tv.scroll + r.height as usize - 1);
        }
        MouseEventKind::ScrollDown => {
            d.tv.scroll = (d.tv.scroll + 3).min(d.tv.rows_n.saturating_sub(r.height as usize));
            d.tv.sel = d.tv.sel.max(d.tv.scroll);
        }
        MouseEventKind::Down(_) => {
            let i = d.tv.scroll + (m.row - r.y) as usize;
            d.tv.follow = false;
            d.tv.sel = i;
            toggle(d, i);
        }
        _ => {}
    }
    true
}

/// Its keys, where it has them (Monitor › Timeline): true when taken.
pub(crate) fn key(d: &mut Dash, k: &KeyEvent) -> bool {
    let all = rows(d);
    let n = all.len();
    let page = d.tv.area.map(|r| r.height as usize).unwrap_or(10).max(1);
    let goto = |d: &mut Dash, i: usize| {
        d.tv.follow = false;
        d.tv.sel = i.min(n.saturating_sub(1));
    };
    match k.code {
        KeyCode::Char('j') | KeyCode::Down => goto(d, d.tv.sel + 1),
        KeyCode::Char('k') | KeyCode::Up => goto(d, d.tv.sel.saturating_sub(1)),
        KeyCode::PageDown => goto(d, d.tv.sel + page),
        KeyCode::PageUp => goto(d, d.tv.sel.saturating_sub(page)),
        KeyCode::Home => goto(d, 0),
        KeyCode::End => goto(d, n.saturating_sub(1)),
        KeyCode::Enter | KeyCode::Char(' ') => toggle(d, d.tv.sel),
        KeyCode::Right | KeyCode::Char('l') => {
            if let Some(r) = all.get(d.tv.sel).filter(|r| r.foldable && r.folded) {
                d.tv.toggled.insert(r.id.clone(), true);
            }
        }
        KeyCode::Left | KeyCode::Char('h') => {
            match all.get(d.tv.sel) {
                Some(r) if r.foldable && !r.folded => {
                    d.tv.toggled.insert(r.id.clone(), false);
                }
                // its parent: the nearest row above, less deep
                Some(r) => {
                    let depth = r.depth;
                    if let Some(p) = (0..d.tv.sel).rev().find(|&i| all[i].depth < depth) {
                        goto(d, p);
                    }
                }
                None => {}
            }
        }
        // folded to a level, in turn (`v`): each session (and subagent) a line,
        // each prompt a line, every step — with what ran under each; enter or
        // a click still opens or closes any one of them
        KeyCode::Char('v') => {
            let level = (d.tv.level + 1) % 3;
            d.tv.level = level;
            fold_to(d, level);
            d.toast(
                super::Level::Info,
                [
                    "by session: what ran in each",
                    "by prompt: the steps of each",
                    "every step",
                ][level as usize],
            );
        }
        KeyCode::Char('z') => fold_all(d),
        KeyCode::Char('Z') => expand_all(d),
        KeyCode::Char(')') => {
            if let Some(i) = all
                .iter()
                .enumerate()
                .skip(d.tv.sel + 1)
                .find(|(_, r)| r.kind == RowKind::Prompt)
                .map(|(i, _)| i)
            {
                goto(d, i);
            }
        }
        KeyCode::Char('(') => {
            if let Some(i) = all
                .iter()
                .enumerate()
                .take(d.tv.sel)
                .rev()
                .find(|(_, r)| r.kind == RowKind::Prompt)
                .map(|(i, _)| i)
            {
                goto(d, i);
            }
        }
        KeyCode::Char('e') => {
            if let Some(i) = all
                .iter()
                .enumerate()
                .skip(d.tv.sel + 1)
                .find(|(_, r)| r.bad)
                .map(|(i, _)| i)
            {
                goto(d, i);
            }
        }
        KeyCode::Char('f') => {
            d.tv.follow = !d.tv.follow;
        }
        KeyCode::Char('w') => {
            d.tv.show_words = !d.tv.show_words;
            if d.cfg.get_or("timeline.words") == "off" {
                d.toast(
                    super::Level::Info,
                    "timeline.words is off: names and numbers only",
                );
            }
        }
        KeyCode::Char('o') => {
            let m = match d.tv.mode(&d.cfg) {
                Fold::Smart => Fold::Open,
                Fold::Open => Fold::Folded,
                Fold::Folded => Fold::Smart,
            };
            d.tv.mode = Some(m);
            d.tv.toggled.clear();
            d.toast(
                super::Level::Info,
                format!(
                    "timeline: {}",
                    match m {
                        Fold::Smart => "smart folding",
                        Fold::Open => "open",
                        Fold::Folded => "folded",
                    }
                ),
            );
        }
        _ => return false,
    }
    true
}

/// Its keys' hints.
pub(crate) fn hints() -> Vec<(&'static str, &'static str)> {
    vec![
        ("j k", "move"),
        ("enter", "fold one"),
        ("z Z", "fold · expand all"),
        ("v", "by session · prompt · step"),
        ("( )", "prompts"),
        ("e", "next error"),
        ("f", "follow"),
        ("w", "words"),
        ("o", "smart · open · folded"),
    ]
}
