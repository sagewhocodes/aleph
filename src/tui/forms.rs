//! Popups that take text: a one-line input (prompt, keys, rename, setting),
//! the new-session form and the add-host form.

use super::{Dash, Handoff, Level};
use crate::agentwatch::{AgentWatch, Avail};
use crate::fleet::Fleet;
use crate::herdr::{Herdr, Session};
use crate::ui::{self, c};
use crate::{cli, history, names};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Res {
    Keep,
    Cancel,
    Submit,
}

/// A single editable line with a cursor.
#[derive(Default, Clone)]
pub(crate) struct Field {
    pub text: String,
    pub cur: usize,
}

impl Field {
    pub fn new(s: &str) -> Field {
        Field {
            text: s.to_string(),
            cur: s.chars().count(),
        }
    }
    fn byte(&self, i: usize) -> usize {
        self.text
            .char_indices()
            .nth(i)
            .map(|x| x.0)
            .unwrap_or(self.text.len())
    }
    /// Handles editing keys; false if the key isn't an editing key.
    pub fn key(&mut self, k: &KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.text.chars().count();
        match k.code {
            KeyCode::Char('u') if ctrl => {
                self.text.clear();
                self.cur = 0;
            }
            KeyCode::Char('a') if ctrl => self.cur = 0,
            KeyCode::Char('e') if ctrl => self.cur = n,
            KeyCode::Char('w') if ctrl => {
                let b = self.byte(self.cur);
                let head = self.text[..b].trim_end();
                let cut = head.rfind(' ').map(|i| i + 1).unwrap_or(0);
                self.text.replace_range(cut..b, "");
                self.cur = self.text[..cut].chars().count();
            }
            KeyCode::Char(ch) if !ctrl => {
                let b = self.byte(self.cur);
                self.text.insert(b, ch);
                self.cur += 1;
            }
            KeyCode::Backspace if self.cur > 0 => {
                let b = self.byte(self.cur - 1);
                self.text.remove(b);
                self.cur -= 1;
            }
            KeyCode::Delete if self.cur < n => {
                let b = self.byte(self.cur);
                self.text.remove(b);
            }
            KeyCode::Left => self.cur = self.cur.saturating_sub(1),
            KeyCode::Right => self.cur = (self.cur + 1).min(n),
            KeyCode::Home => self.cur = 0,
            KeyCode::End => self.cur = n,
            KeyCode::Backspace | KeyCode::Delete => {}
            _ => return false,
        }
        true
    }
    /// The text with a block cursor.
    pub fn spans(&self, focus: bool, placeholder: &str) -> Vec<Span<'static>> {
        if self.text.is_empty() && !focus {
            return vec![Span::styled(placeholder.to_string(), ui::faint())];
        }
        if !focus {
            return vec![Span::raw(self.text.clone())];
        }
        let b = self.byte(self.cur);
        let (a, rest) = self.text.split_at(b);
        let mut it = rest.chars();
        let at = it.next().map(String::from).unwrap_or_else(|| " ".into());
        let mut v = vec![
            Span::raw(a.to_string()),
            Span::styled(at, Style::new().fg(c::BASE()).bg(c::ACCENT())),
            Span::raw(it.collect::<String>()),
        ];
        if self.text.is_empty() {
            v.push(Span::styled(format!(" {placeholder}"), ui::faint()));
        }
        v
    }
}

// ------------------------------------------------------------------ input

pub(crate) enum For {
    Send(Vec<Session>),
    Keys(Session),
    Rename(Session),
    Setting(String),
    /// (host, folder) for a queued task
    Queue(String, Option<String>),
    /// (host, session name): a message sent once that session is free
    QueueTo(String, String),
    /// marked sessions: the same message queued for each
    QueueEach(Vec<Session>),
    /// a launch saved as a preset (the input is its name)
    SavePreset(Box<crate::history::Entry>),
    /// (host, session name): its note
    Note(String, String),
    /// marked sessions saved as a preset (the input is its name)
    SavePresetFrom(Vec<Session>),
    /// a new entry of a family (`approve.`, `action.`): the input is name=value
    AddSetting(String),
    /// settings to import: a file or URL (a preview and a confirm follow)
    Import,
    /// where you are now, saved as a workspace (the input is its title)
    SaveWorkspace,
    /// what you wrote, saved as a prompt (the input is its name); then back
    /// to the input you were writing it in
    SavePrompt(Box<Input>),
    /// (host, repository folder): the prompt a race's agents all get
    Race(String, String),
    /// Usage's filters, typed (`/`)
    UsageQuery,
}

/// The prompt library inside an input (ctrl+p, or `@name` and enter).
pub(crate) struct Lib {
    list: Vec<crate::prompts::Prompt>,
    sel: usize,
    /// the text before the library opened (esc puts it back)
    before: String,
    /// a chosen prompt's variables, asked one by one
    filling: Option<Filling>,
}

struct Filling {
    prompt: crate::prompts::Prompt,
    ctx: crate::prompts::Ctx,
    left: Vec<String>,
    got: Vec<(String, String)>,
}

pub(crate) struct Input {
    pub title: String,
    pub hint: String,
    pub f: Field,
    pub purpose: For,
    pub err: Option<String>,
    pub(crate) lib: Option<Lib>,
}

impl Input {
    pub fn new(title: String, hint: &str, value: String, purpose: For) -> Input {
        Input {
            title,
            hint: hint.into(),
            f: Field::new(&value),
            purpose,
            err: None,
            lib: None,
        }
    }

    /// Is what's typed here a prompt for an agent (the library applies)?
    fn writes_prompt(&self) -> bool {
        matches!(
            self.purpose,
            For::Send(_) | For::QueueTo(..) | For::QueueEach(_) | For::Queue(..) | For::Race(..)
        )
    }

    /// The session a prompt here goes to, for its variables.
    fn ctx(&self) -> crate::prompts::Ctx {
        use crate::prompts::Ctx;
        match &self.purpose {
            For::Send(v) | For::QueueEach(v) => v.first().map(Ctx::of).unwrap_or_default(),
            For::QueueTo(h, n) => Ctx {
                host: h.clone(),
                session: n.clone(),
                ..Default::default()
            },
            For::Queue(h, d) => Ctx {
                host: h.clone(),
                dir: d.clone().unwrap_or_default(),
                ..Default::default()
            },
            For::Race(h, d) => Ctx {
                host: h.clone(),
                dir: d.clone(),
                ..Default::default()
            },
            _ => Ctx::default(),
        }
    }

    /// Use prompt `p`: filled in at once, or its variables asked first.
    fn choose(&mut self, p: crate::prompts::Prompt, before: String) {
        let ctx = self.ctx();
        let left = crate::prompts::asks(&p.text, &ctx);
        if left.is_empty() {
            self.f = Field::new(&crate::prompts::fill(&p.text, &ctx, &[]));
            self.lib = None;
        } else {
            self.f = Field::new("");
            self.lib = Some(Lib {
                list: Vec::new(),
                sel: 0,
                before,
                filling: Some(Filling {
                    prompt: p,
                    ctx,
                    left,
                    got: Vec::new(),
                }),
            });
        }
    }

    /// Keys, with the settings at hand (the prompt library reads them).
    pub fn key_in(&mut self, k: KeyEvent, cfg: &crate::config::Config) -> Res {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(mut lib) = self.lib.take() {
            if k.code == KeyCode::Esc {
                self.f = Field::new(&lib.before);
                return Res::Keep;
            }
            if let Some(mut fl) = lib.filling.take() {
                if k.code == KeyCode::Enter {
                    fl.got
                        .push((fl.left.remove(0), self.f.text.trim().to_string()));
                    if fl.left.is_empty() {
                        self.f =
                            Field::new(&crate::prompts::fill(&fl.prompt.text, &fl.ctx, &fl.got));
                        return Res::Keep;
                    }
                    self.f = Field::new("");
                } else {
                    self.f.key(&k);
                }
                lib.filling = Some(fl);
                self.lib = Some(lib);
                return Res::Keep;
            }
            let n = lib.list.len().max(1);
            match k.code {
                KeyCode::Down => lib.sel = (lib.sel + 1) % n,
                KeyCode::Up => lib.sel = (lib.sel + n - 1) % n,
                KeyCode::Enter => {
                    if let Some(p) = lib.list.get(lib.sel).cloned() {
                        self.choose(p, lib.before);
                    }
                    return Res::Keep;
                }
                _ => {}
            }
            self.lib = Some(lib);
            return Res::Keep;
        }
        if self.writes_prompt() {
            match k.code {
                KeyCode::Char('p') if ctrl => {
                    self.lib = Some(Lib {
                        list: crate::prompts::all(cfg),
                        sel: 0,
                        before: self.f.text.clone(),
                        filling: None,
                    });
                    return Res::Keep;
                }
                KeyCode::Char('s') if ctrl && !self.f.text.trim().is_empty() => {
                    let save = Input::new(
                        "Save as a prompt".into(),
                        "its name: ctrl+p or @name brings it back wherever you write a prompt",
                        String::new(),
                        For::SaveWorkspace,
                    );
                    let orig = std::mem::replace(self, save);
                    self.purpose = For::SavePrompt(Box::new(orig));
                    return Res::Keep;
                }
                // `@name` alone: that prompt, filled in (enter again sends it)
                KeyCode::Enter => {
                    let t = self.f.text.trim().to_string();
                    if let Some(p) = t
                        .strip_prefix('@')
                        .filter(|n| !n.is_empty() && !n.contains(char::is_whitespace))
                        .and_then(|_| crate::prompts::get(cfg, &t))
                    {
                        self.choose(p, t);
                        return Res::Keep;
                    }
                }
                _ => {}
            }
        }
        self.key(k)
    }

    pub fn key(&mut self, k: KeyEvent) -> Res {
        match k.code {
            KeyCode::Esc => Res::Cancel,
            KeyCode::Enter => Res::Submit,
            // a prompt: tab queues it for when it's free (for each one)
            KeyCode::Tab => {
                if let For::Send(v) = &self.purpose {
                    self.purpose = match v.as_slice() {
                        [] => return Res::Keep,
                        [s] => For::QueueTo(s.host.clone(), s.name.clone()),
                        _ => For::QueueEach(v.clone()),
                    };
                    return Res::Submit;
                }
                Res::Keep
            }
            _ => {
                self.f.key(&k);
                self.err = None;
                Res::Keep
            }
        }
    }
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let w = 70u16.min(area.width.saturating_sub(2));
        let mut lines = Vec::new();
        match &self.lib {
            // the prompt library: pick one
            Some(Lib {
                list,
                sel,
                filling: None,
                ..
            }) => {
                lines.push(Line::from(Span::styled(
                    "your prompts (and the starters): enter puts one in",
                    ui::muted(),
                )));
                lines.push(Line::raw(""));
                for (i, p) in list.iter().enumerate() {
                    let on = i == *sel;
                    lines.push(Line::from(vec![
                        Span::styled(if on { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
                        Span::styled(
                            format!("@{}  ", ui::pad(&p.name, 10)),
                            if on {
                                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                            } else {
                                ui::fg(c::ACCENT())
                            },
                        ),
                        Span::styled(
                            ui::trunc(&p.text, (w as usize).saturating_sub(20)),
                            if on { Style::new() } else { ui::muted() },
                        ),
                    ]));
                }
                lines.push(Line::raw(""));
                lines.push(ui::hints(&[
                    ("↑↓", "pick"),
                    ("enter", "use it"),
                    ("esc", "back"),
                ]));
            }
            // its variables, one by one
            Some(Lib {
                filling: Some(fl), ..
            }) => {
                lines.push(Line::from(vec![
                    Span::styled(format!("@{} needs ", fl.prompt.name), ui::muted()),
                    Span::styled(
                        format!("{{{}}}", fl.left[0]),
                        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        if fl.left.len() > 1 {
                            format!(" (then {} more)", fl.left.len() - 1)
                        } else {
                            String::new()
                        },
                        ui::faint(),
                    ),
                ]));
                lines.push(Line::raw(""));
                let mut l = vec![Span::styled("┃ ", ui::fg(c::ACCENT()))];
                l.extend(self.f.spans(true, ""));
                lines.push(Line::from(l));
                lines.push(Line::raw(""));
                lines.push(ui::hints(&[
                    ("enter", if fl.left.len() > 1 { "next" } else { "done" }),
                    ("esc", "back"),
                ]));
            }
            None => {
                lines.push(Line::from(Span::styled(self.hint.clone(), ui::muted())));
                lines.push(Line::raw(""));
                let mut l = vec![Span::styled("┃ ", ui::fg(c::ACCENT()))];
                l.extend(self.f.spans(true, ""));
                lines.push(Line::from(l));
                lines.push(Line::raw(""));
                lines.push(match &self.err {
                    Some(e) => Line::from(vec![
                        Span::styled("error: ", ui::fg(c::RED()).add_modifier(Modifier::BOLD)),
                        Span::raw(e.clone()),
                    ]),
                    None if self.writes_prompt() => ui::hints(&[
                        ("enter", "send"),
                        ("ctrl+p", "prompts"),
                        ("ctrl+s", "save as one"),
                        ("esc", "cancel"),
                    ]),
                    None => ui::hints(&[("enter", "send"), ("esc", "cancel")]),
                });
            }
        }
        let r = ui::centered(area, w, lines.len() as u16 + 2);
        crate::ui::clear(f, r);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(ui::panel(self.title.as_str(), true)),
            r,
        );
    }
}

pub(crate) fn submit_input(d: &mut Dash, mut i: Input) -> Option<Handoff> {
    let text = i.f.text.trim().to_string();
    match i.purpose {
        For::Send(ref v) => {
            if text.is_empty() {
                return None;
            }
            // out of reach (the internet down and its machine not answering):
            // into its session's queue, sent once it's back (and free); a
            // machine that still answers (a local network) gets it now
            let (late, now): (Vec<_>, Vec<_>) = v.iter().cloned().partition(|s| d.cut_off(&s.host));
            for s in &late {
                crate::queue::add(crate::queue::Task {
                    host: s.host.clone(),
                    prompt: text.clone(),
                    to: Some(s.name.clone()),
                    ..Default::default()
                });
            }
            if !late.is_empty() {
                d.toast(
                    Level::Info,
                    format!(
                        "offline — queued for {}; it goes once you're back",
                        if late.len() == 1 {
                            late[0].display().to_string()
                        } else {
                            format!("{} sessions", late.len())
                        }
                    ),
                );
            }
            if now.is_empty() {
                return None;
            }
            // per machine, in parallel; one toast for them all
            super::batch::send(d, now, text);
        }
        For::QueueEach(ref v) => {
            if text.is_empty() {
                return None;
            }
            super::batch::queue(d, v, &text);
        }
        For::Keys(ref s) => {
            let keys: Vec<String> = text.split_whitespace().map(String::from).collect();
            if keys.is_empty() {
                return None;
            }
            let (s, t) = (s.clone(), d.target_of(&s.host));
            d.spawn(Some(s.host.clone()), move || {
                let k: Vec<&str> = keys.iter().map(String::as_str).collect();
                Herdr::new(&t)
                    .send_keys(&s.pane_id, &k)
                    .map(|_| format!("sent {} to {}", keys.join(" "), s.display()))
            });
        }
        For::Rename(ref s) => {
            if !names::valid(&text) {
                i.err =
                    Some("lowercase letters, digits, - and _ (start with a letter, ≤ 32)".into());
                d.mode = super::Mode::Input(i);
                return None;
            }
            let (s, t) = (s.clone(), d.target_of(&s.host));
            d.spawn(Some(s.host.clone()), move || {
                let h = Herdr::new(&t);
                let label = cli::renamed_label(&s.label, &text);
                h.rename_tab(&s.tab_id, &label)?;
                let _ = h.rename_agent(&s.pane_id, &text);
                let _ = h.report_tokens(&s.pane_id, &[("name", &text)]);
                Ok(format!("{} → {label}", s.display()))
            });
        }
        For::Setting(ref key) => {
            let key = key.clone();
            // checked before it's written: a bad value never reaches the file
            if let Err(e) = super::onboard::check_value(&key, &text) {
                i.err = Some(e);
                d.mode = super::Mode::Input(i);
                return None;
            }
            super::settings::save(d, &key, &text);
        }
        For::SavePrompt(_) => {
            let name = crate::prompts::slug(&text);
            if name.is_empty() {
                i.err = Some("a name with a letter or digit in it".into());
                d.mode = super::Mode::Input(i);
                return None;
            }
            let For::SavePrompt(orig) = i.purpose else {
                return None;
            };
            match d.cfg.set(&format!("prompt.{name}"), &orig.f.text) {
                Ok(_) => d.toast(
                    super::Level::Ok,
                    format!("saved @{name} · ctrl+p or @{name} brings it back"),
                ),
                Err(e) => d.error(e.to_string()),
            }
            // back to what you were writing it for, the text still there
            d.mode = super::Mode::Input(*orig);
        }
        For::UsageQuery => match super::usage_filters::parse_query(d, &text) {
            Ok(f) => {
                d.usage.f = f;
                super::usage_filters::changed(d);
            }
            Err(e) => {
                i.err = Some(e);
                d.mode = super::Mode::Input(i);
            }
        },
        For::Race(ref host, ref dir) => {
            if text.is_empty() {
                return None;
            }
            let (t, dir, cfg) = (d.target_of(host), dir.clone(), d.cfg.clone());
            let agents = crate::race::default_agents(&cfg);
            d.toast(
                super::Level::Info,
                format!(
                    "starting a race: {} in worktrees of {dir}",
                    agents.join(" vs ")
                ),
            );
            d.spawn(Some(host.clone()), move || {
                crate::race::start(&cfg, &t, &dir, &text, &agents, &mut |_| {}).map(|r| {
                    format!(
                        "race {} started: {} · ctrl+k › Race {} shows the diffs",
                        r.id,
                        r.racers
                            .iter()
                            .map(|x| x.name.clone())
                            .collect::<Vec<_>>()
                            .join(" vs "),
                        r.id
                    )
                })
            });
        }
        For::SaveWorkspace => {
            if !text.is_empty() {
                super::spaces::save_named(d, &text);
            }
        }
        For::Import => {
            if text.is_empty() {
                return None;
            }
            if let Err(e) = super::settings::import_preview(d, &text) {
                i.err = Some(e);
                d.mode = super::Mode::Input(i);
                return None;
            }
        }
        For::Queue(ref host, ref dir) => {
            if text.is_empty() {
                return None;
            }
            let id = crate::queue::add(crate::queue::Task {
                host: host.clone(),
                prompt: text,
                dir: dir.clone(),
                ..Default::default()
            });
            d.note(format!(
                "task #{id} queued on {host} — the next idle agent{} takes it",
                if dir.is_some() { " in that folder" } else { "" }
            ));
        }
        For::SavePreset(ref l) => {
            if !names::valid(&text) {
                i.err = Some("lowercase letters, digits, - and _ (start with a letter)".into());
                d.mode = super::Mode::Input(i);
                return None;
            }
            let mut set: Vec<(&str, String)> = vec![
                ("host", l.host.clone()),
                ("dir", l.dir.clone()),
                ("agent", l.agent.clone()),
            ];
            if l.worktree {
                set.push(("worktree", "1".into()));
            }
            if let Some(p) = l.prompt.as_deref().filter(|p| !p.is_empty()) {
                set.push(("prompt", p.to_string()));
            }
            if !l.args.is_empty() {
                let q: Vec<String> = l.args.iter().map(|a| crate::proc::sh_quote(a)).collect();
                set.push(("args", q.join(" ")));
            }
            let _ = d.cfg.unset_prefix(&format!("preset.{text}."));
            for (k, v) in set.into_iter().filter(|(_, v)| !v.is_empty()) {
                if let Err(e) = d.cfg.set(&format!("preset.{text}.{k}"), &v) {
                    d.error(format!("couldn't save the preset: {e}"));
                    return None;
                }
            }
            d.toast(
                Level::Ok,
                format!(
                    "saved preset @{text} — P starts it, or {} new @{text}",
                    crate::doctor::me()
                ),
            );
        }
        For::AddSetting(ref prefix) => {
            let Some((name, val)) = text.split_once('=') else {
                i.err = Some("name=value".into());
                d.mode = super::Mode::Input(i);
                return None;
            };
            let (name, val) = (name.trim(), val.trim());
            if name.is_empty() || name.contains(char::is_whitespace) || val.is_empty() {
                i.err = Some("name=value (the name without spaces)".into());
                d.mode = super::Mode::Input(i);
                return None;
            }
            let key = format!("{prefix}{name}");
            super::settings::save(d, &key, val);
        }
        For::SavePresetFrom(ref v) => {
            if !names::valid(&text) {
                i.err = Some("lowercase letters, digits, - and _ (start with a letter)".into());
                d.mode = super::Mode::Input(i);
                return None;
            }
            let p = crate::presets::from_sessions(&text, v);
            match crate::presets::save(&mut d.cfg, &p) {
                Ok(()) => d.toast(
                    Level::Ok,
                    format!(
                        "saved @{text} — {} session{}; P starts them again",
                        v.len(),
                        if v.len() == 1 { "" } else { "s" }
                    ),
                ),
                Err(e) => d.error(format!("couldn't save the preset: {e}")),
            }
            d.fv.marked.clear();
        }
        For::Note(ref host, ref name) => match crate::notes::set(host, name, &text) {
            Ok(()) if text.is_empty() => d.note(format!("cleared the note on {name}")),
            Ok(()) => d.note(format!("noted on {name}")),
            Err(e) => d.error(format!("couldn't save the note: {e}")),
        },
        For::QueueTo(ref host, ref name) => {
            if text.is_empty() {
                return None;
            }
            crate::queue::add(crate::queue::Task {
                host: host.clone(),
                prompt: text,
                to: Some(name.clone()),
                ..Default::default()
            });
            d.note(format!("queued for {name} — sent once it's free again"));
        }
    }
    None
}

// ------------------------------------------------------------------ new session

/// Agents the form cycles through: the registry, then a plain shell.
fn agents_list() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = crate::agents::IDS.to_vec();
    v.push(crate::agents::SHELL);
    v
}

pub(crate) struct NewForm {
    host: usize,
    hosts: Vec<String>,
    folder: Field,
    name: Field,
    agent: usize,
    prompt: Field,
    worktree: bool,
    tile: usize,
    at: usize,
    recent: Vec<String>,
    sugg: usize,
    err: Option<String>,
    /// start detached into this pane (Focus / Tile) instead of attaching
    pub into_pane: Option<u32>,
    /// which agents each machine has: the dashboard's watch
    /// ([`AgentWatch::current`]), or the form's own
    watch: AgentWatch,
    /// the "isn't installed" card is up
    card: bool,
    /// Set when the user picks "install it" on the card: (agent id, host).
    /// `key` then returns [`Res::Submit`]; the dashboard reads this first and,
    /// when it's set, runs `aleph agents install <agent> --host <host>` in the
    /// terminal ([`crate::agentcmd::install_flow`], or [`NewForm::install_argv`])
    /// instead of calling [`submit_new`]. Nothing has been started.
    pub install_request: Option<(String, String)>,
    /// an OpenRouter model (its id, or what's typed to find one) for the
    /// agents that take one at launch (`openrouter::MODEL_ARGS`); empty:
    /// the agent's own default
    model: Field,
    /// the highlighted match under it (0: none, what's typed stands)
    msel: usize,
    /// OpenRouter's models, read once off the UI thread (a day's cache)
    models: crate::openrouter::ModelList,
    /// which of them it offers (`openrouter.models`: all, free, paid)
    or_models: String,
    /// what's typed on the agent field to find one
    aq: Field,
}

// the fields, in order (the model only shows for an agent that takes one)
const AT_HOST: usize = 0;
const AT_FOLDER: usize = 1;
const AT_NAME: usize = 2;
const AT_AGENT: usize = 3;
const AT_MODEL: usize = 4;
const AT_PROMPT: usize = 5;
const AT_WORKTREE: usize = 6;
const AT_TILE: usize = 7;
const NF_FIELDS: usize = 8;
/// Model matches shown under the field.
const MODEL_ROWS: usize = 6;
/// Agent matches shown under the field.
const AGENT_ROWS: usize = 7;

impl NewForm {
    pub fn new(d: &Dash, host: &str, dir: Option<&str>) -> NewForm {
        let hosts: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
        let def = crate::agents::default_agent_for(&d.cfg, host).id;
        let watch = AgentWatch::current().unwrap_or_else(|| AgentWatch::standalone(&[]));
        let mut nf = NewForm::with(hosts, host, dir, def, watch);
        nf.or_models = d.cfg.get_or("openrouter.models");
        nf
    }
    fn with(
        hosts: Vec<String>,
        host: &str,
        dir: Option<&str>,
        agent: &str,
        watch: AgentWatch,
    ) -> NewForm {
        let hi = hosts.iter().position(|h| h == host).unwrap_or(0);
        let agent = agents_list().iter().position(|a| *a == agent).unwrap_or(0);
        let mut nf = NewForm {
            host: hi,
            hosts,
            folder: Field::new(dir.unwrap_or("")),
            name: Field::default(),
            agent,
            prompt: Field::default(),
            worktree: false,
            tile: 1,
            at: 1,
            recent: vec![],
            sugg: 0,
            err: None,
            into_pane: None,
            watch,
            card: false,
            install_request: None,
            model: Field::default(),
            msel: 0,
            models: crate::openrouter::ModelList::default(),
            or_models: "all".into(),
            aq: Field::default(),
        };
        nf.load_recent();
        nf.watch.ensure(&nf.host());
        nf.want_models();
        nf
    }
    /// Can the chosen agent take an OpenRouter model?
    fn has_model(&self) -> bool {
        crate::openrouter::takes_model(self.agent_id())
    }
    /// Read OpenRouter's models once the agent can use one.
    fn want_models(&self) {
        if self.has_model() {
            self.models.ensure();
        }
    }
    /// Is field `i` shown (the model only for an agent that takes one)?
    fn shown(&self, i: usize) -> bool {
        i != AT_MODEL || self.has_model()
    }
    /// The next (or previous) field that's shown.
    fn step_field(&mut self, fwd: bool) {
        for _ in 0..NF_FIELDS {
            self.at = if fwd {
                (self.at + 1) % NF_FIELDS
            } else {
                (self.at + NF_FIELDS - 1) % NF_FIELDS
            };
            if self.shown(self.at) {
                return;
            }
        }
    }
    /// The models matching what's typed, best first (at most `n`), of the
    /// ones `openrouter.models` lets through.
    fn model_matches(&self, n: usize) -> Vec<crate::openrouter::Model> {
        match self.models.get() {
            Some(Ok(v)) => crate::openrouter::search(&v, &self.model.text)
                .into_iter()
                .filter(|m| self.or_ok(m))
                .take(n)
                .cloned()
                .collect(),
            _ => vec![],
        }
    }
    fn or_ok(&self, m: &crate::openrouter::Model) -> bool {
        match self.or_models.as_str() {
            "free" => crate::openrouter::is_free(m),
            "paid" => !crate::openrouter::is_free(m),
            _ => true,
        }
    }
    /// Take the highlighted model into the field.
    fn take_model(&mut self) {
        if self.at == AT_MODEL && self.msel > 0 {
            if let Some(m) = self.model_matches(MODEL_ROWS).get(self.msel - 1) {
                self.model = Field::new(&m.id);
            }
            self.msel = 0;
        }
    }
    /// The model to start on (None: the agent's own).
    pub fn chosen_model(&self) -> Option<String> {
        let m = self.model.text.trim();
        (self.has_model() && !m.is_empty()).then(|| m.to_string())
    }
    /// Use this watch (the dashboard's) for "is the agent installed there".
    pub fn set_watch(&mut self, w: AgentWatch) {
        self.watch = w;
        self.watch.ensure(&self.host());
    }
    pub fn set_agent(&mut self, a: &str) {
        if let Some(i) = agents_list().iter().position(|x| *x == a) {
            self.agent = i;
            self.want_models();
        }
    }
    pub fn set_host(&mut self, h: &str) {
        if let Some(i) = self.hosts.iter().position(|x| x == h) {
            self.host = i;
            self.load_recent();
            self.watch.ensure(h);
        }
    }
    pub fn set_prompt(&mut self, p: &str) {
        self.prompt = Field::new(p);
    }
    pub fn set_worktree(&mut self, on: bool) {
        self.worktree = on;
    }
    /// `claude` arguments for [`install_request`](Self::install_request):
    /// `agents install <agent> --host <host>`.
    pub fn install_argv(&self) -> Option<Vec<String>> {
        let (a, h) = self.install_request.as_ref()?;
        Some(
            ["agents", "install", a.as_str(), "--host", h.as_str()]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    }
    pub(crate) fn host(&self) -> String {
        self.hosts.get(self.host).cloned().unwrap_or_default()
    }
    pub(crate) fn agent_id(&self) -> &'static str {
        agents_list()[self.agent]
    }
    /// The agents matching what's typed, in the order to offer them: the
    /// ones installed on the machine first, then a plain shell, then the
    /// ones still being checked, then those it doesn't have; within each,
    /// the best match first (the registry's order when nothing is typed).
    fn agent_matches(&self) -> Vec<&'static str> {
        let q = self.aq.text.trim();
        let host = self.host();
        let mut v: Vec<(u8, i32, usize, &'static str)> = agents_list()
            .into_iter()
            .enumerate()
            .filter_map(|(i, id)| {
                let label = if id == crate::agents::SHELL {
                    "shell".to_string()
                } else {
                    crate::agents::label(id)
                };
                let score = [label.as_str(), id]
                    .iter()
                    .filter_map(|c| crate::fuzzy::score(q, c).map(|x| x.0))
                    .max()?;
                let rank = if id == crate::agents::SHELL {
                    1
                } else {
                    match self.watch.agent(&host, id) {
                        Avail::Installed(_) => 0,
                        Avail::Missing { .. } => 3,
                        _ => 2,
                    }
                };
                Some((rank, -score, i, id))
            })
            .collect();
        v.sort();
        v.into_iter().map(|x| x.3).collect()
    }
    /// Choose an agent (and what its model field needs).
    fn pick_agent(&mut self, id: &str) {
        if let Some(i) = agents_list().iter().position(|x| *x == id) {
            self.agent = i;
            self.want_models();
        }
    }
    /// Is the chosen agent on the chosen machine? None for a plain shell.
    fn avail(&self) -> Option<Avail> {
        let id = self.agent_id();
        (id != crate::agents::SHELL).then(|| self.watch.agent(&self.host(), id))
    }
    fn load_recent(&mut self) {
        self.recent = history::recent_dirs(Some(&self.host()))
            .into_iter()
            .map(|r| r.dir)
            .take(30)
            .collect();
        // this computer: also the folders agents worked in outside Aleph
        if crate::sshcfg::is_local(&self.host()) {
            for d in crate::moshi::recent_folders() {
                if !self.recent.contains(&d) {
                    self.recent.push(d);
                }
            }
        }
        self.sugg = 0;
    }
    fn suggestions(&self) -> Vec<&String> {
        let q = self.folder.text.trim();
        let mut v: Vec<(i32, &String)> = self
            .recent
            .iter()
            .filter_map(|r| crate::fuzzy::score(q, r).map(|(s, _)| (s, r)))
            .collect();
        if !q.is_empty() {
            v.sort_by_key(|x| -x.0);
        }
        v.into_iter().map(|x| x.1).take(5).collect()
    }
    /// Keys while the "isn't installed" card is up.
    fn card_key(&mut self, k: KeyEvent) -> Res {
        match k.code {
            KeyCode::Char('i') | KeyCode::Char('I') => {
                self.card = false;
                self.install_request = Some((self.agent_id().to_string(), self.host()));
                Res::Submit
            }
            // it may be somewhere we didn't look: start it anyway
            KeyCode::Enter => {
                self.card = false;
                Res::Submit
            }
            KeyCode::Esc => {
                self.card = false;
                Res::Keep
            }
            _ => Res::Keep,
        }
    }
    pub fn key(&mut self, k: KeyEvent, _fleet: &Fleet) -> Res {
        self.err = None;
        if self.card {
            return self.card_key(k);
        }
        match k.code {
            KeyCode::Esc => return Res::Cancel,
            KeyCode::Down if self.at == AT_FOLDER && !self.suggestions().is_empty() => {
                self.sugg = (self.sugg + 1).min(self.suggestions().len());
                return Res::Keep;
            }
            KeyCode::Up if self.at == AT_FOLDER && self.sugg > 0 => {
                self.sugg -= 1;
                return Res::Keep;
            }
            // a search's matches, best first: ↓ ↑ move through them (choosing
            // as they go); with nothing typed they move between fields
            KeyCode::Down | KeyCode::Up if self.at == AT_AGENT && !self.aq.text.is_empty() => {
                let m = self.agent_matches();
                let now = m.iter().position(|a| *a == self.agent_id());
                let next = match (now, k.code == KeyCode::Down) {
                    (Some(i), true) if i + 1 < m.len() => Some(i + 1),
                    (Some(i), false) if i > 0 => Some(i - 1),
                    (None, _) => Some(0),
                    _ => None,
                };
                match next.and_then(|i| m.get(i)) {
                    Some(a) => self.pick_agent(a),
                    // past either end: the next field, as ↓ ↑ go elsewhere
                    None => {
                        self.aq = Field::default();
                        self.step_field(k.code == KeyCode::Down);
                    }
                }
                return Res::Keep;
            }
            // the model's matches: ↓ ↑ move through them, tab / enter take one
            KeyCode::Down if self.at == AT_MODEL && !self.model_matches(1).is_empty() => {
                self.msel = (self.msel + 1).min(self.model_matches(MODEL_ROWS).len());
                return Res::Keep;
            }
            KeyCode::Up if self.at == AT_MODEL && self.msel > 0 => {
                self.msel -= 1;
                return Res::Keep;
            }
            KeyCode::Tab | KeyCode::Down => {
                self.take_suggestion();
                self.take_model();
                self.aq = Field::default();
                self.step_field(true);
                return Res::Keep;
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.aq = Field::default();
                self.step_field(false);
                return Res::Keep;
            }
            KeyCode::Enter => {
                self.take_suggestion();
                self.take_model();
                // don't start an agent the machine doesn't have: offer to install it
                if matches!(self.avail(), Some(Avail::Missing { .. })) {
                    self.card = true;
                    return Res::Keep;
                }
                return Res::Submit;
            }
            _ => {}
        }
        let left = k.code == KeyCode::Left;
        // (space types on the agent field once a search has started)
        let right = k.code == KeyCode::Right
            || (k.code == KeyCode::Char(' ') && !(self.at == AT_AGENT && !self.aq.text.is_empty()));
        match self.at {
            AT_HOST if left || right => {
                let n = self.hosts.len().max(1);
                self.host = if left {
                    (self.host + n - 1) % n
                } else {
                    (self.host + 1) % n
                };
                self.load_recent();
                self.watch.ensure(&self.host());
            }
            AT_FOLDER => {
                if self.folder.key(&k) {
                    self.sugg = 0;
                }
            }
            AT_NAME => {
                self.name.key(&k);
            }
            // ←→ step through the same order the search offers (installed first)
            AT_AGENT if left || right => {
                let m = self.agent_matches();
                if !m.is_empty() {
                    let i = m.iter().position(|a| *a == self.agent_id()).unwrap_or(0);
                    let n = m.len();
                    let a = m[if left { (i + n - 1) % n } else { (i + 1) % n }];
                    self.pick_agent(a);
                }
            }
            // typing searches; the best match is chosen as you type
            AT_AGENT => {
                if self.aq.key(&k) {
                    if let Some(a) = self.agent_matches().first().copied() {
                        self.pick_agent(a);
                    }
                }
            }
            AT_MODEL => {
                if self.model.key(&k) {
                    self.msel = 0;
                }
            }
            AT_PROMPT => {
                self.prompt.key(&k);
            }
            AT_WORKTREE if left || right => self.worktree = !self.worktree,
            AT_TILE if left => self.tile = self.tile.saturating_sub(1).max(1),
            AT_TILE if right => self.tile = (self.tile + 1).min(9),
            AT_TILE => {
                if let KeyCode::Char(ch @ '1'..='9') = k.code {
                    self.tile = ch as usize - '0' as usize;
                }
            }
            _ => {}
        }
        Res::Keep
    }
    fn take_suggestion(&mut self) {
        if self.at == AT_FOLDER && self.sugg > 0 {
            if let Some(s) = self.suggestions().get(self.sugg - 1).map(|s| s.to_string()) {
                self.folder = Field::new(&s);
            }
            self.sugg = 0;
        }
    }
    /// `✓ 0.46.0` · `✗ not installed` · `✻ checking…` next to the agent.
    fn agent_status(&self, t0: Instant) -> Vec<Span<'static>> {
        let Some(a) = self.avail() else {
            return vec![Span::styled("  no agent, just your shell", ui::faint())];
        };
        match a {
            Avail::Checking => vec![
                Span::styled(format!("  {} ", ui::spinner(t0)), ui::fg(c::ACCENT())),
                Span::styled("checking…", ui::faint()),
            ],
            Avail::Installed(f) => {
                let mut v = vec![
                    Span::styled("  ✓ ", ui::fg(c::GREEN()).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        if f.version.is_empty() {
                            "installed".to_string()
                        } else {
                            f.version.clone()
                        },
                        ui::muted(),
                    ),
                ];
                if f.shadowed_by.is_some() {
                    v.push(Span::styled(
                        "  ⚠ another program comes first on the PATH",
                        ui::fg(c::YELLOW()),
                    ));
                } else if f.on_path == Some(false) {
                    v.push(Span::styled("  ⚠ not on the PATH", ui::fg(c::YELLOW())));
                }
                v
            }
            Avail::Missing { broken: Some(_) } => vec![Span::styled(
                "  ✗ installed but doesn't run",
                ui::fg(c::RED()),
            )],
            Avail::Missing { broken: None } => {
                vec![Span::styled("  ✗ not installed", ui::fg(c::RED()))]
            }
            Avail::Unknown(_) => vec![Span::styled("  ? couldn't check", ui::faint())],
        }
    }
    /// The card under the fields: "Codex isn't installed on b31", how it
    /// would install, and the keys.
    fn card_lines(&self, w: usize) -> Vec<Line<'static>> {
        let id = self.agent_id();
        let host = self.host();
        let name = crate::agents::label(id);
        let on = if crate::sshcfg::is_local(&host) {
            "this machine".to_string()
        } else {
            host.clone()
        };
        let broken = match self.avail() {
            Some(Avail::Missing { broken }) => broken,
            _ => None,
        };
        let bar = || Span::styled("┃ ", ui::fg(c::RED()));
        let mut v = vec![Line::from(vec![
            bar(),
            Span::styled("✗ ", ui::fg(c::RED()).add_modifier(Modifier::BOLD)),
            Span::styled(
                match &broken {
                    Some(_) => format!("{name} doesn't run on {on}"),
                    None => format!("{name} isn't installed on {on}"),
                },
                ui::bold(),
            ),
        ])];
        if let Some(why) = broken {
            v.push(Line::from(vec![
                bar(),
                Span::styled(ui::trunc(&why, w.saturating_sub(4)), ui::faint()),
            ]));
        }
        let plan = crate::agents::find(id).and_then(|a| {
            self.watch
                .probe(&host)
                .map(|p| crate::agentinstall::plan(a, &p))
        });
        let (how, detail) = match &plan {
            Some(p) if p.ready() => ("install it", p.command.clone().unwrap_or_default()),
            Some(p) if p.command.is_some() => (
                "install it",
                format!(
                    "{} — {}",
                    p.command.clone().unwrap_or_default(),
                    p.prereqs
                        .iter()
                        .find(|x| !x.ok)
                        .map(|x| format!("needs {} first", x.what))
                        .unwrap_or_else(|| "it explains what's missing".into())
                ),
            ),
            Some(_) => (
                "how to install it",
                format!(
                    "by hand: {}",
                    crate::agents::find(id).map_or("", |a| a.install)
                ),
            ),
            None => ("install it", String::new()),
        };
        if !detail.is_empty() {
            v.push(Line::from(vec![
                bar(),
                Span::styled(ui::trunc(&detail, w.saturating_sub(4)), ui::muted()),
            ]));
        }
        let mut keys = ui::hints(&[("i", how), ("enter", "start anyway"), ("esc", "cancel")]);
        keys.spans.insert(0, bar());
        v.push(keys);
        v
    }
    pub fn draw(&self, d: &Dash, f: &mut Frame, area: Rect) {
        self.render(&d.cfg, d.t0, f, area);
    }
    /// The agent search under the field while it's being edited: matches
    /// (installed first), each with whether the machine has it.
    fn agent_lines(&self, w: usize) -> Vec<Line<'static>> {
        let pad = "            ";
        let host = self.host();
        let m = self.agent_matches();
        let mut v = vec![];
        if m.is_empty() {
            v.push(Line::from(vec![
                Span::raw(pad),
                Span::styled("no agent matches — backspace widens it", ui::faint()),
            ]));
            return v;
        }
        let cur = self.agent_id();
        let at = m.iter().position(|a| *a == cur).unwrap_or(0);
        let start = (at + 1).saturating_sub(AGENT_ROWS);
        let room = w.saturating_sub(pad.len() + 4 + 22).max(8);
        for id in m.iter().skip(start).take(AGENT_ROWS) {
            let sel = *id == cur;
            let shell = *id == crate::agents::SHELL;
            let (state, sst, have) = if shell {
                ("your shell".to_string(), ui::faint(), true)
            } else {
                match self.watch.agent(&host, id) {
                    Avail::Installed(f) => (
                        format!(
                            "✓ {}",
                            if f.version.is_empty() {
                                "installed".to_string()
                            } else {
                                f.version.clone()
                            }
                        ),
                        ui::fg(c::GREEN()),
                        true,
                    ),
                    Avail::Missing { broken: Some(_) } => {
                        ("✗ doesn't run".to_string(), ui::fg(c::RED()), false)
                    }
                    Avail::Missing { broken: None } => (
                        format!(
                            "not on {}",
                            if crate::sshcfg::is_local(&host) {
                                "this machine"
                            } else {
                                host.as_str()
                            }
                        ),
                        ui::faint(),
                        false,
                    ),
                    Avail::Checking => ("checking…".to_string(), ui::faint(), true),
                    Avail::Unknown(_) => ("? couldn't check".to_string(), ui::faint(), true),
                }
            };
            let name = if shell {
                "shell".to_string()
            } else {
                crate::agents::label(id)
            };
            let nst = match (sel, have) {
                (true, _) => ui::bold(),
                (false, true) => Style::new().fg(c::TEXT()),
                (false, false) => ui::faint(),
            };
            v.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(if sel { "› " } else { "  " }, ui::fg(c::ACCENT())),
                crate::icons::agent(id),
                Span::raw(" "),
                Span::styled(ui::pad(&ui::trunc(&name, 20), 21), nst),
                Span::styled(ui::trunc(&state, room), sst),
            ]));
        }
        let more = m.len().saturating_sub(start + AGENT_ROWS);
        if more > 0 {
            v.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(format!("  … {more} more — type to narrow"), ui::faint()),
            ]));
        }
        v
    }
    /// The model field and, while it's being edited, OpenRouter's models
    /// that match what's typed, with their context length and $ per
    /// million tokens in / out.
    fn model_lines(
        &self,
        cfg: &crate::config::Config,
        t0: Instant,
        w: usize,
    ) -> Vec<Line<'static>> {
        let on = self.at == AT_MODEL;
        let mut l = vec![
            Span::styled(if on { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(
                format!("{:<10}", "model"),
                if on {
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                } else {
                    ui::muted()
                },
            ),
        ];
        l.extend(
            self.model
                .spans(on, "the agent's own — type to pick an OpenRouter model"),
        );
        // when this session won't get the key, say so (the setting alone)
        let id = self.agent_id();
        if !crate::openrouter::configured(cfg) {
            l.push(Span::styled("  no OpenRouter key here", ui::faint()));
        } else if !crate::openrouter::wants(cfg, id) {
            l.push(Span::styled(
                "  not in openrouter.agents: no key",
                ui::fg(c::YELLOW()),
            ));
        }
        let mut v = vec![Line::from(l)];
        if !on {
            return v;
        }
        let pad = "            ";
        let inner = w.saturating_sub(4);
        match self.models.get() {
            None => v.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(format!("{} ", ui::spinner(t0)), ui::fg(c::ACCENT())),
                Span::styled("reading OpenRouter's models…", ui::faint()),
            ])),
            Some(Err(e)) => v.push(Line::from(vec![
                Span::raw(pad),
                Span::styled(
                    ui::trunc(
                        &format!("no model list ({e}) — type an id"),
                        inner.saturating_sub(12),
                    ),
                    ui::faint(),
                ),
            ])),
            Some(Ok(all)) => {
                let mut found = crate::openrouter::search(&all, &self.model.text);
                found.retain(|m| self.or_ok(m));
                if found.is_empty() {
                    v.push(Line::from(vec![
                        Span::raw(pad),
                        Span::styled(
                            "no model matches — what's typed is used as it is",
                            ui::faint(),
                        ),
                    ]));
                }
                for (i, m) in found.iter().take(MODEL_ROWS).enumerate() {
                    let sel = self.msel == i + 1;
                    let facts = m.facts();
                    let room = inner
                        .saturating_sub(pad.len() + 2 + ui::width(&facts) + 1)
                        .max(12);
                    v.push(Line::from(vec![
                        Span::raw(pad),
                        Span::styled(if sel { "› " } else { "  " }, ui::fg(c::ACCENT())),
                        Span::styled(
                            ui::pad(&ui::trunc(&m.id, room), room),
                            if sel {
                                ui::bold()
                            } else {
                                Style::new().fg(c::TEXT())
                            },
                        ),
                        Span::raw(" "),
                        Span::styled(facts, if sel { ui::muted() } else { ui::faint() }),
                    ]));
                }
                if found.len() > MODEL_ROWS {
                    v.push(Line::from(vec![
                        Span::raw(pad),
                        Span::styled(
                            format!(
                                "  … {} more — type to narrow · ↓ picks",
                                found.len() - MODEL_ROWS
                            ),
                            ui::faint(),
                        ),
                    ]));
                }
            }
        }
        v
    }
    fn render(&self, cfg: &crate::config::Config, t0: Instant, f: &mut Frame, area: Rect) {
        let w = 74u16.min(area.width.saturating_sub(2));
        let host = self.host();
        let hcol = crate::theme::machine(cfg, &host);
        let label = |i: usize, s: &str| {
            Span::styled(
                format!("{:<10}", s),
                if self.at == i {
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                } else {
                    ui::muted()
                },
            )
        };
        let bar =
            |i: usize| Span::styled(if self.at == i { "┃ " } else { "  " }, ui::fg(c::ACCENT()));
        let cyc = |i: usize, v: Vec<Span<'static>>| {
            let mut l = vec![bar(i), label(i, "")];
            l.pop();
            l.extend(v);
            if self.at == i {
                l.push(Span::styled("   ←/→", ui::faint()));
            }
            l
        };
        let mut lines: Vec<Line> = Vec::new();
        let mut l0 = vec![bar(0), label(0, "machine")];
        l0.extend(
            cyc(
                0,
                vec![
                    Span::styled("▌", ui::fg(hcol)),
                    Span::styled(host.clone(), ui::fg(hcol).add_modifier(Modifier::BOLD)),
                ],
            )
            .into_iter()
            .skip(1),
        );
        lines.push(Line::from(l0));
        let mut l1 = vec![bar(1), label(1, "folder")];
        l1.extend(self.folder.spans(
            self.at == 1,
            if crate::sshcfg::is_local(&host) {
                "this folder, or a path"
            } else {
                "herdr's current folder, or a path"
            },
        ));
        lines.push(Line::from(l1));
        if self.at == 1 {
            for (i, s) in self.suggestions().iter().enumerate() {
                let on = self.sugg == i + 1;
                lines.push(Line::from(vec![
                    Span::raw("            "),
                    Span::styled(if on { "› " } else { "  " }, ui::fg(c::ACCENT())),
                    Span::styled(
                        ui::trunc_left(s, w as usize - 20),
                        if on { ui::bold() } else { ui::faint() },
                    ),
                ]));
            }
        }
        let mut l2 = vec![bar(2), label(2, "name")];
        l2.extend(
            self.name
                .spans(self.at == 2, "auto (a fresh, distinct name)"),
        );
        lines.push(Line::from(l2));
        let id = self.agent_id();
        let mut l3 = vec![bar(3), label(3, "agent")];
        let mut av = vec![
            crate::icons::agent(id),
            Span::raw(" "),
            Span::styled(
                if id == crate::agents::SHELL {
                    "shell".to_string()
                } else {
                    crate::agents::label(id)
                },
                ui::bold(),
            ),
        ];
        av.extend(self.agent_status(t0));
        l3.extend(cyc(3, av).into_iter().skip(1));
        if self.at == AT_AGENT {
            // the search, typed after it
            l3.push(Span::styled("   ⌕ ", ui::fg(c::ACCENT())));
            l3.extend(self.aq.spans(true, "type to search"));
        }
        lines.push(Line::from(l3));
        if self.at == AT_AGENT {
            lines.extend(self.agent_lines(w as usize));
        }
        if self.has_model() {
            lines.extend(self.model_lines(cfg, t0, w as usize));
        }
        let mut l4 = vec![bar(AT_PROMPT), label(AT_PROMPT, "prompt")];
        l4.extend(
            self.prompt
                .spans(self.at == AT_PROMPT, "optional first prompt"),
        );
        lines.push(Line::from(l4));
        let mut l5 = vec![bar(AT_WORKTREE), label(AT_WORKTREE, "worktree")];
        l5.extend(
            cyc(
                AT_WORKTREE,
                vec![Span::styled(
                    if self.worktree {
                        "☑ new git worktree"
                    } else {
                        "☐ work in the folder"
                    },
                    if self.worktree {
                        ui::fg(c::GREEN())
                    } else {
                        ui::muted()
                    },
                )],
            )
            .into_iter()
            .skip(1),
        );
        lines.push(Line::from(l5));
        let mut l6 = vec![bar(AT_TILE), label(AT_TILE, "agents")];
        l6.extend(
            cyc(
                AT_TILE,
                vec![Span::styled(
                    format!(
                        "{} {}",
                        self.tile,
                        if self.tile == 1 {
                            "(one tab)"
                        } else {
                            "tiled in one tab"
                        }
                    ),
                    ui::bold(),
                )],
            )
            .into_iter()
            .skip(1),
        );
        lines.push(Line::from(l6));
        lines.push(Line::raw(""));
        if self.card {
            lines.extend(self.card_lines(w as usize));
        } else {
            lines.push(match &self.err {
                Some(e) => Line::from(vec![
                    Span::styled("error: ", ui::fg(c::RED()).add_modifier(Modifier::BOLD)),
                    Span::raw(e.clone()),
                ]),
                None if self.at == AT_MODEL => ui::hints(&[
                    ("type", "search"),
                    ("↓", "pick"),
                    ("tab", "take · next"),
                    ("enter", "start"),
                    ("esc", "cancel"),
                ]),
                None if self.at == AT_AGENT => ui::hints(&[
                    ("type", "search"),
                    (
                        if self.aq.text.is_empty() {
                            "←→"
                        } else {
                            "↑↓ ←→"
                        },
                        "pick",
                    ),
                    ("tab", "next"),
                    ("enter", "start"),
                    ("esc", "cancel"),
                ]),
                None => ui::hints(&[
                    ("tab", "next"),
                    ("←→", "change"),
                    ("enter", "start"),
                    ("esc", "cancel"),
                ]),
            });
        }
        let r = ui::centered(area, w, lines.len() as u16 + 2);
        crate::ui::clear(f, r);
        let block = ui::panel("New session", true).border_style(ui::fg(hcol));
        f.render_widget(Paragraph::new(lines).block(block), r);
    }
}

pub(crate) fn submit_new(d: &mut Dash, mut nf: NewForm) -> Option<Handoff> {
    if let Some((agent, host)) = nf.install_request.take() {
        // the guided install runs on the real terminal, then the form comes back
        let dir = Some(nf.folder.text.trim().to_string()).filter(|x| !x.is_empty());
        return Some(Handoff::Install { agent, host, dir });
    }
    let host = nf.host();
    let name = nf.name.text.trim().to_string();
    if !name.is_empty() && !names::valid(&name) {
        nf.err = Some("names are lowercase letters, digits, - and _".into());
        nf.at = AT_NAME;
        d.mode = super::Mode::NewForm(nf);
        return None;
    }
    let mut o = match cli::parse_new(&[], &d.cfg) {
        Ok(x) => x.opts,
        Err(e) => {
            d.error(e);
            return None;
        }
    };
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = match nf.folder.text.trim() {
        "" if crate::sshcfg::is_local(&host) => cwd.as_str(),
        d => d,
    };
    if !dir.is_empty() {
        let dir = if let Some(rest) = dir.strip_prefix("~/") {
            let user = d
                .fleet
                .host(&host)
                .map(|h| h.user.clone())
                .unwrap_or_default();
            if crate::sshcfg::is_local(&host) {
                crate::config::home()
                    .join(rest)
                    .to_string_lossy()
                    .into_owned()
            } else if user.is_empty() {
                dir.to_string()
            } else {
                format!("/home/{user}/{rest}")
            }
        } else {
            dir.to_string()
        };
        o.dir = Some(crate::path::normalize(dir.as_bytes()));
    }
    if !name.is_empty() {
        o.name = Some(name);
    }
    o.agent = agents_list()[nf.agent].to_string();
    // an OpenRouter model: the agent's own flag for it, before the rest
    if let Some(m) = nf.chosen_model() {
        match crate::openrouter::model_args(&o.agent, &m) {
            Some(mut a) => {
                a.append(&mut o.args);
                o.args = a;
            }
            None => {
                nf.err = Some(format!("{m}: not a model id (provider/model, no spaces)"));
                nf.at = AT_MODEL;
                d.mode = super::Mode::NewForm(nf);
                return None;
            }
        }
    }
    let p = nf.prompt.text.trim().to_string();
    if !p.is_empty() {
        // `@review`: a saved prompt, filled in for where it starts
        let ctx = crate::prompts::Ctx {
            host: host.clone(),
            dir: o
                .dir
                .as_deref()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default(),
            agent: o.agent.clone(),
            ..Default::default()
        };
        match crate::prompts::expand(&d.cfg, &p, &ctx, &[]) {
            Some(Ok(t)) => o.prompt = Some(t),
            Some(Err(e)) => {
                nf.err = Some(format!("{e} — or write the prompt out"));
                d.mode = super::Mode::NewForm(nf);
                return None;
            }
            None => o.prompt = Some(p),
        }
    }
    if nf.worktree {
        o.worktree = Some(String::new());
    }
    o.tile = nf.tile;
    o.live = d.fleet.live_names();
    cli::fill_names(&mut o);
    if let Some(pane) = nf.into_pane {
        // into a Focus / Tile pane: start it detached, then stream it there
        let what = format!("{} on {host}", crate::agents::label(&o.agent));
        super::wall::starting(d, pane, what);
        let (t, tx) = (d.target_of(&host), d.tx.clone());
        std::thread::spawn(move || {
            let r = crate::herdr::Herdr::new(&t)
                .new_session(&o, &mut |_, _| {})
                .and_then(|v| {
                    crate::launch::after_start(&t, &o, &v);
                    v.first()
                        .map(|s| format!("{}|{}", t.ssh, s.pane))
                        .ok_or_else(|| "nothing started".to_string())
                });
            let _ = tx.send(super::Bg::PaneStarted(pane, r));
        });
        return None;
    }
    super::quick::begin(d, &host, o, false);
    None
}

// ------------------------------------------------------------------ add host

pub(crate) struct HostForm {
    pub f: [Field; 4],
    /// editing this existing machine (its alias stays) instead of adding one
    pub editing: Option<String>,
    pub at: usize,
    pub err: Option<String>,
    /// where adding it stands: editing, testing the connection, or asking
    pub stage: HostStage,
    /// drops results of a test that was cancelled or redone
    pub gen: u64,
}

/// The add-a-machine form, after enter: try it before saving it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HostStage {
    Edit,
    Testing(Instant),
    /// first contact: show the server's key fingerprints, trust them or not
    NewKey {
        fingerprints: Vec<String>,
        lines: Vec<String>,
    },
    /// it answered, but no key of ours is accepted (password only)
    NeedKey,
    Failed(String),
}

const HF: [(&str, &str); 4] = [
    ("Alias", "a short name, e.g. gpu2"),
    ("Connects to", "user@hostname[:port]"),
    ("Identity file", "optional, e.g. ~/.ssh/id_ed25519"),
    (
        "Jump via",
        "optional: reach it through another machine (ProxyJump), e.g. bastion",
    ),
];
const NF: usize = HF.len();

impl HostForm {
    pub fn new() -> HostForm {
        HostForm {
            f: Default::default(),
            editing: None,
            at: 0,
            err: None,
            stage: HostStage::Edit,
            gen: 0,
        }
    }

    /// The form for an existing machine, filled from its ssh config.
    pub fn edit(alias: &str) -> HostForm {
        let mut hf = HostForm::new();
        hf.editing = Some(alias.to_string());
        hf.f[0] = Field::new(alias);
        if let Some(r) = crate::sshcfg::resolve(alias) {
            let mut t = String::new();
            if !r.user.is_empty() {
                t.push_str(&r.user);
                t.push('@');
            }
            t.push_str(&r.hostname);
            if !r.port.is_empty() && r.port != "22" {
                t.push(':');
                t.push_str(&r.port);
            }
            hf.f[1] = Field::new(&t);
            if let Some(id) = r
                .identities
                .first()
                .filter(|i| !i.contains("id_rsa") || r.identities.len() == 1)
            {
                hf.f[2] = Field::new(id);
            }
            if let Some(j) = r.proxy.as_deref().filter(|j| !j.is_empty() && *j != "none") {
                hf.f[3] = Field::new(j.trim_start_matches("ProxyJump ").trim());
            }
        }
        hf.at = 1;
        hf
    }
    /// `user@hostname[:port]` as typed (for messages).
    pub fn target(&self) -> String {
        self.f[1].text.trim().to_string()
    }
    pub fn key(&mut self, k: KeyEvent) -> Res {
        match k.code {
            KeyCode::Esc => Res::Cancel,
            KeyCode::Tab | KeyCode::Down => {
                self.at = (self.at + 1) % NF;
                if self.at == 0 && self.editing.is_some() {
                    self.at = 1; // the alias stays when editing
                }
                Res::Keep
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.at = (self.at + NF - 1) % NF;
                if self.at == 0 && self.editing.is_some() {
                    self.at = NF - 1;
                }
                Res::Keep
            }
            KeyCode::Enter if self.at < NF - 1 => {
                self.at += 1;
                Res::Keep
            }
            KeyCode::Enter => Res::Submit,
            _ => {
                self.f[self.at].key(&k);
                self.err = None;
                Res::Keep
            }
        }
    }
    pub fn draw(&self, f: &mut Frame, area: Rect) {
        let w = 78u16.min(area.width.saturating_sub(2));
        let mut lines: Vec<Line> = vec![
            Line::from(Span::styled(
                if self.editing.is_some() {
                    "Tests the connection, then rewrites this machine's Host block in ~/.ssh/config (backed up first)."
                } else {
                    "Tests the connection, then adds a Host block to ~/.ssh/config (backed up first) and joins the fleet."
                },
                ui::muted(),
            )),
            Line::raw(""),
        ];
        for (i, (label, ph)) in HF.iter().enumerate() {
            let focus = self.at == i;
            lines.push(Line::from(Span::styled(
                label.to_string(),
                if focus {
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD)
                } else {
                    ui::muted()
                },
            )));
            let mut l = vec![Span::styled(
                if focus { "┃ " } else { "│ " },
                ui::fg(if focus { c::ACCENT() } else { c::LINE() }),
            )];
            l.extend(self.f[i].spans(focus, ph));
            lines.push(Line::from(l));
        }
        lines.push(Line::raw(""));
        let target = self.target();
        match &self.stage {
            HostStage::Edit => lines.push(match &self.err {
                Some(e) => Line::from(vec![
                    Span::styled("error: ", ui::fg(c::RED()).add_modifier(Modifier::BOLD)),
                    Span::raw(e.clone()),
                ]),
                None => ui::hints(&[
                    ("tab", "next"),
                    (
                        "enter",
                        match (self.at == NF - 1, self.editing.is_some()) {
                            (true, true) => "test the connection, then save",
                            (true, false) => "test the connection, then add",
                            _ => "next",
                        },
                    ),
                    ("esc", "cancel"),
                ]),
            }),
            HostStage::Testing(t) => {
                lines.push(Line::from(vec![
                    Span::styled(format!("{} ", ui::spinner(*t)), ui::fg(c::ACCENT())),
                    Span::styled(
                        format!("connecting to {target} — key login only, no password prompts…"),
                        ui::muted(),
                    ),
                ]));
                lines.push(ui::hints(&[("esc", "cancel")]));
            }
            HostStage::NewKey { fingerprints, .. } => {
                lines.push(Line::from(Span::styled(
                    format!("First time connecting to {target}. Its host key:"),
                    ui::bold(),
                )));
                for fp in fingerprints {
                    lines.push(Line::from(Span::styled(
                        format!("  {fp}"),
                        ui::fg(c::YELLOW()),
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "Unsure? On that machine: ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub",
                    ui::faint(),
                )));
                lines.push(ui::hints(&[("y", "trust it and connect"), ("n", "don't")]));
            }
            HostStage::NeedKey => {
                lines.push(Line::from(Span::styled(
                    format!("{target} answered, but none of your keys is accepted."),
                    ui::bold(),
                )));
                lines.push(Line::from(Span::styled(
                    "k sets up key login: ssh-copy-id asks the password once, in ssh itself —",
                    ui::muted(),
                )));
                lines.push(Line::from(Span::styled(
                    "aleph never sees or stores it; from then on logins are key-only.",
                    ui::muted(),
                )));
                lines.push(ui::hints(&[
                    ("k", "key login now"),
                    ("s", "save anyway"),
                    ("esc", "edit"),
                ]));
            }
            HostStage::Failed(e) => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "couldn't connect: ",
                        ui::fg(c::RED()).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(e.clone()),
                ]));
                lines.push(ui::hints(&[
                    ("enter", "try again"),
                    ("e", "edit"),
                    ("s", "save anyway"),
                    ("esc", "cancel"),
                ]));
            }
        }
        let title = match &self.editing {
            Some(a) => format!("Edit {a}"),
            None => "Add a machine".to_string(),
        };
        let block = ui::panel(title, true);
        // rows after wrapping, so nothing is cut off at the bottom
        let inner_w = block.inner(Rect::new(0, 0, w, 10)).width.max(1) as usize;
        let rows: usize = lines
            .iter()
            .map(|l| ui::line_width(l).div_ceil(inner_w).max(1))
            .sum();
        let pad = ui::padding();
        let h = (rows as u16 + 2 + pad.top + pad.bottom).min(area.height);
        let r = ui::centered(area, w, h);
        crate::ui::clear(f, r);
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(block),
            r,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use ratatui::{backend::TestBackend, Terminal};

    /// b31: codex installed, curl + bash, no node; gpu2 still being looked at.
    fn watch() -> AgentWatch {
        let b31 = crate::probe::parse(
            "b31",
            "CCT\tcurl\t/usr/bin/curl\nCCT\tbash\t/bin/bash\n\
             CCC\t1\tcodex\t/h/.local/bin/codex\t0\t0\tcodex-cli 0.46.0\n\
             CCC\t2\tqwen\t/h/.local/bin/qwen\t127\t0\tenv: node: No such file or directory\n\
             CCO\tLinux x86_64\n",
        );
        AgentWatch::fixed(vec![b31], &["gpu2"])
    }
    fn form(host: &str, agent: &str) -> NewForm {
        NewForm::with(
            vec!["b31".into(), "gpu2".into()],
            host,
            Some("/srv/x"),
            agent,
            watch(),
        )
    }
    fn screen(nf: &NewForm) -> String {
        let cfg = Config::in_memory(&[]);
        let mut t = Terminal::new(TestBackend::new(90, 26)).unwrap();
        t.draw(|f| nf.render(&cfg, Instant::now(), f, f.area()))
            .unwrap();
        let b = t.backend().buffer().clone();
        (0..b.area.height)
            .map(|y| {
                (0..b.area.width)
                    .map(|x| b[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn press(nf: &mut NewForm, code: KeyCode) -> Res {
        let fleet = Fleet::new(&Config::in_memory(&["hosts=b31,gpu2"]), None);
        nf.key(KeyEvent::new(code, KeyModifiers::NONE), &fleet)
    }

    #[test]
    fn installed_agent_starts() {
        let mut nf = form("b31", "codex");
        let s = screen(&nf);
        let (g, _) = crate::icons::brand("codex");
        assert!(s.contains(&format!("{g} Codex")), "{s}");
        assert!(s.contains("✓ 0.46.0"), "{s}");
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Submit));
        assert!(nf.install_request.is_none() && nf.install_argv().is_none());
    }

    #[test]
    fn the_agent_search_offers_what_the_machine_has_first() {
        let mut nf = form("b31", "claude");
        nf.at = AT_AGENT;
        // b31 has Codex; then a plain shell; then every agent it hasn't got
        let m = nf.agent_matches();
        assert_eq!(&m[..2], &["codex", crate::agents::SHELL]);
        assert_eq!(m.len(), agents_list().len());
        let s = screen(&nf);
        assert!(
            s.contains("⌕") && s.contains("✓ 0.46.0") && s.contains("not on b31"),
            "{s}"
        );
        // typing chooses the best match as it goes
        for ch in "gem".chars() {
            press(&mut nf, KeyCode::Char(ch));
        }
        assert_eq!(nf.agent_id(), "gemini");
        // ↓ moves through the matches while a search is typed
        press(&mut nf, KeyCode::Backspace);
        press(&mut nf, KeyCode::Backspace);
        press(&mut nf, KeyCode::Backspace);
        for ch in "co".chars() {
            press(&mut nf, KeyCode::Char(ch));
        }
        assert_eq!(nf.agent_id(), "codex", "the installed one first");
        press(&mut nf, KeyCode::Down);
        assert_ne!(nf.agent_id(), "codex");
        press(&mut nf, KeyCode::Up);
        assert_eq!(nf.agent_id(), "codex");
        // nothing matches: it says so, and the choice stands
        for ch in "zzzq".chars() {
            press(&mut nf, KeyCode::Char(ch));
        }
        assert!(screen(&nf).contains("no agent matches"));
        assert_eq!(nf.agent_id(), "codex");
        // tab leaves the field (the search cleared), enter starts it
        press(&mut nf, KeyCode::Tab);
        assert!(nf.aq.text.is_empty() && nf.at != AT_AGENT);
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Submit));
        // with nothing typed, ←→ step through the same order
        let mut nf = form("b31", "codex");
        nf.at = AT_AGENT;
        press(&mut nf, KeyCode::Right);
        assert_eq!(nf.agent_id(), crate::agents::SHELL);
        press(&mut nf, KeyCode::Left);
        assert_eq!(nf.agent_id(), "codex");
    }

    #[test]
    fn missing_agent_offers_install() {
        let mut nf = form("b31", "claude");
        assert!(screen(&nf).contains("✗ not installed"));
        // enter doesn't start it: the card comes up
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Keep));
        let s = screen(&nf);
        assert!(s.contains("Claude Code isn't installed on b31"), "{s}");
        assert!(
            s.contains("curl -fsSL https://claude.ai/install.sh | bash"),
            "{s}"
        );
        assert!(s.contains(" i  install it"), "{s}");
        assert!(s.contains("start anyway"), "{s}");
        // i: hand the install to the dashboard
        assert!(matches!(press(&mut nf, KeyCode::Char('i')), Res::Submit));
        assert_eq!(
            nf.install_request,
            Some(("claude".to_string(), "b31".to_string()))
        );
        assert_eq!(
            nf.install_argv().unwrap(),
            vec!["agents", "install", "claude", "--host", "b31"]
        );
    }

    #[test]
    fn card_esc_and_start_anyway() {
        let mut nf = form("b31", "gemini");
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Keep));
        let s = screen(&nf);
        // npm-based, and b31 has no node: say so up front
        assert!(s.contains("npm i -g @google/gemini-cli"), "{s}");
        assert!(s.contains("needs Node.js ≥ 20 first"), "{s}");
        // esc closes the card, not the form
        assert!(matches!(press(&mut nf, KeyCode::Esc), Res::Keep));
        assert!(!screen(&nf).contains("isn't installed"));
        // other keys don't leak into the form while the card is up
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Keep));
        assert!(matches!(press(&mut nf, KeyCode::Char('x')), Res::Keep));
        // enter on the card: start it anyway
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Submit));
        assert!(nf.install_request.is_none());
    }

    #[test]
    fn broken_manual_checking_and_shell() {
        // installed but doesn't run
        let mut nf = form("b31", "qwen");
        assert!(screen(&nf).contains("installed but doesn't run"));
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Keep));
        let s = screen(&nf);
        assert!(s.contains("Qwen Code doesn't run on b31"), "{s}");
        assert!(s.contains("No such file"), "{s}");
        // not installed: its install command, and i installs it
        let mut nf = form("b31", "grok");
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Keep));
        let s = screen(&nf);
        assert!(
            s.contains("isn't installed on b31")
                && s.contains("curl -fsSL https://x.ai/cli/install.sh | bash")
                && s.contains("install it"),
            "{s}"
        );
        // still checking: don't hold the user up
        let mut nf = form("gpu2", "claude");
        assert!(screen(&nf).contains("checking…"));
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Submit));
        // a plain shell isn't an agent
        let mut nf = form("b31", "shell");
        assert!(screen(&nf).contains("no agent, just your shell"));
        assert!(matches!(press(&mut nf, KeyCode::Enter), Res::Submit));
    }

    #[test]
    fn openrouter_models_for_the_agents_that_take_one() {
        let models = crate::openrouter::parse_models(
            &serde_json::from_str(include_str!("../../tests/fixtures/openrouter/models.json"))
                .unwrap(),
        );
        // Codex takes no OpenRouter model: no field, tab passes it by
        let mut nf = form("b31", "codex");
        assert!(!screen(&nf).contains("model "));
        nf.at = AT_AGENT;
        press(&mut nf, KeyCode::Tab);
        assert_eq!(nf.at, AT_PROMPT);
        press(&mut nf, KeyCode::BackTab);
        assert_eq!(nf.at, AT_AGENT);
        // OpenCode does: the field, and OpenRouter's models under it
        let mut nf = form("b31", "opencode");
        nf.models = crate::openrouter::ModelList::with(models);
        nf.at = AT_AGENT;
        press(&mut nf, KeyCode::Tab);
        assert_eq!(nf.at, AT_MODEL);
        let s = screen(&nf);
        assert!(
            s.contains("model") && s.contains("type to pick an OpenRouter model"),
            "{s}"
        );
        assert!(
            s.contains("qwen/qwen3-coder") && s.contains("262k ctx · $0.22/$0.95 per M"),
            "{s}"
        );
        assert!(
            s.contains("free") && s.contains("1M ctx · $3.00/$15 per M"),
            "{s}"
        );
        // typed: fuzzy, best first
        for ch in "sonnet".chars() {
            press(&mut nf, KeyCode::Char(ch));
        }
        let s = screen(&nf);
        assert!(s.contains("anthropic/claude-sonnet-4.5"), "{s}");
        assert!(s.contains("↓  pick"), "{s}");
        assert!(!s.contains("z-ai/glm-4.6"), "{s}");
        // ↓ picks one, tab takes it and moves on
        press(&mut nf, KeyCode::Down);
        assert!(screen(&nf).contains("› anthropic/claude-sonnet-4.5"));
        press(&mut nf, KeyCode::Tab);
        assert_eq!(nf.at, AT_PROMPT);
        assert_eq!(
            nf.chosen_model().as_deref(),
            Some("anthropic/claude-sonnet-4.5")
        );
        // away from the field: just the choice, no list
        let s = screen(&nf);
        assert!(
            s.contains("anthropic/claude-sonnet-4.5") && !s.contains("qwen/qwen3-coder"),
            "{s}"
        );
        // what it starts with: the agent's own flag
        assert_eq!(
            crate::openrouter::model_args("opencode", &nf.chosen_model().unwrap()).unwrap(),
            vec!["--model", "openrouter/anthropic/claude-sonnet-4.5"]
        );
        // an agent that can't take it: nothing chosen
        nf.at = AT_AGENT;
        for ch in "codex".chars() {
            press(&mut nf, KeyCode::Char(ch));
        }
        assert_eq!(nf.agent_id(), "codex");
        assert_eq!(nf.chosen_model(), None);
        // still reading the list: says so
        let mut nf = form("b31", "pi");
        nf.models = crate::openrouter::ModelList::default();
        nf.at = AT_MODEL;
        assert!(screen(&nf).contains("reading OpenRouter's models"));
    }

    #[test]
    fn cycling_shows_glyphs_and_follows_the_machine() {
        let mut nf = form("b31", "codex");
        nf.at = 3;
        // what b31 has comes first: Codex, a shell, then the rest
        press(&mut nf, KeyCode::Right);
        assert_eq!(nf.agent_id(), crate::agents::SHELL);
        press(&mut nf, KeyCode::Right);
        let id = nf.agent_id();
        let (g, _) = crate::icons::brand(id);
        let s = screen(&nf);
        assert!(
            s.contains(&format!("{g} {}", crate::agents::label(id))),
            "{s}"
        );
        assert!(s.contains("✗ not installed"), "{s}");
        // another machine: its own answer
        nf.at = 0;
        press(&mut nf, KeyCode::Right);
        assert!(screen(&nf).contains("checking…"));
    }
}
