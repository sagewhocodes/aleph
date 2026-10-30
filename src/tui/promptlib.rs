//! The prompt library, as a place of its own (ctrl+p outside a text box, or
//! ctrl+k): browse every prompt by scenario and agent, read one whole, use it,
//! edit it, make a new one. Letters search. Enter sends it to the session you're
//! on, or starts a new one with it when you're on none.

use super::forms::{Field, For, Input, NewForm};
use super::{Dash, Handoff, Level, Mode, View};
use crate::herdr::Session;
use crate::prompts::{self, Prompt};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

pub(crate) struct PromptLib {
    q: Field,
    /// the scenario shown (None: all of them); tab moves through them
    tag: Option<String>,
    /// only what suits this agent (None: every agent's)
    agent: Option<String>,
    /// the session it's for, whose agent that is
    kind: Option<String>,
    sel: usize,
    /// ctrl+d asked once
    deleting: Option<String>,
}

/// The session you're on, wherever you are.
fn here(d: &Dash) -> Option<Session> {
    match d.view {
        View::Wall => super::wall::focused_session(d),
        View::Fleet => super::fleet::selected_session(d),
        _ => None,
    }
}

pub(crate) fn open(d: &mut Dash) {
    let kind = here(d)
        .map(|s| s.agent)
        .filter(|a| a != crate::agents::SHELL);
    d.mode = Mode::Prompts(PromptLib {
        q: Field::default(),
        tag: None,
        agent: kind.clone(),
        kind,
        sel: 0,
        deleting: None,
    });
}

impl PromptLib {
    fn base(&self, d: &Dash) -> Vec<Prompt> {
        match &self.agent {
            Some(a) => prompts::for_agent(&d.cfg, a),
            None => prompts::all(&d.cfg),
        }
    }

    /// What's listed, in order.
    fn list(&self, d: &Dash) -> Vec<Prompt> {
        self.base(d)
            .into_iter()
            .filter(|p| self.tag.as_ref().is_none_or(|t| p.tags.contains(t)))
            .filter(|p| prompts::matches(p, &self.q.text))
            .collect()
    }

    /// Tab: the next scenario, then all of them again.
    fn step_tag(&mut self, d: &Dash, fwd: bool) {
        let tags = prompts::scenarios(&self.base(d));
        let n = tags.len() + 1;
        let at = self
            .tag
            .as_ref()
            .and_then(|t| tags.iter().position(|x| x == t))
            .map_or(0, |i| i + 1);
        let next = if fwd { (at + 1) % n } else { (at + n - 1) % n };
        self.tag = next.checked_sub(1).and_then(|i| tags.get(i).cloned());
        self.sel = 0;
    }
}

/// Put it to use: filled in for the session you're on, ready to send (its
/// variables asked first). On no session, the new-session form's first prompt.
fn use_it(d: &mut Dash, p: Prompt) {
    let mut i = match here(d) {
        Some(s) => Input::new(
            format!("Prompt for {}", s.display()),
            "enter sends it",
            String::new(),
            For::Send(vec![s]),
        ),
        None => {
            let host =
                d.fv.selected_host(d)
                    .or_else(|| d.fleet.hosts.first().map(|h| h.alias.clone()));
            let Some(host) = host else {
                d.note("no machine to start it on");
                return;
            };
            let nf = NewForm::new(d, &host, None);
            Input::new(
                "First prompt".into(),
                "enter puts it in the new session's form",
                String::new(),
                For::FormPrompt(Box::new(nf)),
            )
        }
    };
    i.choose(p, String::new());
    d.mode = Mode::Input(i);
}

/// A text box over the library; esc or enter comes back to it.
fn ask(d: &mut Dash, title: String, hint: &str, value: String, purpose: For) {
    d.mode = Mode::Input(Input::new(title, hint, value, purpose));
}

/// Keys. The mode was taken: anything but a hand-over to a box puts it back.
pub(crate) fn key(d: &mut Dash, mut lib: PromptLib, k: KeyEvent) -> Option<Handoff> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let list = lib.list(d);
    let cur = list.get(lib.sel).cloned();
    let asked = lib.deleting.take();
    match k.code {
        KeyCode::Esc if !lib.q.text.is_empty() => {
            lib.q = Field::default();
            lib.sel = 0;
        }
        KeyCode::Esc => return None,
        KeyCode::Enter => {
            if let Some(p) = cur {
                use_it(d, p);
                return None;
            }
        }
        KeyCode::Up => lib.sel = lib.sel.saturating_sub(1),
        KeyCode::Down => lib.sel = (lib.sel + 1).min(list.len().saturating_sub(1)),
        KeyCode::Tab => lib.step_tag(d, true),
        KeyCode::BackTab => lib.step_tag(d, false),
        KeyCode::Char('a') if ctrl => {
            lib.agent = if lib.agent.is_some() {
                None
            } else {
                lib.kind.clone()
            };
            lib.sel = 0;
        }
        KeyCode::Char('n') if ctrl => {
            ask(
                d,
                "New prompt".into(),
                "its name: ctrl+p or @name brings it back",
                String::new(),
                For::PromptNew,
            );
            return None;
        }
        KeyCode::Char('e') if ctrl => {
            if let Some(p) = cur {
                ask(
                    d,
                    format!("Edit @{}", p.name),
                    "{dir} {folder} {host} {session} {agent} {task} {date} {time} fill themselves in; any other {word} is asked",
                    p.text,
                    For::PromptEdit(p.name),
                );
                return None;
            }
        }
        KeyCode::Char('t') if ctrl => {
            if let Some(p) = cur {
                ask(
                    d,
                    format!("Scenarios for @{}", p.name),
                    "comma separated: review, tests, debug, or your own",
                    p.tags.join(", "),
                    For::PromptTags(p.name),
                );
                return None;
            }
        }
        KeyCode::Char('g') if ctrl => {
            if let Some(p) = cur {
                ask(
                    d,
                    format!("Which agents @{} is for", p.name),
                    "comma separated: claude, codex, … empty is any agent",
                    p.agents.join(", "),
                    For::PromptAgents(p.name),
                );
                return None;
            }
        }
        KeyCode::Char('d') if ctrl => {
            if let Some(p) = cur {
                if p.starter {
                    d.toast(
                        Level::Info,
                        "a starter stays: save your own by its name to replace it",
                    );
                } else if asked.as_deref() == Some(p.name.as_str()) {
                    prompts::remove(&mut d.cfg, &p.name);
                    d.toast(Level::Ok, format!("@{} deleted", p.name));
                    lib.sel = lib.sel.saturating_sub(1);
                } else {
                    lib.deleting = Some(p.name);
                }
            }
        }
        _ => {
            if lib.q.key(&k) {
                lib.sel = 0;
            }
        }
    }
    d.mode = Mode::Prompts(lib);
    None
}

/// A prompt's text, its variables picked out.
fn text_lines(text: &str, w: usize) -> Vec<Line<'static>> {
    ui::wrap(text, w)
        .into_iter()
        .map(|l| {
            let mut spans = Vec::new();
            let mut rest = l.as_str();
            while let Some(a) = rest.find('{') {
                let Some(b) = rest[a..].find('}') else {
                    break;
                };
                spans.push(Span::raw(rest[..a].to_string()));
                spans.push(Span::styled(
                    rest[a..=a + b].to_string(),
                    ui::fg(c::ACCENT()),
                ));
                rest = &rest[a + b + 1..];
            }
            spans.push(Span::raw(rest.to_string()));
            Line::from(spans)
        })
        .collect()
}

pub(crate) fn draw(d: &Dash, lib: &PromptLib, f: &mut Frame, area: Rect) {
    let w = 112u16.min(area.width.saturating_sub(2));
    let h = 24u16.min(area.height.saturating_sub(2));
    let r = ui::centered(area, w, h);
    ui::clear(f, r);
    let block = ui::panel("Prompts", true);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let [top, chips, _, body, _, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);
    let mut ql = vec![Span::styled(
        " › ",
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    )];
    ql.extend(lib.q.spans(
        true,
        "type to search: a name, a scenario, an agent, any word…",
    ));
    f.render_widget(Paragraph::new(Line::from(ql)), top);
    // the scenarios, and whose prompts these are
    let chip = |name: String, on: bool| {
        if on {
            Span::styled(
                format!(" {name} "),
                ui::selected().add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {name} "), ui::muted())
        }
    };
    let mut cl = vec![Span::raw(" "), chip("all".into(), lib.tag.is_none())];
    for t in prompts::scenarios(&lib.base(d)) {
        let on = lib.tag.as_deref() == Some(t.as_str());
        cl.push(chip(t, on));
    }
    f.render_widget(Paragraph::new(Line::from(cl)), chips);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(44), Constraint::Percentage(56)])
            .spacing(2)
            .areas(body);
    let list = lib.list(d);
    let room = left.height as usize;
    let start = lib.sel.saturating_sub(room.saturating_sub(1));
    let lw = left.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (i, p) in list.iter().enumerate().skip(start).take(room) {
        let on = i == lib.sel;
        let name = format!("@{}", p.name);
        let tags = p.tags.join(" ");
        let used = ui::width(&name) + 4;
        let mut l = vec![
            Span::styled(if on { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(
                name,
                if on {
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                } else {
                    ui::fg(c::ACCENT())
                },
            ),
        ];
        if lw > used + 4 {
            l.push(Span::styled(
                format!("  {}", ui::trunc(&tags, lw - used)),
                ui::faint(),
            ));
        }
        let line = Line::from(l);
        lines.push(if on { line.style(ui::selected()) } else { line });
    }
    if list.is_empty() {
        lines.push(Line::from(Span::styled("  nothing matches", ui::muted())));
    }
    f.render_widget(Paragraph::new(lines), left);
    // the one under the cursor, whole
    if let Some(p) = list.get(lib.sel) {
        let rw = right.width.max(10) as usize;
        let who = if p.agents.is_empty() {
            "for any agent".to_string()
        } else {
            format!(
                "for {}",
                p.agents
                    .iter()
                    .map(|a| crate::agents::label(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(format!("@{}", p.name), ui::bold()),
            Span::styled(
                format!("  {who}{}", if p.starter { " · a starter" } else { "" }),
                ui::faint(),
            ),
        ])];
        lines.push(Line::raw(""));
        lines.extend(text_lines(&p.text, rw));
        let vars = prompts::vars(&p.text);
        let asked: Vec<&String> = vars.iter().filter(|v| !prompts::is_automatic(v)).collect();
        if !asked.is_empty() {
            lines.push(Line::raw(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "asks for {} when you use it",
                    asked
                        .iter()
                        .map(|v| format!("{{{v}}}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                ui::muted(),
            )));
        }
        f.render_widget(Paragraph::new(lines), right);
    }
    let enter = if here(d).is_some() {
        "send to this session"
    } else {
        "start a session with it"
    };
    let whose = match (&lib.agent, &lib.kind) {
        (Some(_), _) => "every agent's",
        (None, Some(_)) => "this agent's",
        (None, None) => "",
    };
    let mut hints = vec![
        ("enter", enter),
        ("⇥", "scenario"),
        ("ctrl+e", "edit"),
        ("ctrl+n", "new"),
        ("ctrl+t", "scenarios"),
        ("ctrl+g", "agents"),
    ];
    if !whose.is_empty() {
        hints.push(("ctrl+a", whose));
    }
    hints.push(if lib.deleting.is_some() {
        ("ctrl+d", "again deletes it")
    } else {
        ("ctrl+d", "delete")
    });
    hints.push(("esc", "close"));
    f.render_widget(Paragraph::new(ui::hints_fit(&hints, foot.width)), foot);
}
