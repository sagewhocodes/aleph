//! What Monitor › Timeline lists, and how: this machine or all of them
//! (`timeline.mine`), the conversation beside the list or not
//! (`timeline.conversation`), grouped by machine, project, agent or account
//! (`timeline.group`), and narrowed the way Usage is: any of its machines, agents,
//! accounts and projects, from a picker. A bar of chips on top says what's on; each
//! one changes it, and so do their keys.

use super::usage_filters::{Dim, Item, Picker};
use super::Dash;
use crate::herdr::Session;
use crate::timeline::Brief;
use crate::ui::{self, c};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;
use std::collections::{BTreeMap, BTreeSet};

/// What the list is grouped by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum By {
    Machine,
    Project,
    Agent,
    Account,
}

impl By {
    pub fn of(cfg: &crate::config::Config) -> By {
        match cfg.get_or("timeline.group").as_str() {
            "project" => By::Project,
            "agent" => By::Agent,
            "account" => By::Account,
            _ => By::Machine,
        }
    }
    pub fn word(self) -> &'static str {
        match self {
            By::Machine => "machine",
            By::Project => "project",
            By::Agent => "agent",
            By::Account => "account",
        }
    }
    pub fn next(self) -> By {
        match self {
            By::Machine => By::Project,
            By::Project => By::Agent,
            By::Agent => By::Account,
            By::Account => By::Machine,
        }
    }
}

/// What a row of the list is about.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Facts {
    pub host: String,
    pub agent: String,
    /// its project's name (empty: no project known)
    pub project: String,
    /// its account's id (empty: none known)
    pub acct: String,
}

impl Facts {
    /// Its group's key.
    pub fn key(&self, by: By) -> &str {
        match by {
            By::Machine => &self.host,
            By::Project => &self.project,
            By::Agent => &self.agent,
            By::Account => &self.acct,
        }
    }
}

/// A group's heading.
pub(crate) fn label(by: By, key: &str) -> String {
    match by {
        By::Machine => crate::showcase::shown(key).into_owned(),
        By::Project if key.is_empty() => "no project".into(),
        By::Project => crate::showcase::text(key).into_owned(),
        By::Agent => crate::agents::label(key),
        By::Account if key.is_empty() => "no account known".into(),
        By::Account => crate::usage::accounts::name(key),
    }
}

/// What the list is narrowed to: any of each, like Usage's filters.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Narrow {
    pub hosts: Vec<String>,
    pub agents: Vec<String>,
    pub accounts: Vec<String>,
    /// project names (a live session's folder has no id until it's been read)
    pub projects: Vec<String>,
}

impl Narrow {
    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
            && self.agents.is_empty()
            && self.accounts.is_empty()
            && self.projects.is_empty()
    }

    pub fn clear(&mut self) {
        *self = Narrow::default();
    }

    /// Each list given must hold its value (pure — tested).
    pub fn keeps(&self, f: &Facts) -> bool {
        let one = |v: &[String], x: &str| v.is_empty() || v.iter().any(|y| y == x);
        one(&self.hosts, &f.host)
            && one(&self.agents, &f.agent)
            && one(&self.accounts, &f.acct)
            && one(&self.projects, &f.project)
    }

    /// Every (kind, value) in it, in the order the chips show them.
    fn flat(&self) -> Vec<(char, String)> {
        let mut v = Vec::new();
        for (k, l) in [
            ('h', &self.hosts),
            ('a', &self.agents),
            ('c', &self.accounts),
            ('p', &self.projects),
        ] {
            v.extend(l.iter().map(|x| (k, x.clone())));
        }
        v
    }

    fn drop_one(&mut self, kind: char, value: &str) {
        let l = match kind {
            'h' => &mut self.hosts,
            'a' => &mut self.agents,
            'c' => &mut self.accounts,
            _ => &mut self.projects,
        };
        l.retain(|x| x != value);
    }
}

// ------------------------------------------------------------------ what a row is

fn brief_of<'a>(d: &'a Dash, s: &Session) -> Option<&'a Brief> {
    let id = s.native_id.as_deref()?;
    d.tl.hist
        .iter()
        .find(|b| b.host == s.host && b.agent == s.agent && b.session == id)
}

/// A live session. Its conversation says which project and account; before that's
/// been read, its folder's name stands in for the project.
pub(crate) fn of_live(d: &Dash, s: &Session) -> Facts {
    let b = brief_of(d, s);
    let folder = || {
        let p = crate::path::basename(s.cwd.as_bytes());
        if s.cwd == "-" || p.is_empty() {
            String::new()
        } else {
            crate::path::show(p)
        }
    };
    Facts {
        host: s.host.clone(),
        agent: s.agent.clone(),
        project: b.map_or_else(folder, |b| b.project.clone()),
        acct: b.map_or(String::new(), |b| b.acct.clone()),
    }
}

/// An earlier conversation.
pub(crate) fn of_past(b: &Brief) -> Facts {
    Facts {
        host: b.host.clone(),
        agent: b.agent.clone(),
        project: b.project.clone(),
        acct: b.acct.clone(),
    }
}

/// A session that ended: the state log has no folder, so the project and account
/// are the conversation of that agent on that machine that was going when it did.
pub(crate) fn of_ended(hist: &[Brief], host: &str, agent: &str, ended: u64) -> Facts {
    const NEAR: u64 = 300;
    let b = hist.iter().find(|b| {
        b.host == host && b.agent == agent && b.first <= ended + NEAR && b.last + NEAR >= ended
    });
    Facts {
        host: host.to_string(),
        agent: agent.to_string(),
        project: b.map_or(String::new(), |b| b.project.clone()),
        acct: b.map_or(String::new(), |b| b.acct.clone()),
    }
}

/// This machine: the one on Monitor's strip.
pub(crate) fn this_machine(d: &Dash) -> Option<&str> {
    d.fleet
        .hosts
        .get(d.mon.sel.min(d.fleet.hosts.len().saturating_sub(1)))
        .map(|h| h.alias.as_str())
}

/// Does the list keep it?
pub(crate) fn keeps(d: &Dash, f: &Facts) -> bool {
    if d.cfg.flag("timeline.mine") && this_machine(d).is_some_and(|m| m != f.host) {
        return false;
    }
    d.tl.narrow.keeps(f)
}

// ------------------------------------------------------------------ keys

fn save(d: &mut Dash, key: &str, value: &str, said: String) {
    match d.cfg.set(key, value) {
        Ok(()) => d.note(said),
        Err(e) => d.error(format!("couldn't save: {e}")),
    }
}

/// `O`: the next grouping.
pub(crate) fn cycle_group(d: &mut Dash) {
    let next = By::of(&d.cfg).next();
    save(
        d,
        "timeline.group",
        next.word(),
        format!("timeline: by {}", next.word()),
    );
    d.tl.sel = 0;
    d.tl.scroll = 0;
}

/// `M`: this machine only, or every machine.
pub(crate) fn toggle_mine(d: &mut Dash) {
    let on = !d.cfg.flag("timeline.mine");
    let said = if on {
        let m = this_machine(d).map_or(String::new(), |m| crate::showcase::shown(m).into_owned());
        format!("timeline: {m} only · ctrl+[ ] another machine · M all of them")
    } else {
        "timeline: every machine".to_string()
    };
    save(d, "timeline.mine", if on { "true" } else { "false" }, said);
    d.tl.sel = 0;
    d.tl.scroll = 0;
}

/// `C`: the conversation beside the list, or the list alone.
pub(crate) fn toggle_panel(d: &mut Dash) {
    let on = d.cfg.get_or("timeline.conversation") == "off";
    save(
        d,
        "timeline.conversation",
        if on { "on" } else { "off" },
        if on {
            "conversation panel on".to_string()
        } else {
            "conversation panel hidden · C shows it, G still opens the turn view".to_string()
        },
    );
}

/// `x`: nothing narrowed.
pub(crate) fn clear(d: &mut Dash) -> bool {
    if d.tl.narrow.is_empty() {
        return false;
    }
    d.tl.narrow.clear();
    d.tl.sel = 0;
    d.note("timeline: everything");
    true
}

/// The picker's lines: what's there to narrow to, with what each has cost.
pub(crate) fn items(rows: &[Facts], hist: &[Brief]) -> Vec<Item> {
    let mut cost: BTreeMap<(char, String), f64> = BTreeMap::new();
    for b in hist {
        for (k, v) in [
            ('h', &b.host),
            ('a', &b.agent),
            ('c', &b.acct),
            ('p', &b.project),
        ] {
            if !v.is_empty() {
                *cost.entry((k, v.clone())).or_default() += b.cost;
            }
        }
    }
    let mut seen: BTreeSet<(char, String)> = cost.keys().cloned().collect();
    for f in rows {
        for (k, v) in [
            ('h', &f.host),
            ('a', &f.agent),
            ('c', &f.acct),
            ('p', &f.project),
        ] {
            if !v.is_empty() {
                seen.insert((k, v.clone()));
            }
        }
    }
    let mut out = Vec::new();
    for (kind, what, by) in [
        ('h', "machine", By::Machine),
        ('a', "agent", By::Agent),
        ('c', "account", By::Account),
        ('p', "project", By::Project),
    ] {
        out.extend(seen.iter().filter(|(k, _)| *k == kind).map(|(k, id)| Item {
            kind: *k,
            id: id.clone(),
            label: format!("{} · {what}", label(by, id)),
            depth: 0,
            cost: cost.get(&(*k, id.clone())).copied().unwrap_or(0.0),
        }));
    }
    out
}

/// `F` or `/`: the picker, with what's on ticked.
pub(crate) fn open_picker(d: &mut Dash, rows: Vec<Facts>) {
    let on: BTreeSet<(char, String)> = d.tl.narrow.flat().into_iter().collect();
    d.mode = super::Mode::TimelinePick(Picker {
        dim: Dim::Timeline,
        items: items(&rows, &d.tl.hist),
        q: super::forms::Field::new(""),
        sel: 0,
        on,
    });
}

/// The picker's answer. Nothing ticked: the line under the cursor alone.
pub(crate) fn apply(d: &mut Dash, p: Picker) {
    let mut on = p.on.clone();
    if on.is_empty() {
        if let Some(it) = p.shown().get(p.sel).map(|&i| &p.items[i]) {
            on.insert((it.kind, it.id.clone()));
        }
    }
    let pick = |k: char| -> Vec<String> {
        on.iter()
            .filter(|(x, _)| *x == k)
            .map(|(_, v)| v.clone())
            .collect()
    };
    d.tl.narrow = Narrow {
        hosts: pick('h'),
        agents: pick('a'),
        accounts: pick('c'),
        projects: pick('p'),
    };
    d.tl.sel = 0;
    d.tl.scroll = 0;
}

// ------------------------------------------------------------------ the chips

/// A chip's click.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TChip {
    Group,
    Mine,
    Panel,
    Open,
    /// drop this one of `Narrow::flat`
    Drop(usize),
    Clear,
}

/// A chip clicked.
pub(crate) fn click(d: &mut Dash, c: TChip) {
    match c {
        TChip::Group => cycle_group(d),
        TChip::Mine => toggle_mine(d),
        TChip::Panel => toggle_panel(d),
        TChip::Open => super::timechart::open_filter(d),
        TChip::Drop(i) => {
            if let Some((k, v)) = d.tl.narrow.flat().get(i).cloned() {
                d.tl.narrow.drop_one(k, &v);
                d.tl.sel = 0;
            }
        }
        TChip::Clear => {
            clear(d);
        }
    }
}

/// The bar: `by machine ▾  all machines  claude ×  + filter (F)  conversation on`.
pub(crate) fn chip_bar(d: &mut Dash, f: &mut Frame, r: Rect) {
    let by = By::of(&d.cfg);
    let mine = d.cfg.flag("timeline.mine");
    let panel = d.cfg.get_or("timeline.conversation") != "off";
    let pill = |t: String| (format!(" {t} "), Style::new().fg(c::BASE()).bg(c::ACCENT()));
    let mut parts: Vec<(String, Style, Option<TChip>)> = vec![(
        format!(" ◆ by {} ▾ ", by.word()),
        ui::selected().add_modifier(Modifier::BOLD),
        Some(TChip::Group),
    )];
    parts.push((" ".into(), Style::new(), None));
    if mine {
        let m = this_machine(d).map_or(String::new(), |m| crate::showcase::shown(m).into_owned());
        let (t, st) = pill(format!("{m} only"));
        parts.push((t, st, Some(TChip::Mine)));
    } else {
        parts.push(("all machines".into(), ui::muted(), Some(TChip::Mine)));
    }
    for (i, (k, v)) in d.tl.narrow.flat().iter().enumerate() {
        let shown = match k {
            'h' => label(By::Machine, v),
            'a' => label(By::Agent, v),
            'c' => label(By::Account, v),
            _ => label(By::Project, v),
        };
        let (t, st) = pill(shown);
        parts.push((" ".into(), Style::new(), None));
        parts.push((t, st, Some(TChip::Drop(i))));
        parts.push(("× ".into(), ui::faint(), Some(TChip::Drop(i))));
    }
    parts.push((" ".into(), Style::new(), None));
    if d.tl.narrow.is_empty() {
        parts.push(("+ filter (F)".into(), ui::faint(), Some(TChip::Open)));
    } else {
        parts.push(("+ filter".into(), ui::faint(), Some(TChip::Open)));
        parts.push((" · ".into(), ui::faint(), None));
        parts.push(("x clears".into(), ui::faint(), Some(TChip::Clear)));
    }
    parts.push(("  ".into(), Style::new(), None));
    parts.push((
        if panel {
            "conversation on".into()
        } else {
            "conversation off".into()
        },
        ui::faint(),
        Some(TChip::Panel),
    ));
    let mut x = r.x;
    let mut spans = Vec::new();
    for (t, st, hit) in parts {
        let w = ui::width(&t) as u16;
        if x + w > r.x + r.width {
            break;
        }
        if let Some(h) = hit {
            d.hits
                .chips
                .push((Rect::new(x, r.y, w, 1), super::Chip::Timeline(h)));
        }
        spans.push(Span::styled(t, st));
        x += w;
    }
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(host: &str, agent: &str, project: &str, acct: &str) -> Facts {
        Facts {
            host: host.into(),
            agent: agent.into(),
            project: project.into(),
            acct: acct.into(),
        }
    }

    #[test]
    fn grouping_goes_round() {
        let mut by = By::Machine;
        let mut seen = vec![];
        for _ in 0..4 {
            seen.push(by.word());
            by = by.next();
        }
        assert_eq!(seen, ["machine", "project", "agent", "account"]);
        assert_eq!(by, By::Machine);
    }

    #[test]
    fn each_list_must_hold_its_value_and_an_empty_one_holds_all() {
        let f = facts("b31", "claude", "aleph", "a1");
        assert!(Narrow::default().keeps(&f));
        let on = |n: Narrow| n.keeps(&f);
        assert!(on(Narrow {
            hosts: vec!["gpu2".into(), "b31".into()],
            ..Default::default()
        }));
        assert!(!on(Narrow {
            hosts: vec!["gpu2".into()],
            ..Default::default()
        }));
        assert!(!on(Narrow {
            hosts: vec!["b31".into()],
            agents: vec!["codex".into()],
            ..Default::default()
        }));
        assert!(on(Narrow {
            projects: vec!["aleph".into()],
            accounts: vec!["a1".into()],
            ..Default::default()
        }));
    }

    #[test]
    fn an_ended_session_goes_with_the_conversation_going_when_it_ended() {
        let hist = vec![
            Brief {
                host: "b31".into(),
                agent: "claude".into(),
                project: "later".into(),
                first: 5_000,
                last: 6_000,
                ..Default::default()
            },
            Brief {
                host: "b31".into(),
                agent: "claude".into(),
                project: "aleph".into(),
                acct: "a1".into(),
                first: 1_000,
                last: 2_000,
                ..Default::default()
            },
        ];
        let f = of_ended(&hist, "b31", "claude", 2_100);
        assert_eq!((f.project.as_str(), f.acct.as_str()), ("aleph", "a1"));
        assert_eq!(of_ended(&hist, "b31", "claude", 3_000).project, "");
        assert_eq!(of_ended(&hist, "gpu2", "claude", 2_100).project, "");
        assert_eq!(of_ended(&hist, "b31", "codex", 2_100).project, "");
    }

    #[test]
    fn the_picker_lists_what_is_there_with_its_cost() {
        let hist = vec![Brief {
            host: "b31".into(),
            agent: "claude".into(),
            project: "aleph".into(),
            acct: "a1".into(),
            cost: 2.5,
            ..Default::default()
        }];
        let rows = vec![facts("gpu2", "codex", "train", "")];
        let v = items(&rows, &hist);
        let kinds: Vec<(char, &str)> = v.iter().map(|i| (i.kind, i.id.as_str())).collect();
        assert_eq!(
            kinds,
            [
                ('h', "b31"),
                ('h', "gpu2"),
                ('a', "claude"),
                ('a', "codex"),
                ('c', "a1"),
                ('p', "aleph"),
                ('p', "train"),
            ]
        );
        assert_eq!(v[0].cost, 2.5);
        assert_eq!(v[1].cost, 0.0, "a machine with only a live session");
        assert!(v[0].label.contains("machine"), "{}", v[0].label);
    }

    #[test]
    fn dropping_a_chip_leaves_the_others() {
        let mut n = Narrow {
            hosts: vec!["b31".into(), "gpu2".into()],
            agents: vec!["claude".into()],
            ..Default::default()
        };
        assert_eq!(n.flat().len(), 3);
        n.drop_one('h', "b31");
        assert_eq!(n.hosts, vec!["gpu2".to_string()]);
        assert_eq!(n.agents.len(), 1);
    }
}
