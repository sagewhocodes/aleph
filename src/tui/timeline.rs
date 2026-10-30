//! Focus's timeline (`t`): one session at the side — its states over the
//! last hours as a strip (the state log, `timechart`), then what it did: its
//! conversation's tree (prompts, turns, tool calls, subagents, with time,
//! tokens and $: `tree_view`; the wheel scrolls it, a click folds), or, for a
//! pane with no conversation (a shell), its story (started, needed you,
//! answered, finished); what's queued for it, and how its context grew
//! request by request.

use super::Dash;
use crate::herdr::Session;
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

/// How wide the panel is (wider when the screen is).
pub(crate) const WIDTH: u16 = 38;

/// Its width on a screen this wide.
pub(crate) fn width(screen: u16) -> u16 {
    if screen >= 180 {
        56
    } else if screen >= 150 {
        48
    } else {
        WIDTH
    }
}

/// Is it shown? (`t` in Focus; on its own on wide screens)
pub(crate) fn shown(d: &Dash, width: u16) -> bool {
    width >= 100 && d.wall.timeline.unwrap_or(width >= 150)
}

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect, s: &Session) {
    let block = ui::panel(
        Line::from(vec![
            Span::styled(" Timeline · ", ui::faint()),
            Span::styled(s.display().to_string(), ui::bold()),
            Span::raw(" "),
        ]),
        false,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    if inner.height < 3 || inner.width < 12 {
        return;
    }
    let w = inner.width as usize;
    // the bottom: its usage, when its logs have been read
    let mut foot: Vec<Line> = Vec::new();
    if let Some(l) = super::usage::live_for(d, s) {
        if !l.ctx_series.is_empty() {
            let peak = l
                .window
                .or_else(|| l.ctx_series.iter().copied().max())
                .unwrap_or(1)
                .max(1) as f64;
            let pts: Vec<f64> = l.ctx_series.iter().map(|v| *v as f64 / peak).collect();
            foot.push(Line::from(Span::styled(
                "context, request by request",
                ui::faint(),
            )));
            foot.push(Line::from(ui::spark(&pts, w.min(60), c::ACCENT())));
        }
        let mut model = l.model.clone();
        if !l.effort.is_empty() {
            model = format!("{model} {}", l.effort);
        }
        foot.push(Line::from(Span::styled(ui::trunc(&model, w), ui::muted())));
        foot.push(Line::from(super::usage::chip(l, w)));
    }
    // what's queued for it
    let queued = crate::queue::for_session(&s.host, &s.name);
    let mut mid: Vec<Line> = Vec::new();
    for t in queued.iter().take(3) {
        let when = match t.after {
            Some(a) => format!("at {}", ui::clock(a)),
            None => "when free".to_string(),
        };
        mid.push(Line::from(vec![
            Span::styled("≡ ", ui::fg(c::BLUE()).add_modifier(Modifier::BOLD)),
            Span::styled(format!("queued, {when}"), ui::fg(c::BLUE())),
        ]));
        mid.push(Line::from(Span::styled(
            format!("  {}", ui::trunc(&t.prompt, w.saturating_sub(2))),
            ui::faint(),
        )));
    }
    // a terminal: the commands it ran while this has been watching, newest first
    let ran = d.fleet.commands_of(&s.key());
    if !ran.is_empty() {
        mid.push(Line::from(Span::styled("commands", ui::faint())));
    }
    for r in ran.iter().take(12) {
        let (g, col, took) = match r.to {
            None => (
                ui::spinner(d.t0).to_string(),
                c::WORKING(),
                ui::dur(crate::cache::now().saturating_sub(r.from)),
            ),
            Some(to) => (
                "✓".to_string(),
                c::DONE(),
                ui::dur(to.saturating_sub(r.from)),
            ),
        };
        // the end of a long one is cut, its time always shown
        let tail = format!("  {took}");
        let room = w.saturating_sub(2 + ui::width(&tail));
        mid.push(Line::from(vec![
            Span::styled(format!("{g} "), ui::fg(col).add_modifier(Modifier::BOLD)),
            Span::styled(ui::trunc(&r.cmd, room), ui::muted()),
            Span::styled(tail, ui::faint()),
        ]));
    }
    // at the top: its states over the last hours
    let mut top: Vec<Line> = Vec::new();
    if inner.height >= 12 && super::timechart::has_any(d, s) {
        top.push(Line::from(Span::styled(
            format!("last {}h", super::timechart::hours(&d.cfg)),
            ui::faint(),
        )));
        top.push(Line::from(super::timechart::session_bar(d, s, w)));
        top.push(Line::raw(""));
    }
    let room = (inner.height as usize)
        .saturating_sub(top.len() + foot.len() + mid.len() + usize::from(!foot.is_empty()));
    // an agent's conversation: its tree, in the room the story would take
    if let (Some(id), true) = (s.native_id.clone(), room >= 4) {
        let at = Rect {
            y: inner.y + top.len() as u16,
            height: room as u16,
            ..inner
        };
        f.render_widget(
            Paragraph::new(top),
            Rect {
                height: at.y - inner.y,
                ..inner
            },
        );
        super::tree_view::show(d, &s.host, &s.agent, &id, s.status == "working");
        super::tree_view::draw(d, f, at, false);
        let mut rest: Vec<Line> = mid;
        if !foot.is_empty() {
            let left = (inner.y + inner.height).saturating_sub(at.y + at.height) as usize;
            rest.extend(std::iter::repeat_n(
                Line::raw(""),
                left.saturating_sub(rest.len() + foot.len()),
            ));
            rest.extend(foot);
        }
        let below = Rect {
            y: at.y + at.height,
            height: (inner.y + inner.height).saturating_sub(at.y + at.height),
            ..inner
        };
        f.render_widget(Paragraph::new(rest), below);
        return;
    }
    // the story, newest at the bottom, as much as fits
    let mut story: Vec<Line> = Vec::new();
    for e in super::inbox::of_session(d, &s.host, &s.name) {
        let (g, col, word) = e.kind.look();
        story.push(Line::from(vec![
            Span::styled(format!("{} ", ui::clock(e.t)), ui::faint()),
            Span::styled(format!("{g} "), ui::fg(col).add_modifier(Modifier::BOLD)),
            Span::styled(word.to_string(), ui::fg(col)),
        ]));
        let say = if e.body.is_empty() { &e.title } else { &e.body };
        if !say.is_empty() && e.kind != crate::events::Kind::New {
            story.push(Line::from(Span::styled(
                format!("      {}", ui::trunc(say, w.saturating_sub(6))),
                ui::faint(),
            )));
        }
    }
    if story.is_empty() && ran.is_empty() {
        for l in ui::wrap("nothing yet: its changes show up here as they happen", w) {
            story.push(Line::from(Span::styled(l, ui::faint())));
        }
    }
    let skip = story.len().saturating_sub(room);
    let mut lines: Vec<Line> = top;
    lines.extend(story.into_iter().skip(skip));
    lines.extend(mid);
    if !foot.is_empty() {
        // the usage at the very bottom
        let gap = (inner.height as usize).saturating_sub(lines.len() + foot.len());
        lines.extend(std::iter::repeat_n(Line::raw(""), gap));
        lines.extend(foot);
    }
    f.render_widget(Paragraph::new(lines), inner);
}
