//! The approval board (`ctrl+a` in every view; `key.approvals`): every
//! agent waiting on you, its question as the agent shows it — herdr's
//! `detection` read of its pane, in its own colours — beside the list of
//! them, or as cards (`v`; `approvals.layout`). Live: the list follows the
//! fleet, each question is read again every few seconds, and one that's no
//! longer asked (answered here or elsewhere) is marked and then leaves.
//!
//! Keys: ↑↓ (and ←→ between cards) move · `y` approves (its yes option) ·
//! `n` denies (its no option, else esc) · `1`–`9` its option · `Y` every
//! question with a clear yes (asks once) · enter opens it in Focus · `r`
//! reads them again · `v` list / cards · esc.
//!
//! An answer is checked before it goes: the question is read afresh and
//! must be the one shown (else "it changed — look again"), a question on
//! the deny list (`approve_deny`) is answered only at the terminal, and the
//! keys are the ones its menu takes from where its cursor is now (`ask.rs`).

use super::{Act, Dash, Handoff, Level, Mode, Pending, View};
use crate::ask::{Ask, Choice};
use crate::herdr::{Herdr, Session};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

/// How often a question shown is read again.
const REREAD: Duration = Duration::from_secs(3);
/// How long a question no longer asked stays (marked) before it leaves.
/// A card's size in the cards layout.
const CARD_W: u16 = 46;
const CARD_H: u16 = 13;

/// One agent waiting on you.
struct Entry {
    /// the session as last seen
    s: Session,
    /// its question's herdr seq (a new question while still waiting: a new one)
    seq: u64,
    /// the question as the agent shows it (its colours)
    lines: Vec<Line<'static>>,
    ask: Option<Ask>,
    err: Option<String>,
    /// when it was last read (asked for)
    read: Option<Instant>,
    reading: bool,
    /// no longer asking: since when
    /// what was sent to it from here
    sent: Option<String>,
}

impl Entry {
    fn new(s: Session) -> Entry {
        Entry {
            seq: s.seq,
            s,
            lines: vec![],
            ask: None,
            err: None,
            read: None,
            reading: false,
            sent: None,
        }
    }
}

/// An answer on its way: the question it answers (as shown), and which.
#[derive(Clone, Debug)]
pub(crate) struct Answer {
    host: String,
    pane: String,
    name: String,
    ask: Ask,
    choice: Choice,
}

pub(crate) struct State {
    entries: Vec<Entry>,
    /// the selected one (by session key: the list re-sorts under it)
    sel: Option<String>,
    /// cards instead of the list
    pub cards: Option<bool>,
    /// cards in a row, as last drawn (↑↓ move a row)
    cols: usize,
    /// where each entry was last drawn (a card, or a list row's two lines):
    /// a click there
    rects: Vec<(Rect, usize)>,
    /// marked with space (session keys): y, n and 1–9 answer them all
    marked: std::collections::HashSet<String>,
    tx: Sender<(String, u64, Result<String, String>)>,
    rx: Receiver<(String, u64, Result<String, String>)>,
}

impl Default for State {
    fn default() -> State {
        let (tx, rx) = channel();
        State {
            entries: vec![],
            sel: None,
            cards: None,
            cols: 1,
            rects: vec![],
            marked: Default::default(),
            tx,
            rx,
        }
    }
}

/// The sessions whose questions are on the board now.
pub(crate) fn shown(d: &Dash) -> Vec<Session> {
    d.appr.entries.iter().map(|e| e.s.clone()).collect()
}

/// Open the board (from any view).
pub(crate) fn open(d: &mut Dash) {
    d.mode = Mode::Approvals;
    pump(d);
}

fn cards(d: &Dash) -> bool {
    d.appr
        .cards
        .unwrap_or_else(|| d.cfg.get_or("approvals.layout") == "cards")
}

/// Every loop while it's up: the fleet's waiting sessions in, the ones no
/// longer asking marked and then out, questions read (again).
pub(crate) fn pump(d: &mut Dash) {
    if !matches!(d.mode, Mode::Approvals) {
        return;
    }
    while let Ok((key, seq, r)) = d.appr.rx.try_recv() {
        let Some(e) = d
            .appr
            .entries
            .iter_mut()
            .find(|e| e.s.key() == key && e.seq == seq)
        else {
            continue;
        };
        e.reading = false;
        match r {
            Ok(t) => {
                let lines = crate::ansi::lines(&t);
                let plain: Vec<String> = lines.iter().map(crate::ansi::plain).collect();
                let ask = Ask::parse(&plain.join("\n"));
                // a form moves on with herdr's seq standing still: a new page
                // (or a tick) is the answer having landed, and it's your turn
                if ask.form.is_some() && e.ask.as_ref() != Some(&ask) {
                    e.sent = None;
                }
                e.ask = Some(ask);
                e.lines = lines;
                e.err = None;
            }
            Err(x) => e.err = Some(x),
        }
    }
    let waiting: Vec<Session> = d
        .fleet
        .sessions()
        .into_iter()
        .filter(|s| s.blocked())
        .cloned()
        .collect();
    let st = &mut d.appr;
    for s in &waiting {
        match st.entries.iter_mut().find(|e| e.s.key() == s.key()) {
            Some(e) if e.seq == s.seq => e.s = s.clone(),
            // a new question (or asking again after it stopped)
            Some(e) => *e = Entry::new(s.clone()),
            None => st.entries.push(Entry::new(s.clone())),
        }
    }
    // answered (here, in Focus, at its own terminal, from the phone): gone
    st.entries
        .retain(|e| waiting.iter().any(|s| s.key() == e.s.key()));
    let entries = &st.entries;
    st.marked
        .retain(|k| entries.iter().any(|e| e.s.key() == *k));
    let now = Instant::now();
    // the longest waiting first
    st.entries
        .sort_by_key(|e| (e.s.since.unwrap_or(u64::MAX), e.s.key()));
    if st
        .sel
        .as_ref()
        .is_none_or(|k| !st.entries.iter().any(|e| e.s.key() == *k))
    {
        st.sel = st.entries.first().map(|e| e.s.key());
    }
    // read the questions (again)
    let due: Vec<(Session, u64)> = st
        .entries
        .iter_mut()
        .filter(|e| !e.reading && e.read.is_none_or(|t| t.elapsed() > REREAD))
        .map(|e| {
            e.reading = true;
            e.read = Some(now);
            (e.s.clone(), e.seq)
        })
        .collect();
    for (s, seq) in due {
        let (t, tx) = (d.target_of(&s.host), d.appr.tx.clone());
        std::thread::spawn(move || {
            let r = Herdr::new(&t).read(&s.pane_id, "detection", 0, true);
            let _ = tx.send((s.key(), seq, r));
        });
    }
}

fn selected(d: &Dash) -> Option<usize> {
    let k = d.appr.sel.as_ref()?;
    d.appr.entries.iter().position(|e| e.s.key() == *k)
}

/// The selected question is one page of a form.
fn form_selected(d: &Dash) -> bool {
    selected(d)
        .and_then(|i| d.appr.entries[i].ask.as_ref())
        .is_some_and(|a| a.form.is_some())
}

fn select(d: &mut Dash, i: usize) {
    if let Some(e) = d.appr.entries.get(i) {
        d.appr.sel = Some(e.s.key());
    }
}

/// The list's rows: each question its own, or with `approvals.group =
/// same`, the agents asking the same thing (same agent, words and options)
/// together — indices into the entries.
fn rows(d: &Dash) -> Vec<Vec<usize>> {
    let group = !cards(d) && d.cfg.get_or("approvals.group") == "same";
    let sig = |e: &Entry| {
        e.ask.as_ref().map(|a| {
            let opts: Vec<&str> = a.options.iter().map(|o| o.label.as_str()).collect();
            (e.s.agent.clone(), summary_of(a), opts.join("\n"))
        })
    };
    let mut v: Vec<Vec<usize>> = Vec::new();
    for (i, e) in d.appr.entries.iter().enumerate() {
        let me = sig(e).filter(|_| group && e.sent.is_none());
        match v.iter_mut().find(|r| {
            me.is_some() && d.appr.entries[r[0]].sent.is_none() && sig(&d.appr.entries[r[0]]) == me
        }) {
            Some(r) => r.push(i),
            None => v.push(vec![i]),
        }
    }
    v
}

/// What y, n and 1–9 answer: the marked ones, else the selected row (its
/// whole group).
fn targets(d: &Dash) -> Vec<usize> {
    if !d.appr.marked.is_empty() {
        return (0..d.appr.entries.len())
            .filter(|i| d.appr.marked.contains(&d.appr.entries[*i].s.key()))
            .collect();
    }
    let Some(i) = selected(d) else {
        return vec![];
    };
    rows(d)
        .into_iter()
        .find(|r| r.contains(&i))
        .unwrap_or_else(|| vec![i])
}

/// Keys while it's up.
pub(crate) fn key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    let ks = super::key_str(&k);
    let n = d.appr.entries.len();
    let at = selected(d).unwrap_or(0);
    let rs = rows(d);
    let row = rs.iter().position(|r| r.contains(&at)).unwrap_or(0);
    // ↑↓: a row of the list, or a row of cards
    let step = |up: bool| -> usize {
        if cards(d) {
            let per = d.appr.cols.max(1);
            if up {
                at.saturating_sub(per)
            } else {
                (at + per).min(n.saturating_sub(1))
            }
        } else {
            let r = if up {
                row.saturating_sub(1)
            } else {
                (row + 1).min(rs.len().saturating_sub(1))
            };
            rs.get(r).map_or(at, |r| r[0])
        }
    };
    let (up, down) = (step(true), step(false));
    match k.code {
        // esc drops the marks first
        KeyCode::Esc if !d.appr.marked.is_empty() => d.appr.marked.clear(),
        KeyCode::Esc | KeyCode::Char('q') => return None,
        _ if d.keys.owner(&ks) == Some(Act::Approvals) => return None,
        KeyCode::Up | KeyCode::Char('k') => select(d, up),
        KeyCode::Down | KeyCode::Char('j') => select(d, down),
        KeyCode::Left | KeyCode::Char('h') => select(d, at.saturating_sub(1)),
        KeyCode::Right | KeyCode::Char('l') => select(d, (at + 1).min(n.saturating_sub(1))),
        // space: mark (or unmark) this row, then the next
        KeyCode::Char(' ') => {
            let keys: Vec<String> = rs
                .get(row)
                .into_iter()
                .flatten()
                .map(|i| d.appr.entries[*i].s.key())
                .collect();
            let on = keys.iter().all(|k| d.appr.marked.contains(k));
            for k in keys {
                if on {
                    d.appr.marked.remove(&k);
                } else {
                    d.appr.marked.insert(k);
                }
            }
            select(d, down);
        }
        KeyCode::Tab if form_selected(d) => answer(d, Choice::Next),
        KeyCode::BackTab if form_selected(d) => answer(d, Choice::Back),
        KeyCode::Char('y') => answer(d, Choice::Approve),
        KeyCode::Char('n') => answer(d, Choice::Deny),
        KeyCode::Char(ch @ '1'..='9') => answer(d, Choice::Option(ch as usize - '1' as usize)),
        KeyCode::Char('Y') => {
            approve_all(d);
            return None;
        }
        KeyCode::Char('r') => {
            for e in d.appr.entries.iter_mut() {
                e.read = None;
            }
        }
        KeyCode::Char('v') => {
            let on = !cards(d);
            d.appr.cards = Some(on);
            let _ = d
                .cfg
                .set("approvals.layout", if on { "cards" } else { "list" });
        }
        KeyCode::Enter => {
            let i = selected(d)?;
            answer_in_focus(d, i);
            return None;
        }
        _ => {}
    }
    d.mode = Mode::Approvals;
    None
}

/// Answer it in Focus: the session full screen, typing into it whatever
/// `focus.keys` says; leaving Focus comes back here while others still wait.
fn answer_in_focus(d: &mut Dash, i: usize) {
    let Some(key) = d.appr.entries.get(i).map(|e| e.s.key()) else {
        return;
    };
    let from = if d.view == View::Wall {
        d.wall.back.unwrap_or(View::Fleet)
    } else {
        d.view
    };
    d.mode = Mode::Normal;
    super::wall::focus_on(d, &key, from);
    d.wall.typing = true;
    d.wall.back_to_board = true;
}

/// A click on the board: a card answers in Focus; a list row is picked,
/// and answered in Focus when it already was.
pub(crate) fn click(d: &mut Dash, x: u16, y: u16) {
    let at = Position::new(x, y);
    let Some(i) = d
        .appr
        .rects
        .iter()
        .find(|(r, _)| r.contains(at))
        .map(|x| x.1)
    else {
        return;
    };
    if cards(d) || selected(d) == Some(i) {
        answer_in_focus(d, i);
    } else {
        select(d, i);
    }
}

/// Answer the marked questions, else the selected row's (each checked
/// again before it goes); the ones that can't be answered from here stay.
fn answer(d: &mut Dash, c: Choice) {
    let mut ok = Vec::new();
    let mut left: Vec<(Level, String)> = Vec::new();
    for i in targets(d) {
        match answer_of(d, i, c) {
            Ok(a) => ok.push((i, a)),
            Err(x) => left.push(x),
        }
    }
    match (ok.len(), left.first()) {
        (0, Some((lv, why))) => {
            let (lv, why) = (*lv, why.clone());
            d.toast(lv, why);
            return;
        }
        (0, None) => return,
        (n, Some((_, why))) => {
            let why = format!(
                "{} {n} · {} left for you — {why}",
                word(&ok[0].1),
                left.len()
            );
            d.toast(Level::Warn, why);
        }
        _ => {}
    }
    for (i, a) in &ok {
        d.appr.entries[*i].sent = Some(word(a).to_string());
    }
    d.appr.marked.clear();
    send(d, ok.into_iter().map(|x| x.1).collect());
}

/// The answer `c` to entry `i`, or why it can't go from here.
fn answer_of(d: &Dash, i: usize, c: Choice) -> Result<Answer, (Level, String)> {
    let e = &d.appr.entries[i];
    let name = e.s.display().to_string();
    let Some(ask) = e.ask.clone() else {
        return Err((Level::Info, format!("still reading {name}'s question…")));
    };
    if let Some(p) = crate::permit::terminal_only(&d.cfg, &ask.text()) {
        return Err((
            Level::Err,
            format!("{name}: on the deny list ({p}) — answer it at the terminal (enter opens it)"),
        ));
    }
    if ask.keys(c).is_none() {
        let what = match c {
            Choice::Approve => "no yes option",
            _ => "no such option",
        };
        return Err((
            Level::Err,
            format!("{name}: {what} in its question — enter opens it to answer there"),
        ));
    }
    Ok(Answer {
        host: e.s.host.clone(),
        pane: e.s.pane_id.clone(),
        name,
        ask,
        choice: c,
    })
}

fn word(a: &Answer) -> &'static str {
    match a.choice {
        Choice::Approve => "approved",
        Choice::Deny => "denied",
        Choice::Option(_) => "answered",
        Choice::Next | Choice::Back => "moved on in",
    }
}

/// `Y`: every question with a clear yes, not on the deny list (asks once).
fn approve_all(d: &mut Dash) {
    let v: Vec<Answer> = d
        .appr
        .entries
        .iter()
        .filter(|e| e.sent.is_none())
        .filter_map(|e| {
            let ask = e.ask.clone().filter(|a| a.form.is_none())?;
            ask.yes()?;
            crate::permit::terminal_only(&d.cfg, &ask.text())
                .is_none()
                .then(|| Answer {
                    host: e.s.host.clone(),
                    pane: e.s.pane_id.clone(),
                    name: e.s.display().to_string(),
                    ask,
                    choice: Choice::Approve,
                })
        })
        .collect();
    let left = d.appr.entries.len() - v.len();
    if v.is_empty() {
        d.toast(
            Level::Info,
            "nothing to approve at once — no clear yes (or on the deny list): answer them one by one",
        );
        d.mode = Mode::Approvals;
        return;
    }
    let names: Vec<&str> = v.iter().map(|a| a.name.as_str()).collect();
    d.mode = Mode::Confirm {
        title: format!(
            "Approve {} question{}?",
            v.len(),
            if v.len() == 1 { "" } else { "s" }
        ),
        text: format!(
            "{} — each with its own yes{}",
            ui::trunc(&names.join(", "), 80),
            if left > 0 {
                format!("; {left} left for you (no clear yes, or on the deny list)")
            } else {
                String::new()
            }
        ),
        on_yes: Pending::Approve(v),
    };
}

/// Send answers: each question read afresh first — still the one shown,
/// not on the deny list — then the keys its menu takes from its cursor now.
pub(crate) fn send(d: &mut Dash, v: Vec<Answer>) {
    for a in v {
        for e in d
            .appr
            .entries
            .iter_mut()
            .filter(|e| e.s.host == a.host && e.s.pane_id == a.pane)
        {
            e.sent = Some(word(&a).to_string());
        }
        let (t, cfg) = (d.target_of(&a.host), d.cfg.clone());
        d.spawn(Some(a.host.clone()), move || {
            let h = Herdr::new(&t);
            let now = h.read(&a.pane, "detection", 0, false)?;
            let fresh = Ask::parse(&now);
            if !fresh.same(&a.ask) {
                return Err(format!("{}: its question changed — look again", a.name));
            }
            if crate::permit::terminal_only(&cfg, &fresh.text()).is_some() {
                return Err(format!(
                    "{}: on the deny list — answer it at the terminal",
                    a.name
                ));
            }
            let keys = fresh
                .keys(a.choice)
                .ok_or_else(|| format!("{}: no option to choose any more", a.name))?;
            let k: Vec<&str> = keys.iter().map(String::as_str).collect();
            h.send_keys(&a.pane, &k)?;
            let label = match a.choice {
                Choice::Approve => fresh.yes(),
                Choice::Deny => fresh.no(),
                Choice::Option(i) => Some(i),
                Choice::Next | Choice::Back => None,
            }
            .and_then(|i| fresh.options.get(i))
            .map(|o| format!(" · {}", ui::trunc(&o.label, 40)))
            .unwrap_or_default();
            Ok(format!("{} {}{label}", word(&a), a.name))
        });
    }
}

// ------------------------------------------------------------------ drawing

/// Waiting how long, from its state's start.
fn waited(e: &Entry) -> String {
    let now = crate::cache::now();
    e.s.since
        .map(|t| ui::dur(now.saturating_sub(t)))
        .unwrap_or_default()
}

/// The state glyph and its colour: asking, or an answer on its way.
fn mark(d: &Dash, e: &Entry) -> (String, Color) {
    if e.sent.is_some() {
        (ui::spinner(d.t0).to_string(), c::ACCENT())
    } else {
        (crate::look::get().glyphs.needs.to_string(), c::NEEDS())
    }
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, area: Rect) {
    d.appr.rects.clear();
    let w = area.width.saturating_sub(4).min(160);
    let h = area.height.saturating_sub(2).max(10);
    let r = ui::centered(area, w, h);
    ui::clear(f, r);
    let live = d.appr.entries.len();
    let mut title = vec![
        Span::raw("Approvals"),
        Span::styled(
            if live == 0 {
                " · nothing waiting".to_string()
            } else {
                format!(" · {live} waiting")
            },
            ui::fg(if live == 0 { c::DONE() } else { c::NEEDS() }),
        ),
    ];
    if !d.appr.marked.is_empty() {
        title.push(Span::styled(
            format!(" · {} marked", d.appr.marked.len()),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        ));
    }
    // machines whose questions can't show up here: paused, or not answering
    let unseen: Vec<String> = d
        .fleet
        .hosts
        .iter()
        .filter_map(|h| {
            if h.paused {
                Some(format!("⏸ {} paused (Z in Fleet resumes it)", h.alias))
            } else if matches!(h.conn, crate::fleet::Conn::Down(_)) && !h.sessions.is_empty() {
                Some(format!("✕ {} not answering", h.alias))
            } else {
                None
            }
        })
        .collect();
    if !unseen.is_empty() {
        title.push(Span::styled(
            format!(" · {} — not watched", unseen.join(", ")),
            ui::fg(c::YELLOW()),
        ));
    }
    let title = Line::from(title);
    let block = ui::panel(title, true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [body, bar, foot] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(2),
        Constraint::Length(1),
    ])
    .areas(inner);
    if d.appr.entries.is_empty() {
        let lines = vec![
            Line::from(Span::styled(
                "✓ Nothing is waiting for you",
                ui::fg(c::DONE()).add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
            Line::from(Span::styled(
                "an agent that asks shows up here as it asks, its question as it shows it",
                ui::muted(),
            )),
        ];
        let y = body.y + body.height / 2 - 1;
        f.render_widget(
            Paragraph::new(lines).alignment(Alignment::Center),
            Rect::new(body.x, y, body.width, 3.min(body.height)),
        );
    } else if cards(d) {
        draw_cards(d, f, body);
    } else {
        draw_list(d, f, body);
    }
    if let Some(i) = selected(d) {
        f.render_widget(Paragraph::new(options_bar(d, i, bar.width)), bar);
    }
    let layout = if cards(d) { "list" } else { "cards" };
    let hints = [
        ("↑↓", "move"),
        ("space", "mark"),
        ("y", "approve"),
        ("n", "deny"),
        ("1–9", "option"),
        ("Y", "approve all"),
        ("enter", "answer in Focus"),
        ("v", layout),
        ("r", "read again"),
        ("esc", "close"),
    ];
    f.render_widget(Paragraph::new(ui::hints_fit(&hints, foot.width)), foot);
}

/// The selected question's options, one key each (yes green, no red), or
/// why it can't be answered from here.
fn options_bar(d: &Dash, i: usize, w: u16) -> Vec<Line<'static>> {
    let e = &d.appr.entries[i];
    let name = e.s.display().to_string();
    let m = d.appr.marked.len();
    if m > 0 {
        return vec![Line::from(vec![
            Span::styled(
                format!("{m} marked  "),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "y approves each (its own yes) · n denies each · 1–9 picks that option in each · space unmarks · esc clears",
                ui::muted(),
            ),
        ])];
    }
    if let Some(s) = &e.sent {
        return vec![Line::from(Span::styled(
            format!("{s} — waiting for {name} to move on"),
            ui::muted(),
        ))];
    }
    let Some(ask) = &e.ask else {
        return vec![Line::from(Span::styled(
            format!("{} reading its question…", ui::spinner(d.t0)),
            ui::muted(),
        ))];
    };
    if let Some(p) = crate::permit::terminal_only(&d.cfg, &ask.text()) {
        return vec![Line::from(vec![
            Span::styled(
                " ⛔ deny list ",
                Style::new()
                    .fg(c::BASE())
                    .bg(c::RED())
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  matches {p} — answer it at the terminal: enter opens it"),
                ui::fg(c::RED()),
            ),
        ])];
    }
    if ask.options.is_empty() {
        return vec![Line::from(Span::styled(
            "no menu I can read in it — enter opens it to answer there (n sends esc)",
            ui::muted(),
        ))];
    }
    let (yes, no) = (ask.yes(), ask.no());
    let n = ask.options.len().min(9);
    let each = ((w as usize).saturating_sub(4) / n).clamp(10, 60);
    let mut l: Vec<Span<'static>> = vec![];
    let head = ask.form.as_ref().map(form_line);
    for (j, o) in ask.options.iter().take(9).enumerate() {
        let col = if Some(j) == yes {
            c::GREEN()
        } else if Some(j) == no {
            c::RED()
        } else {
            c::TEXT()
        };
        l.push(ui::keycap(&(j + 1).to_string()));
        if Some(j) == yes {
            l.push(ui::keycap("y"));
        } else if Some(j) == no {
            l.push(ui::keycap("n"));
        }
        let tick = match o.ticked {
            Some(true) => "☒ ",
            Some(false) => "☐ ",
            None => "",
        };
        l.push(Span::styled(
            format!(" {tick}{}  ", ui::trunc(&o.label, each.saturating_sub(8))),
            Style::new().fg(col).add_modifier(Modifier::BOLD),
        ));
    }
    let mut v: Vec<Line<'static>> = head.into_iter().collect();
    v.push(Line::from(l));
    if ask.options.len() > 9 {
        v.push(Line::from(Span::styled(
            format!(
                "{} more options past 9 — enter opens it",
                ask.options.len() - 9
            ),
            ui::faint(),
        )));
    }
    v
}

/// A form's place: where it's at, its questions, what ⇥ does.
fn form_line(form: &crate::ask::Form) -> Line<'static> {
    let mut l = vec![Span::styled(
        format!("{}  ", form.progress()),
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    )];
    for (i, (name, done)) in form.steps.iter().enumerate() {
        let here = !form.review && i == form.at();
        l.push(Span::styled(
            format!("{} {name}  ", if *done { "☒" } else { "☐" }),
            if here { ui::bold() } else { ui::muted() },
        ));
    }
    l.push(ui::keycap("⇥"));
    l.push(Span::styled(" next  ", ui::muted()));
    l.push(ui::keycap("⇧⇥"));
    l.push(Span::styled(" back", ui::muted()));
    Line::from(l)
}

/// The list beside the selected question.
fn draw_list(d: &mut Dash, f: &mut Frame, body: Rect) {
    let lw = (body.width * 2 / 5).clamp(44, 64).min(body.width / 2);
    let [left, _, right] = Layout::horizontal([
        Constraint::Length(lw),
        Constraint::Length(1),
        Constraint::Min(10),
    ])
    .areas(body);
    let sel = selected(d);
    let w = lw as usize;
    let rs = rows(d);
    let mut lines: Vec<Line> = vec![];
    for r in &rs {
        let e = &d.appr.entries[r[0]];
        let on = sel.is_some_and(|i| r.contains(&i));
        let marked = r
            .iter()
            .all(|i| d.appr.marked.contains(&d.appr.entries[*i].s.key()));
        let (g, gc) = mark(d, e);
        let bar = Span::styled(if on { "┃" } else { " " }, ui::fg(c::ACCENT()));
        let name_st = if on {
            ui::bold().fg(c::TEXT())
        } else {
            Style::new().fg(c::TEXT())
        };
        // who: the agent and its session (a group: how many)
        let who = if r.len() > 1 {
            format!("{} agents", r.len())
        } else {
            e.s.display().to_string()
        };
        let hosts: Vec<String> = {
            let mut v: Vec<String> = r
                .iter()
                .map(|i| d.appr.entries[*i].s.host.clone())
                .collect();
            v.dedup();
            v
        };
        let host = hosts.join(" ");
        let hcol = crate::theme::machine(&d.cfg, &e.s.host);
        let wait = waited(e);
        let label = crate::agents::label(&e.s.agent);
        let fixed = 1 + 2 + ui::width(&g) + 1 + 2 + ui::width(&label) + 3;
        let tail = ui::width(&host) + 2 + ui::width(&wait);
        let room = w.saturating_sub(fixed + tail + 1);
        let who = ui::trunc(&who, room);
        let pad = w.saturating_sub(fixed + ui::width(&who) + tail);
        let mut l = vec![
            bar.clone(),
            Span::styled(
                if marked { "✓ " } else { "  " },
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{g} "), ui::fg(gc).add_modifier(Modifier::BOLD)),
            crate::icons::agent(&e.s.agent),
            Span::styled(format!(" {label} · "), ui::muted()),
            Span::styled(who, name_st.fg(e.s.color())),
            Span::raw(" ".repeat(pad)),
            Span::styled(host, ui::fg(hcol).add_modifier(Modifier::BOLD)),
            Span::styled(format!("  {wait}"), ui::fg(c::NEEDS())),
        ];
        // under it, what it asks, in a line
        let mut l2 = vec![
            bar,
            Span::styled(
                format!("    {}", ui::trunc(&summary(e), w.saturating_sub(5))),
                if on { ui::muted() } else { ui::faint() },
            ),
        ];
        if on || marked {
            let bg = if on { c::SEL() } else { c::LINE() };
            for l in [&mut l, &mut l2] {
                for s in l.iter_mut().skip(1) {
                    s.style = s.style.bg(bg);
                }
                let used: usize = l.iter().map(|s| ui::width(&s.content)).sum();
                l.push(Span::styled(
                    " ".repeat(w.saturating_sub(used)),
                    Style::new().bg(bg),
                ));
            }
        }
        lines.push(Line::from(l));
        lines.push(Line::from(l2));
    }
    let at = sel
        .and_then(|i| rs.iter().position(|r| r.contains(&i)))
        .unwrap_or(0);
    let skip = (at * 2).saturating_sub(left.height.saturating_sub(2) as usize);
    f.render_widget(Paragraph::new(lines).scroll((skip as u16, 0)), left);
    // each row's two lines, where they landed (a click picks its first)
    for (j, r) in rs.iter().enumerate() {
        let Some(y) = (j * 2)
            .checked_sub(skip)
            .filter(|y| y + 1 < left.height as usize)
        else {
            continue;
        };
        d.appr
            .rects
            .push((Rect::new(left.x, left.y + y as u16, left.width, 2), r[0]));
    }
    // the divider
    for y in body.y..body.y + body.height {
        if let Some(c_) = f.buffer_mut().cell_mut((left.x + left.width, y)) {
            c_.set_symbol("│").set_style(ui::fg(c::LINE()));
        }
    }
    let Some(i) = sel else {
        return;
    };
    draw_question(d, f, right, i);
}

/// What a question asks, in a line: its last words before the question
/// itself ("Bash command · pytest -x tests/"), else the question.
fn summary_of(a: &Ask) -> String {
    let words = question_words(a);
    match &a.form {
        Some(f) if f.review => format!("{} · {words}", f.progress()),
        Some(f) => format!("{}/{} · {words}", f.at() + 1, f.steps.len()),
        None => words,
    }
}

fn question_words(a: &Ask) -> String {
    let lines: Vec<&str> = a
        .question
        .iter()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .collect();
    let body: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|q| !q.ends_with('?'))
        .collect();
    if body.is_empty() {
        return lines.last().map(|s| s.to_string()).unwrap_or_default();
    }
    body[body.len().saturating_sub(2)..].join(" · ")
}

/// An entry's line under its name: its answer on the way, what it asks, or
/// that it's being read.
fn summary(e: &Entry) -> String {
    if let Some(s) = &e.sent {
        return format!("{s} — waiting for it to move on");
    }
    match &e.ask {
        Some(a) => summary_of(a),
        None if e.err.is_some() => "couldn't read its question".into(),
        None => "reading its question…".into(),
    }
}

/// One question, as the agent shows it: who asks, then its screen's last
/// lines in its own colours.
fn draw_question(d: &Dash, f: &mut Frame, r: Rect, i: usize) {
    let e = &d.appr.entries[i];
    let hcol = crate::theme::machine(&d.cfg, &e.s.host);
    let head = Line::from(vec![
        crate::icons::agent(&e.s.agent),
        Span::styled(
            format!(" {} ", crate::agents::label(&e.s.agent)),
            ui::muted(),
        ),
        Span::styled(
            e.s.display().to_string(),
            Style::new().fg(e.s.color()).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" on ", ui::faint()),
        Span::styled(e.s.host.clone(), ui::fg(hcol).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" · waiting {}", waited(e)), ui::muted()),
    ]);
    // a card: its border, who asks in the title, the screen inside
    let block = Block::bordered()
        .border_set(ui::border_set(true))
        .border_style(ui::fg(c::NEEDS()))
        .title(head);
    let screen = block.inner(r);
    f.render_widget(block, r);
    if e.lines.is_empty() {
        let t = match &e.err {
            Some(x) => Span::styled(format!("couldn't read it: {x}"), ui::fg(c::RED())),
            None => Span::styled(
                format!("{} reading its question…", ui::spinner(d.t0)),
                ui::muted(),
            ),
        };
        f.render_widget(Paragraph::new(t), screen);
        return;
    }
    // its screen's bottom, as many lines as fit (the question ends there)
    let mut lines = e.lines.clone();
    while lines
        .last()
        .is_some_and(|l| crate::ansi::plain(l).trim().is_empty())
    {
        lines.pop();
    }
    let n = lines.len();
    let shown: Vec<Line> = lines
        .into_iter()
        .skip(n.saturating_sub(screen.height as usize))
        .collect();
    f.render_widget(Paragraph::new(shown), screen);
    ui::agent_screen(screen);
    // showcase: the screen redacted or hidden (`showcase.screens`)
    crate::showcase::screen_area(f.buffer_mut(), screen);
}

/// Cards: each question in a box of its own, in rows.
fn draw_cards(d: &mut Dash, f: &mut Frame, body: Rect) {
    let cols = (body.width / CARD_W).max(1);
    d.appr.cols = cols as usize;
    let cw = body.width / cols;
    let rows_fit = (body.height / CARD_H).max(1) as usize;
    let sel = selected(d).unwrap_or(0);
    let row0 = (sel / cols as usize).saturating_sub(rows_fit - 1);
    let mut rects = Vec::new();
    for (i, e) in d.appr.entries.iter().enumerate() {
        let (row, col) = (i / cols as usize, i % cols as usize);
        if row < row0 || row >= row0 + rows_fit {
            continue;
        }
        let r = Rect::new(
            body.x + col as u16 * cw,
            body.y + ((row - row0) as u16) * CARD_H,
            cw.saturating_sub(1),
            CARD_H.min(body.height),
        );
        rects.push((r, i));
        let on = i == sel;
        let (g, gc) = mark(d, e);
        let hcol = crate::theme::machine(&d.cfg, &e.s.host);
        let title = Line::from(vec![
            Span::styled(format!(" {g} "), ui::fg(gc).add_modifier(Modifier::BOLD)),
            crate::icons::agent(&e.s.agent),
            Span::styled(
                format!(" {} ", ui::trunc(e.s.display(), 18)),
                Style::new().fg(e.s.color()).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{} ", e.s.host), ui::fg(hcol)),
            Span::styled(format!("{} ", waited(e)), ui::muted()),
        ]);
        let block = Block::bordered()
            .border_set(ui::border_set(on))
            .border_style(ui::fg(if on { c::ACCENT() } else { c::LINE() }))
            .title(title);
        let inner = block.inner(r);
        f.render_widget(block, r);
        let mut lines: Vec<Line> = vec![];
        match &e.ask {
            None => lines.push(Line::from(Span::styled(
                format!("{} reading its question…", ui::spinner(d.t0)),
                ui::muted(),
            ))),
            Some(a) => {
                let opts = a.options.len().min(inner.height as usize / 2);
                let room = (inner.height as usize).saturating_sub(opts + 1);
                let q = a.question.len();
                for l in a.question.iter().skip(q.saturating_sub(room)) {
                    lines.push(Line::from(Span::styled(
                        ui::trunc(l, inner.width as usize),
                        Style::new().fg(c::TEXT()),
                    )));
                }
                lines.push(Line::raw(""));
                let (yes, no) = (a.yes(), a.no());
                for (j, o) in a.options.iter().take(opts).enumerate() {
                    let col = if Some(j) == yes {
                        c::GREEN()
                    } else if Some(j) == no {
                        c::RED()
                    } else {
                        c::MUTED()
                    };
                    let cur = if a.cursor == Some(j) { "› " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(cur, ui::fg(c::ACCENT())),
                        Span::styled(format!("{} ", j + 1), ui::faint()),
                        Span::styled(
                            ui::trunc(&o.label, (inner.width as usize).saturating_sub(5)),
                            ui::fg(col),
                        ),
                    ]));
                }
            }
        }
        f.render_widget(Paragraph::new(lines), inner);
        ui::agent_screen(inner);
        crate::showcase::screen_area(f.buffer_mut(), inner);
    }
    d.appr.rects = rects;
}

/// A question read for a session (tests: what herdr would say).
#[cfg(test)]
pub(crate) fn read_back(d: &mut Dash, key: &str, text: &str) {
    pump(d);
    let seq = d
        .appr
        .entries
        .iter()
        .find(|e| e.s.key() == key)
        .map_or(0, |e| e.seq);
    let _ = d.appr.tx.send((key.into(), seq, Ok(text.into())));
    pump(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_says_what_it_did() {
        let a = Answer {
            host: "b31".into(),
            pane: "w1:p2".into(),
            name: "temper".into(),
            ask: Ask::parse("Run?\n❯ 1. Yes\n  2. No"),
            choice: Choice::Deny,
        };
        assert_eq!(word(&a), "denied");
        assert_eq!(a.ask.keys(a.choice).unwrap(), ["down", "enter"]);
    }
}
