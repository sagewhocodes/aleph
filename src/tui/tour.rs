//! The tour, after setup or from ctrl+k: chapters you do with the real keys on
//! your real sessions (Fleet, Focus and splits, Inbox, Usage, ctrl+k), with
//! reading cards alongside. A step waits for its key and lets nothing else
//! through, so while it runs nothing starts, closes or is sent: not even in
//! Focus, where your keys would otherwise go to the agent. Steps that can't apply
//! (no session yet, no conversation to show) are skipped. `ctrl+n` skips one,
//! `ctrl+x` or esc ends it, and finishing saves `tour = done`, so it shows once.

use super::{Act, Dash, Handoff, Mode, View};
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

/// What a step waits for.
#[derive(Clone, Copy)]
enum Wait {
    /// a reading card: enter moves on
    Read,
    /// one of these keys
    Keys(&'static [&'static str]),
    /// the key this command has (`key.<action>` can move it)
    Act(Act),
    /// the key that leaves typing, or Focus (`focus.exit`)
    Exit,
}

#[derive(Clone, Copy)]
struct Step {
    title: &'static str,
    text: &'static str,
    wait: Wait,
    /// does it apply now? (false: skipped)
    when: fn(&Dash) -> bool,
    /// typing is allowed meanwhile: into a search, say
    types: bool,
    /// done by the tour rather than by the key (what the key does can depend on
    /// settings, and this has to stay harmless)
    run: Option<fn(&mut Dash)>,
}

const fn read(title: &'static str, text: &'static str) -> Step {
    Step {
        title,
        text,
        wait: Wait::Read,
        when: always,
        types: false,
        run: None,
    }
}

const fn press(keys: &'static [&'static str], title: &'static str, text: &'static str) -> Step {
    Step {
        title,
        text,
        wait: Wait::Keys(keys),
        when: always,
        types: false,
        run: None,
    }
}

const fn only(mut s: Step, when: fn(&Dash) -> bool) -> Step {
    s.when = when;
    s
}

const fn typing_ok(mut s: Step) -> Step {
    s.types = true;
    s
}

fn always(_: &Dash) -> bool {
    true
}
fn has_session(d: &Dash) -> bool {
    super::fleet::selected_session(d).is_some()
}
fn in_wall(d: &Dash) -> bool {
    d.view == View::Wall
}
fn is_typing(d: &Dash) -> bool {
    d.view == View::Wall && d.wall.typing
}
fn wall_keys(d: &Dash) -> bool {
    d.view == View::Wall && !d.wall.typing
}
fn in_focus(d: &Dash) -> bool {
    d.view == View::Wall && d.wall.in_focus() && !d.wall.typing
}
fn has_turns(d: &Dash) -> bool {
    wall_keys(d) && super::wall::focused_session(d).is_some_and(|s| s.native_id.is_some())
}
fn in_turns(d: &Dash) -> bool {
    matches!(d.mode, Mode::Turns)
}
fn picking(d: &Dash) -> bool {
    matches!(d.mode, Mode::Palette(_))
}

/// Enter in Focus's chapter: Focus on the selected session, whatever enter is set
/// to do (`enter_opens` could hand the terminal over to herdr).
fn open_focus(d: &mut Dash) {
    if let Some(s) = super::fleet::selected_session(d) {
        super::wall::focus_on(d, &s.key(), View::Fleet);
    }
}

/// (name, the view it starts in, its steps)
type Chapter = (&'static str, View, &'static [Step]);

const CHAPTERS: &[Chapter] = &[
    (
        "Fleet",
        View::Fleet,
        &[
            read(
                "Fleet: every session, every machine",
                "Sessions that need you come first. This tour uses the real keys on your real sessions, and lets nothing through but the key it asks for: nothing starts, closes or gets sent.",
            ),
            only(
                Step {
                    wait: Wait::Act(Act::Down),
                    ..press(&[], "Move", "Move down the list.")
                },
                has_session,
            ),
            only(
                Step {
                    wait: Wait::Act(Act::NextSession),
                    ..press(
                        &[],
                        "Sessions and machines",
                        "This steps to the next session, on any machine. [ goes back, and ctrl+[ ctrl+] step through machines.",
                    )
                },
                has_session,
            ),
            Step {
                wait: Wait::Act(Act::Filter),
                ..press(
                    &[],
                    "Search",
                    "This searches by name, agent, folder or machine.",
                )
            },
            typing_ok(press(
                &["esc"],
                "Search",
                "Type a few letters and watch the list narrow. Esc clears it.",
            )),
            read(
                "Starting things",
                "{new} starts something: pick a place, then an agent or a terminal. i starts the machine's default agent in a folder. ctrl+t opens a terminal in a folder you pick, and ! one in a session's folder. Its row shows at once, and says why if it fails.",
            ),
        ],
    ),
    (
        "Focus and splits",
        View::Fleet,
        &[
            read(
                "Focus: one session, its real screen",
                "Focus shows a session's own terminal, with your keys going straight to it. The tour holds them back while it runs.",
            ),
            only(
                Step {
                    run: Some(open_focus),
                    ..press(&["enter"], "Open it", "Enter opens the selected session in Focus.")
                },
                has_session,
            ),
            only(
                Step {
                    wait: Wait::Exit,
                    ..press(
                        &[],
                        "Keys back",
                        "This gives the keys back to Aleph. The agent keeps running.",
                    )
                },
                is_typing,
            ),
            only(
                press(
                    &["t"],
                    "Its timeline",
                    "t shows or hides the timeline at the side: its states over the last hours, then its turns.",
                ),
                in_focus,
            ),
            only(
                press(
                    &["G"],
                    "The turn view",
                    "G opens every prompt, turn and tool call on the whole screen, with time, tokens and cost. Compactions show there too.",
                ),
                has_turns,
            ),
            only(
                press(&["z"], "Fold all", "z folds everything to a line per prompt."),
                in_turns,
            ),
            only(
                press(
                    &["Z"],
                    "Expand all",
                    "Z opens everything again. Enter folds just the one you're on, and a click does too.",
                ),
                in_turns,
            ),
            only(press(&["esc"], "Back", "Esc comes back."), in_turns),
            only(
                press(
                    &["v"],
                    "Split",
                    "v splits side by side, s stacks.",
                ),
                wall_keys,
            ),
            only(
                press(
                    &["esc"],
                    "What goes in it",
                    "It asks what goes in the new pane: a session on any machine, a new agent or a terminal. Esc leaves it empty, and nothing starts.",
                ),
                picking,
            ),
            only(
                press(&["x"], "Close a pane", "x closes a pane. A session in it lives on."),
                wall_keys,
            ),
            only(
                read(
                    "Splits",
                    "A pane can hold any machine's session, a new agent or a terminal. alt+arrows move between panes while typing, drag selects text and copies it, z maximises a pane, L locks the layout. When an agent or terminal exits, its window closes and you go back.",
                ),
                in_wall,
            ),
            only(
                Step {
                    wait: Wait::Exit,
                    ..press(&[], "Out", "This goes back to Fleet.")
                },
                in_wall,
            ),
        ],
    ),
    (
        "Inbox",
        View::Fleet,
        &[
            press(&["3"], "Inbox", "3 goes to the Inbox, and Tab steps through every view."),
            read(
                "Inbox: what happened while you were away",
                "Questions, finished turns, limits, machines going down and back. Enter goes to the session, f filters, and J opens the day's journal.",
            ),
        ],
    ),
    (
        "Usage",
        View::Fleet,
        &[
            press(
                &["2"],
                "Monitor",
                "2 goes to Monitor: each machine's CPU, GPU, memory and processes.",
            ),
            press(&["u"], "Usage", "u jumps to Usage."),
            read(
                "Usage: tokens, time and cost",
                "All of it comes from the agents' own logs on each machine, never from a provider. t a m p filter by time, agent, model and project, and the tabs break it down by day, turn and session.",
            ),
        ],
    ),
    (
        "ctrl+k",
        View::Fleet,
        &[
            press(
                &["ctrl+k"],
                "Go anywhere, do anything",
                "ctrl+k lists your sessions by machine, then a few actions. Everything else comes when you type.",
            ),
            typing_ok(press(
                &["esc"],
                "Words, in any order",
                "Type a machine and an agent, in either order. Every command is here too, and windows by name. ctrl+p there pins a session on top. Esc closes it.",
            )),
            press(
                &["ctrl+p"],
                "Prompts",
                "ctrl+p opens the prompt library: by scenario and agent, to use, edit or add to. It's there wherever you write a prompt too.",
            ),
            typing_ok(press(&["esc"], "Prompts", "Esc closes it.")),
            read(
                "Worth knowing",
                "ctrl+a is every agent waiting on you, ctrl+n the alerts you missed, W saved workspaces, ? every key. launch.<key> binds a folder or a preset to one key. ctrl+k and Tour replays any of this.",
            ),
        ],
    ),
];

/// Where the tour is.
pub(crate) struct Tour {
    chapter: usize,
    step: usize,
    /// this one chapter, then stop (from ctrl+k)
    alone: bool,
}

/// The chapters' names, for ctrl+k.
pub(crate) fn chapters() -> Vec<&'static str> {
    CHAPTERS.iter().map(|c| c.0).collect()
}

fn step_of(d: &Dash) -> Option<(Step, usize, usize)> {
    let t = d.tour.as_ref()?;
    let steps = CHAPTERS.get(t.chapter)?.2;
    steps.get(t.step).map(|s| (*s, t.step, steps.len()))
}

/// Start at this chapter: `alone` plays only that one.
pub(crate) fn start(d: &mut Dash, chapter: usize, alone: bool) {
    d.tour = Some(Tour {
        chapter,
        step: 0,
        alone,
    });
    arrive(d);
}

/// A chapter begins in its view, with nothing open and no keys going to an agent.
fn arrive(d: &mut Dash) {
    let Some(t) = &d.tour else { return };
    let Some((_, v, _)) = CHAPTERS.get(t.chapter) else {
        return;
    };
    let v = *v;
    d.mode = Mode::Normal;
    d.wall.typing = false;
    d.switch(v);
    settle(d);
}

/// Past any step that doesn't apply; past the end of a chapter, the next one.
fn settle(d: &mut Dash) {
    loop {
        let Some(t) = &d.tour else { return };
        let (chapter, step, alone) = (t.chapter, t.step, t.alone);
        let Some(steps) = CHAPTERS.get(chapter).map(|c| c.2) else {
            finish(d);
            return;
        };
        match steps.get(step) {
            Some(s) if (s.when)(d) => return,
            Some(_) => {
                if let Some(t) = d.tour.as_mut() {
                    t.step += 1;
                }
            }
            None if alone || chapter + 1 >= CHAPTERS.len() => {
                finish(d);
                return;
            }
            None => {
                if let Some(t) = d.tour.as_mut() {
                    t.chapter += 1;
                    t.step = 0;
                }
                arrive(d);
                return;
            }
        }
    }
}

fn next(d: &mut Dash) {
    if let Some(t) = d.tour.as_mut() {
        t.step += 1;
    }
    settle(d);
}

/// The end, however it came: back in Fleet, and it won't show by itself again.
fn finish(d: &mut Dash) {
    d.tour = None;
    d.mode = Mode::Normal;
    d.wall.typing = false;
    let _ = d.cfg.set("tour", "done");
    d.switch(View::Fleet);
    d.toast(super::Level::Info, "ctrl+k and Tour plays any of it again");
}

/// Is this the key the step is waiting for?
fn wanted(d: &Dash, wait: Wait, k: &KeyEvent) -> bool {
    let ks = super::key_str(k);
    match wait {
        Wait::Read => false,
        Wait::Keys(keys) => keys.contains(&ks.as_str()),
        Wait::Act(a) => d.keys.owner(&ks) == Some(a),
        Wait::Exit => super::wall::exit_keys(d).contains(&ks),
    }
}

/// Letters and the like, for a step that's fine with typing.
fn is_typing_key(k: &KeyEvent) -> bool {
    !k.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(
            k.code,
            KeyCode::Char(_)
                | KeyCode::Backspace
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Up
                | KeyCode::Down
        )
}

/// Every key while it runs: what the step wants goes through, and nothing else.
pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    let ks = super::key_str(&k);
    if ks == "ctrl+x" {
        finish(d);
        return None;
    }
    if ks == "ctrl+n" {
        next(d);
        return None;
    }
    let (step, ..) = step_of(d)?;
    if matches!(step.wait, Wait::Read) {
        match k.code {
            KeyCode::Enter | KeyCode::Right | KeyCode::Char(' ') => next(d),
            KeyCode::Esc => finish(d),
            _ => {}
        }
        return None;
    }
    if wanted(d, step.wait, &k) {
        let h = match step.run {
            Some(run) => {
                run(d);
                None
            }
            None => d.key_now(k),
        };
        next(d);
        return h;
    }
    if k.code == KeyCode::Esc {
        finish(d);
        return None;
    }
    if step.types && is_typing_key(&k) {
        return d.key_now(k);
    }
    None
}

/// The key a step asks for, as this keymap has it.
fn key_label(d: &Dash, wait: Wait) -> String {
    match wait {
        Wait::Read => "enter".to_string(),
        Wait::Keys(keys) => keys.first().copied().unwrap_or("").to_string(),
        Wait::Act(a) => d.keys.key_for(a),
        Wait::Exit => super::wall::keys_back(d),
    }
}

/// The card, low on the right, so what it's talking about stays in view.
pub(crate) fn draw(d: &Dash, f: &mut Frame, area: Rect) {
    let Some((step, n, of)) = step_of(d) else {
        return;
    };
    let name = d
        .tour
        .as_ref()
        .and_then(|t| CHAPTERS.get(t.chapter))
        .map_or("", |c| c.0);
    let w = 62u16.min(area.width.saturating_sub(4));
    if w < 24 || area.height < 8 {
        return;
    }
    let pad = ui::padding();
    let tw = (w - 2 - pad.left - pad.right).max(10) as usize;
    let key = key_label(d, step.wait);
    let text = step
        .text
        .replace("{new}", &d.keys.key_for(Act::New))
        .replace("{exit}", &super::wall::keys_back(d));
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        step.title,
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    ))];
    lines.extend(ui::wrap(&text, tw).into_iter().map(Line::from));
    lines.push(Line::raw(""));
    let what = if matches!(step.wait, Wait::Read) {
        "next"
    } else {
        "do it"
    };
    lines.push(ui::hints_fit(
        &[
            (key.as_str(), what),
            ("ctrl+n", "skip"),
            ("ctrl+x", "end the tour"),
        ],
        tw as u16,
    ));
    let h = (lines.len() as u16 + 2 + pad.top + pad.bottom).min(area.height.saturating_sub(2));
    let r = Rect::new(
        area.x + area.width.saturating_sub(w + 1),
        area.y + area.height.saturating_sub(h + 2),
        w,
        h,
    );
    ui::clear(f, r);
    let title = format!(" Tour · {name} · {}/{of} ", n + 1);
    f.render_widget(
        Paragraph::new(lines).block(ui::panel(title.as_str(), true)),
        r,
    );
}
