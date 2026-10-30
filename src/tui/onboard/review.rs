//! Step 12, Review & apply. Everything the plan holds: the settings (and
//! the diff of `aleph.toml` they make, secrets masked), presets, and the
//! actions — the ones that run on other machines called out, the ones that
//! need the terminal or the internet said so. Enter on any of them goes back
//! to its step. Apply writes the settings in one go (through `Config`, so
//! comments and the history survive), then runs each action: quiet ones on
//! their own thread, the ones with their own prompts in the terminal. One
//! failure never stops the others; the summary links to the steps that
//! failed, and what failed stays in the plan for next time.

use super::plan::Set;
use super::rows::{Kind, Row, St};
use super::{Action, Aside, Ev, Exit, Hand, Msg, Run, Step, Wiz, Yes};
use crate::config::Config;
use crate::presets::{self, Preset};
use crate::ui::{self, c};
use ratatui::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ItemSt {
    Todo,
    Running,
    Ok(String),
    Failed(String),
    Skipped(String),
}

#[derive(Clone, Debug)]
pub(crate) enum What {
    Settings(Vec<Set>, Vec<Preset>),
    Act(Action),
}

#[derive(Clone, Debug)]
pub(crate) struct Item {
    pub what: What,
    pub step: Step,
    pub label: String,
    pub st: ItemSt,
    /// its output, escapes out (the last `install::KEEP` lines)
    pub log: Vec<String>,
    /// how far, when its output says (0–1)
    pub pct: Option<f64>,
    /// when it started, and how long it took
    pub t0: Option<Instant>,
    pub took: Option<Duration>,
    /// set by `x` on the install screen: its run is stopped (an agent's)
    pub stop: Arc<AtomicBool>,
}

impl Item {
    pub fn new(what: What, step: Step, label: String) -> Item {
        Item {
            what,
            step,
            label,
            st: ItemSt::Todo,
            log: Vec::new(),
            pct: None,
            t0: None,
            took: None,
            stop: Arc::default(),
        }
    }
}

/// `x` on the install screen: stop a running agent install (it and
/// everything it started); it's marked failed, `i` runs it again.
pub(crate) fn stop_one(w: &mut Wiz, k: usize) -> bool {
    let Some(it) = w.rv.items.get(k) else {
        return false;
    };
    if it.st != ItemSt::Running || !matches!(it.what, What::Act(Action::AgentInstall { .. })) {
        return false;
    }
    it.stop.store(true, Ordering::Relaxed);
    true
}

#[derive(Default)]
pub(crate) struct State {
    /// the settings file as it was when Review opened
    pub before: Option<String>,
    pub items: Vec<Item>,
    pub next: usize,
    pub busy: bool,
    pub done: bool,
    /// when the last hand-off started (ms): only its own report counts
    pub handed_at: u64,
    // the install screen
    /// when Apply started, and how long all of it took
    pub t0: Option<Instant>,
    pub ended: Option<Duration>,
    /// put aside (esc): Review's list shows the progress instead
    pub hidden: bool,
    /// the item picked with ↑↓ (None: follow the one running)
    pub look: Option<usize>,
    /// the whole output of the item shown
    pub full_log: bool,
    /// the next item needs you: it waits for enter
    pub waiting: Option<usize>,
    /// enter was pressed: hand the terminal over
    pub go: bool,
    /// all of it went in: the celebration's start
    pub celebrate: Option<Instant>,
    /// quick start is looking at this computer before it lines things up
    pub preparing: bool,
    /// agents installing now, side by side (items)
    pub par: Vec<usize>,
    /// the install log: every item's output and how it went
    pub log: Option<std::path::PathBuf>,
}

/// At most this many agents install at once.
const PAR_MAX: usize = 4;

pub(crate) fn enter(w: &mut Wiz) {
    w.rv.before = Some(std::fs::read_to_string(&w.cfg.path).unwrap_or_default());
    if w.rv.items.is_empty() {
        w.select_label("apply");
    }
}

pub(crate) fn retry(w: &mut Wiz) {
    if w.rv.done && w.rv.items.iter().any(|i| matches!(i.st, ItemSt::Failed(_))) {
        w.rv.items.clear();
        w.rv.done = false;
        enter(w);
        w.toast(
            false,
            "the failed ones are still in the plan — apply again when ready",
        );
    } else {
        enter(w);
    }
}

// ------------------------------------------------------------------ the diff

/// The settings file after the plan (pure).
pub(crate) fn after(before: &str, toml: bool, sets: &[Set], presets: &[Preset]) -> String {
    if toml {
        let mut doc: toml_edit::DocumentMut = before.parse().unwrap_or_default();
        for s in sets {
            match &s.val {
                Some(v) => crate::tomlcfg::set(&mut doc, &s.key, v),
                None => {
                    crate::tomlcfg::unset(&mut doc, &s.key);
                }
            }
        }
        for p in presets {
            presets::put(&mut doc, p);
        }
        return doc.to_string();
    }
    let mut lines: Vec<String> = before.lines().map(String::from).collect();
    for s in sets {
        lines.retain(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != s.key));
        if let Some(v) = &s.val {
            lines.push(format!("{}={v}", s.key));
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// The same text with secret values shown masked (pure).
pub(crate) fn masked(text: &str, toml: bool) -> String {
    if toml {
        let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
            return text.to_string();
        };
        for (k, v) in crate::tomlcfg::entries(&doc) {
            if crate::configcmd::is_secret(&k) && !v.starts_with("secret:") && !v.is_empty() {
                crate::tomlcfg::set(&mut doc, &k, &super::rows::masked(&v));
            }
        }
        return doc.to_string();
    }
    text.lines()
        .map(|l| match l.split_once('=') {
            Some((k, v))
                if crate::configcmd::is_secret(k.trim()) && !v.trim().starts_with("secret:") =>
            {
                format!("{}={}", k, super::rows::masked(v.trim()))
            }
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A line diff with `ctx` lines of context: (' ' | '+' | '-' | '…', text).
pub(crate) fn line_diff(a: &str, b: &str, ctx: usize) -> Vec<(char, String)> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    let (n, m) = (a.len(), b.len());
    let mut ops: Vec<(char, &str)> = Vec::new();
    if n.saturating_mul(m) > 4_000_000 {
        ops.extend(a.iter().map(|l| ('-', *l)));
        ops.extend(b.iter().map(|l| ('+', *l)));
    } else {
        let mut dp = vec![vec![0u32; m + 1]; n + 1];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                dp[i][j] = if a[i] == b[j] {
                    dp[i + 1][j + 1] + 1
                } else {
                    dp[i + 1][j].max(dp[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if a[i] == b[j] {
                ops.push((' ', a[i]));
                i += 1;
                j += 1;
            } else if dp[i + 1][j] >= dp[i][j + 1] {
                ops.push(('-', a[i]));
                i += 1;
            } else {
                ops.push(('+', b[j]));
                j += 1;
            }
        }
        ops.extend(a[i..].iter().map(|l| ('-', *l)));
        ops.extend(b[j..].iter().map(|l| ('+', *l)));
    }
    // only the changes and their context
    let near: Vec<bool> = (0..ops.len())
        .map(|k| {
            let lo = k.saturating_sub(ctx);
            let hi = (k + ctx + 1).min(ops.len());
            ops[lo..hi].iter().any(|(c, _)| *c != ' ')
        })
        .collect();
    let mut out = Vec::new();
    let mut gap = 0;
    for (k, (c, l)) in ops.iter().enumerate() {
        if near[k] {
            if gap > 0 {
                out.push((
                    '…',
                    format!("{gap} unchanged line{}", if gap == 1 { "" } else { "s" }),
                ));
                gap = 0;
            }
            out.push((*c, l.to_string()));
        } else {
            gap += 1;
        }
    }
    if gap > 0 && !out.is_empty() {
        out.push((
            '…',
            format!("{gap} unchanged line{}", if gap == 1 { "" } else { "s" }),
        ));
    }
    out
}

fn diff_now(w: &Wiz) -> Vec<(char, String)> {
    let before = w.rv.before.clone().unwrap_or_default();
    let toml = w.cfg.is_toml()
        || before.trim().is_empty() && w.cfg.path.extension().is_some_and(|e| e == "toml");
    let presets: Vec<Preset> = w.plan.presets.iter().map(|(_, p)| p.clone()).collect();
    let new = after(&before, toml, &w.plan.sets, &presets);
    line_diff(&masked(&before, toml), &masked(&new, toml), 2)
}

// ------------------------------------------------------------------ rows

fn shown(key: &str, v: &str) -> String {
    if v.is_empty() {
        return "default".into();
    }
    if crate::configcmd::is_secret(key) {
        super::rows::masked(v)
    } else {
        ui::trunc(v, 40)
    }
}

pub(crate) fn rows(w: &Wiz) -> Vec<Row> {
    if !w.rv.items.is_empty() {
        return applied_rows(w);
    }
    let mut v = Vec::new();
    if w.plan.is_empty() {
        v.push(Row::info(if w.finished {
            "Setup is done: nothing waits to be changed."
        } else {
            "Nothing to change yet — the steps put what they'd change here."
        }));
        v.push(Row::button("dashboard", dash_label(w)));
        v.push(Row::button("finish", "finish"));
        return v;
    }
    if !w.plan.sets.is_empty() {
        v.push(Row::head(format!(
            "Settings · {}",
            crate::doctor::tilde(&w.cfg.path)
        )));
        for s in &w.plan.sets {
            let old = w.cfg.get_raw(&s.key).unwrap_or_default();
            let new = s.val.clone().unwrap_or_default();
            v.push(
                Row::check(
                    St::Info,
                    format!("set:{}", s.key),
                    s.key.clone(),
                    format!("{} → {}", shown(&s.key, &old), shown(&s.key, &new)),
                )
                .fix(format!("go to {}", s.step.title())),
            );
        }
    }
    if !w.plan.presets.is_empty() {
        v.push(Row::head("Presets"));
        for (_, p) in &w.plan.presets {
            v.push(
                Row::check(
                    St::Info,
                    format!("preset:{}", p.name),
                    format!("@{}", p.name),
                    format!(
                        "{} pane{} · nothing starts now",
                        p.panes.len(),
                        if p.panes.len() == 1 { "" } else { "s" }
                    ),
                )
                .fix("go to Presets"),
            );
        }
    }
    if !w.plan.actions.is_empty() {
        v.push(Row::head("Actions"));
        for (i, (step, a)) in w.plan.actions.iter().enumerate() {
            // the machine in the label column (other machines called out)
            let mut spans = vec![];
            if a.host().is_some() {
                spans.push(Span::styled(
                    "▲ ",
                    ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::styled(a.label(), Style::new().fg(c::TEXT())));
            if a.tty() {
                spans.push(Span::styled(" · in the terminal", ui::faint()));
            }
            if a.online() && w.offline() {
                spans.push(Span::styled(
                    " · needs the internet (offline)",
                    ui::fg(c::YELLOW()),
                ));
            }
            let at = a.host().unwrap_or("here").to_string();
            let mut r = Row::check(St::Info, format!("act:{i}"), at, String::new())
                .fix(format!("go to {}", step.title()));
            r.note = spans;
            v.push(r);
        }
        let remote: Vec<&str> = w
            .plan
            .actions
            .iter()
            .filter_map(|(_, a)| a.host())
            .collect();
        if !remote.is_empty() {
            let mut hs: Vec<&str> = remote.clone();
            hs.sort_unstable();
            hs.dedup();
            v.push(Row::line(vec![
                Span::raw("  "),
                Span::styled("▲ ", ui::fg(c::ACCENT())),
                Span::styled(
                    format!(
                        "{} action{} change{} other machines: {}",
                        remote.len(),
                        if remote.len() == 1 { "" } else { "s" },
                        if remote.len() == 1 { "s" } else { "" },
                        hs.join(", ")
                    ),
                    ui::muted(),
                ),
            ]));
        }
    }
    v.push(Row::blank());
    v.push(Row::button(
        "apply",
        format!(
            "apply {} change{}",
            w.plan.len(),
            if w.plan.len() == 1 { "" } else { "s" }
        ),
    ));
    v
}

fn dash_label(w: &Wiz) -> String {
    if tour_due(w) {
        "open the dashboard (a 4-card tour)".into()
    } else {
        "open the dashboard".into()
    }
}

pub(crate) fn tour_due(w: &Wiz) -> bool {
    w.value("tour") != "done"
}

fn applied_rows(w: &Wiz) -> Vec<Row> {
    let mut v = vec![Row::head(if w.rv.done { "Applied" } else { "Applying" })];
    if w.rv.hidden {
        v.push(Row::button("installing", "show the install screen"));
    }
    for (i, it) in w.rv.items.iter().enumerate() {
        let (st, text) = match &it.st {
            ItemSt::Todo => (St::Todo, "waiting".to_string()),
            ItemSt::Running => (St::Busy, "running".to_string()),
            ItemSt::Ok(m) => (St::Ok, m.clone()),
            ItemSt::Failed(e) => (St::Bad, e.clone()),
            ItemSt::Skipped(e) => (St::Info, e.clone()),
        };
        let mut r = Row::check(st, format!("item:{i}"), it.label.clone(), text);
        if matches!(it.st, ItemSt::Failed(_) | ItemSt::Skipped(_)) && it.step != Step::Review {
            r = r.fix(format!("go to {}", it.step.title()));
        }
        v.push(r);
    }
    if w.rv.done {
        let bad =
            w.rv.items
                .iter()
                .filter(|i| matches!(i.st, ItemSt::Failed(_)))
                .count();
        let skip =
            w.rv.items
                .iter()
                .filter(|i| matches!(i.st, ItemSt::Skipped(_)))
                .count();
        let good = w.rv.items.len() - bad - skip;
        v.push(Row::blank());
        v.push(Row::line(vec![
            Span::raw("  "),
            Span::styled(format!("{good} done"), ui::fg(c::GREEN()).add_modifier(Modifier::BOLD)),
            Span::styled(
                if bad + skip > 0 {
                    format!(" · {bad} failed · {skip} skipped — they stay in the plan; the steps are marked !")
                } else {
                    " · all of it".to_string()
                },
                ui::muted(),
            ),
        ]));
        v.push(Row::button("dashboard", dash_label(w)));
        v.push(Row::button("finish", "finish"));
    }
    v
}

pub(crate) fn aside(w: &Wiz, width: u16) -> Option<Aside> {
    let tw = (width as usize).saturating_sub(6).max(20);
    if let Some(Row {
        kind: Kind::Check { id, .. },
        ..
    }) = w.selected()
    {
        if let Some(i) = id
            .strip_prefix("item:")
            .and_then(|x| x.parse::<usize>().ok())
        {
            let it = w.rv.items.get(i)?;
            let t = match &it.st {
                ItemSt::Ok(m) | ItemSt::Failed(m) | ItemSt::Skipped(m) => m.clone(),
                _ => "…".into(),
            };
            let mut lines = vec![Line::from(Span::styled(it.label.clone(), ui::bold()))];
            lines.extend(
                ui::wrap(&t, tw)
                    .into_iter()
                    .map(|x| Line::from(Span::styled(x, ui::muted()))),
            );
            return Some(Aside {
                title: it.step.title().into(),
                lines,
            });
        }
    }
    if w.plan.sets.is_empty() && w.plan.presets.is_empty() {
        return None;
    }
    let d = diff_now(w);
    let lines: Vec<Line<'static>> = if d.is_empty() {
        vec![Line::from(Span::styled(
            "no change to the file",
            ui::faint(),
        ))]
    } else {
        d.into_iter()
            .map(|(k, l)| match k {
                '+' => Line::from(Span::styled(format!("+ {l}"), ui::fg(c::GREEN()))),
                '-' => Line::from(Span::styled(format!("- {l}"), ui::fg(c::RED()))),
                '…' => Line::from(Span::styled(format!("  ⋯ {l}"), ui::faint())),
                _ => Line::from(Span::styled(format!("  {l}"), ui::muted())),
            })
            .collect()
    };
    Some(Aside {
        title: format!(
            "{} · diff",
            w.cfg
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default()
        ),
        lines,
    })
}

// ------------------------------------------------------------------ acting

fn step_of(w: &Wiz, id: &str) -> Option<Step> {
    if let Some(k) = id.strip_prefix("set:") {
        return w.plan.sets.iter().find(|s| s.key == k).map(|s| s.step);
    }
    if id.starts_with("preset:") {
        return Some(Step::Presets);
    }
    if let Some(i) = id
        .strip_prefix("act:")
        .and_then(|x| x.parse::<usize>().ok())
    {
        return w.plan.actions.get(i).map(|(s, _)| *s);
    }
    if let Some(i) = id
        .strip_prefix("item:")
        .and_then(|x| x.parse::<usize>().ok())
    {
        return w.rv.items.get(i).map(|it| it.step);
    }
    None
}

pub(crate) fn enter_word(w: &Wiz) -> &'static str {
    if w.rv.done || w.plan.is_empty() {
        "finish"
    } else {
        "apply"
    }
}

/// Enter on a row that does nothing of its own.
pub(crate) fn enter_key(w: &mut Wiz) {
    if !w.rv.items.is_empty() && !w.rv.done {
        return;
    }
    if w.rv.done || w.plan.is_empty() {
        w.exit = Some(Exit::Quit);
    } else {
        ask(w);
    }
}

fn ask(w: &mut Wiz) {
    let p = &w.plan;
    let mut hosts: Vec<&str> = p.actions.iter().filter_map(|(_, a)| a.host()).collect();
    hosts.sort_unstable();
    hosts.dedup();
    let tty = p.actions.iter().filter(|(_, a)| a.tty()).count();
    let mut lines = vec![Line::raw(format!(
        "{} setting{}, {} preset{} and {} action{}.",
        p.sets.len(),
        if p.sets.len() == 1 { "" } else { "s" },
        p.presets.len(),
        if p.presets.len() == 1 { "" } else { "s" },
        p.actions.len(),
        if p.actions.len() == 1 { "" } else { "s" },
    ))];
    if !hosts.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("▲ ", ui::fg(c::ACCENT())),
            Span::raw(format!("changes other machines: {}", hosts.join(", "))),
        ]));
    }
    if tty > 0 {
        lines.push(Line::raw(format!(
            "{tty} run{} in the terminal, with {} own questions.",
            if tty == 1 { "s" } else { "" },
            if tty == 1 { "its" } else { "their" }
        )));
    }
    lines.push(Line::from(Span::styled(
        "The settings file keeps a copy of what it was (aleph config undo).",
        ui::faint(),
    )));
    w.mode = super::Overlay::Confirm {
        title: "Apply?".into(),
        lines,
        yes: Yes::Apply,
    };
}

pub(crate) fn act(w: &mut Wiz, ev: Ev) -> Option<Hand> {
    match ev {
        Ev::Fix(id) => {
            if let Some(s) = step_of(w, &id) {
                w.go(s);
            }
        }
        Ev::Button(id) if id == "apply" => {
            if w.plan.is_empty() {
                w.toast(false, "nothing to apply");
            } else {
                ask(w);
            }
        }
        Ev::Button(id) if id == "dashboard" => {
            w.exit = Some(Exit::Dashboard { tour: tour_due(w) });
        }
        Ev::Button(id) if id == "finish" => w.exit = Some(Exit::Quit),
        Ev::Button(id) if id == "installing" => w.rv.hidden = false,
        Ev::After(id, r) => {
            let x = id.strip_prefix("apply:")?;
            match x.split_once('-') {
                // a machine's agents, items i..=j
                Some((i, j)) => {
                    if let (Ok(i), Ok(j)) = (i.parse::<usize>(), j.parse::<usize>()) {
                        installed(w, i, j, r);
                    }
                }
                None => {
                    if let Ok(i) = x.parse::<usize>() {
                        applied(w, i, r);
                    }
                }
            }
        }
        _ => {}
    }
    None
}

/// Apply was confirmed: line everything up (the settings first, then the
/// quiet actions, then the ones that need the terminal).
pub(crate) fn start(w: &mut Wiz) {
    let mut items = Vec::new();
    let sets = w.plan.sets.clone();
    let presets: Vec<Preset> = w.plan.presets.iter().map(|(_, p)| p.clone()).collect();
    if !sets.is_empty() || !presets.is_empty() {
        let mut label = format!(
            "write {} setting{}",
            sets.len(),
            if sets.len() == 1 { "" } else { "s" }
        );
        if !presets.is_empty() {
            label.push_str(&format!(
                " and {} preset{}",
                presets.len(),
                if presets.len() == 1 { "" } else { "s" }
            ));
        }
        items.push(Item::new(
            What::Settings(sets, presets),
            Step::Review,
            label,
        ));
    }
    // what this computer's agents' installers need (Kiro's unzip, Hermes's
    // git): on first
    let mut need: Vec<&'static str> = Vec::new();
    let mut uv = false;
    for (_, a) in &w.plan.actions {
        if let Action::AgentInstall { host, agent } = a {
            if crate::sshcfg::is_local(host) {
                let Some(g) = crate::agents::find(agent) else {
                    continue;
                };
                // build tools and the like: a Mac has them with its command line tools
                if !cfg!(target_os = "macos") {
                    for d in g.deps {
                        if !need.contains(d) && crate::proc::which(d).is_none() {
                            need.push(d);
                        }
                    }
                }
                uv |= matches!(
                    crate::agentinstall::methods(g).first(),
                    Some(crate::agentinstall::Method::Uv(_))
                );
            }
        }
    }
    if !w.quiet {
        for d in need {
            if let Some(a) = Action::tool_here(d).or_else(|| Action::sys_pkg(d)) {
                w.plan.want(Step::Agents, a, true);
            }
        }
        // uv's own installer (no sudo, into ~/.local/bin) for the agents that
        // install with it
        let home_uv = crate::config::home().join(".local/bin/uv");
        if uv && crate::proc::which("uv").is_none() && !home_uv.is_file() {
            w.plan.want(Step::Agents, Action::uv(), true);
        }
    }
    let mut acts = w.plan.actions.clone();
    // installs leave caches and build files behind: tidied at the end
    let mut hosts: Vec<String> = Vec::new();
    for (_, a) in &acts {
        let h = match a {
            Action::AgentInstall { host, .. } => host.clone(),
            Action::HerdrLocal
            | Action::Brew { .. }
            | Action::SysPkg { .. }
            | Action::Script { .. }
            | Action::Download { .. }
            | Action::FontDownload => crate::sshcfg::LOCAL.to_string(),
            _ => continue,
        };
        if !hosts.contains(&h) {
            hosts.push(h);
        }
    }
    acts.retain(|(_, a)| !matches!(a, Action::Tidy { .. }));
    if !hosts.is_empty() {
        acts.push((Step::Review, Action::Tidy { hosts }));
    }
    acts.sort_by_key(|(_, a)| order(a));
    for (step, a) in acts {
        items.push(Item::new(What::Act(a.clone()), step, a.label()));
    }
    w.rv.items = items;
    w.rv.next = 0;
    w.rv.busy = false;
    w.rv.done = false;
    w.rv.t0 = Some(Instant::now());
    w.rv.ended = None;
    w.rv.hidden = false;
    w.rv.look = None;
    w.rv.waiting = None;
    w.rv.go = false;
    w.rv.celebrate = None;
    w.rv.par.clear();
    w.rv.log = (!w.quiet).then(new_log).flatten();
    w.select_label("");
}

/// A new install log: `state/install/install-YYYYmmddHHMMSS.log`.
fn new_log() -> Option<std::path::PathBuf> {
    let dir = crate::config::state_dir().join("install");
    std::fs::create_dir_all(&dir).ok()?;
    let now = crate::cache::now();
    let local = (now as i64 + crate::ui::utc_offset()).max(0) as u64;
    let p = dir.join(format!("install-{}.log", crate::herdrtheme::stamp(local)));
    std::fs::write(
        &p,
        format!(
            "Aleph setup — what Apply installed, {} (local time)\n\n",
            crate::herdrtheme::stamp(local)
        ),
    )
    .ok()?;
    Some(p)
}

/// A line into the install log, under its item's name.
pub(crate) fn log_line(w: &Wiz, i: usize, line: &str) {
    let (Some(p), Some(it)) = (&w.rv.log, w.rv.items.get(i)) else {
        return;
    };
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(p) {
        let _ = writeln!(f, "[{}] {line}", it.label);
    }
}

/// How an item went, into the log.
fn log_end(w: &Wiz, i: usize) {
    let Some(it) = w.rv.items.get(i) else {
        return;
    };
    let took = it
        .took
        .map(|d| format!(" ({:.1}s)", d.as_secs_f64()))
        .unwrap_or_default();
    let how = match &it.st {
        ItemSt::Ok(m) => format!("✓ {m}"),
        ItemSt::Failed(e) => format!("✗ {e}"),
        ItemSt::Skipped(e) => format!("↷ {e}"),
        _ => return,
    };
    log_line(w, i, &format!("== {how}{took}"));
}

/// Run one failed item again (`i` on it in the install screen).
pub(crate) fn retry_one(w: &mut Wiz, k: usize) {
    let Some(it) = w.rv.items.get_mut(k) else {
        return;
    };
    if !matches!(it.st, ItemSt::Failed(_) | ItemSt::Skipped(_)) {
        return;
    }
    it.st = ItemSt::Todo;
    it.log.clear();
    it.pct = None;
    it.t0 = None;
    it.took = None;
    w.rv.next = w.rv.next.min(k);
    w.rv.done = false;
    w.rv.ended = None;
    w.rv.celebrate = None;
}

/// The queue's order (pure): what needs you first, while you're there (a
/// key's passphrase, herdr's question on each machine); then herdr here,
/// Homebrew's installs, each machine's agents together, their integrations,
/// the rest; an `aleph daemon` run last.
pub(crate) fn order(a: &Action) -> (u8, String) {
    match a {
        // sudo's password first, while you're there
        Action::SysPkg { .. } => (0, String::new()),
        Action::Script { url, .. } if url.contains("tailscale") => (0, String::new()),
        Action::SshKey => (0, String::new()),
        Action::HerdrSave { host, .. } => (1, host.clone()),
        Action::HerdrLocal => (2, String::new()),
        Action::Brew { .. } | Action::Download { .. } => (3, String::new()),
        // what agents install with (uv) goes before them
        Action::Script { what, .. } if what == "uv" => (3, String::new()),
        Action::AgentInstall { host, .. } => (4, host.clone()),
        Action::Integration { host, .. } => (5, host.clone()),
        Action::Tidy { .. } => (8, String::new()),
        Action::Daemon { .. } => (9, String::new()),
        _ => (6, String::new()),
    }
}

/// The package manager an agent's install goes through, when two of them
/// mustn't run at once (npm, uv and Homebrew lock their own folders; npm
/// may put Node.js on first). A script installer has none.
fn lane(agent: &str) -> Option<&'static str> {
    let ag = crate::agents::find(agent)?;
    match crate::agentinstall::methods(ag).first()? {
        crate::agentinstall::Method::Npm(_) => Some("npm"),
        crate::agentinstall::Method::Uv(_) => Some("uv"),
        crate::agentinstall::Method::Brew(_) => Some("brew"),
        _ => None,
    }
}

/// Start the agents that may start now: from the queue's next item on,
/// while they're agent installs — up to [`PAR_MAX`] at once, one per package
/// manager and machine. Each is its own `aleph agents install`, with its own
/// output and its own tick.
fn start_agents(w: &mut Wiz) {
    let mut k = w.rv.next;
    while let Some(What::Act(Action::AgentInstall { host, agent })) =
        w.rv.items.get(k).map(|x| x.what.clone())
    {
        if w.rv.par.len() >= PAR_MAX {
            break;
        }
        if w.rv.items[k].st == ItemSt::Todo {
            let l = lane(&agent);
            let busy_lane = l.is_some()
                && w.rv.par.iter().any(|&o| {
                    matches!(&w.rv.items[o].what,
                        What::Act(Action::AgentInstall { host: h, agent: a }) if *h == host && lane(a) == l)
                });
            if !crate::sshcfg::is_local(&host) && w.no_network() {
                w.rv.items[k].st = ItemSt::Skipped("skipped: no network".into());
            } else if w.offline() {
                w.rv.items[k].st =
                    ItemSt::Skipped("skipped: offline — it needs the internet".into());
            } else if !busy_lane {
                w.rv.items[k].st = ItemSt::Running;
                w.rv.items[k].t0 = Some(Instant::now());
                let stop = Arc::new(AtomicBool::new(false));
                w.rv.items[k].stop = stop.clone();
                w.rv.par.push(k);
                let label = format!("Installing {} on {host}", crate::agents::label(&agent));
                w.spawn_lines(&format!("apply:{k}"), label, k, move |emit| {
                    Msg::AgentDone(k, install_agents_until(&host, &[agent], &stop, emit))
                });
            }
        }
        k += 1;
    }
}

/// The folder herdr says is missing (`… not found at /home/x/.qwen. install
/// qwen code first`), when it says so (pure).
pub(crate) fn missing_dir(e: &str) -> Option<String> {
    let i = e.find("not found at ")? + "not found at ".len();
    let rest = &e[i..];
    let end = rest.find(|c: char| c.is_whitespace()).unwrap_or(rest.len());
    let p = rest[..end].trim_end_matches(['.', ',', ';']);
    (p.starts_with('/') || p.starts_with('~')).then(|| p.to_string())
}

/// One agent's run came back.
pub(crate) fn agent_done(w: &mut Wiz, k: usize, r: Result<String, String>) {
    w.rv.par.retain(|&x| x != k);
    let Some(it) = w.rv.items.get_mut(k) else {
        return;
    };
    it.st = match r {
        Ok(m) => ItemSt::Ok(m),
        Err(e) => ItemSt::Failed(e),
    };
    it.took = it.t0.map(|t| t.elapsed());
    let host = match &it.what {
        What::Act(Action::AgentInstall { host, .. }) => host.clone(),
        _ => return,
    };
    log_end(w, k);
    // the machine's last one: look at it again (its agents, now)
    let more = w.rv.items.iter().any(|x| {
        matches!(&x.what, What::Act(Action::AgentInstall { host: h, .. }) if *h == host)
            && matches!(x.st, ItemSt::Todo | ItemSt::Running)
    });
    if !w.quiet {
        w.cfg = Config::load();
        if !more {
            super::agent_setup::probe(w, &host, true);
        }
    }
}

/// Run the failed ones again (the rest stays as it went).
pub(crate) fn retry_failed(w: &mut Wiz) {
    let mut first = None;
    for (k, it) in w.rv.items.iter_mut().enumerate() {
        if matches!(it.st, ItemSt::Failed(_)) {
            it.st = ItemSt::Todo;
            it.log.clear();
            it.pct = None;
            it.t0 = None;
            it.took = None;
            first.get_or_insert(k);
        }
    }
    if let Some(k) = first {
        w.rv.next = k;
        w.rv.done = false;
        w.rv.busy = false;
        w.rv.ended = None;
        w.rv.look = None;
        w.rv.celebrate = None;
    }
}

/// Keep the apply going: start the next item (a thread, or a hand-off).
pub(crate) fn pump(w: &mut Wiz) -> Option<Hand> {
    if w.rv.items.is_empty() || w.rv.done {
        return None;
    }
    // agents installing side by side: more of them may start, nothing else
    if !w.rv.par.is_empty() {
        start_agents(w);
        return None;
    }
    if w.rv.busy {
        return None;
    }
    // a retry runs only what's left
    while w
        .rv
        .items
        .get(w.rv.next)
        .is_some_and(|it| it.st != ItemSt::Todo)
    {
        w.rv.next += 1;
    }
    let i = w.rv.next;
    if i >= w.rv.items.len() {
        finish(w);
        return None;
    }
    let it = w.rv.items[i].clone();
    if let What::Act(a) = &it.what {
        let why = if a.online() && w.offline() {
            Some("skipped: offline — it needs the internet")
        } else if a.host().is_some() && w.no_network() {
            Some("skipped: no network")
        } else {
            None
        };
        if let Some(why) = why {
            w.rv.items[i].st = ItemSt::Skipped(why.into());
            w.rv.next += 1;
            return None;
        }
        // one that needs you waits on the install screen for enter
        if a.tty() && !(w.quiet || w.rv.hidden || w.rv.go) {
            w.rv.waiting = Some(i);
            return None;
        }
    }
    // a machine's agents: each its own run, side by side
    if matches!(it.what, What::Act(Action::AgentInstall { .. })) {
        start_agents(w);
        return None;
    }
    w.rv.waiting = None;
    w.rv.go = false;
    let now = Instant::now();
    w.rv.items[i].st = ItemSt::Running;
    w.rv.items[i].t0 = Some(now);
    w.rv.busy = true;
    match it.what {
        What::Settings(sets, presets) => {
            w.spawn(&format!("apply:{i}"), "Writing the settings", move || {
                Msg::Applied(i, apply_settings(&sets, &presets))
            });
        }
        What::Act(a) if a.tty() => return Some(hand_for(&a, i)),
        What::Act(a) => {
            w.spawn_lines(&format!("apply:{i}"), a.label(), i, move |emit| {
                Msg::Applied(i, run_quiet(&a, emit))
            });
        }
    }
    None
}

pub(crate) fn applied(w: &mut Wiz, i: usize, r: Result<String, String>) {
    // herdr machine add for a machine herdr has already fails — it's saved
    let r = match (&r, w.rv.items.get(i).map(|x| &x.what)) {
        (Err(_), Some(What::Act(Action::HerdrSave { host, .. })))
            if !w.quiet && crate::herdr::has_machine(host) =>
        {
            Ok(format!("{host} was saved in herdr already"))
        }
        _ => r,
    };
    let Some(it) = w.rv.items.get_mut(i) else {
        return;
    };
    it.st = match r {
        Ok(m) => ItemSt::Ok(m),
        Err(e) => ItemSt::Failed(e),
    };
    it.took = it.t0.map(|t| t.elapsed());
    let it = it.clone();
    log_end(w, i);
    w.rv.busy = false;
    w.rv.next = i + 1;
    if let (ItemSt::Ok(_), What::Act(Action::HerdrSave { host, .. })) = (&it.st, &it.what) {
        if !w.quiet {
            // what onboard.rs does after `herdr machine add`
            let m = crate::herdr::machine_for(host);
            let mut cfg = Config::load();
            if cfg.get("ssh_host").as_deref() == Some(host.as_str()) {
                let _ = cfg.set("machine", &m);
            } else if m != *host {
                let _ = cfg.set(&format!("host.{host}.machine"), &m);
            }
            crate::chan::forget(host);
        }
    }
    if !w.quiet {
        w.cfg = Config::load();
    }
}

/// A machine's agents came back from the terminal (items `i..=j`): each one
/// as `aleph agents install` reported it (its note, `refresh.rs`) — else
/// the run's own result. The machine is looked at again.
pub(crate) fn installed(w: &mut Wiz, i: usize, j: usize, r: Result<String, String>) {
    let host = match w.rv.items.get(i).map(|x| &x.what) {
        Some(What::Act(Action::AgentInstall { host, .. })) => host.clone(),
        _ => return applied(w, i, r),
    };
    let report = if w.quiet {
        None
    } else {
        crate::refresh::last(&host)
            .filter(|n| n.at >= w.rv.handed_at)
            .and_then(|n| n.report)
    };
    for k in i..=j.min(w.rv.items.len().saturating_sub(1)) {
        let agent = match &w.rv.items[k].what {
            What::Act(Action::AgentInstall { agent, .. }) => agent.clone(),
            _ => continue,
        };
        let res = report.as_ref().and_then(|v| {
            v.get("results")?
                .as_array()?
                .iter()
                .find(|x| x.get("id").and_then(|i| i.as_str()) == Some(agent.as_str()))
                .cloned()
        });
        w.rv.items[k].st = item_of(res.as_ref(), &r);
        w.rv.items[k].took = w.rv.items[k].t0.map(|t| t.elapsed());
    }
    w.rv.busy = false;
    w.rv.next = j + 1;
    if !w.quiet {
        w.cfg = Config::load();
        super::agent_setup::probe(w, &host, true);
    }
}

/// One agent's line in the report → its item's state (pure).
pub(crate) fn item_of(res: Option<&serde_json::Value>, run: &Result<String, String>) -> ItemSt {
    let s = |k: &str| {
        res.and_then(|v| v.get(k))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    match s("status").as_str() {
        "installed" => ItemSt::Ok(format!("installed {}", s("version")).trim_end().to_string()),
        "already" => ItemSt::Ok(
            format!("already there {}", s("version"))
                .trim_end()
                .to_string(),
        ),
        "failed" => ItemSt::Failed(match s("fix") {
            f if f.is_empty() => s("text"),
            f => format!("{} — {f}", s("text")),
        }),
        "skipped" => ItemSt::Skipped(s("text")),
        _ => match run {
            Ok(m) => ItemSt::Ok(m.clone()),
            Err(e) => ItemSt::Failed(e.clone()),
        },
    }
}

/// Everything ran: what worked leaves the plan, what didn't stays (and its
/// step is marked).
fn finish(w: &mut Wiz) {
    w.rv.done = true;
    let mut failed = false;
    for it in w.rv.items.clone() {
        let ok = matches!(it.st, ItemSt::Ok(_));
        match (&it.what, ok) {
            (What::Settings(..), true) => {
                w.plan.sets.clear();
                w.plan.presets.clear();
            }
            (What::Act(a), true) => w.plan.want(it.step, a.clone(), false),
            (What::Settings(sets, _), false) => {
                failed = true;
                for s in sets {
                    w.marks[s.step.index()] = super::Mark::Warn;
                }
            }
            (What::Act(_), false) => {
                failed = true;
                w.marks[it.step.index()] = super::Mark::Warn;
            }
        }
    }
    w.finished = !failed;
    w.rv.ended = w.rv.t0.map(|t| t.elapsed());
    w.rv.waiting = None;
    if !failed {
        w.rv.celebrate = Some(Instant::now());
    }
    w.marks[Step::Review.index()] = if failed {
        super::Mark::Warn
    } else {
        super::Mark::Done
    };
    w.save();
    w.toast(
        failed,
        if failed {
            "applied, with failures — the steps are marked !"
        } else {
            "all applied"
        },
    );
}

// ------------------------------------------------------------------ running them

/// Write the settings in one go (one history entry) — TOML, or the old
/// key=value file one key at a time.
pub(crate) fn apply_settings(sets: &[Set], presets: &[Preset]) -> Result<String, String> {
    // secrets (ntfy, Slack, the webhook, an OpenRouter key from a setup file)
    // go to the keystore; the file says secret:<name>
    let sets: Vec<Set> = sets
        .iter()
        .map(|s| match &s.val {
            Some(v)
                if crate::secrets::SECRET_KEYS.contains(&s.key.as_str())
                    && !v.is_empty()
                    && !v.starts_with("secret:") =>
            {
                let name = crate::secrets::name_for(&s.key);
                match crate::secrets::set(name, v) {
                    Ok(_) => Set {
                        val: Some(format!("secret:{name}")),
                        ..s.clone()
                    },
                    Err(_) => s.clone(),
                }
            }
            _ => s.clone(),
        })
        .collect();
    let sets = &sets[..];
    let mut cfg = Config::load();
    if cfg.is_toml() {
        let (sets, presets) = (sets.to_vec(), presets.to_vec());
        cfg.edit_doc(move |doc| {
            for s in &sets {
                match &s.val {
                    Some(v) => crate::tomlcfg::set(doc, &s.key, v),
                    None => {
                        crate::tomlcfg::unset(doc, &s.key);
                    }
                }
            }
            for p in &presets {
                presets::put(doc, p);
            }
        })
        .map_err(|e| e.to_string())?;
    } else {
        for s in sets {
            match &s.val {
                Some(v) => cfg.set(&s.key, v),
                None => cfg.unset(&s.key),
            }
            .map_err(|e| format!("{}: {e}", s.key))?;
        }
        if !presets.is_empty() {
            return Err(
                "presets need aleph.toml (aleph config check moves the old file over)".into(),
            );
        }
    }
    Ok(format!(
        "{} setting{}{} → {} (aleph config undo steps back)",
        sets.len(),
        if sets.len() == 1 { "" } else { "s" },
        if presets.is_empty() {
            String::new()
        } else {
            format!(
                ", {} preset{}",
                presets.len(),
                if presets.len() == 1 { "" } else { "s" }
            )
        },
        crate::doctor::tilde(&cfg.path)
    ))
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// Our own subcommand, quietly, with a timeout.
fn aleph_quiet(args: &[&str], t: Duration) -> Result<String, String> {
    let o = crate::proc::run(
        std::process::Command::new(super::self_exe()).args(args),
        t,
        Some(b""),
    )
    .map_err(|e| e.to_string())?;
    let text = format!("{}\n{}", o.text(), o.stderr);
    let plain = strip(&text);
    if o.ok() {
        Ok(first_line(&plain))
    } else {
        Err(if plain.trim().is_empty() {
            o.reason()
        } else {
            first_line(&plain)
        })
    }
}

pub(crate) fn strip(s: &str) -> String {
    let mut out = String::new();
    let mut esc = false;
    for ch in s.chars() {
        if esc {
            if ch.is_ascii_alphabetic() {
                esc = false;
            }
            continue;
        }
        if ch == '\x1b' {
            esc = true;
            continue;
        }
        out.push(ch);
    }
    out
}

/// What an action runs, in a line (the install screen's card).
pub(crate) fn runs_line(a: &Action) -> String {
    let on = |h: &str| {
        if crate::sshcfg::is_local(h) {
            String::new()
        } else {
            format!(" (on {h})")
        }
    };
    match a {
        Action::HerdrSave { host, .. } => format!("herdr machine add {host}"),
        Action::Headless { host, cols, rows } => format!(
            "headless_cols = {cols}, headless_rows = {rows} in herdr's config{}",
            on(host)
        ),
        Action::Integration { host, name } => {
            format!("herdr integration install {name}{}", on(host))
        }
        Action::Hooks { host } => format!("aleph hooks install{}", on(host)),
        Action::AgentInstall { host, agent } => match crate::agents::find(agent) {
            Some(ag) if !ag.install.is_empty() => format!("{}{}", ag.install, on(host)),
            _ => format!("aleph agents install {agent} --host {host}"),
        },
        Action::Service => "aleph daemon install (a service at login)".into(),
        Action::Completions => "two lines in your shell's rc file (a backup is kept)".into(),
        Action::Wrapper => "a claude() function in ~/.zshrc (a backup is kept)".into(),
        Action::HerdrPlugin => "herdr plugin link, and prefix keys in herdr's config".into(),
        Action::Raycast => "npm install, then ray develop, stopped once Raycast has it; Raycast asks once to run the menu bar".into(),
        Action::MoshiOn => "aleph moshi install".into(),
        Action::Daemon { args } => format!("aleph daemon {}", args.join(" ")),
        Action::HerdrLocal => herdr_install_line(),
        Action::Brew { args, .. } => format!("brew {}", args.join(" ")),
        Action::SshKey => format!(
            "ssh-keygen -t ed25519 -N '' -f {} (no passphrase)",
            crate::doctor::tilde(&crate::sshcfg::new_key_path())
        ),
        Action::FontDownload => format!(
            "the Nerd Fonts release's JetBrainsMono.tar.xz → {}",
            crate::doctor::tilde(&font_dir())
        ),
        Action::SysPkg {
            mgr,
            pkg,
            ask: true,
        } => Action::sys_line(mgr, pkg),
        Action::SysPkg {
            mgr,
            pkg,
            ask: false,
        } => Action::sys_line_quiet(mgr, pkg),
        Action::Script { url, .. } => format!("curl -fsSL {url} | sh"),
        Action::Download { url, name, .. } => {
            format!("curl -fsSL {url} -o ~/.local/bin/{name} (its release build)")
        }
        Action::Tidy { hosts } => format!(
            "npm cache clean, uv cache prune, node-gyp's headers and broken links removed — on {}",
            hosts.join(", ")
        ),
    }
}

/// Why one needs you, for its card while it waits.
pub(crate) fn why_tty(it: &Item) -> String {
    match &it.what {
        What::Act(Action::SysPkg { .. }) => "sudo asks for your password (once: it remembers it for a few minutes). Aleph never sees it.".into(),
        What::Act(Action::Script { .. }) => "Tailscale's installer uses sudo: it asks for your password. Aleph never sees it.".into(),
        What::Act(Action::HerdrSave { .. }) => "herdr machine add saves the machine, and asks once more before it installs herdr there — answer y.".into(),
        What::Act(Action::Daemon { .. }) => "It says what it will do, and asks before it changes anything.".into(),
        _ => "It runs in the terminal, with its own questions.".into(),
    }
}

fn herdr_install_line() -> String {
    if crate::proc::which("brew").is_some() {
        "brew install herdr".into()
    } else {
        "curl -fsSL https://herdr.dev/install.sh | sh".into()
    }
}

/// The last line worth showing of a run that failed.
fn tail_of(o: &crate::proc::Out) -> String {
    let text = strip(&format!("{}\n{}", o.text(), o.stderr));
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("npm warn") && !l.starts_with("Warning:"))
        .map(String::from)
        .unwrap_or_else(|| o.reason())
}

/// herdr on this computer: Homebrew's, else herdr's own script (it goes to
/// ~/.local/bin); then it has to answer `--version`.
fn install_herdr(emit: &dyn Fn(&str)) -> Result<String, String> {
    let mut c = if let Some(b) = crate::proc::which("brew") {
        let mut c = std::process::Command::new(b);
        c.args(["install", "herdr"])
            .env("NONINTERACTIVE", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1");
        c
    } else {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", "curl -fsSL https://herdr.dev/install.sh | sh"]);
        c
    };
    emit(&herdr_install_line());
    let o = crate::proc::stream(&mut c, Duration::from_secs(600), None, emit)
        .map_err(|e| e.to_string())?;
    if !o.ok() {
        return Err(tail_of(&o));
    }
    let bin = crate::proc::which(&crate::herdr::bin())
        .or_else(|| {
            let p = crate::config::home().join(".local/bin/herdr");
            p.is_file().then_some(p)
        })
        .ok_or("installed, but herdr isn't on the PATH — open a new terminal")?;
    let v = crate::proc::cap(
        &bin.to_string_lossy(),
        &["--version"],
        Duration::from_secs(10),
    );
    Ok(format!(
        "herdr {} · {}",
        crate::doctor::version(&v.text()),
        crate::doctor::tilde(&bin)
    ))
}

/// `herdr machine add` for a machine whose herdr matches this one's: no
/// download, so no question — it runs here with its output on the screen.
/// One herdr has already is saved, not failed.
fn herdr_save(host: &str, emit: &dyn Fn(&str)) -> Result<String, String> {
    if crate::herdr::has_machine(host) {
        return Ok(format!("{host} was saved in herdr already"));
    }
    emit(&format!("herdr machine add {host}"));
    let mut c = std::process::Command::new(crate::herdr::bin());
    c.args(["machine", "add", host]);
    let o = crate::proc::stream(&mut c, Duration::from_secs(180), None, emit)
        .map_err(|e| e.to_string())?;
    let text = format!("{}\n{}", o.text(), o.stderr);
    if o.ok() {
        Ok(format!("{host} saved in herdr"))
    } else if text.contains("already exists") || crate::herdr::has_machine(host) {
        Ok(format!("{host} was saved in herdr already"))
    } else {
        Err(tail_of(&o))
    }
}

/// Where a downloaded font goes: your own fonts folder.
fn font_dir() -> std::path::PathBuf {
    let h = crate::config::home();
    if cfg!(target_os = "macos") {
        h.join("Library/Fonts")
    } else {
        h.join(".local/share/fonts/JetBrainsMonoNerdFont")
    }
}

/// JetBrains Mono Nerd Font from the Nerd Fonts release (no Homebrew): the
/// archive straight into your fonts folder, then fontconfig told (Linux).
fn font_download(emit: &dyn Fn(&str)) -> Result<String, String> {
    let dir = font_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let url =
        "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.tar.xz";
    emit(&format!("{url} → {}", crate::doctor::tilde(&dir)));
    let mut c = std::process::Command::new("sh");
    c.args(["-c", "curl -fSL --progress-bar \"$1\" | tar -xJ -C \"$2\" && { command -v fc-cache >/dev/null && fc-cache -f \"$2\" || true; }", "sh", url])
        .arg(&dir);
    let o = crate::proc::stream(&mut c, Duration::from_secs(600), None, emit)
        .map_err(|e| e.to_string())?;
    if o.ok() {
        Ok(format!(
            "JetBrains Mono Nerd Font in {} — pick it as your terminal's font",
            crate::doctor::tilde(&dir)
        ))
    } else {
        Err(tail_of(&o))
    }
}

/// How much a folder holds (bytes; links aren't followed).
fn size_of(p: &std::path::Path) -> u64 {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if !m.is_dir() {
        return m.len();
    }
    std::fs::read_dir(p)
        .map(|rd| rd.flatten().map(|e| size_of(&e.path())).sum())
        .unwrap_or(0)
}

pub(crate) fn human(b: u64) -> String {
    match b {
        0..=999_999 => format!("{} KB", b / 1000),
        1_000_000..=999_999_999 => format!("{} MB", b / 1_000_000),
        _ => format!("{:.1} GB", b as f64 / 1e9),
    }
}

/// Links in a bin folder that a failed npm install left pointing at nothing
/// (only those into node_modules: anything else is yours).
pub(crate) fn broken_links(bin: &std::path::Path) -> Vec<std::path::PathBuf> {
    let Ok(rd) = std::fs::read_dir(bin) else {
        return vec![];
    };
    rd.flatten()
        .map(|e| e.path())
        .filter(|p| {
            std::fs::read_link(p)
                .is_ok_and(|t| t.to_string_lossy().contains("node_modules") && !p.exists())
        })
        .collect()
}

/// After the installs: their caches and leftovers. Here — npm's cache,
/// node-gyp's headers, uv's unused cache, a failed install's links and
/// Hermes's half-cloned tree — measured before and after; on another
/// machine the same in one sh line.
fn tidy(hosts: &[String], emit: &dyn Fn(&str)) -> Result<String, String> {
    let mut said = Vec::new();
    for h in hosts {
        if !crate::sshcfg::is_local(h) {
            emit(&format!(
                "{h}: npm cache clean, uv cache prune, node-gyp's headers"
            ));
            let cmd = format!(
                "{}; npm cache clean --force >/dev/null 2>&1; uv cache prune >/dev/null 2>&1; rm -rf \"$HOME/.cache/node-gyp\"; true",
                crate::chan::PATH_PRELUDE
            );
            let _ = crate::proc::run(
                &mut crate::sshcfg::shell(h, &cmd),
                Duration::from_secs(120),
                None,
            );
            said.push(format!("{h} tidied"));
            continue;
        }
        let home = crate::config::home();
        let npm_cache = home.join(".npm/_cacache");
        let uv_cache = home.join(".cache/uv");
        let gyp = home.join(".cache/node-gyp");
        let before = size_of(&npm_cache) + size_of(&uv_cache) + size_of(&gyp);
        let npm = crate::proc::which("npm").or_else(|| {
            std::fs::read_dir(home.join(".local/share/aleph/node"))
                .ok()?
                .flatten()
                .map(|e| e.path().join("bin/npm"))
                .find(|p| p.is_file())
        });
        if let Some(npm) = npm {
            emit("npm cache clean --force");
            let _ = crate::proc::cap(
                &npm.to_string_lossy(),
                &["cache", "clean", "--force"],
                Duration::from_secs(120),
            );
        }
        let uv = crate::proc::which("uv").or_else(|| {
            let p = home.join(".local/bin/uv");
            p.is_file().then_some(p)
        });
        if let Some(uv) = uv {
            emit("uv cache prune");
            let _ = crate::proc::cap(
                &uv.to_string_lossy(),
                &["cache", "prune"],
                Duration::from_secs(120),
            );
        }
        if gyp.is_dir() {
            emit("node-gyp's downloaded headers");
            let _ = std::fs::remove_dir_all(&gyp);
        }
        let mut gone = 0;
        for l in broken_links(&home.join(".local/bin")) {
            emit(&format!("a broken link: {}", crate::doctor::tilde(&l)));
            if std::fs::remove_file(&l).is_ok() {
                gone += 1;
            }
        }
        // a failed Hermes install's half-cloned tree
        if let Ok(rd) = std::fs::read_dir(home.join(".hermes")) {
            for e in rd.flatten() {
                if e.file_name()
                    .to_string_lossy()
                    .starts_with(".hermes-clone-")
                {
                    emit(&format!(
                        "a half-cloned tree: {}",
                        crate::doctor::tilde(&e.path())
                    ));
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let after = size_of(&npm_cache) + size_of(&uv_cache) + size_of(&gyp);
        let mut s = format!("freed {}", human(before.saturating_sub(after)));
        if gone > 0 {
            s.push_str(&format!(
                " · {gone} broken link{}",
                if gone == 1 { "" } else { "s" }
            ));
        }
        said.push(s);
    }
    Ok(said.join(" · "))
}

/// A program's ready-to-run release build into ~/.local/bin (on the PATH of
/// your login shells), then asked its version.
fn download(what: &str, url: &str, name: &str, emit: &dyn Fn(&str)) -> Result<String, String> {
    let dir = crate::config::home().join(".local/bin");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let dest = dir.join(name);
    let tmp = dir.join(format!(".{name}.aleph-download"));
    emit(&format!("{url} → {}", crate::doctor::tilde(&dest)));
    let mut c = std::process::Command::new("curl");
    c.args(["-fSL", "--progress-bar", url, "-o"]).arg(&tmp);
    let o = crate::proc::stream(&mut c, Duration::from_secs(600), None, emit)
        .map_err(|e| e.to_string())?;
    if !o.ok() {
        let _ = std::fs::remove_file(&tmp);
        return Err(tail_of(&o));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    std::fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    let v = crate::proc::cap(
        &dest.to_string_lossy(),
        &["--version"],
        Duration::from_secs(10),
    );
    if !v.ok() {
        return Err(format!(
            "{what} downloaded but doesn't run here: {}",
            first_line(&format!("{}{}", v.text(), v.stderr))
        ));
    }
    Ok(format!(
        "{what} {} · {}",
        crate::doctor::version(&v.text()),
        crate::doctor::tilde(&dest)
    ))
}

/// An installer script (moshi-hook's): fetched and run as you.
fn script(what: &str, url: &str, emit: &dyn Fn(&str)) -> Result<String, String> {
    emit(&format!("curl -fsSL {url} | sh"));
    let mut c = std::process::Command::new("sh");
    c.args(["-c", "curl -fsSL \"$1\" | sh", "sh", url]);
    let o = crate::proc::stream(&mut c, Duration::from_secs(600), None, emit)
        .map_err(|e| e.to_string())?;
    if o.ok() {
        Ok(format!("{what} installed"))
    } else {
        Err(tail_of(&o))
    }
}

/// A new ed25519 key with no passphrase — nothing to type, and no agent
/// needed (`ssh-keygen -p` adds a passphrase later). Never over a key that's
/// there (`sshcfg::new_key_path`).
fn make_key(emit: &dyn Fn(&str)) -> Result<String, String> {
    let path = crate::sshcfg::new_key_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
    }
    let comment = format!(
        "aleph {}@{}",
        std::env::var("USER").unwrap_or_default(),
        crate::proc::cap("hostname", &["-s"], Duration::from_secs(3))
            .text()
            .trim()
    );
    emit(&format!(
        "ssh-keygen -t ed25519 -N '' -f {}",
        crate::doctor::tilde(&path)
    ));
    let mut c = std::process::Command::new("ssh-keygen");
    c.args(["-q", "-t", "ed25519", "-N", "", "-C", &comment, "-f"])
        .arg(&path);
    let o = crate::proc::stream(&mut c, Duration::from_secs(30), None, emit)
        .map_err(|e| e.to_string())?;
    if o.ok() && path.is_file() {
        Ok(format!("{} (no passphrase)", crate::doctor::tilde(&path)))
    } else {
        Err(tail_of(&o))
    }
}

/// A Homebrew install, its output on the screen.
fn brew(what: &str, args: &[String], emit: &dyn Fn(&str)) -> Result<String, String> {
    let b = crate::proc::which("brew").ok_or_else(|| {
        format!(
            "Homebrew isn't installed (brew.sh) — then: brew {}",
            args.join(" ")
        )
    })?;
    emit(&format!("brew {}", args.join(" ")));
    let mut c = std::process::Command::new(b);
    c.args(args)
        .env("NONINTERACTIVE", "1")
        .env("HOMEBREW_NO_ENV_HINTS", "1");
    let o = crate::proc::stream(&mut c, Duration::from_secs(1200), None, emit)
        .map_err(|e| e.to_string())?;
    if o.ok() {
        Ok(format!("{what} installed"))
    } else {
        Err(tail_of(&o))
    }
}

/// Why an agent install failed, from what it printed on both streams
/// (pure): its own `error:` line, else its last ✗ line, else its last line —
/// never Aleph's bookkeeping (the note for an open dashboard, the log's
/// path, a hint).
pub(crate) fn failure_reason(text: &str) -> Option<String> {
    let lines = || text.lines().map(str::trim);
    let ours =
        |l: &str| l.contains("left a note") || l.starts_with("log:") || l.starts_with("hint:");
    lines()
        .rfind(|l| l.starts_with("error:"))
        .map(|l| l.trim_start_matches("error:").trim().to_string())
        .or_else(|| {
            lines()
                .filter(|l| l.starts_with(['✗', '×']))
                .map(|l| l.trim_start_matches(['✗', '×']).trim())
                .rfind(|l| !l.is_empty() && !ours(l))
                .map(String::from)
        })
        .or_else(|| {
            lines()
                .map(|l| l.trim_start_matches(['✗', '×']).trim())
                .rfind(|l| !l.is_empty() && !ours(l))
                .map(String::from)
        })
}

/// A machine's agents, one `aleph agents install` run: each is checked
/// (on the PATH, its --version runs, herdr sees it); its report says how
/// each went (`installed`).
fn install_agents(host: &str, ids: &[String], emit: &dyn Fn(&str)) -> Result<String, String> {
    install_agents_until(host, ids, &AtomicBool::new(false), emit)
}

/// [`install_agents`], stopped when `stop` is set (`x` on the install
/// screen).
fn install_agents_until(
    host: &str,
    ids: &[String],
    stop: &AtomicBool,
    emit: &dyn Fn(&str),
) -> Result<String, String> {
    let cfg = Config::load();
    let secs = |k: &str, d: u64| {
        cfg.get(k)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(d)
    };
    // `aleph agents install` keeps the same limit and says so itself: this
    // one only catches a run that doesn't end after it
    let limit = crate::agentsetup::install_limit(&cfg) + Duration::from_secs(120);
    let quiet = Duration::from_secs(secs("agents.install_quiet", 60).max(15));
    let mut c = std::process::Command::new(super::self_exe());
    c.args(["agents", "install"])
        .args(ids)
        .args(["--host", host, "--yes"])
        .env("NO_COLOR", "1")
        // installers that know these don't ask
        .env("CI", "1")
        .env("NONINTERACTIVE", "1")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .env("npm_config_yes", "true")
        .env("PIP_NO_INPUT", "1");
    // its own session, no terminal: a prompt can't wait for an answer
    let (o, stalled) = crate::proc::stream_watch(
        &mut c,
        limit,
        None,
        // stopped when it asks something nobody can answer; a long silent
        // clone or build goes on until the limit
        crate::proc::Watch {
            detach: true,
            quiet: None,
            asked: Some(quiet),
        },
        emit,
        || stop.load(Ordering::Relaxed),
    )
    .map_err(|e| e.to_string())?;
    let said = |mark: char| {
        strip(&o.text())
            .lines()
            .map(str::trim)
            .rfind(|l| l.starts_with(mark))
            .map(|l| l.trim_start_matches(mark).trim().to_string())
    };
    // both streams: `aleph agents install` says what went wrong on stderr
    let text = strip(&format!("{}\n{}", o.text(), o.stderr));
    if o.ok() {
        // its "✓ … installed" line, not a bare tick
        let done = text
            .lines()
            .map(str::trim)
            .rfind(|l| l.starts_with('✓') && (l.contains(" installed") || l.contains("already")))
            .map(|l| l.trim_start_matches('✓').trim().to_string());
        return Ok(done
            .or_else(|| said('✓'))
            .unwrap_or_else(|| first_line(&text)));
    }
    // stopped, or it failed: is the agent there anyway? (an installer that
    // put it in place and then asked to log in, or stumbled after)
    let why = if stop.load(Ordering::Relaxed) {
        "stopped by you (x) — i runs it again".to_string()
    } else if stalled {
        format!(
            "its installer asked a question nobody can answer here (nothing for {}s) and was stopped",
            quiet.as_secs()
        )
    } else if o.timed_out {
        format!(
            "its installer ran past {} min and was stopped",
            limit.as_secs() / 60
        )
    } else {
        failure_reason(&text).unwrap_or_else(|| tail_of(&o))
    };
    let p = crate::probe::run_cached(host, true);
    let ids_found: Vec<String> = ids
        .iter()
        .filter_map(|id| {
            p.agent(id).map(|f| {
                format!("{} {}", crate::agents::label(id), f.version)
                    .trim()
                    .to_string()
            })
        })
        .collect();
    if !ids_found.is_empty() && ids_found.len() == ids.len() {
        let exe = ids
            .first()
            .and_then(|id| crate::agents::find(id))
            .and_then(|a| a.exes.first().copied())
            .unwrap_or("it");
        Ok(format!(
            "{} installed — {why}; run {exe} once to finish (a login, say)",
            ids_found.join(", ")
        ))
    } else {
        emit(&format!("✗ {why}"));
        Err(why)
    }
}

/// An action that needs no terminal; its output goes to `emit` as it comes.
pub(crate) fn run_quiet(a: &Action, emit: &dyn Fn(&str)) -> Result<String, String> {
    match a {
        Action::Integration { host, name } => {
            let cfg = Config::load();
            let run = || {
                crate::doctor::Fix::Integration {
                    host: host.clone(),
                    name: name.clone(),
                }
                .run_quiet(&cfg)
                .unwrap_or_else(|| Err("can't run here".into()))
            };
            let mut r = run();
            // "omp extension directory not found at ~/.omp/agent/extensions.
            // install omp first": a fresh agent hasn't made its folder yet
            if let Err(e) = &r {
                if let Some(dir) = missing_dir(e) {
                    emit(&format!("herdr wants {dir} first: making it"));
                    let _ = crate::proc::run(
                        &mut crate::sshcfg::shell(
                            host,
                            &format!("mkdir -p {}", crate::proc::sh_quote(&dir)),
                        ),
                        Duration::from_secs(20),
                        None,
                    );
                    r = run();
                }
            }
            if r.is_ok() && cfg.host_get(host, "hooks").as_deref() == Some("on") {
                let _ = crate::hooks::ensure(host);
            }
            r
        }
        Action::Hooks { host } => {
            let done = crate::hooks::install_quiet(host)?;
            let mut cfg = Config::load();
            let _ = cfg.set(&format!("host.{host}.hooks"), "on");
            Ok(format!("{host}: {}", done.join(" · ")))
        }
        Action::Headless { host, cols, rows } => {
            super::herdr_setup::apply_headless(host, *cols, *rows)
        }
        // `aleph daemon install` where this build has it, else the older way
        Action::Service if super::has_sub("daemon", "install") => {
            aleph_quiet(&["daemon", "install"], Duration::from_secs(60))
        }
        Action::Service => aleph_quiet(&["watch", "--install"], Duration::from_secs(60)),
        Action::Completions => super::integrations::apply_completions(),
        Action::Wrapper => super::integrations::apply_wrapper(),
        Action::HerdrPlugin => super::integrations::apply_plugin(),
        Action::MoshiOn => aleph_quiet(&["moshi", "install"], Duration::from_secs(60)),
        Action::Raycast => super::integrations::apply_raycast(emit),
        Action::HerdrSave { host, .. } => herdr_save(host, emit),
        Action::HerdrLocal => install_herdr(emit),
        Action::Brew { what, args } => brew(what, args, emit),
        Action::SshKey => make_key(emit),
        Action::FontDownload => font_download(emit),
        Action::Script { what, url } => script(what, url, emit),
        Action::Download { what, url, name } => download(what, url, name, emit),
        Action::Tidy { hosts } => tidy(hosts, emit),
        Action::SysPkg { mgr, pkg, .. } => {
            let line = Action::sys_line_quiet(mgr, pkg);
            emit(&line);
            let mut c = std::process::Command::new("sh");
            c.args(["-c", &line]);
            let o = crate::proc::stream(&mut c, Duration::from_secs(900), None, emit)
                .map_err(|e| e.to_string())?;
            if o.ok() {
                Ok(format!("{pkg} installed"))
            } else if format!("{}{}", o.text(), o.stderr).contains("Unable to locate package") {
                Err(format!(
                    "{pkg} isn't in this system's package lists ({mgr}) — install it another way"
                ))
            } else {
                Err(tail_of(&o))
            }
        }
        other => Err(format!("{} needs the terminal", other.label())),
    }
}

/// The hand-off for an action with its own prompts.
pub(crate) fn hand_for(a: &Action, i: usize) -> Hand {
    let (title, note, run) = match a {
        Action::HerdrSave { host, why } => (
            format!("Save {host} in herdr"),
            format!(
                "herdr machine add {host}: saves the machine in herdr{}. herdr asks once more before it downloads anything — answer y; it goes to ~/.local/bin there.",
                if why.is_empty() { String::new() } else { format!(" and {why}") }
            ),
            Run::cmd(&crate::herdr::bin(), &["machine", "add", host]),
        ),
        Action::AgentInstall { host, agent } => (
            format!("Install {} on {host}", crate::agents::label(agent)),
            "aleph agents install shows the command and what it needs, runs it with its output, and checks the agent runs. It never runs sudo.".into(),
            Run::aleph(&["agents", "install", agent, "--host", host, "--yes"]),
        ),
        Action::SysPkg { mgr, pkg, .. } => (
            format!("Install {pkg}"),
            "sudo asks for your password (once: it remembers it for a few minutes); Aleph never sees it.".into(),
            Run::Cmd {
                prog: "sh".into(),
                args: vec!["-c".into(), Action::sys_line(mgr, pkg)],
                cwd: None,
            },
        ),
        Action::Script { what, url } => (
            format!("Install {what}"),
            "Its installer uses sudo, which asks for your password; Aleph never sees it.".into(),
            Run::Cmd {
                prog: "sh".into(),
                args: vec!["-c".into(), format!("curl -fsSL {url} | sh")],
                cwd: None,
            },
        ),
        Action::Daemon { args } => (
            format!("aleph daemon {}", args.join(" ")),
            "It says what it will do, and asks before it changes anything.".into(),
            Run::Cmd {
                prog: super::self_exe(),
                args: std::iter::once("daemon".to_string()).chain(args.iter().cloned()).collect(),
                cwd: None,
            },
        ),
        other => (other.label(), String::new(), Run::cmd("true", &[])),
    };
    Hand {
        title,
        note,
        run,
        step: Step::Review,
        id: format!("apply:{i}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(k: &str, v: Option<&str>) -> Set {
        Set {
            step: Step::Look,
            key: k.into(),
            val: v.map(String::from),
        }
    }

    #[test]
    fn the_diff_shows_what_changes_and_masks_secrets() {
        let before = "# mine\n[fleet]\ndefault = \"b31\"\nmachines = [\"b31\"]\n\n[ui]\ntheme = \"auto\" # keep this\n";
        let sets = vec![
            set("theme", Some("dark")),
            set("hosts", Some("b31,gpu2")),
            set(
                "notify.ntfy",
                Some("https://ntfy.sh/aleph-0123456789abcdef"),
            ),
        ];
        let new = after(before, true, &sets, &[]);
        assert!(new.starts_with("# mine\n"), "{new}");
        assert!(new.contains("machines = [\"b31\", \"gpu2\"]"), "{new}");
        let d = line_diff(&masked(before, true), &masked(&new, true), 1);
        let plus: Vec<&String> = d
            .iter()
            .filter(|(c, _)| *c == '+')
            .map(|(_, l)| l)
            .collect();
        let minus: Vec<&String> = d
            .iter()
            .filter(|(c, _)| *c == '-')
            .map(|(_, l)| l)
            .collect();
        assert!(plus.iter().any(|l| l.contains("theme = \"dark\"")), "{d:?}");
        assert!(
            minus.iter().any(|l| l.contains("theme = \"auto\"")),
            "{d:?}"
        );
        assert!(plus.iter().any(|l| l.contains("ntfy")), "{d:?}");
        assert!(
            !d.iter().any(|(_, l)| l.contains("0123456789abcdef")),
            "secret shown: {d:?}"
        );
        // a key back to its default disappears
        let gone = after(&new, true, &[set("theme", None)], &[]);
        assert!(!gone.contains("theme ="), "{gone}");
        // presets go in as tables
        let p = super::super::starters::starters(&["b31".into()]).remove(1);
        let with = after(before, true, &[], &[p]);
        assert!(
            with.contains("[presets.pair]") && with.contains("[[presets.pair.panes]]"),
            "{with}"
        );
    }

    #[test]
    fn diffs_keep_context_and_fold_the_rest() {
        let a: String = (0..20).map(|i| format!("line {i}\n")).collect();
        let b = a.replace("line 10\n", "line ten\n");
        let d = line_diff(&a, &b, 2);
        assert_eq!(d.first().map(|x| x.0), Some('…'));
        assert_eq!(d.iter().filter(|x| x.0 == '-').count(), 1);
        assert_eq!(d.iter().filter(|x| x.0 == '+').count(), 1);
        assert_eq!(d.iter().filter(|x| x.0 == ' ').count(), 4);
        assert!(line_diff("same\n", "same\n", 2).is_empty());
        // the old key=value file
        let flat = after(
            "ssh_host=b31\nnotify.ntfy=https://ntfy.sh/topic-secret-123\n",
            false,
            &[set("ssh_host", Some("gpu2"))],
            &[],
        );
        assert!(
            flat.contains("ssh_host=gpu2") && !flat.contains("ssh_host=b31"),
            "{flat}"
        );
        assert!(!masked(&flat, false).contains("secret-123"));
    }
}
