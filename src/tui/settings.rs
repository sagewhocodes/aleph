//! Settings: every option by section — General, Appearance, Machines, …,
//! Doctor — with the doctor folded in. When it's wide, three columns: the
//! sections (a mark where a check is unhappy), the section's settings (or a
//! search across all of them), and on the right the selected setting in
//! detail over the section's Health: the doctor's checks for it, each with
//! what `f` can do about it. Keys outside the registry get rows too: a
//! machine's own settings, services, auto-approve rules, custom actions,
//! presets and the Fleet keymap.
//!
//! Each setting says where its value comes from (the file, an include, a
//! profile, the environment, or the default), is checked before it's saved
//! (and marked ✗ when the file holds a bad one), and the look ones preview
//! live. Per-machine × per-agent settings are grids (remote control,
//! analytics, Moshi per machine); a Fleet key is bound by pressing it, with
//! clashes refused.
//!
//! Keys: ←→ panel · ↑↓ move · `[` `]` section · enter edit (a grid, a key) ·
//! space toggle · d reset · / search · f fix · r check again · E the file in
//! $EDITOR · u undo the last change · i import · x export · S this
//! section's setup step.

use super::{doctor, forms, onboard, Act, Dash, Handoff, Mode, Pending, ACTS};
use crate::config::{self, Kind, Known, Source, KNOWN, SECTIONS};
use crate::doctor::St;
use crate::permit;
use crate::ui::{self, c};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};

/// Settings that only take effect on the next start.
const RESTART: &[&str] = &["mouse"];

/// Keys the dashboard keeps for itself before any Fleet action sees them.
const RESERVED: &[&str] = &[
    "1",
    "2",
    "3",
    "4",
    "5",
    "6",
    "7",
    "8",
    "9",
    "tab",
    "shift+tab",
    "?",
    ":",
    "ctrl+k",
    "esc",
    "q",
    "p",
    "up",
    "down",
];

/// A per-machine grid.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mx {
    /// machines × agents: `remote.allow` (`remote.deny` still wins)
    Remote,
    /// machines × agents: `analytics.include` / `analytics.exclude`
    Analytics,
    /// machines × what goes to Moshi: `host.<machine>.moshi`
    Moshi,
}

impl Mx {
    fn title(self) -> &'static str {
        match self {
            Mx::Remote => "▦ machines × agents",
            Mx::Analytics => "▦ machines × agents",
            Mx::Moshi => "▦ Moshi per machine",
        }
    }
    fn help(self) -> &'static str {
        match self {
            Mx::Remote => "which agents, on which machines, a phone, the web console or Raycast may answer (remote.allow; remote.deny always wins)",
            Mx::Analytics => "whose logs are read for token and time analytics (analytics.include / analytics.exclude)",
            Mx::Moshi => "what each machine's sessions send to Moshi: auto (all but what moshi-hook sends itself), everything, only the machine's own news, or nothing (host.<machine>.moshi)",
        }
    }
}

/// Moshi's per-machine choices: (value, label).
const MOSHI: [(&str, &str); 4] = [
    ("auto", "auto"),
    ("full", "everything"),
    ("machine-only", "machine only"),
    ("off", "off"),
];

/// A grid being edited.
pub(crate) struct Grid {
    mx: Mx,
    hosts: Vec<String>,
    cols: Vec<String>,
    on: Vec<Vec<bool>>,
    r: usize,
    c: usize,
}

/// Waiting for the key to bind to a Fleet action.
pub(crate) struct Capture {
    act: Act,
    name: &'static str,
    def: &'static str,
    err: Option<String>,
}

#[derive(Default)]
pub(crate) struct State {
    /// the section (index into `SECTIONS`)
    pub(crate) sec: usize,
    /// the row in the section (or in the search results)
    sel: usize,
    scroll: usize,
    /// which panel the arrows move in: 0 sections · 1 settings · 2 health
    pub(crate) focus: u8,
    /// the selected check in Health
    hsel: usize,
    pub search: String,
    pub searching: bool,
    /// a grid being edited (enter on a grid row)
    grid: Option<Grid>,
    /// a key being bound (enter on a Keys row)
    capture: Option<Capture>,
    /// a value being picked (enter on a choice)
    pick: Option<Pick>,
    /// a list being ticked (enter on a list)
    check: Option<Check>,
}

impl State {
    /// A grid, a key capture, a picker or a checklist takes every key.
    pub(crate) fn captures(&self) -> bool {
        self.grid.is_some() || self.capture.is_some() || self.pick.is_some() || self.check.is_some()
    }
}

/// Every value of a choice, with what each does: ↑↓ and enter.
pub(crate) struct Pick {
    key: &'static str,
    opts: Vec<(String, String)>,
    sel: usize,
    now: usize,
}

/// A list setting's options, ticked: space, a all, n none, enter saves.
pub(crate) struct Check {
    key: &'static str,
    opts: Vec<(String, String)>,
    on: Vec<bool>,
    sel: usize,
}

/// What each of `opts` does, from the setting's help (`name (what it does)`).
fn described(help: &str, opts: &[&str]) -> Vec<(String, String)> {
    opts.iter()
        .map(|o| {
            let what = help
                .match_indices(&format!("{o} ("))
                .find(|(i, _)| {
                    *i == 0
                        || !help[..*i]
                            .chars()
                            .next_back()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
                })
                .and_then(|(i, m)| {
                    let rest = &help[i + m.len()..];
                    // to the bracket that closes it (one level of nesting)
                    let mut depth = 0;
                    let end = rest.char_indices().find(|(_, c)| match c {
                        '(' => {
                            depth += 1;
                            false
                        }
                        ')' if depth == 0 => true,
                        ')' => {
                            depth -= 1;
                            false
                        }
                        _ => false,
                    })?;
                    Some(rest[..end.0].to_string())
                })
                .unwrap_or_default();
            (o.to_string(), what)
        })
        .collect()
}

/// A list setting's words for "every option" and "none" (what to save when
/// all or none are ticked).
fn list_words(key: &str) -> (&'static str, &'static str) {
    match key {
        // (showcase's own settings: only in a build that has it)
        #[cfg(feature = "showcase")]
        "showcase.machines" | "showcase.users" | "showcase.things" => ("any", "any"),
        #[cfg(feature = "showcase")]
        "showcase.universes" => ("all", "all"),
        #[cfg(feature = "showcase")]
        "showcase.watermark_on" => ("", "off"),
        _ => ("all", "off"),
    }
}

/// Which of `opts` a list value has on (`all`, `any`, `off`, `all, -x`, a list).
fn ticked(v: &str, opts: &[&str]) -> Vec<bool> {
    let words: Vec<String> = v
        .split(',')
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    let every = words
        .iter()
        .any(|w| matches!(w.as_str(), "all" | "any" | "on"))
        || (!words.is_empty() && words.iter().all(|w| w.starts_with('-')));
    opts.iter()
        .map(|o| {
            let minus = words.iter().any(|w| w.strip_prefix('-') == Some(o));
            let named = words.iter().any(|w| w == o);
            // `mine` is never part of "every kind"
            (every && *o != "mine" && !minus) || named
        })
        .collect()
}

/// The value to save for `on` (pure — tested).
fn list_value(key: &str, opts: &[String], on: &[bool]) -> String {
    let (all, none) = list_words(key);
    let kinds: Vec<usize> = (0..opts.len()).filter(|&i| opts[i] != "mine").collect();
    let n = on.iter().filter(|x| **x).count();
    let mine = opts.iter().zip(on).any(|(o, x)| o == "mine" && *x);
    if n == 0 {
        return none.to_string();
    }
    if !all.is_empty() && !mine && kinds.iter().all(|&i| on[i]) {
        return all.to_string();
    }
    opts.iter()
        .zip(on)
        .filter(|(_, x)| **x)
        .map(|(o, _)| o.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

/// Durations a setting steps through (nearest below or above the value).
const LADDER: &[&str] = &[
    "0", "100ms", "250ms", "500ms", "700ms", "1s", "1500ms", "2s", "3s", "5s", "10s", "15s", "30s",
    "1m", "2m", "5m", "10m", "15m", "30m", "1h", "2h", "6h", "12h", "1d",
];

fn ms(v: &str) -> Option<u128> {
    crate::log::parse_duration(v).map(|d| d.as_millis())
}

/// Does this setting hold a duration (its default reads as one)?
fn is_duration(k: &Known) -> bool {
    matches!(k.kind, Kind::Text) && !k.default.is_empty() && ms(k.default).is_some()
}

/// The next value of a stepping setting `dir` (±1) from `cur` (pure —
/// tested): on/off flips, a choice turns round, a number goes ±1 (±0.05
/// with a decimal point), a duration along the ladder. None: no steps.
fn stepped(k: &Known, cur: &str, dir: i32) -> Option<String> {
    match k.kind {
        Kind::Bool => {
            let on = matches!(cur, "true" | "1" | "yes" | "on");
            Some(if on { "false" } else { "true" }.into())
        }
        Kind::Choice(opts) => {
            let n = opts.len() as i32;
            let i = opts.iter().position(|o| *o == cur).map_or(0, |i| i as i32);
            Some(opts[((i + dir).rem_euclid(n)) as usize].into())
        }
        Kind::Num => {
            let x: f64 = cur.parse().unwrap_or(0.0);
            Some(if cur.contains('.') {
                format!("{:.2}", (x + 0.05 * dir as f64).max(0.0))
            } else {
                ((x as i64 + dir as i64).max(0)).to_string()
            })
        }
        _ if is_duration(k) => {
            let now = ms(cur).or_else(|| ms(k.default))?;
            let zero = k.default == "0" || k.help.contains("0 for") || k.help.contains("; 0");
            let ladder = LADDER.iter().filter(|v| zero || **v != "0");
            let next = if dir > 0 {
                ladder.clone().find(|v| ms(v).is_some_and(|m| m > now))
            } else {
                ladder.clone().rfind(|v| ms(v).is_some_and(|m| m < now))
            };
            next.map(|v| v.to_string())
        }
        _ => None,
    }
}

/// ←/→ (and space, shift+space) on the selected row: a step, when it has
/// steps (true: it did).
fn step(d: &mut Dash, dir: i32) -> bool {
    let Some(Row::Known(k)) = current(d) else {
        return false;
    };
    let (cur, _) = value(d, k.key);
    match stepped(k, &cur, dir) {
        Some(v) => {
            save(d, k.key, &v);
            true
        }
        None => false,
    }
}

fn open_pick(d: &mut Dash, k: &'static Known, opts: &[&str]) {
    let (cur, _) = value(d, k.key);
    let now = opts.iter().position(|o| *o == cur).unwrap_or(0);
    d.set.pick = Some(Pick {
        key: k.key,
        opts: described(k.help, opts),
        sel: now,
        now,
    });
}

fn open_check(d: &mut Dash, k: &'static Known, opts: &[&str]) {
    let (cur, _) = value(d, k.key);
    d.set.check = Some(Check {
        key: k.key,
        opts: described(k.help, opts),
        on: ticked(&cur, opts),
        sel: 0,
    });
}

fn pick_key(d: &mut Dash, k: KeyEvent) {
    let Some(p) = d.set.pick.as_mut() else {
        return;
    };
    let n = p.opts.len();
    match k.code {
        KeyCode::Esc => d.set.pick = None,
        KeyCode::Up | KeyCode::Char('k') | KeyCode::Left => p.sel = (p.sel + n - 1) % n,
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Right | KeyCode::Tab => {
            p.sel = (p.sel + 1) % n
        }
        KeyCode::Home => p.sel = 0,
        KeyCode::End => p.sel = n - 1,
        KeyCode::Enter | KeyCode::Char(' ') => {
            if let Some(p) = d.set.pick.take() {
                save(d, p.key, &p.opts[p.sel].0);
            }
        }
        _ => {}
    }
}

fn check_key(d: &mut Dash, k: KeyEvent) {
    let Some(c) = d.set.check.as_mut() else {
        return;
    };
    let n = c.opts.len();
    match k.code {
        KeyCode::Esc => d.set.check = None,
        KeyCode::Up | KeyCode::Char('k') => c.sel = (c.sel + n - 1) % n,
        KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => c.sel = (c.sel + 1) % n,
        KeyCode::Home => c.sel = 0,
        KeyCode::End => c.sel = n - 1,
        KeyCode::Char(' ') | KeyCode::Char('x') => c.on[c.sel] = !c.on[c.sel],
        KeyCode::Char('a') | KeyCode::Char('A') => {
            for (i, o) in c.opts.iter().enumerate() {
                c.on[i] = o.0 != "mine";
            }
        }
        KeyCode::Char('n') | KeyCode::Char('N') => c.on.iter_mut().for_each(|x| *x = false),
        KeyCode::Enter => {
            if let Some(c) = d.set.check.take() {
                let opts: Vec<String> = c.opts.iter().map(|o| o.0.clone()).collect();
                save(d, c.key, &list_value(c.key, &opts, &c.on));
            }
        }
        _ => {}
    }
}

/// One row of the settings list.
#[derive(Clone)]
enum Row {
    Known(&'static Known),
    /// a key outside the registry (a machine's own setting, a rule, …)
    Extra(String),
    /// a Fleet key binding: (action, its config name, default key, help)
    Key(Act, &'static str, &'static str, &'static str),
    /// add one more of a family (`approve.<rule>`, `action.<key>`)
    Add(&'static str),
    /// a per-machine grid
    Grid(Mx),
}

impl Row {
    fn key(&self) -> String {
        match self {
            Row::Known(k) => k.key.to_string(),
            Row::Extra(k) => k.clone(),
            Row::Key(_, n, _, _) => format!("key.{n}"),
            Row::Add(p) => format!("{p}…"),
            Row::Grid(m) => m.title().to_string(),
        }
    }
    fn help(&self) -> String {
        match self {
            Row::Known(k) => k.help.to_string(),
            Row::Extra(k) => family_help(k),
            Row::Key(_, _, _, h) => h.to_string(),
            Row::Add(p) => add_help(p).to_string(),
            Row::Grid(m) => m.help().to_string(),
        }
    }
}

fn family_help(k: &str) -> String {
    let fam = |p: &str| k.strip_prefix(p);
    if let Some(r) = fam("host.") {
        let (h, f) = r.rsplit_once('.').unwrap_or((r, ""));
        if let Some((_, help)) = config::HOST_KEYS.iter().find(|(k, _)| *k == f) {
            return format!("{h}: {help}");
        }
        return format!("{h}'s own {f} (overrides the fleet-wide setting for that machine)");
    }
    if let Some(r) = fam("service.") {
        return format!("whether Aleph looks for {r} on your machines");
    }
    if let Some(r) = fam("approve.") {
        return format!("auto-approve rule {r}: questions matching this pattern are answered yes (the deny list always wins)");
    }
    if let Some(r) = fam("action.") {
        return format!(
            "custom key {r} in Fleet: label|command, run in a new tab next to the session"
        );
    }
    if let Some(r) = fam("preset.") {
        let (n, f) = r.rsplit_once('.').unwrap_or((r, ""));
        return format!("preset @{n}: its {f} (aleph new @{n}, or P in Fleet)");
    }
    "a setting Aleph doesn't know (kept as it is)".into()
}

fn add_help(p: &str) -> &'static str {
    match p {
        "approve." => "a new auto-approve rule: name=pattern, e.g. tests=*pytest*",
        "action." => "a new custom key: key=label|command, e.g. g=Git|lazygit",
        _ => "a new entry: name=value",
    }
}

fn sec_name(d: &Dash) -> &'static str {
    SECTIONS[d.set.sec.min(SECTIONS.len() - 1)]
}

/// The rows of one section.
fn rows_of(d: &Dash, sec: &str) -> Vec<Row> {
    let mut v: Vec<Row> = KNOWN
        .iter()
        .filter(|k| config::section_of(k.key) == sec)
        .map(Row::Known)
        .collect();
    let mut extra: Vec<String> = d
        .cfg
        .entries()
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| config::known(k).is_none() && !k.starts_with("key."))
        .filter(|k| config::section_of(k) == sec)
        .collect();
    extra.sort();
    extra.dedup();
    v.extend(extra.into_iter().map(Row::Extra));
    match sec {
        "Keys" => v.extend(ACTS.iter().map(|(a, n, def, h)| Row::Key(*a, n, def, h))),
        "Automation" => v.push(Row::Add("approve.")),
        "Integrations" => v.push(Row::Add("action.")),
        "Remote control" => v.insert(0, Row::Grid(Mx::Remote)),
        "Analytics" => v.insert(0, Row::Grid(Mx::Analytics)),
        "Notifications" => v.push(Row::Grid(Mx::Moshi)),
        _ => {}
    }
    v
}

/// What a value-holding row's value is wrong about (None: fine, or unset).
fn bad(d: &Dash, r: &Row) -> Option<String> {
    let key = match r {
        Row::Known(k) => k.key.to_string(),
        Row::Extra(k) => k.clone(),
        Row::Key(a, n, def, _) => return key_clash(d, *a, n, def),
        _ => return None,
    };
    let v = raw(d, &key)?;
    if v.starts_with("secret:") {
        return None;
    }
    onboard::check_value(&key, &v).err()
}

fn act_name(a: Act) -> &'static str {
    ACTS.iter()
        .find(|(x, ..)| *x == a)
        .map(|(_, n, _, _)| *n)
        .unwrap_or("another action")
}

/// A Fleet action whose key doesn't reach it: the dashboard keeps it, or
/// another binding took it.
fn key_clash(d: &Dash, a: Act, name: &str, def: &str) -> Option<String> {
    let keys: Vec<String> = match d.cfg.get(&format!("key.{name}")) {
        Some(k) => k
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        None => vec![def.to_string()],
    };
    for k in keys {
        if RESERVED.contains(&k.as_str()) {
            return Some(format!("`{k}` is the dashboard's own"));
        }
        match d.keys.owner(&k) {
            Some(o) if o != a => return Some(format!("`{k}` runs {} instead", act_name(o))),
            _ => {}
        }
    }
    None
}

/// A section's setup step (`S`).
fn step_of(sec: &str) -> Option<onboard::Step> {
    use onboard::Step;
    Some(match sec {
        "Appearance" => Step::Look,
        "Machines" => Step::Machines,
        "Agents" => Step::Agents,
        "Notifications" | "Remote control" => Step::Notify,
        "Analytics" => Step::Analytics,
        "Automation" => Step::Automation,
        "Presets" => Step::Presets,
        "Daemon & API" => Step::Access,
        "Integrations" => Step::Integrations,
        "General" | "Network" | "Doctor" => Step::Preflight,
        _ => return None,
    })
}

/// What the list shows: the section's rows, or every row matching the search.
fn rows(d: &Dash) -> Vec<(Row, &'static str)> {
    if d.set.search.is_empty() {
        let s = sec_name(d);
        return rows_of(d, s).into_iter().map(|r| (r, s)).collect();
    }
    // every word, in any order, in its key, its name in aleph.toml
    // (notify.desktop_done for notify_done), its help or its value; dots,
    // underscores and dashes are spaces ("splash ms" finds ui.splash_ms)
    let norm = |s: &str| s.to_lowercase().replace(['.', '_', '-'], " ");
    let words: Vec<String> = norm(&d.set.search)
        .split_whitespace()
        .map(String::from)
        .collect();
    SECTIONS
        .iter()
        .flat_map(|s| rows_of(d, s).into_iter().map(move |r| (r, *s)))
        .filter(|(r, _)| {
            let key = r.key();
            let hay = norm(&format!(
                "{key} {} {} {}",
                crate::tomlcfg::to_path(&key).join("."),
                r.help(),
                shown(d, r)
            ));
            words.iter().all(|w| hay.contains(w.as_str()))
        })
        .collect()
}

/// The stored value (never a secret's own value: `secret:NAME` stays as is).
fn raw(d: &Dash, key: &str) -> Option<String> {
    d.cfg.get_raw(key)
}

fn value(d: &Dash, key: &str) -> (String, bool) {
    match raw(d, key) {
        Some(v) => (v, true),
        None => (
            config::known(key)
                .map(|k| k.default.to_string())
                .unwrap_or_default(),
            false,
        ),
    }
}

/// A row's value as the list shows it.
fn shown(d: &Dash, r: &Row) -> String {
    match r {
        Row::Known(k) => {
            let (v, _) = value(d, k.key);
            match k.kind {
                Kind::Bool if matches!(v.as_str(), "true" | "1" | "yes" | "on") => "☑ on".into(),
                Kind::Bool => "☐ off".into(),
                _ if v.is_empty() => "—".into(),
                _ => v,
            }
        }
        Row::Extra(k) => raw(d, k).unwrap_or_default(),
        Row::Key(a, ..) => d.keys.key_for(*a),
        Row::Add(_) => String::new(),
        Row::Grid(m) => grid_summary(d, *m),
    }
}

fn is_set(d: &Dash, r: &Row) -> bool {
    match r {
        Row::Known(k) => raw(d, k.key).is_some(),
        Row::Extra(_) => true,
        Row::Key(_, n, _, _) => raw(d, &format!("key.{n}")).is_some(),
        Row::Add(_) => false,
        Row::Grid(m) => grid_keys(d, *m).iter().any(|k| raw(d, k).is_some()),
    }
}

/// Is `val`, typed into the Settings view for `key`, a secret to keep in the
/// keystore instead of the file (pure — tested)? Its name there: the
/// OpenRouter key, a sink's URL; not a reference, `off` or nothing.
pub(crate) fn keystore_name(key: &str, val: &str) -> Option<String> {
    (crate::secrets::SECRET_KEYS.contains(&key)
        && !val.is_empty()
        && val != "off"
        && !val.starts_with("secret:"))
    .then(|| crate::secrets::name_for(key).to_string())
}

pub(crate) fn save(d: &mut Dash, key: &str, val: &str) {
    // a secret pasted in: into the keystore (a local call), and the setting
    // points at it — never the file, never a toast
    let stored;
    let val = if let Some(name) = keystore_name(key, val) {
        match crate::secrets::set(&name, val) {
            Ok(_) => {
                if key == "openrouter.key" {
                    crate::openrouter::forget();
                    d.usage.or.again();
                }
                stored = format!("secret:{name}");
                stored.as_str()
            }
            Err(e) => {
                d.error(format!(
                    "couldn't keep it in the keystore: {e} — nothing saved"
                ));
                return;
            }
        }
    } else {
        val
    };
    // the profile in use names it: that's where the change goes (and wins)
    let prof = d
        .cfg
        .profile_of(key)
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()));
    let r = if val.is_empty() {
        d.cfg.unset(key)
    } else {
        d.cfg.set(key, val)
    };
    match r {
        Ok(()) => {
            let note = if RESTART.contains(&key) {
                " (applies on restart)"
            } else {
                ""
            };
            d.note(format!(
                "{key} = {}{note}{}",
                if val.is_empty() { "default" } else { val },
                prof.map(|p| format!(" · in profile {p}"))
                    .unwrap_or_default()
            ));
            apply_live(d, key);
        }
        Err(e) => d.error(format!("couldn't save: {e}")),
    }
}

/// Settings the dashboard can pick up without a restart.
fn apply_live(d: &mut Dash, key: &str) {
    match key {
        // the palette is the whole process's: never from tests
        "theme" | "accent" if !cfg!(test) => {
            crate::theme::load_custom(&d.cfg);
            let theme = d.cfg.get_or("theme");
            // the theme first: named accents have a light and a dark shade
            ui::c::init(&theme, None);
            if let Some(a) = onboard::look::accent_color(&d.cfg.get_or("accent")) {
                ui::c::init(&theme, Some(a));
            }
        }
        "refresh" => d.fleet.interval = d.cfg.refresh_dur(),
        "ui_scale" => crate::ui::set_scale(&d.cfg.get_or("ui_scale")),
        "ui.spinner" => crate::ui::set_spinner(&d.cfg.get_or("ui.spinner")),
        "ui.ground" | "ui.panels" | "ui.ambience" => crate::look::configure(&d.cfg),
        k if k == "services" || k == "services_interval" || k.starts_with("service.") => {
            let hosts: Vec<String> = d.fleet.hosts.iter().map(|h| h.alias.clone()).collect();
            d.services = hosts
                .iter()
                .cloned()
                .zip(crate::services::watch(&d.cfg, &hosts))
                .collect();
        }
        "icons" => crate::icons::init(&d.cfg.get_or("icons")),
        "agents" => d.fleet.set_agents(&d.cfg.get_or("agents")),
        k if k.starts_with("inbox.") => super::inbox::apply_settings(d),
        k if k.starts_with("host.") && k.ends_with(".session") => crate::chan::set_sessions(&d.cfg),
        k if k.starts_with("key.") || k.starts_with("action.") => {
            d.keys = super::Keys::from_config(&d.cfg)
        }
        _ => {}
    }
}

/// After the file changed under us ($EDITOR, undo, import, setup): pick up
/// everything that applies live.
pub(crate) fn reapply_all(d: &mut Dash) {
    for k in [
        "theme",
        "refresh",
        "ui_scale",
        "ui.spinner",
        "ui.ground",
        "icons",
        "agents",
        "key.",
        "inbox.group",
    ] {
        apply_live(d, k);
    }
}

pub(crate) fn hints(d: &Dash) -> Vec<(&'static str, &'static str)> {
    if d.set.searching {
        return vec![("enter", "keep"), ("esc", "clear")];
    }
    if sec_name(d) == "Doctor" && d.set.search.is_empty() {
        let mut v = vec![("↑↓", "move")];
        if doctor::selected_head(&d.doc).is_some() {
            v.push(("↵", "every check"));
        }
        if doctor::selected_fix(d).is_some() {
            v.push(("f", "fix"));
        }
        v.push((
            "v",
            if d.doc.by_agent {
                "by machine"
            } else {
                "by agent"
            },
        ));
        v.extend([("r", "check again"), ("←", "sections"), ("esc", "back")]);
        return v;
    }
    if d.set.grid.as_ref().is_some_and(|g| g.mx == Mx::Moshi) {
        return vec![
            ("↑↓←→", "move"),
            ("space", "choose"),
            ("enter", "save"),
            ("esc", "cancel"),
        ];
    }
    if d.set.grid.is_some() {
        return vec![
            ("↑↓←→", "move"),
            ("space", "tick"),
            ("a", "whole row"),
            ("A", "whole column"),
            ("enter", "save"),
            ("esc", "cancel"),
        ];
    }
    if d.set.capture.is_some() {
        return vec![
            ("any key", "bind it"),
            ("backspace", "the default"),
            ("esc", "cancel"),
        ];
    }
    if d.set.pick.is_some() {
        return vec![("↑↓", "choose"), ("enter", "set it"), ("esc", "cancel")];
    }
    if d.set.check.is_some() {
        return vec![
            ("↑↓", "move"),
            ("space", "tick"),
            ("a", "all"),
            ("n", "none"),
            ("enter", "save"),
            ("esc", "cancel"),
        ];
    }
    let mut v = vec![("enter", "edit"), ("space ←→", "change"), ("d", "reset")];
    if !doctor::for_section(d, sec_name(d)).is_empty() {
        v.push(("f", "fix"));
    }
    if step_of(sec_name(d)).is_some() {
        v.push(("S", "setup step"));
    }
    v.extend([
        ("E", "$EDITOR"),
        ("u", "undo"),
        ("i", "import"),
        ("x", "export"),
        ("/", "search"),
        ("[ ]", "section"),
    ]);
    v
}

fn current(d: &Dash) -> Option<Row> {
    rows(d).into_iter().nth(d.set.sel).map(|x| x.0)
}

fn act(d: &mut Dash, toggle: bool) {
    let Some(r) = current(d) else {
        return;
    };
    match r {
        Row::Known(k) => {
            let (cur, _) = value(d, k.key);
            match (k.kind, toggle) {
                (Kind::Bool, _) => {
                    let on = matches!(cur.as_str(), "true" | "1" | "yes" | "on");
                    save(d, k.key, if on { "false" } else { "true" });
                }
                // the theme and the accent: the gallery, live across the dashboard
                _ if !toggle && (k.key == "theme" || k.key == "accent") => super::gallery::open(d),
                // the splash: the gallery, each previewed
                _ if !toggle && k.key == "ui.splash" => super::splashes::open(d),
                // enter: every value, with what each does; space: the next one
                (Kind::Choice(opts), false) => open_pick(d, k, opts),
                (Kind::List(opts), _) => open_check(d, k, opts),
                (_, true) => {
                    step(d, 1);
                }
                _ => {
                    d.mode = Mode::Input(forms::Input::new(
                        format!("Set {}", k.key),
                        k.help,
                        raw(d, k.key).unwrap_or_default(),
                        forms::For::Setting(k.key.to_string()),
                    ))
                }
            }
        }
        Row::Extra(k) if !toggle => {
            let help = family_help(&k);
            d.mode = Mode::Input(forms::Input::new(
                format!("Set {k}"),
                &help,
                raw(d, &k).unwrap_or_default(),
                forms::For::Setting(k),
            ));
        }
        Row::Key(a, n, def, _) if !toggle => {
            d.set.capture = Some(Capture {
                act: a,
                name: n,
                def,
                err: None,
            });
        }
        Row::Grid(m) if !toggle => d.set.grid = Some(open_grid(d, m)),
        Row::Add(p) if !toggle => {
            d.mode = Mode::Input(forms::Input::new(
                format!("New {}", p.trim_end_matches('.')),
                add_help(p),
                String::new(),
                forms::For::AddSetting(p.to_string()),
            ));
        }
        _ => {}
    }
}

fn reset(d: &mut Dash) {
    let Some(r) = current(d) else {
        return;
    };
    match r {
        Row::Known(k) => save(d, k.key, ""),
        Row::Extra(k) => save(d, &k, ""),
        Row::Key(_, n, _, _) => save(d, &format!("key.{n}"), ""),
        Row::Add(_) => {}
        Row::Grid(m) => {
            for k in grid_keys(d, m) {
                let _ = d.cfg.unset(&k);
            }
            d.note(format!(
                "{} back to the defaults",
                m.title().trim_start_matches("▦ ")
            ));
        }
    }
}

// ------------------------------------------------------------------ grids

fn grid_hosts(d: &Dash) -> Vec<String> {
    d.fleet.hosts.iter().map(|h| h.alias.clone()).collect()
}

/// The settings a grid writes.
fn grid_keys(d: &Dash, m: Mx) -> Vec<String> {
    match m {
        Mx::Remote => vec!["remote.allow".into()],
        Mx::Analytics => vec!["analytics.include".into(), "analytics.exclude".into()],
        Mx::Moshi => grid_hosts(d)
            .iter()
            .map(|h| format!("host.{h}.moshi"))
            .collect(),
    }
}

/// The grid's columns: agents installed somewhere in the fleet (Claude
/// Code and Codex at least; for analytics, the ones with a log reader), or
/// Moshi's choices.
fn grid_cols(d: &Dash, m: Mx) -> Vec<String> {
    if m == Mx::Moshi {
        return MOSHI.iter().map(|x| x.0.to_string()).collect();
    }
    let mut v: Vec<String> = vec!["claude".into(), "codex".into()];
    for h in grid_hosts(d) {
        let found = d.agents.snapshot(&h).unwrap_or_default();
        let running = d
            .fleet
            .host(&h)
            .map(|x| {
                x.sessions
                    .iter()
                    .map(|s| s.agent.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in found.iter().map(|f| f.id.to_string()).chain(running) {
            if !id.is_empty() && !v.contains(&id) && crate::agents::find(&id).is_some() {
                v.push(id);
            }
        }
    }
    if m == Mx::Analytics {
        v.retain(|a| crate::usage::extract::READERS.contains(&a.as_str()));
    }
    v
}

fn open_grid(d: &Dash, m: Mx) -> Grid {
    let hosts = grid_hosts(d);
    let cols = grid_cols(d, m);
    let g = |k: &str| permit::globs(&d.cfg, k);
    let on = match m {
        Mx::Remote => permit::grid(&g("remote.allow"), &g("remote.deny"), &hosts, &cols, false),
        Mx::Analytics => permit::grid(
            &g("analytics.include"),
            &g("analytics.exclude"),
            &hosts,
            &cols,
            true,
        ),
        Mx::Moshi => hosts
            .iter()
            .map(|h| {
                let v = d.cfg.host_get(h, "moshi").unwrap_or_default();
                let i = MOSHI.iter().position(|x| x.0 == v).unwrap_or(0);
                (0..MOSHI.len()).map(|j| j == i).collect()
            })
            .collect(),
    };
    Grid {
        mx: m,
        hosts,
        cols,
        on,
        r: 0,
        c: 0,
    }
}

/// One line for the list: how much of the grid is on.
fn grid_summary(d: &Dash, m: Mx) -> String {
    let g = open_grid(d, m);
    if g.hosts.is_empty() {
        return "no machines yet".into();
    }
    if m == Mx::Moshi {
        return g
            .hosts
            .iter()
            .zip(&g.on)
            .map(|(h, r)| {
                let j = r.iter().position(|x| *x).unwrap_or(0);
                format!("{h} {}", MOSHI[j].1)
            })
            .collect::<Vec<_>>()
            .join(" · ");
    }
    let n: usize = g.on.iter().map(|r| r.iter().filter(|x| **x).count()).sum();
    let all = g.hosts.len() * g.cols.len();
    match n {
        0 => "nothing".into(),
        x if x == all => "everything".into(),
        x => format!("{x} of {all} on"),
    }
}

/// Keys while a grid is open.
fn grid_key(d: &mut Dash, k: KeyEvent) {
    let Some(g) = d.set.grid.as_mut() else {
        return;
    };
    let (nr, nc) = (g.hosts.len(), g.cols.len());
    if nr == 0 || nc == 0 {
        d.set.grid = None;
        return;
    }
    match k.code {
        KeyCode::Esc => d.set.grid = None,
        KeyCode::Up | KeyCode::Char('k') => g.r = g.r.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => g.r = (g.r + 1).min(nr - 1),
        KeyCode::Left | KeyCode::Char('h') => g.c = g.c.saturating_sub(1),
        KeyCode::Right | KeyCode::Char('l') => g.c = (g.c + 1).min(nc - 1),
        KeyCode::Char(' ') if g.mx == Mx::Moshi => {
            let c = g.c;
            g.on[g.r] = (0..nc).map(|j| j == c).collect();
        }
        KeyCode::Char(' ') => g.on[g.r][g.c] = !g.on[g.r][g.c],
        // a whole row, a whole column
        KeyCode::Char('a') if g.mx != Mx::Moshi => {
            let to = !g.on[g.r].iter().all(|x| *x);
            g.on[g.r].iter_mut().for_each(|x| *x = to);
        }
        KeyCode::Char('A') if g.mx != Mx::Moshi => {
            let c = g.c;
            let to = !g.on.iter().all(|r| r[c]);
            g.on.iter_mut().for_each(|r| r[c] = to);
        }
        KeyCode::Enter => {
            if let Some(g) = d.set.grid.take() {
                save_grid(d, g);
            }
        }
        _ => {}
    }
}

fn put(d: &mut Dash, key: &str, val: &str) -> Result<(), String> {
    let r = if val.is_empty() {
        d.cfg.unset(key)
    } else {
        d.cfg.set(key, val)
    };
    r.map_err(|e| format!("couldn't save {key}: {e}"))
}

fn save_grid(d: &mut Dash, g: Grid) {
    let r = match g.mx {
        Mx::Remote => {
            let keep = permit::globs(&d.cfg, "remote.allow");
            let v = permit::to_globs(&g.hosts, &g.cols, &g.on, &keep).join(",");
            put(d, "remote.allow", &v).map(|_| {
                // a tick remote.deny still blocks says so
                let deny = permit::globs(&d.cfg, "remote.deny");
                let blocked: Vec<String> = g
                    .hosts
                    .iter()
                    .zip(&g.on)
                    .flat_map(|(h, r)| {
                        g.cols
                            .iter()
                            .zip(r)
                            .filter(|(a, on)| **on && permit::matches(&deny, h, a))
                            .map(move |(a, _)| format!("{h}:{a}"))
                    })
                    .collect();
                if blocked.is_empty() {
                    format!(
                        "remote.allow = {}",
                        if v.is_empty() { "nothing" } else { &v }
                    )
                } else {
                    format!("saved — remote.deny still blocks {}", blocked.join(", "))
                }
            })
        }
        Mx::Analytics => {
            let keep = permit::globs(&d.cfg, "analytics.include");
            let (inc, exc) = permit::include_exclude(&g.hosts, &g.cols, &g.on, &keep);
            put(d, "analytics.include", &inc)
                .and_then(|_| put(d, "analytics.exclude", &exc))
                .map(|_| "analytics: what's read is saved".into())
        }
        Mx::Moshi => g
            .hosts
            .iter()
            .zip(&g.on)
            .try_for_each(|(h, r)| {
                let j = r.iter().position(|x| *x).unwrap_or(0);
                let v = MOSHI[j].0;
                put(
                    d,
                    &format!("host.{h}.moshi"),
                    if v == "auto" { "" } else { v },
                )
            })
            .map(|_| "Moshi per machine saved".into()),
    };
    match r {
        Ok(m) => d.note(m),
        Err(e) => d.error(e),
    }
}

/// Keys while waiting for the key to bind.
fn capture_key(d: &mut Dash, k: KeyEvent) {
    let Some(cp) = d.set.capture.as_mut() else {
        return;
    };
    let ks = super::key_str(&k);
    match k.code {
        KeyCode::Esc => {
            d.set.capture = None;
            return;
        }
        KeyCode::Backspace => {
            let n = cp.name;
            d.set.capture = None;
            save(d, &format!("key.{n}"), "");
            return;
        }
        _ => {}
    }
    if ks.is_empty() {
        cp.err = Some("that key can't be bound — try another".into());
        return;
    }
    if RESERVED.contains(&ks.as_str()) {
        cp.err = Some(format!(
            "`{ks}` is the dashboard's own (tabs, help, the palette, quit) — try another"
        ));
        return;
    }
    if let Some(o) = d.keys.owner(&ks).filter(|o| *o != cp.act) {
        cp.err = Some(format!(
            "`{ks}` already runs {} — try another, or give {} a new key first",
            act_name(o),
            act_name(o)
        ));
        return;
    }
    let n = cp.name;
    d.set.capture = None;
    save(d, &format!("key.{n}"), &ks);
}

// ------------------------------------------------------------------ the file

/// `x`: the settings as a TOML file in the home folder, secrets left out.
fn export(d: &mut Dash) {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let now = crate::cache::now();
    let (y, m, dd) = ui::civil(ui::local_day(now));
    let path = std::path::Path::new(&home).join(format!("aleph-settings-{y}-{m:02}-{dd:02}.toml"));
    let text = crate::configcmd::export(&d.cfg, false);
    match config::write_private(&path, text.as_bytes()) {
        Ok(()) => d.note(format!(
            "exported to {} (no secrets) — i imports one",
            crate::doctor::tilde(&path)
        )),
        Err(e) => d.error(format!("couldn't write {}: {e}", path.display())),
    }
}

/// `i`, then a file or URL: what it would change, then a confirm.
pub(crate) fn import_preview(d: &mut Dash, src: &str) -> Result<(), String> {
    let theirs = crate::configcmd::read_entries(src)?;
    let ch = crate::configcmd::changes(&d.cfg.entries(), &theirs);
    if ch.is_empty() {
        d.note("nothing to change — those settings are what you have");
        return Ok(());
    }
    let show = |k: &str, v: &str| {
        if crate::configcmd::is_secret(k) && !v.starts_with("secret:") {
            "…".to_string()
        } else {
            ui::trunc(v, 30)
        }
    };
    let mut parts: Vec<String> = ch
        .iter()
        .take(8)
        .map(|(k, old, new)| match old {
            Some(o) => format!("{k}: {} → {}", show(k, o), show(k, new)),
            None => format!("+ {k} = {}", show(k, new)),
        })
        .collect();
    if ch.len() > 8 {
        parts.push(format!("and {} more", ch.len() - 8));
    }
    let risky: Vec<&str> = ch
        .iter()
        .map(|(k, ..)| k.as_str())
        .filter(|k| crate::configcmd::risky(k))
        .collect();
    let mut text = parts.join(" · ");
    if !risky.is_empty() {
        text.push_str(&format!(
            ". It also sets commands, approvals or remote control ({}): read them before saying yes",
            risky.join(", ")
        ));
    }
    text.push_str(". u undoes it after.");
    d.mode = Mode::Confirm {
        title: format!(
            "Import {} setting{}?",
            ch.len(),
            if ch.len() == 1 { "" } else { "s" }
        ),
        text,
        on_yes: Pending::Import(ch.into_iter().map(|(k, _, v)| (k, v)).collect()),
    };
    Ok(())
}

pub(crate) fn import(d: &mut Dash, v: Vec<(String, String)>) {
    let n = v.len();
    for (k, val) in &v {
        if let Err(e) = d.cfg.set(k, val) {
            d.error(format!("{k}: {e}"));
            return;
        }
    }
    reapply_all(d);
    d.note(format!(
        "{n} setting{} imported — u undoes",
        if n == 1 { "" } else { "s" }
    ));
}

/// `u`: the version before the last change.
fn undo(d: &mut Dash) {
    match crate::configcmd::undo_last(&d.cfg) {
        Ok(m) => {
            d.cfg = config::Config::load();
            reapply_all(d);
            d.note(m);
        }
        Err(e) => d.error(e),
    }
}

/// `f`: the selected check's fix (Health focused), else the section's first.
fn fix(d: &mut Dash) {
    if sec_name(d) == "Doctor" && d.set.search.is_empty() {
        let Some(f) = doctor::selected_fix(d) else {
            d.note(match doctor::selected(&d.doc) {
                Some((_, l)) if !l.fix.is_empty() => {
                    "no fix Aleph can do for that one — its fix says what to do"
                }
                Some(_) => "nothing to fix there",
                None => "pick a check first",
            });
            return;
        };
        d.mode = Mode::Confirm {
            title: "Fix it?".into(),
            text: format!("{}.", f.label()),
            on_yes: Pending::Fix(f),
        };
        return;
    }
    let checks = doctor::for_section(d, sec_name(d));
    let pick = if d.set.focus == 2 {
        checks.get(d.set.hsel).and_then(|(_, l)| l.action.clone())
    } else {
        checks
            .iter()
            .filter(|(_, l)| matches!(l.st, St::Bad | St::Warn | St::Info))
            .find_map(|(_, l)| l.action.clone())
    };
    let Some(f) = pick else {
        d.note(if checks.is_empty() {
            "nothing to fix here"
        } else {
            "no fix Aleph can do for that one — its hint says what to do"
        });
        return;
    };
    d.mode = Mode::Confirm {
        title: "Fix it?".into(),
        text: format!("{}.", f.label()),
        on_yes: Pending::Fix(f),
    };
}

/// Keys while typing a search.
pub(crate) fn search_key(d: &mut Dash, k: KeyEvent) {
    match k.code {
        KeyCode::Esc => {
            d.set.search.clear();
            d.set.searching = false;
            d.set.sel = 0;
        }
        KeyCode::Enter => d.set.searching = false,
        KeyCode::Backspace => {
            d.set.search.pop();
            d.set.sel = 0;
        }
        KeyCode::Char(ch) => {
            d.set.search.push(ch);
            d.set.sel = 0;
            d.set.focus = 1;
        }
        _ => {}
    }
}

pub(crate) fn enter(d: &mut Dash) {
    doctor::ensure(d);
}

/// A click on a section.
pub(crate) fn set_section(d: &mut Dash, i: usize) {
    if i < SECTIONS.len() {
        d.set.sec = i;
        d.set.sel = 0;
        d.set.scroll = 0;
        d.set.hsel = 0;
        d.set.search.clear();
        d.set.focus = 0;
    }
}

/// Open a section by name (`:doctor`).
pub(crate) fn go_section(d: &mut Dash, name: &str) {
    if let Some(i) = SECTIONS.iter().position(|s| *s == name) {
        d.set.sec = i;
        d.set.sel = 0;
        d.set.search.clear();
        // the doctor's list takes the arrows at once
        if name == "Doctor" {
            d.set.focus = 1;
        }
    }
}

/// A click on one of the doctor's checks.
pub(crate) fn click_check(d: &mut Dash, i: usize) {
    doctor::select(d, i);
    d.set.focus = 1;
}

pub(crate) fn click(d: &mut Dash, i: usize, double: bool) {
    let n = rows(d).len();
    if n == 0 {
        return;
    }
    d.set.sel = i.min(n - 1);
    d.set.focus = 1;
    if double {
        act(d, false);
    }
}

fn step_section(d: &mut Dash, fwd: bool) {
    let n = SECTIONS.len();
    d.set.sec = if fwd {
        (d.set.sec + 1) % n
    } else {
        (d.set.sec + n - 1) % n
    };
    d.set.sel = 0;
    d.set.scroll = 0;
    d.set.hsel = 0;
    d.set.search.clear();
}

pub(crate) fn on_key(d: &mut Dash, k: KeyEvent) -> Option<Handoff> {
    if d.set.pick.is_some() {
        pick_key(d, k);
        return None;
    }
    if d.set.check.is_some() {
        check_key(d, k);
        return None;
    }
    if d.set.grid.is_some() {
        grid_key(d, k);
        return None;
    }
    if d.set.capture.is_some() {
        capture_key(d, k);
        return None;
    }
    let doctor_view = sec_name(d) == "Doctor" && d.set.search.is_empty();
    let nrows = rows(d).len();
    let nchecks = doctor::for_section(d, sec_name(d)).len();
    match k.code {
        // ←/→ on a row with steps change its value; elsewhere (and h/l) move panels
        KeyCode::Left if d.set.focus == 1 && !doctor_view && step(d, -1) => {}
        KeyCode::Right if d.set.focus == 1 && !doctor_view && step(d, 1) => {}
        KeyCode::Left | KeyCode::Char('h') => d.set.focus = d.set.focus.saturating_sub(1),
        KeyCode::Right | KeyCode::Char('l') | KeyCode::F(6) => {
            let max = if nchecks > 0 && !doctor_view { 2 } else { 1 };
            d.set.focus = (d.set.focus + 1).min(max);
        }
        KeyCode::Char('[') => step_section(d, false),
        KeyCode::Char(']') => step_section(d, true),
        // the Doctor: by machine ⇄ by agent
        KeyCode::Char('v') if sec_name(d) == "Doctor" && d.set.search.is_empty() => {
            doctor::toggle_by_agent(d)
        }
        KeyCode::Char('/') => {
            d.set.searching = true;
            d.set.search.clear();
        }
        KeyCode::Char('r') => {
            doctor::run(d);
            d.note("checking every machine again…");
        }
        KeyCode::Char('f') => fix(d),
        KeyCode::Up | KeyCode::Char('k') => match d.set.focus {
            0 => step_section(d, false),
            _ if doctor_view => doctor::step(d, -1),
            2 => d.set.hsel = d.set.hsel.saturating_sub(1),
            _ => d.set.sel = d.set.sel.saturating_sub(1),
        },
        KeyCode::Down | KeyCode::Char('j') => match d.set.focus {
            0 => step_section(d, true),
            _ if doctor_view => doctor::step(d, 1),
            2 => d.set.hsel = (d.set.hsel + 1).min(nchecks.saturating_sub(1)),
            _ => d.set.sel = (d.set.sel + 1).min(nrows.saturating_sub(1)),
        },
        KeyCode::PageDown if doctor_view => {
            d.set.focus = 1;
            doctor::step(d, d.doc.page as isize / 2);
        }
        KeyCode::PageUp if doctor_view => {
            d.set.focus = 1;
            doctor::step(d, -(d.doc.page as isize / 2));
        }
        KeyCode::Home if doctor_view => doctor::select(d, 0),
        KeyCode::End if doctor_view => doctor::select(d, usize::MAX),
        KeyCode::Enter => match d.set.focus {
            0 => d.set.focus = 1,
            2 => fix(d),
            // a machine's (or agent's) name opens or folds its block; a check, its fix
            _ if doctor_view => {
                if !doctor::toggle_open(d) {
                    fix(d)
                }
            }
            _ => act(d, false),
        },
        KeyCode::Char(' ') if d.set.focus == 1 && k.modifiers.contains(KeyModifiers::SHIFT) => {
            step(d, -1);
        }
        KeyCode::Char(' ') if d.set.focus == 1 => act(d, true),
        KeyCode::Char('d') | KeyCode::Backspace if d.set.focus == 1 => reset(d),
        KeyCode::Char('E') => return Some(Handoff::EditConfig),
        KeyCode::Char('u') => undo(d),
        KeyCode::Char('x') => export(d),
        KeyCode::Char('i') => d.mode = Mode::Input(forms::Input::new(
            "Import settings".into(),
            "a TOML file or an https:// URL — what it changes is shown before anything is saved",
            String::new(),
            forms::For::Import,
        )),
        KeyCode::Char('S') => match step_of(sec_name(d)) {
            Some(st) => return Some(Handoff::Setup(st)),
            None => d.note("no setup step for this section"),
        },
        _ => {}
    }
    None
}

// ------------------------------------------------------------------ drawing

pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect) {
    let r = r.intersection(f.area());
    if r.width < 20 || r.height < 6 {
        return;
    }
    let doctor_view = sec_name(d) == "Doctor" && d.set.search.is_empty();
    if r.width >= 100 {
        let sw = if r.width >= 140 { 24 } else { 22 };
        let [secs, rest] =
            Layout::horizontal([Constraint::Length(sw), Constraint::Min(40)]).areas(r);
        draw_sections(d, f, secs);
        if doctor_view {
            doctor::draw(d, f, rest, d.set.focus >= 1);
            return;
        }
        // the layout keys: p hides the detail and health, z maximises
        let shape = super::panes::shape(d, "settings");
        if r.width >= 140 {
            let rw = (rest.width * 44 / 100).clamp(44, 90);
            let (list, right) = shape.columns(rest, rw, 0);
            if list.width > 0 {
                draw_list(d, f, list);
            }
            if right.width > 0 {
                let [detail, health] =
                    Layout::vertical([Constraint::Percentage(45), Constraint::Min(5)]).areas(right);
                draw_detail(d, f, detail);
                draw_health(d, f, health);
            }
        } else {
            use super::panes::Max;
            let rh = match shape.max {
                Max::Side => rest.height,
                Max::Main => 0,
                _ if shape.hidden => 0,
                _ => 12,
            };
            let [list, right] =
                Layout::vertical([Constraint::Min(0), Constraint::Length(rh)]).areas(rest);
            if list.height > 0 {
                draw_list(d, f, list);
            }
            if right.height > 0 {
                let [detail, health] =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .areas(right);
                draw_detail(d, f, detail);
                draw_health(d, f, health);
            }
        }
    } else {
        // narrow: the section as a chip line, then the list, then the detail
        let [bar, list, detail] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(7),
        ])
        .areas(r);
        let s = sec_name(d);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ‹ ", ui::faint()),
                Span::styled(format!(" {s} "), ui::selected().fg(c::ACCENT())),
                Span::styled(" ›  [ ] changes the section", ui::faint()),
            ])),
            bar,
        );
        if doctor_view {
            let [rest] = Layout::vertical([Constraint::Min(1)]).areas(Rect {
                height: list.height + detail.height,
                ..list
            });
            doctor::draw(d, f, rest, d.set.focus >= 1);
            return;
        }
        draw_list(d, f, list);
        draw_detail(d, f, detail);
    }
    draw_overlay(d, f, r);
}

/// The picker or the checklist, over the view (true: one was drawn).
fn draw_choices(d: &Dash, f: &mut Frame, area: Rect) -> bool {
    type Rows<'a> = Vec<(bool, &'a (String, String))>;
    let (key, rows, sel, check): (&str, Rows, usize, bool) = if let Some(p) = &d.set.pick {
        (
            p.key,
            p.opts
                .iter()
                .enumerate()
                .map(|(i, o)| (i == p.now, o))
                .collect(),
            p.sel,
            false,
        )
    } else if let Some(c) = &d.set.check {
        (
            c.key,
            c.on.iter().copied().zip(c.opts.iter()).collect(),
            c.sel,
            true,
        )
    } else {
        return false;
    };
    let w = (area.width as usize).saturating_sub(6).clamp(40, 96);
    let nw = rows
        .iter()
        .map(|(_, o)| ui::width(&o.0))
        .max()
        .unwrap_or(4)
        .min(22);
    let mut lines: Vec<Line> = Vec::new();
    for (i, (on, (name, what))) in rows.iter().enumerate() {
        let here = i == sel;
        let mark = match (check, on) {
            (true, true) => "☑",
            (true, false) => "☐",
            (false, true) => "●",
            (false, false) => "○",
        };
        let what = ui::trunc(what, w.saturating_sub(nw + 10));
        let mut l = Line::from(vec![
            Span::styled(if here { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(
                format!(" {mark} "),
                ui::fg(if *on { c::ACCENT() } else { c::FAINT() }),
            ),
            Span::styled(
                ui::pad(name, nw),
                if *on { ui::bold() } else { Style::new() },
            ),
            Span::raw("  "),
            Span::styled(what, ui::muted()),
        ]);
        if here {
            l = l.style(ui::selected());
        }
        lines.push(l);
    }
    lines.push(Line::raw(""));
    lines.push(ui::hints(&if check {
        vec![
            ("space", "tick"),
            ("a", "all"),
            ("n", "none"),
            ("enter", "save"),
            ("esc", "cancel"),
        ]
    } else {
        vec![("↑↓", "choose"), ("enter", "set it"), ("esc", "cancel")]
    }));
    let block = ui::panel(format!("Set {key}"), true);
    let h = (lines.len() as u16 + chrome(&block)).min(area.height.saturating_sub(2));
    let r = ui::centered(area, w as u16 + 4, h);
    crate::ui::clear(f, r);
    f.render_widget(Paragraph::new(lines).block(block), r);
    true
}

/// The grid or the key capture, over the view.
fn draw_overlay(d: &Dash, f: &mut Frame, area: Rect) {
    if draw_choices(d, f, area) {
        return;
    }
    if let Some(cp) = &d.set.capture {
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Press the key for ", ui::muted()),
                Span::styled(cp.name, ui::bold()),
            ]),
            Line::from(Span::styled(
                format!("now {} · default {}", d.keys.key_for(cp.act), cp.def),
                ui::faint(),
            )),
            Line::raw(""),
        ];
        if let Some(e) = &cp.err {
            for l in ui::wrap(e, 52) {
                lines.push(Line::from(Span::styled(l, ui::fg(c::RED()))));
            }
            lines.push(Line::raw(""));
        }
        lines.push(ui::hints(&[
            ("any key", "bind it"),
            ("backspace", "default"),
            ("esc", "cancel"),
        ]));
        let block = ui::panel("Bind a key", true);
        let r = ui::centered(area, 58, lines.len() as u16 + chrome(&block));
        crate::ui::clear(f, r);
        f.render_widget(Paragraph::new(lines).block(block), r);
        return;
    }
    let Some(g) = &d.set.grid else {
        return;
    };
    let hw = g
        .hosts
        .iter()
        .map(|h| ui::width(h))
        .max()
        .unwrap_or(6)
        .clamp(6, 18);
    let label = |c: &str| -> String {
        match g.mx {
            Mx::Moshi => MOSHI
                .iter()
                .find(|x| x.0 == c)
                .map(|x| x.1.to_string())
                .unwrap_or_default(),
            _ => crate::agents::label(c),
        }
    };
    let cw: Vec<usize> = g
        .cols
        .iter()
        .map(|c| ui::width(&label(c)).clamp(5, 14))
        .collect();
    let w = (hw + 4 + cw.iter().map(|x| x + 2).sum::<usize>())
        .max(64)
        .min((area.width as usize).saturating_sub(4));
    let mut lines: Vec<Line> = ui::wrap(g.mx.help(), w.saturating_sub(2))
        .into_iter()
        .map(|l| Line::from(Span::styled(l, ui::muted())))
        .collect();
    lines.push(Line::raw(""));
    let mut head = vec![Span::raw(" ".repeat(hw + 3))];
    for (j, col) in g.cols.iter().enumerate() {
        let mut st = ui::faint().add_modifier(Modifier::BOLD);
        if j == g.c {
            st = ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD);
        }
        head.push(Span::styled(
            ui::pad(&ui::trunc(&label(col), cw[j]), cw[j] + 2),
            st,
        ));
    }
    lines.push(Line::from(head));
    for (i, h) in g.hosts.iter().enumerate() {
        let mut l = vec![
            Span::styled(if i == g.r { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
            Span::styled(
                ui::pad(&ui::trunc(h, hw), hw + 1),
                ui::fg(crate::theme::machine(&d.cfg, h)).add_modifier(Modifier::BOLD),
            ),
        ];
        for (j, on) in g.on[i].iter().enumerate() {
            let mark = match (g.mx, *on) {
                (Mx::Moshi, true) => "◉",
                (Mx::Moshi, false) => "○",
                (_, true) => "☑",
                (_, false) => "☐",
            };
            let mut st = if *on {
                ui::fg(c::GREEN()).add_modifier(Modifier::BOLD)
            } else {
                ui::faint()
            };
            if i == g.r && j == g.c {
                st = st.patch(ui::selected()).fg(c::ACCENT());
            }
            l.push(Span::styled(format!(" {mark} "), st));
            l.push(Span::raw(" ".repeat(cw[j].saturating_sub(1))));
        }
        lines.push(Line::from(l));
    }
    if g.hosts.is_empty() {
        lines.push(Line::from(Span::styled(
            "no machines in the fleet yet",
            ui::muted(),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(ui::hints(if g.mx == Mx::Moshi {
        &[("space", "choose"), ("enter", "save"), ("esc", "cancel")][..]
    } else {
        &[
            ("space", "tick"),
            ("a · A", "row · column"),
            ("enter", "save"),
            ("esc", "cancel"),
        ][..]
    }));
    let title = match g.mx {
        Mx::Remote => "Remote control · machines × agents",
        Mx::Analytics => "Analytics · machines × agents",
        Mx::Moshi => "Moshi · per machine",
    };
    let block = ui::panel(title, true);
    let r = ui::centered(area, w as u16 + 4, lines.len() as u16 + chrome(&block));
    crate::ui::clear(f, r);
    f.render_widget(Paragraph::new(lines).block(block), r);
}

/// Rows a block's borders and padding take.
fn chrome(b: &ratatui::widgets::Block) -> u16 {
    let r = Rect::new(0, 0, 100, 100);
    100 - b.inner(r).height
}

fn draw_sections(d: &mut Dash, f: &mut Frame, r: Rect) {
    let on = d.set.focus == 0 && d.set.search.is_empty();
    let block = ui::panel("Settings", on);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    for (i, s) in SECTIONS.iter().enumerate() {
        if (i as u16) < inner.height {
            d.hits.chips.push((
                Rect::new(inner.x, inner.y + i as u16, inner.width, 1),
                super::Chip::Section(i),
            ));
        }
        let sel = i == d.set.sec && d.set.search.is_empty();
        let (mark, col) = if *s == "Doctor" {
            match d.doc.sum() {
                None if d.doc.running() => (ui::spinner(d.t0).to_string(), c::ACCENT()),
                Some(x) if x.bad > 0 => (format!("✗{}", x.bad), c::RED()),
                Some(x) if x.warn > 0 => (format!("!{}", x.warn), c::YELLOW()),
                Some(_) => ("✓".to_string(), c::GREEN()),
                None => (String::new(), c::FAINT()),
            }
        } else {
            match doctor::counts(d, s) {
                (b, _) if b > 0 => (format!("✗{b}"), c::RED()),
                (_, w) if w > 0 => (format!("!{w}"), c::YELLOW()),
                _ => (String::new(), c::FAINT()),
            }
        };
        let name_w = w.saturating_sub(ui::width(&mark) + 3);
        let mut l = vec![
            Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(
                ui::pad(&ui::trunc(s, name_w), name_w),
                if sel {
                    ui::bold().fg(if on { c::ACCENT() } else { c::TEXT() })
                } else {
                    ui::muted()
                },
            ),
            Span::raw(" "),
            Span::styled(mark, ui::fg(col).add_modifier(Modifier::BOLD)),
        ];
        if sel {
            l = l
                .into_iter()
                .map(|s| s.patch_style(ui::selected()))
                .collect();
        }
        lines.push(Line::from(l));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_list(d: &mut Dash, f: &mut Frame, r: Rect) {
    let rs = rows(d);
    let on = d.set.focus == 1 || !d.set.search.is_empty();
    let title = if d.set.searching || !d.set.search.is_empty() {
        Line::from(vec![
            Span::styled(" / ", ui::fg(c::ACCENT())),
            Span::styled(d.set.search.clone(), ui::bold()),
            Span::styled(if d.set.searching { "▏" } else { "" }, ui::fg(c::ACCENT())),
            Span::styled(format!("  {} found", rs.len()), ui::muted()),
        ])
    } else {
        Line::from(vec![
            Span::raw(sec_name(d).to_string()),
            Span::styled(
                format!("  {}", crate::doctor::tilde(&d.cfg.path)),
                ui::faint(),
            ),
        ])
    };
    let block = ui::panel(title, on);
    let inner = block.inner(r);
    f.render_widget(block, r);
    if rs.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                if d.set.search.is_empty() {
                    "nothing to set here yet"
                } else {
                    "nothing matches — esc clears the search"
                },
                ui::muted(),
            ))),
            inner,
        );
        return;
    }
    d.set.sel = d.set.sel.min(rs.len() - 1);
    let h = inner.height as usize;
    if d.set.sel < d.set.scroll {
        d.set.scroll = d.set.sel;
    } else if h > 0 && d.set.sel >= d.set.scroll + h {
        d.set.scroll = d.set.sel + 1 - h;
    }
    let kw = rs
        .iter()
        .map(|(r, _)| ui::width(&r.key()))
        .max()
        .unwrap_or(10)
        .clamp(10, 26);
    let w = inner.width as usize;
    let searching = !d.set.search.is_empty();
    let mut lines = Vec::new();
    for (i, (row, sec)) in rs.iter().enumerate().skip(d.set.scroll).take(h) {
        let set = is_set(d, row);
        let v = shown(d, row);
        let wrong = bad(d, row).is_some();
        let mut l = vec![
            Span::styled(if i == d.set.sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(
                ui::pad(&ui::trunc(&row.key(), kw), kw),
                match row {
                    Row::Add(_) | Row::Grid(_) => ui::fg(c::ACCENT()),
                    _ if set => ui::bold(),
                    _ => Style::new(),
                },
            ),
            Span::raw("  "),
        ];
        let tail = if searching { ui::width(sec) + 2 } else { 0 };
        let vw = w.saturating_sub(kw + 4 + tail);
        let vtext = match row {
            Row::Add(p) => add_help(p).to_string(),
            _ if wrong => format!("✗ {v}"),
            _ => v,
        };
        // a list's values as chips
        if let (Row::Known(k), false) = (row, wrong) {
            if let Kind::List(_) = k.kind {
                let mut used = 0;
                for item in vtext.split(',').map(str::trim).filter(|x| !x.is_empty()) {
                    let chip = format!(" {item} ");
                    let cw = ui::width(&chip) + 1;
                    if used + cw > vw {
                        l.push(Span::styled("…", ui::faint()));
                        used += 1;
                        break;
                    }
                    let st = if matches!(item, "off" | "none") {
                        ui::faint()
                    } else {
                        Style::new()
                            .fg(if set { c::ACCENT() } else { c::TEXT() })
                            .bg(c::SEL())
                    };
                    l.push(Span::styled(chip, st));
                    l.push(Span::raw(" "));
                    used += cw;
                }
                l.push(Span::raw(" ".repeat(vw.saturating_sub(used))));
                if searching {
                    l.push(Span::styled(format!("  {sec}"), ui::faint()));
                }
                let mut line = Line::from(l);
                if i == d.set.sel {
                    line = line.style(ui::selected());
                }
                lines.push(line);
                continue;
            }
        }
        l.push(Span::styled(
            ui::pad(&ui::trunc(&vtext, vw), vw),
            match row {
                Row::Add(_) => ui::faint(),
                _ if wrong => ui::fg(c::RED()).add_modifier(Modifier::BOLD),
                _ if set => ui::fg(c::ACCENT()),
                _ => ui::muted(),
            },
        ));
        if searching {
            l.push(Span::styled(format!("  {sec}"), ui::faint()));
        }
        let mut line = Line::from(l);
        if i == d.set.sel {
            line = line.style(ui::selected());
        }
        lines.push(line);
    }
    f.render_widget(Paragraph::new(lines), inner);
    d.hits.list = inner;
    d.hits.list_first = inner.y;
    d.hits.list_offset = d.set.scroll;
    d.hits.list_len = rs.len();
}

fn draw_detail(d: &Dash, f: &mut Frame, r: Rect) {
    let Some(row) = current(d) else {
        f.render_widget(ui::panel("", false), r);
        return;
    };
    let key = row.key();
    let mut l: Vec<Line> = vec![
        Line::from(Span::styled(row.help(), ui::bold())),
        Line::raw(""),
    ];
    let kv = |k: &str, v: Span<'static>| -> Line<'static> {
        Line::from(vec![Span::styled(format!("{k:<9}"), ui::faint()), v])
    };
    match &row {
        Row::Known(k) => {
            let kind = match k.kind {
                Kind::Text => "text".to_string(),
                Kind::Bool => "on / off".to_string(),
                Kind::Num => "number".to_string(),
                Kind::Choice(o) => o.join(" · "),
                Kind::List(o) => format!("a list of {}", o.join(" · ")),
            };
            l.push(kv(
                "default",
                Span::raw(
                    if k.default.is_empty() {
                        "—"
                    } else {
                        k.default
                    }
                    .to_string(),
                ),
            ));
            l.push(kv("values", Span::raw(kind)));
            l.push(kv("now", now_span(d, k.key)));
            l.push(kv("from", source_span(d, k.key)));
            if RESTART.contains(&k.key) {
                l.push(Line::from(Span::styled(
                    "applies on restart",
                    ui::fg(c::YELLOW()),
                )));
            }
            l.extend(preview(d, k.key));
        }
        Row::Extra(k) => {
            l.push(kv("now", now_span(d, k)));
            l.push(kv("from", source_span(d, k)));
            l.push(Line::from(Span::styled(
                "d removes it · enter edits it",
                ui::faint(),
            )));
        }
        Row::Key(a, n, def, _) => {
            l.push(kv("default", Span::raw(def.to_string())));
            l.push(kv(
                "now",
                Span::styled(d.keys.key_for(*a), ui::fg(c::ACCENT())),
            ));
            l.push(kv("setting", Span::styled(format!("key.{n}"), ui::muted())));
        }
        Row::Add(_) => {
            l.push(Line::from(Span::styled(
                "enter adds one",
                ui::fg(c::ACCENT()),
            )));
        }
        Row::Grid(m) => {
            l.push(kv(
                "now",
                Span::styled(grid_summary(d, *m), ui::fg(c::ACCENT())),
            ));
            l.push(kv(
                "settings",
                Span::styled(grid_keys(d, *m).join(" · "), ui::muted()),
            ));
            l.push(Line::from(Span::styled(
                "enter opens the grid · d resets it",
                ui::faint(),
            )));
        }
    }
    if let Some(e) = bad(d, &row) {
        l.push(Line::raw(""));
        l.push(Line::from(Span::styled(
            format!("✗ {e}"),
            ui::fg(c::RED()).add_modifier(Modifier::BOLD),
        )));
        l.push(Line::from(Span::styled(
            if matches!(row, Row::Key(..)) {
                "enter binds another key"
            } else {
                "enter fixes it · d puts the default back"
            },
            ui::faint(),
        )));
    }
    f.render_widget(
        Paragraph::new(l)
            .wrap(Wrap { trim: false })
            .block(ui::panel(key.as_str(), false)),
        r,
    );
}

fn now_span(d: &Dash, key: &str) -> Span<'static> {
    match raw(d, key) {
        Some(v) if v.starts_with("secret:") => {
            Span::styled(format!("{v}  (kept in the keystore)"), ui::fg(c::ACCENT()))
        }
        Some(v) => Span::styled(v, ui::fg(c::ACCENT())),
        None => Span::styled("default", ui::muted()),
    }
}

/// The look settings, shown as they are now (they apply as you change them).
fn preview(d: &Dash, key: &str) -> Vec<Line<'static>> {
    let mut v = Vec::new();
    match key {
        "theme" | "accent" => {
            v.push(Line::raw(""));
            let mut sw = vec![Span::styled("preview  ", ui::faint())];
            for (name, col) in [
                ("accent", c::ACCENT()),
                ("working", c::WORKING()),
                ("needs you", c::NEEDS()),
                ("done", c::DONE()),
                ("down", c::RED()),
                ("limit", c::LIMIT()),
            ] {
                sw.push(Span::styled("■ ", ui::fg(col)));
                sw.push(Span::styled(format!("{name}  "), ui::muted()));
            }
            v.push(Line::from(sw));
            v.push(Line::from(vec![
                Span::styled("         ", ui::faint()),
                Span::styled("┃", ui::fg(c::ACCENT())),
                Span::styled(" ignite ", ui::selected().add_modifier(Modifier::BOLD)),
                Span::styled(
                    format!(" {} working ", ui::spinner(d.t0)),
                    ui::selected().fg(c::WORKING()),
                ),
            ]));
        }
        "ui.spinner" => v.push(Line::from(vec![
            Span::styled("preview  ", ui::faint()),
            Span::styled(ui::spinner(d.t0), ui::fg(c::ACCENT())),
            Span::styled(" looking at every machine…", ui::muted()),
        ])),
        "icons" | "ui.logos" => {
            let mut sw = vec![Span::styled("preview  ", ui::faint())];
            for a in ["claude", "codex", "opencode", "pi", "gemini"] {
                sw.push(crate::icons::agent(a));
                sw.push(Span::styled(
                    format!(" {}  ", crate::agents::label(a)),
                    ui::muted(),
                ));
            }
            v.push(Line::from(sw));
        }
        _ => {}
    }
    v
}

fn source_span(d: &Dash, key: &str) -> Span<'static> {
    if let Some((name, v)) = config::env_override(key) {
        return Span::styled(
            format!("env {name} = {v} (wins over the file, this run)"),
            ui::fg(c::YELLOW()),
        );
    }
    match d.cfg.source(key) {
        Some(Source::Main) => Span::raw(crate::doctor::tilde(&d.cfg.path)),
        Some(Source::Include(p)) => Span::raw(format!("include {}", crate::doctor::tilde(&p))),
        Some(Source::Profile(p)) => Span::raw(format!("profile {}", crate::doctor::tilde(&p))),
        None => Span::styled("built-in default", ui::muted()),
    }
}

/// The section's checks, from the doctor (`f` fixes the selected one).
fn draw_health(d: &Dash, f: &mut Frame, r: Rect) {
    let s = sec_name(d);
    let checks = doctor::for_section(d, s);
    let on = d.set.focus == 2;
    let block = ui::panel(format!("Health · {s}"), on);
    let inner = block.inner(r);
    f.render_widget(block, r);
    let w = inner.width as usize;
    let mut lines: Vec<Line> = Vec::new();
    if checks.is_empty() {
        lines.push(Line::from(Span::styled(
            if d.doc.running() {
                format!("{} checking…", ui::spinner(d.t0))
            } else {
                "no checks for this section".into()
            },
            ui::muted(),
        )));
    }
    let hsel = d.set.hsel.min(checks.len().saturating_sub(1));
    let mut last_head = "";
    for (i, (head, line)) in checks.iter().enumerate() {
        if *head != last_head && !head.is_empty() {
            lines.push(Line::from(Span::styled(head.to_string(), ui::faint())));
            last_head = head;
        }
        let k = crate::doctor::Kind::of(line);
        let (m, st) = k.mark();
        let b = crate::doctor::brief(head, line);
        let fact = [
            b.fact.clone(),
            b.version.clone().unwrap_or_default(),
            b.extra.join(" · "),
        ]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        let sel = on && i == hsel;
        let mut l = vec![
            Span::styled(if sel { "┃" } else { " " }, ui::fg(c::ACCENT())),
            Span::styled(format!("{m} "), st),
            Span::styled(ui::pad(&ui::trunc(&b.title, 14), 14), ui::bold()),
            Span::raw(" "),
            Span::styled(
                ui::trunc(&fact, w.saturating_sub(20)),
                match k {
                    crate::doctor::Kind::Fail | crate::doctor::Kind::Warn => ui::fg(k.color()),
                    _ => ui::muted(),
                },
            ),
        ];
        if sel {
            l = l
                .into_iter()
                .map(|s| s.patch_style(ui::selected()))
                .collect();
        }
        lines.push(Line::from(l));
        if sel || (!on && matches!(line.st, St::Bad | St::Warn)) {
            if let Some(a) = &line.action {
                lines.push(Line::from(vec![
                    Span::raw("   "),
                    ui::keycap("f"),
                    Span::styled(
                        format!(" {}", ui::trunc(&a.label(), w.saturating_sub(8))),
                        ui::fg(c::ACCENT()),
                    ),
                ]));
            } else if let Some(h) = line.fix.first() {
                lines.push(Line::from(Span::styled(
                    format!("   hint: {}", ui::trunc(h, w.saturating_sub(10))),
                    ui::faint(),
                )));
            }
        }
    }
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_say_what_they_do_from_the_help() {
        let d = described(
            "the cockpit's effects: tunnel (warp rings), shock (dropping out), waves (gravitational waves (a chirp) ripple), comet",
            &["tunnel", "waves", "comet", "nope"],
        );
        assert_eq!(d[0], ("tunnel".into(), "warp rings".into()));
        assert_eq!(d[1].1, "gravitational waves (a chirp) ripple");
        assert_eq!(d[2].1, "");
        assert_eq!(d[3].1, "");
    }

    #[test]
    fn lists_tick_and_save_in_their_own_words() {
        let fx = ["tunnel", "shock", "glow"];
        assert_eq!(ticked("all", &fx), vec![true, true, true]);
        assert_eq!(ticked("off", &fx), vec![false, false, false]);
        assert_eq!(ticked("all, -shock", &fx), vec![true, false, true]);
        assert_eq!(ticked("glow,tunnel", &fx), vec![true, false, true]);
        let o = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            list_value("ui.splash_fx", &o(&fx), &[true, true, true]),
            "all"
        );
        assert_eq!(
            list_value("ui.splash_fx", &o(&fx), &[false, false, false]),
            "off"
        );
        assert_eq!(
            list_value("ui.splash_fx", &o(&fx), &[true, false, true]),
            "tunnel,glow"
        );
        // showcase's own lists, only in a build that has it
        #[cfg(feature = "showcase")]
        {
            // name kinds: every kind is "any"; your own words alone are "mine"
            let kinds = o(&["ais", "people", "ships", "places", "things", "mine"]);
            assert_eq!(ticked("any", &["ais", "mine"]), vec![true, false]);
            assert_eq!(
                list_value(
                    "showcase.machines",
                    &kinds,
                    &[true, true, true, true, true, false]
                ),
                "any"
            );
            assert_eq!(
                list_value(
                    "showcase.machines",
                    &kinds,
                    &[false, false, false, false, false, true]
                ),
                "mine"
            );
            assert_eq!(
                list_value(
                    "showcase.machines",
                    &kinds,
                    &[false, false, true, false, false, true]
                ),
                "ships,mine"
            );
            // the watermark's places: none is off, all of them listed
            let at = o(&["tui", "raycast", "web"]);
            assert_eq!(
                list_value("showcase.watermark_on", &at, &[true, true, true]),
                "tui,raycast,web"
            );
            assert_eq!(
                list_value("showcase.watermark_on", &at, &[false, false, false]),
                "off"
            );
        }
    }

    #[test]
    fn values_step_by_their_kind() {
        let k = |key: &str| config::known(key).unwrap();
        assert_eq!(
            stepped(k("ui.splash"), "cockpit", 1).as_deref(),
            Some("classic")
        );
        assert_eq!(
            stepped(k("ui.splash"), "cockpit", -1).as_deref(),
            Some("theme")
        );
        assert_eq!(stepped(k("ui.splash"), "theme", -1).as_deref(), Some("off"));
        assert_eq!(stepped(k("mouse"), "true", 1).as_deref(), Some("false"));
        // durations along the ladder
        assert_eq!(
            stepped(k("ui.splash_hold"), "700ms", 1).as_deref(),
            Some("1s")
        );
        assert_eq!(
            stepped(k("ui.splash_hold"), "700ms", -1).as_deref(),
            Some("500ms")
        );
        assert_eq!(
            stepped(k("ui.splash_hold"), "100ms", -1).as_deref(),
            Some("0")
        );
        assert_eq!(
            stepped(k("inbox.needs_ttl"), "5m", 1).as_deref(),
            Some("10m")
        );
        // text that isn't a duration has no steps
        assert_eq!(stepped(k("ui.splash_fx"), "all", 1), None);
    }

    #[test]
    fn default_keys_never_clash() {
        let mut seen: std::collections::HashMap<&str, &str> = Default::default();
        for (_, name, def, _) in ACTS {
            for k in def.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                assert!(
                    !RESERVED.contains(&k),
                    "{name}'s default `{k}` is the dashboard's own"
                );
                if let Some(other) = seen.insert(k, name) {
                    panic!("`{k}` is the default for both {other} and {name}");
                }
            }
        }
    }
}
