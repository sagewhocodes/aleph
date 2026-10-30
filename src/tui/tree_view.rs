//! The timeline drawn: one agent conversation's tree (`crate::timeline`) in a
//! panel — Fleet's right panel (`T`), Focus's side, Monitor › Timeline.
//! Loaded off the UI thread, again every few seconds while it's on screen
//! (and its machine's logs asked to be read then too), folds kept per
//! session. The wheel scrolls it, a click folds or opens a row; in Monitor ›
//! Timeline the keys too: j/k, PgUp/PgDn, enter/space fold, ← → close /
//! open, z / Z everything, v by session › by prompt › every step (what ran
//! under each, counted), ( ) prompts, e the next error, f follows the end, w
//! words (read live from the machine, never kept).

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

/// Every frame: take what was loaded; load again when due and it's on screen.
pub(crate) fn pump(d: &mut Dash) {
    if let Some(rx) = &d.tv.rx {
        if let Ok((g, k, t)) = rx.try_recv() {
            if g == d.tv.gen && d.tv.key.as_ref() == Some(&k) {
                d.tv.tree = Some(t);
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
    match &d.tv.tree {
        Some(t) => timeline::rows(t, d.tv.mode(&d.cfg), &d.tv.toggled, d.tv.running),
        None => Vec::new(),
    }
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
                RowKind::PromptEnd | RowKind::Compact => ui::muted(),
                RowKind::Group => ui::fg(c::ACCENT()),
                _ => Style::new(),
            }
        };
        let indent = "  ".repeat(row.depth as usize);
        let mut left = vec![
            gutter,
            Span::styled(if w >= 40 { clock } else { String::new() }, ui::faint()),
            Span::raw(indent),
            Span::styled(glyph(row), ui::fg(c::ACCENT())),
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
        KeyCode::Char('z') => {
            for r in all.iter().filter(|r| r.foldable) {
                d.tv.toggled.insert(r.id.clone(), false);
            }
        }
        KeyCode::Char('Z') => {
            // open each level: again until nothing new opens
            for _ in 0..6 {
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
        ("enter", "fold"),
        ("z Z", "all"),
        ("v", "by session · prompt · step"),
        ("( )", "prompts"),
        ("e", "next error"),
        ("f", "follow"),
        ("w", "words"),
        ("o", "smart · open · folded"),
    ]
}
