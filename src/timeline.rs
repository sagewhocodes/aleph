//! A session's timeline: what one agent conversation did, as a tree —
//! session → lanes (the agent; each subagent it started, beside it) →
//! prompts (who gave each: you, another agent, itself) → turns (one model
//! request and the tool calls it made) → tool calls (how long, how they
//! went). Every node sums its time, tokens and cost. Built from the agents'
//! own logs (`usage::steps` and the usage records); pure from there, so the
//! CLI, the API and the dashboard draw the same thing.
//!
//! **Folding** (`timeline.fold`): runs of one kind of turn fold to "Read +
//! 7 more", a repeating pattern to "8 × think → Bash"; `smart` also closes
//! every finished prompt to its line (the running one, errors and costly
//! turns stay open), `open` folds only repeats, `folded` closes everything.

use crate::prices;
use crate::usage::parse::Record;
use crate::usage::steps::{Kind, Step};
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Sums for a node.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sums {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache: u64,
    pub cost: f64,
    pub estimated: bool,
    pub requests: u64,
    pub tools: u64,
    pub errors: u64,
}

impl Sums {
    /// What ran: its requests and tool calls.
    pub fn steps(&self) -> u64 {
        self.requests + self.tools
    }
    fn add(&mut self, o: &Sums) {
        self.input += o.input;
        self.output += o.output;
        self.reasoning += o.reasoning;
        self.cache += o.cache;
        self.cost += o.cost;
        self.estimated |= o.estimated;
        self.requests += o.requests;
        self.tools += o.tools;
        self.errors += o.errors;
    }
    pub fn tokens(&self) -> u64 {
        self.input + self.output + self.cache
    }
}

/// One tool call.
#[derive(Clone, Debug, PartialEq)]
pub struct Tool {
    pub key: String,
    pub name: String,
    /// tool · web · mcp · agent
    pub class: String,
    pub start: u64,
    /// when it ended (None: still running, or its end not read yet)
    pub end: Option<u64>,
    /// ok · err · denied · "" not ended
    pub status: String,
    /// the subagent it started (a lane's id)
    pub child: Option<String>,
}

/// One turn: a model request (when the logs priced one) and the tools it called.
#[derive(Clone, Debug, PartialEq)]
pub struct Turn {
    pub start: u64,
    pub end: u64,
    pub model: String,
    pub sums: Sums,
    pub tools: Vec<Tool>,
}

/// One prompt and the work it led to.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Prompt {
    pub key: String,
    /// u you · a an agent · s the agent itself · "" before any prompt in the logs
    pub by: String,
    pub start: u64,
    pub end: u64,
    pub turns: Vec<Turn>,
    /// compactions in it (ms, tokens before, after)
    pub compactions: Vec<(u64, u64, u64)>,
    /// errors in it (ms, kind)
    pub errors: Vec<(u64, String)>,
    pub sums: Sums,
}

/// The agent, or a subagent beside it.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Lane {
    /// "" the agent itself
    pub id: String,
    /// a subagent's type (Explore, worker, …)
    pub kind: String,
    /// the call that started it (its key)
    pub parent: String,
    pub prompts: Vec<Prompt>,
    pub start: u64,
    pub end: u64,
    pub sums: Sums,
}

/// A session.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Tree {
    pub host: String,
    pub agent: String,
    pub session: String,
    pub lanes: Vec<Lane>,
    pub start: u64,
    pub end: u64,
    pub sums: Sums,
}

/// The lane a step or record of a session is in: its own lane, or the lane
/// whose id is its session (a child thread or session, Codex / OpenCode).
fn lane_of(lane: &str, session: &str, root: &str) -> String {
    if !lane.is_empty() {
        lane.to_string()
    } else if session != root {
        session.to_string()
    } else {
        String::new()
    }
}

/// Build a session's tree from its steps and usage records (both for that
/// host, the session and its lanes' own sessions). `attach_after`: tool
/// calls belong to the request after them (Codex writes a response's calls
/// before its usage); else to the one before (Claude Code, the rest).
pub fn build(
    host: &str,
    agent: &str,
    session: &str,
    steps: &[Step],
    recs: &[Record],
    table: &prices::Table,
) -> Tree {
    let attach_after = agent == "codex";
    // lanes: the agent's own, and each subagent's link
    let mut lanes: BTreeMap<String, Lane> = BTreeMap::new();
    let lane = |id: &str| -> Lane {
        Lane {
            id: id.to_string(),
            kind: String::new(),
            parent: String::new(),
            prompts: Vec::new(),
            start: u64::MAX,
            end: 0,
            sums: Sums::default(),
        }
    };
    lanes.insert(String::new(), lane(""));
    for s in steps.iter().filter(|s| s.kind == Kind::Lane) {
        let e = lanes.entry(s.lane.clone()).or_insert_with(|| lane(&s.lane));
        if !s.name.is_empty() {
            e.kind = s.name.clone();
        }
        if !s.class.is_empty() {
            e.parent = s.class.clone();
        }
    }
    // each lane's events, in time order
    #[derive(Clone)]
    enum Ev<'a> {
        Prompt(&'a Step),
        Req(&'a Record),
        Start(&'a Step),
        Compact(&'a Step),
        Error(&'a Step),
    }
    let mut evs: BTreeMap<String, Vec<(u64, u8, Ev)>> = BTreeMap::new();
    let mut ends: HashMap<String, &Step> = HashMap::new();
    for s in steps {
        let l = lane_of(&s.lane, &s.session, session);
        lanes.entry(l.clone()).or_insert_with(|| lane(&l));
        let e = evs.entry(l).or_default();
        match s.kind {
            Kind::Prompt => e.push((s.ms, 0, Ev::Prompt(s))),
            Kind::ToolStart => e.push((s.ms / 1000 * 1000, 2, Ev::Start(s))),
            Kind::Compact => e.push((s.ms, 3, Ev::Compact(s))),
            Kind::Error => e.push((s.ms, 3, Ev::Error(s))),
            Kind::ToolEnd => {
                // the last end of a call wins (Codex: its output, then the item with its time)
                ends.insert(s.key.clone(), s);
            }
            Kind::Lane => {}
        }
    }
    for r in recs.iter().filter(|r| !r.is_turn()) {
        let l = lane_of(&r.lane, &r.session, session);
        lanes.entry(l.clone()).or_insert_with(|| lane(&l));
        evs.entry(l).or_default().push((r.ts * 1000, 1, Ev::Req(r)));
    }
    // which call started which lane
    let child_of: HashMap<String, String> = lanes
        .values()
        .filter(|l| !l.parent.is_empty())
        .map(|l| (l.parent.clone(), l.id.clone()))
        .collect();
    let mut pricer = table.pricer();
    let mut price = |r: &Record| -> (f64, bool) {
        if let Some(c) = r.cost {
            return (c, false);
        }
        match pricer.at(&r.model, &r.effort, r.ts) {
            Some(p) => (prices::cost(&p, &r.price_tokens(), r.fast(), r.us()), true),
            None => (0.0, false),
        }
    };
    for (id, mut list) in evs {
        list.sort_by_key(|(t, o, _)| (*t, *o));
        let mut prompts: Vec<Prompt> = Vec::new();
        let new_prompt = |key: &str, by: &str, t: u64| Prompt {
            key: key.to_string(),
            by: by.to_string(),
            start: t,
            end: t,
            turns: Vec::new(),
            compactions: Vec::new(),
            errors: Vec::new(),
            sums: Sums::default(),
        };
        // tool calls waiting for their request (Codex)
        let mut waiting: Vec<Tool> = Vec::new();
        for (t, _, ev) in &list {
            let t = *t;
            if prompts.is_empty() && !matches!(ev, Ev::Prompt(_)) {
                prompts.push(new_prompt("", "", t));
            }
            match ev {
                Ev::Prompt(s) => prompts.push(new_prompt(&s.key, &s.name, s.ms)),
                Ev::Req(r) => {
                    let (c, est) = price(r);
                    let p = prompts.last_mut().expect("one at least");
                    let mut turn = Turn {
                        start: t,
                        end: t,
                        model: r.model.clone(),
                        sums: Sums {
                            input: r.input,
                            output: r.output,
                            reasoning: r.reasoning,
                            cache: r.cache_read + r.cache_write_5m + r.cache_write_1h,
                            cost: c,
                            estimated: est,
                            requests: 1,
                            ..Default::default()
                        },
                        tools: std::mem::take(&mut waiting),
                    };
                    if let Some(f) = turn.tools.first() {
                        turn.start = turn.start.min(f.start);
                    }
                    p.turns.push(turn);
                }
                Ev::Start(s) => {
                    let end = ends.get(&s.key);
                    let tool = Tool {
                        key: s.key.clone(),
                        name: s.name.clone(),
                        class: s.class.clone(),
                        start: s.ms,
                        end: end.map(|e| {
                            if e.n1 > 0 {
                                s.ms + e.n1
                            } else {
                                e.ms.max(s.ms)
                            }
                        }),
                        status: end.map(|e| e.name.clone()).unwrap_or_default(),
                        child: child_of.get(&s.key).cloned(),
                    };
                    let p = prompts.last_mut().expect("one at least");
                    match (attach_after, p.turns.last_mut()) {
                        (false, Some(turn)) => turn.tools.push(tool),
                        // no request yet (or Codex): it waits for the next one
                        _ => waiting.push(tool),
                    }
                }
                Ev::Compact(s) => prompts
                    .last_mut()
                    .expect("one at least")
                    .compactions
                    .push((s.ms, s.n1, s.n2)),
                Ev::Error(s) => prompts
                    .last_mut()
                    .expect("one at least")
                    .errors
                    .push((s.ms, s.name.clone())),
            }
        }
        // calls no request followed: a turn of their own
        if !waiting.is_empty() {
            if prompts.is_empty() {
                prompts.push(new_prompt("", "", waiting[0].start));
            }
            let start = waiting[0].start;
            prompts.last_mut().expect("one").turns.push(Turn {
                start,
                end: start,
                model: String::new(),
                sums: Sums::default(),
                tools: waiting,
            });
        }
        // sums and ends, bottom up
        let l = lanes.get_mut(&id).expect("made above");
        for p in &mut prompts {
            for turn in &mut p.turns {
                for tool in &turn.tools {
                    turn.sums.tools += 1;
                    if matches!(tool.status.as_str(), "err" | "denied") {
                        turn.sums.errors += 1;
                    }
                    turn.end = turn.end.max(tool.end.unwrap_or(tool.start));
                }
                p.sums.add(&turn.sums);
                p.end = p.end.max(turn.end);
            }
            p.sums.errors += p.errors.len() as u64;
            for (t, ..) in &p.compactions {
                p.end = p.end.max(*t);
            }
            l.sums.add(&p.sums);
            l.start = l.start.min(p.start);
            l.end = l.end.max(p.end);
        }
        l.prompts = prompts;
    }
    let mut tree = Tree {
        host: host.to_string(),
        agent: agent.to_string(),
        session: session.to_string(),
        lanes: lanes
            .into_values()
            .filter(|l| !l.prompts.is_empty() || l.id.is_empty())
            .collect(),
        start: u64::MAX,
        end: 0,
        sums: Sums::default(),
    };
    for l in &tree.lanes {
        tree.sums.add(&l.sums);
        if !l.prompts.is_empty() {
            tree.start = tree.start.min(l.start);
            tree.end = tree.end.max(l.end);
        }
    }
    if tree.start == u64::MAX {
        tree.start = 0;
    }
    tree
}

// ------------------------------------------------------------------ folding

/// How a session's timeline starts out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fold {
    Smart,
    Open,
    Folded,
}

impl Fold {
    pub fn parse(s: &str) -> Fold {
        match s {
            "open" => Fold::Open,
            "folded" => Fold::Folded,
            _ => Fold::Smart,
        }
    }
}

/// A turn's kind for folding: the tools it called (`Read`, `Read Bash`), or
/// `think` (none).
pub fn signature(t: &Turn) -> String {
    if t.tools.is_empty() {
        return "think".into();
    }
    let mut v: Vec<&str> = t.tools.iter().map(|x| x.name.as_str()).collect();
    v.dedup();
    v.join(" ")
}

/// Turns grouped: one alone, a run of one signature (`n` of them), or a
/// repeating unit of `p` signatures (`times` over).
#[derive(Clone, Debug, PartialEq)]
pub enum Group {
    One(usize),
    /// first index, how many
    Run(usize, usize),
    /// first index, unit length, times
    Repeat(usize, usize, usize),
}

/// Group a prompt's turns: runs of 3 or more of one kind; units of 2 or 3
/// repeated at least twice (4 turns or more). Greedy, longest cover first.
pub fn group(sigs: &[String]) -> Vec<Group> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < sigs.len() {
        let mut best: Option<(usize, usize)> = None; // (unit, times)
        for p in 1..=3 {
            if i + p > sigs.len() {
                break;
            }
            let mut times = 1;
            while i + (times + 1) * p <= sigs.len()
                && sigs[i + times * p..i + (times + 1) * p] == sigs[i..i + p]
            {
                times += 1;
            }
            let ok = if p == 1 {
                times >= 3
            } else {
                times >= 2 && times * p >= 4
            };
            if ok && best.is_none_or(|(bp, bt)| times * p > bp * bt) {
                best = Some((p, times));
            }
        }
        match best {
            Some((1, n)) => {
                out.push(Group::Run(i, n));
                i += n;
            }
            Some((p, n)) => {
                out.push(Group::Repeat(i, p, n));
                i += p * n;
            }
            None => {
                out.push(Group::One(i));
                i += 1;
            }
        }
    }
    out
}

/// A row of the drawn timeline.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// stable across rebuilds: what folds remember
    pub id: String,
    pub depth: u8,
    pub kind: RowKind,
    /// what it says
    pub label: String,
    pub start: u64,
    pub end: u64,
    pub sums: Sums,
    /// it has rows under it, and they're hidden
    pub foldable: bool,
    pub folded: bool,
    /// it went wrong (an error, a denial)
    pub bad: bool,
    /// the lane it's in ("" the agent's)
    pub lane: String,
    /// a prompt's or a tool call's key (its live words, `steps::words`)
    pub key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Lane,
    Prompt,
    PromptEnd,
    Turn,
    Group,
    Tool,
    Compact,
    Error,
}

/// Turns whose cost is far above the rest stay open in `smart` (over 3×
/// the median, and a cent at least).
fn costly(turns: &[Turn]) -> BTreeSet<usize> {
    let mut c: Vec<f64> = turns.iter().map(|t| t.sums.cost).collect();
    c.sort_by(f64::total_cmp);
    let med = c.get(c.len() / 2).copied().unwrap_or(0.0);
    turns
        .iter()
        .enumerate()
        .filter(|(_, t)| t.sums.cost >= 0.01 && t.sums.cost > 3.0 * med && turns.len() > 3)
        .map(|(i, _)| i)
        .collect()
}

fn who(by: &str) -> &'static str {
    match by {
        "u" => "you",
        "a" => "an agent",
        "s" => "itself",
        _ => "before the logs",
    }
}

fn dur(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{}.{}s", s, (ms % 1000) / 100)
    } else {
        crate::ui::dur(s)
    }
}

/// The rows to draw: the tree flattened, folded as `mode` says and as the
/// user opened or closed (`toggled`: row id → open), `running`: the last
/// prompt is still being worked on (it stays open in `smart`).
pub fn rows(tree: &Tree, mode: Fold, toggled: &HashMap<String, bool>, running: bool) -> Vec<Row> {
    let mut out = Vec::new();
    let open = |id: &str, default: bool| toggled.get(id).copied().unwrap_or(default);
    for lane in &tree.lanes {
        let lid = format!("L{}", lane.id);
        // the whole conversation folded to one line (`1`): what it took, in all
        if lane.id.is_empty() && toggled.get(&lid) == Some(&false) {
            out.push(Row {
                id: lid.clone(),
                depth: 0,
                kind: RowKind::Lane,
                label: format!(
                    "this session · {} prompt{} · {} steps",
                    lane.prompts.len(),
                    if lane.prompts.len() == 1 { "" } else { "s" },
                    lane.sums.steps()
                ),
                start: lane.start,
                end: lane.end,
                sums: lane.sums.clone(),
                foldable: true,
                folded: true,
                bad: lane.sums.errors > 0,
                lane: String::new(),
                key: String::new(),
            });
            continue;
        }
        // a subagent's lane is drawn under the call that started it; one with no link, here
        if !lane.id.is_empty() {
            if tree.lanes.iter().any(|l| {
                l.prompts.iter().any(|p| {
                    p.turns
                        .iter()
                        .any(|t| t.tools.iter().any(|x| x.child.as_deref() == Some(&lane.id)))
                })
            }) {
                continue;
            }
            let o = open(&lid, mode == Fold::Open);
            out.push(Row {
                id: lid.clone(),
                depth: 0,
                kind: RowKind::Lane,
                label: {
                    let name = format!(
                        "{} agent",
                        if lane.kind.is_empty() {
                            "a sub"
                        } else {
                            &lane.kind
                        }
                    );
                    if o {
                        name
                    } else {
                        format!(
                            "{name} · {} prompt{} · {} steps",
                            lane.prompts.len(),
                            if lane.prompts.len() == 1 { "" } else { "s" },
                            lane.sums.steps()
                        )
                    }
                },
                start: lane.start,
                end: lane.end,
                sums: lane.sums.clone(),
                foldable: true,
                folded: !o,
                bad: lane.sums.errors > 0,
                lane: lane.id.clone(),
                key: String::new(),
            });
            if !o {
                continue;
            }
        }
        lane_rows(
            tree,
            lane,
            if lane.id.is_empty() { 0 } else { 1 },
            mode,
            toggled,
            running,
            &mut out,
        );
    }
    out
}

/// A compaction's row under its prompt.
fn compact_row(pid: &str, depth: u8, lane: &str, t: u64, before: u64, after: u64) -> Row {
    Row {
        id: format!("{pid}/C{t}"),
        depth: depth + 1,
        kind: RowKind::Compact,
        label: format!(
            "compacted · {} → {}",
            crate::usage::dash::tokens(before),
            crate::usage::dash::tokens(after)
        ),
        start: t,
        end: t,
        sums: Sums::default(),
        foldable: false,
        folded: false,
        bad: false,
        lane: lane.to_string(),
        key: String::new(),
    }
}

fn lane_rows(
    tree: &Tree,
    lane: &Lane,
    depth: u8,
    mode: Fold,
    toggled: &HashMap<String, bool>,
    running: bool,
    out: &mut Vec<Row>,
) {
    let open = |id: &str, default: bool| toggled.get(id).copied().unwrap_or(default);
    let n = lane.prompts.len();
    for (pi, p) in lane.prompts.iter().enumerate() {
        let pid = format!("P{}:{}:{}", lane.id, pi, p.key);
        let live = running && lane.id.is_empty() && pi + 1 == n;
        // a failed tool call marks it (red, "2 failed"); one that stopped on
        // an API error or was interrupted stays open in smart
        let bad = p.sums.errors > 0;
        let stopped = !p.errors.is_empty();
        let default = match mode {
            Fold::Open => true,
            Fold::Folded => false,
            Fold::Smart => live || stopped,
        };
        let o = open(&pid, default);
        out.push(Row {
            id: pid.clone(),
            depth,
            kind: RowKind::Prompt,
            label: {
                let l = format!(
                    "prompt {} · {}",
                    pi + if lane.prompts.first().is_some_and(|f| f.by.is_empty()) {
                        0
                    } else {
                        1
                    },
                    who(&p.by)
                );
                // folded: how many steps ran under it
                if o || p.turns.is_empty() {
                    l
                } else {
                    format!("{l} · {} steps", p.sums.steps())
                }
            },
            start: p.start,
            end: p.end,
            sums: p.sums.clone(),
            foldable: !p.turns.is_empty(),
            folded: !o,
            bad,
            lane: lane.id.clone(),
            key: p.key.clone(),
        });
        if !o {
            // the context changed here: that shows even with its prompt folded
            for (t, before, after) in &p.compactions {
                out.push(compact_row(&pid, depth, &lane.id, *t, *before, *after));
            }
            continue;
        }
        let sigs: Vec<String> = p.turns.iter().map(signature).collect();
        let hot = costly(&p.turns);
        // the prompt's children, each a chunk of rows with its time: turns (or
        // their groups), compactions and errors, drawn in time order
        let mut chunks: Vec<(u64, Vec<Row>)> = Vec::new();
        for g in group(&sigs) {
            let mut out: Vec<Row> = Vec::new();
            let at = match g {
                Group::One(i) | Group::Run(i, _) | Group::Repeat(i, _, _) => p.turns[i].start,
            };
            match g {
                Group::One(i) => turn_rows(
                    tree,
                    lane,
                    &pid,
                    i,
                    &p.turns[i],
                    depth + 1,
                    mode,
                    toggled,
                    running,
                    &mut out,
                ),
                Group::Run(i, k) | Group::Repeat(i, _, k) => {
                    let unit = match g {
                        Group::Repeat(_, u, _) => u,
                        _ => 1,
                    };
                    let span = &p.turns[i..i + unit * k];
                    let mut sums = Sums::default();
                    for t in span {
                        sums.add(&t.sums);
                    }
                    let gid = format!("{pid}/G{i}");
                    let label = if unit == 1 {
                        let s = &sigs[i];
                        if s == "think" {
                            format!("{k} turns of thinking")
                        } else {
                            format!("{s} + {} more", k - 1)
                        }
                    } else {
                        format!("{k} × {}", sigs[i..i + unit].join(" → "))
                    };
                    // a run with an error or a costly turn in it stays open in smart
                    let inner_hot = (i..i + unit * k).any(|x| hot.contains(&x)) || sums.errors > 0;
                    let go = open(&gid, mode == Fold::Smart && inner_hot);
                    out.push(Row {
                        id: gid.clone(),
                        depth: depth + 1,
                        kind: RowKind::Group,
                        label,
                        start: span[0].start,
                        end: span.iter().map(|t| t.end).max().unwrap_or(span[0].end),
                        sums: sums.clone(),
                        foldable: true,
                        folded: !go,
                        bad: sums.errors > 0,
                        lane: lane.id.clone(),
                        key: String::new(),
                    });
                    if go {
                        for (j, t) in span.iter().enumerate() {
                            turn_rows(
                                tree,
                                lane,
                                &pid,
                                i + j,
                                t,
                                depth + 2,
                                mode,
                                toggled,
                                running,
                                &mut out,
                            );
                        }
                    }
                }
            }
            chunks.push((at, out));
        }
        for (t, before, after) in &p.compactions {
            chunks.push((
                *t,
                vec![compact_row(&pid, depth, &lane.id, *t, *before, *after)],
            ));
        }
        for (t, kind) in &p.errors {
            chunks.push((
                *t,
                vec![Row {
                    id: format!("{pid}/E{t}"),
                    depth: depth + 1,
                    kind: RowKind::Error,
                    label: match kind.as_str() {
                        "interrupted" => "interrupted".into(),
                        _ => "API error".into(),
                    },
                    start: *t,
                    end: *t,
                    sums: Sums::default(),
                    foldable: false,
                    folded: false,
                    bad: true,
                    lane: lane.id.clone(),
                    key: String::new(),
                }],
            ));
        }
        chunks.sort_by_key(|(t, _)| *t);
        out.extend(chunks.into_iter().flat_map(|(_, r)| r));
        // the prompt's end, with its totals
        if !(running && lane.id.is_empty() && pi + 1 == n) {
            out.push(Row {
                id: format!("{pid}/end"),
                depth,
                kind: RowKind::PromptEnd,
                label: "done".into(),
                start: p.end,
                end: p.end,
                sums: p.sums.clone(),
                foldable: false,
                folded: false,
                bad: false,
                lane: lane.id.clone(),
                key: String::new(),
            });
        }
    }
    let _ = tree;
}

#[allow(clippy::too_many_arguments)]
fn turn_rows(
    tree: &Tree,
    lane: &Lane,
    pid: &str,
    i: usize,
    t: &Turn,
    depth: u8,
    mode: Fold,
    toggled: &HashMap<String, bool>,
    running: bool,
    out: &mut Vec<Row>,
) {
    let tid = format!("{pid}/T{i}");
    let o = toggled.get(&tid).copied().unwrap_or(mode != Fold::Folded);
    out.push(Row {
        id: tid.clone(),
        depth,
        kind: RowKind::Turn,
        label: if t.model.is_empty() {
            "tools".into()
        } else {
            format!("turn · {}", short_model(&t.model))
        },
        start: t.start,
        end: t.end,
        sums: t.sums.clone(),
        foldable: !t.tools.is_empty(),
        folded: !o && !t.tools.is_empty(),
        bad: t.sums.errors > 0,
        lane: lane.id.clone(),
        key: String::new(),
    });
    if !o {
        return;
    }
    // runs of one tool (3 or more, none of them failed, none starting a
    // subagent) under one row: "Read + 7 more" (repeats fold in every mode)
    let plain = |x: &Tool| x.child.is_none() && !matches!(x.status.as_str(), "err" | "denied");
    let mut k = 0;
    while k < t.tools.len() {
        let x = &t.tools[k];
        let mut n = 1;
        while plain(x)
            && k + n < t.tools.len()
            && t.tools[k + n].name == x.name
            && plain(&t.tools[k + n])
        {
            n += 1;
        }
        if n >= 3 {
            let gid = format!("{tid}/R{k}");
            let go = toggled.get(&gid).copied().unwrap_or(false);
            let span = &t.tools[k..k + n];
            out.push(Row {
                id: gid,
                depth: depth + 1,
                kind: RowKind::Group,
                label: format!("{} + {} more", x.name, n - 1),
                start: x.start,
                end: span
                    .iter()
                    .map(|y| y.end.unwrap_or(y.start))
                    .max()
                    .unwrap_or(x.start),
                sums: Sums {
                    tools: n as u64,
                    ..Default::default()
                },
                foldable: true,
                folded: !go,
                bad: false,
                lane: lane.id.clone(),
                key: String::new(),
            });
            if go {
                for (j, y) in span.iter().enumerate() {
                    tool_rows(
                        tree,
                        lane,
                        &tid,
                        k + j,
                        y,
                        depth + 2,
                        mode,
                        toggled,
                        running,
                        out,
                    );
                }
            }
            k += n;
            continue;
        }
        tool_rows(
            tree,
            lane,
            &tid,
            k,
            x,
            depth + 1,
            mode,
            toggled,
            running,
            out,
        );
        k += 1;
    }
}

/// A tool call's row, and the subagent it started under it (when open).
#[allow(clippy::too_many_arguments)]
fn tool_rows(
    tree: &Tree,
    lane: &Lane,
    tid: &str,
    k: usize,
    x: &Tool,
    depth: u8,
    mode: Fold,
    toggled: &HashMap<String, bool>,
    running: bool,
    out: &mut Vec<Row>,
) {
    let xid = format!("{tid}/X{k}");
    let took = x.end.map(|e| e.saturating_sub(x.start));
    out.push(Row {
        id: xid.clone(),
        depth,
        kind: RowKind::Tool,
        label: match (x.status.as_str(), took) {
            ("denied", _) => format!("{} · denied", x.name),
            ("err", Some(d)) => format!("{} · failed after {}", x.name, dur(d)),
            ("err", None) => format!("{} · failed", x.name),
            (_, None) => format!("{} · running", x.name),
            _ => x.name.clone(),
        },
        start: x.start,
        end: x.end.unwrap_or(x.start),
        sums: Sums::default(),
        foldable: x.child.is_some(),
        folded: x.child.is_some() && !toggled.get(&xid).copied().unwrap_or(mode == Fold::Open),
        bad: matches!(x.status.as_str(), "err" | "denied"),
        lane: lane.id.clone(),
        key: x.key.clone(),
    });
    // the subagent it started, under it
    if let Some(c) = &x.child {
        if toggled.get(&xid).copied().unwrap_or(mode == Fold::Open) {
            if let Some(sub) = tree.lanes.iter().find(|l| &l.id == c) {
                lane_rows(tree, sub, depth + 1, mode, toggled, running, out);
            }
        }
    }
}

/// `opus-4-5` from `claude-opus-4-5-20251101`; a provider's prefix dropped
/// (`anthropic/claude-sonnet-4.5` through OpenRouter is `sonnet-4.5`), a free
/// variant said (`qwen3-coder free`).
pub fn short_model(m: &str) -> String {
    let (m, free) = match m.strip_suffix(":free") {
        Some(b) => (b, true),
        None => (m, false),
    };
    let m = m.rsplit_once('/').map_or(m, |x| x.1);
    let m = m.strip_prefix("claude-").unwrap_or(m);
    let m = match m.rsplit_once('-') {
        Some((a, d)) if d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) => a,
        _ => m,
    };
    if free {
        format!("{m} free")
    } else {
        m.to_string()
    }
}

// ------------------------------------------------------------------ loading

/// A session's steps and usage records from the store (this month and the
/// one before), its subagents' own sessions too; its tree.
pub fn load(cfg: &crate::config::Config, host: &str, agent: &str, session: &str) -> Tree {
    use crate::usage::{steps, store};
    if session.is_empty() {
        return build(
            host,
            agent,
            session,
            &[],
            &[],
            &prices::Table::from_cfg(cfg),
        );
    }
    let months: Vec<String> = steps::months().into_iter().rev().take(2).collect();
    // the session's lines first; its lanes' own sessions (child threads) from them
    let mine_first: Vec<Step> = months
        .iter()
        .flat_map(|m| steps::read_month_matching(m, &[session]))
        .filter(|s| s.host == host && s.agent == agent)
        .collect();
    let kids: BTreeSet<String> = mine_first
        .iter()
        .filter(|s| s.kind == Kind::Lane && s.session == session)
        .map(|s| s.lane.clone())
        .collect();
    let mut needles: Vec<&str> = vec![session];
    needles.extend(kids.iter().map(String::as_str));
    let mine =
        |sess: &str, lane: &str| sess == session || kids.contains(sess) || kids.contains(lane);
    let mut st: Vec<Step> = if kids.is_empty() {
        mine_first
    } else {
        months
            .iter()
            .flat_map(|m| steps::read_month_matching(m, &needles))
            .filter(|s| s.host == host && s.agent == agent)
            .collect()
    };
    st.retain(|s| mine(&s.session, &s.lane));
    st.sort_by_key(|s| s.ms);
    let rec_months: Vec<String> = store::months().into_iter().rev().take(2).collect();
    let recs: Vec<Record> = store::dedup(
        rec_months
            .iter()
            .flat_map(|m| store::read_month_matching(m, &needles))
            .filter(|r| r.host == host && r.agent == agent && mine(&r.session, &r.lane))
            .collect(),
    );
    build(
        host,
        agent,
        session,
        &st,
        &recs,
        &prices::Table::from_cfg(cfg),
    )
}

/// The latest conversations with a timeline, each with its project, prompts,
/// requests, tokens and $ when the usage store has them (`timeline.recent`,
/// `aleph timeline ls --json`).
pub fn recent_json(cfg: &crate::config::Config, n: usize) -> Vec<serde_json::Value> {
    let v = recent(n);
    let days = v
        .last()
        .map(|x| crate::cache::now().saturating_sub(x.3 / 1000) / 86_400 + 2)
        .unwrap_or(1)
        .min(120);
    let briefs: HashMap<(String, String, String), Brief> = sessions(cfg, days)
        .into_iter()
        .map(|b| ((b.host.clone(), b.agent.clone(), b.session.clone()), b))
        .collect();
    v.into_iter()
        .map(|(h, ag, s, t)| {
            let b = briefs.get(&(h.clone(), ag.clone(), s.clone()));
            serde_json::json!({
                "host": h, "agent": ag, "session": s, "last_ms": t,
                "project": b.map(|b| crate::showcase::text(&b.project).into_owned()),
                "project_id": b.map(|b| b.pid.clone()),
                "account": b.map(|b| b.acct.clone()),
                "account_label": b.map(|b| crate::usage::accounts::name(&b.acct)),
                "first": b.map(|b| b.first),
                "prompts": b.map(|b| b.prompts),
                "requests": b.map(|b| b.requests),
                "tokens": b.map(|b| b.tokens),
                "cost": b.and_then(|b| crate::showcase::money(b.cost)),
                "estimated": b.map(|b| b.estimated),
            })
        })
        .collect()
}

/// A conversation as the history lists it: where, which agent, its project,
/// when (unix seconds) and what it cost — from the usage store's rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Brief {
    pub host: String,
    pub agent: String,
    pub session: String,
    /// its project's id and name (`usage::projects`)
    pub pid: String,
    pub project: String,
    pub acct: String,
    pub first: u64,
    pub last: u64,
    pub cost: f64,
    pub estimated: bool,
    pub prompts: u64,
    pub requests: u64,
    pub tokens: u64,
}

/// The conversations of the last `days` days, the latest first.
pub fn sessions(cfg: &crate::config::Config, days: u64) -> Vec<Brief> {
    let (rows, ix) = crate::usage::rows_ix(
        cfg,
        crate::usage::Range::days(days),
        &crate::usage::Filter::default(),
    );
    briefs(&rows, |pid| ix.name(pid))
}

fn briefs(rows: &[crate::usage::store::Row], name: impl Fn(&str) -> String) -> Vec<Brief> {
    let mut m: HashMap<(String, String, String), Brief> = HashMap::new();
    for r in rows.iter().filter(|r| !r.session.is_empty()) {
        let b = m
            .entry((r.host.clone(), r.agent.clone(), r.session.clone()))
            .or_insert_with(|| Brief {
                host: r.host.clone(),
                agent: r.agent.clone(),
                session: r.session.clone(),
                pid: r.pid.clone(),
                project: name(&r.pid),
                acct: r.acct.clone(),
                first: r.first,
                ..Default::default()
            });
        b.first = b.first.min(r.first);
        b.last = b.last.max(r.last);
        b.cost += r.cost_reported + r.cost_estimated;
        b.estimated |= r.cost_estimated > 0.0;
        b.prompts += r.prompts;
        b.requests += r.requests;
        b.tokens += r.input + r.output + r.cache_read + r.cache_write_5m + r.cache_write_1h;
    }
    let mut v: Vec<Brief> = m.into_values().collect();
    v.sort_by(|a, b| b.last.cmp(&a.last).then(a.session.cmp(&b.session)));
    v
}

/// The sessions with steps, newest first: (host, agent, session, last ms).
pub fn recent(n: usize) -> Vec<(String, String, String, u64)> {
    use crate::usage::steps;
    let mut last: HashMap<(String, String, String), u64> = HashMap::new();
    // a subagent's own session (a child thread) counts as its parent's
    let mut kids: BTreeSet<(String, String, String)> = BTreeSet::new();
    for m in steps::months().into_iter().rev().take(2) {
        for s in steps::read_month(&m) {
            if s.kind == Kind::Lane {
                kids.insert((s.host.clone(), s.agent.clone(), s.lane.clone()));
                continue;
            }
            let e = last
                .entry((s.host.clone(), s.agent.clone(), s.session.clone()))
                .or_default();
            *e = (*e).max(s.ms);
        }
    }
    let mut v: Vec<(String, String, String, u64)> = last
        .into_iter()
        .filter(|(k, _)| !kids.contains(k))
        .map(|((h, a, s), t)| (h, a, s, t))
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.3));
    v.truncate(n);
    v
}

/// A tree and its rows as JSON (`aleph timeline --json`, `timeline.get`).
pub fn to_json(t: &Tree, rows: &[Row]) -> serde_json::Value {
    use serde_json::json;
    let sums = |s: &Sums| {
        json!({"input": s.input, "output": s.output, "reasoning": s.reasoning, "cache": s.cache,
               "tokens": s.tokens(), "cost": crate::showcase::money(s.cost), "estimated": s.estimated,
               "requests": s.requests, "tools": s.tools, "errors": s.errors})
    };
    json!({
        "host": t.host, "agent": t.agent, "session": t.session,
        "start_ms": t.start, "end_ms": t.end, "sums": sums(&t.sums),
        "lanes": t.lanes.iter().map(|l| json!({"id": l.id, "kind": l.kind, "prompts": l.prompts.len(), "sums": sums(&l.sums)})).collect::<Vec<_>>(),
        "rows": rows.iter().map(|r| json!({
            "id": r.id, "depth": r.depth, "kind": format!("{:?}", r.kind).to_lowercase(), "label": r.label,
            "start_ms": r.start, "end_ms": r.end, "sums": sums(&r.sums),
            "foldable": r.foldable, "folded": r.folded, "bad": r.bad, "lane": r.lane, "key": r.key,
            "figures": figures(r),
        })).collect::<Vec<_>>(),
    })
}

/// `$0.42`, `<$0.01`, "" for nothing (showcase: scaled or hidden).
pub fn money(c: f64) -> String {
    if c <= 0.0 {
        return String::new();
    }
    match crate::showcase::money(c) {
        None => crate::showcase::MONEY_HIDDEN.into(),
        Some(v) if v < 0.01 => "<$0.01".into(),
        Some(v) => format!("${v:.2}"),
    }
}

/// A row's figures: how long, tokens (in → out, ✦ reasoning), $.
pub fn figures(r: &Row) -> String {
    figures_in(r, usize::MAX)
}

/// [`figures`] in `room` columns: the counts go first, then the tokens, then
/// the time; the $ stays.
pub fn figures_in(r: &Row, room: usize) -> String {
    let mut v = parts(r);
    let w = |v: &[(u8, String)]| {
        v.iter().map(|(_, s)| crate::ui::width(s)).sum::<usize>() + v.len().saturating_sub(1) * 3
    };
    for drop in [4, 3, 2, 1] {
        if w(&v) <= room {
            break;
        }
        v.retain(|(p, _)| *p != drop);
    }
    v.into_iter()
        .map(|(_, s)| s)
        .collect::<Vec<_>>()
        .join(" · ")
}

/// A row's figures with how much they're wanted (0 the most).
fn parts(r: &Row) -> Vec<(u8, String)> {
    let mut v: Vec<(u8, String)> = Vec::new();
    if r.end > r.start {
        v.push((1, dur(r.end - r.start)));
    }
    if r.sums.tokens() > 0 {
        let mut t = format!(
            "{}→{}",
            crate::usage::dash::tokens(r.sums.input + r.sums.cache),
            crate::usage::dash::tokens(r.sums.output)
        );
        if r.sums.reasoning > 0 {
            t.push_str(&format!(
                " ✦{}",
                crate::usage::dash::tokens(r.sums.reasoning)
            ));
        }
        v.push((2, t));
    }
    if r.kind == RowKind::Prompt || r.kind == RowKind::PromptEnd || r.kind == RowKind::Lane {
        if r.sums.requests > 0 {
            v.push((3, format!("{} req", r.sums.requests)));
        }
        if r.sums.tools > 0 {
            v.push((4, format!("{} tools", r.sums.tools)));
        }
        // a failure outranks the token counts: it stays when room is short
        if r.sums.errors > 0 {
            v.push((1, format!("{} failed", r.sums.errors)));
        }
    }
    let m = money(r.sums.cost);
    if !m.is_empty() {
        v.push((0, m));
    }
    v
}

const USAGE: &str = "usage: aleph timeline [SESSION | AGENT-SESSION-ID | last | ls [-n N]] [--host H] [--fold smart|open|folded] [--words] [--json]";

/// `aleph timeline`: one agent conversation as a tree — its prompts, turns,
/// tool calls and subagents, with time, tokens and $ (names and numbers
/// from its logs, never what was said). `ls`: the latest sessions.
pub fn cmd(a: crate::cli::Args) -> std::process::ExitCode {
    use crate::out::{fail, head, hint, json, out};
    use ratatui::prelude::*;
    let p = a.positional();
    let host_flag = a
        .rest
        .iter()
        .position(|x| x == "--host" || x == "-H")
        .and_then(|i| a.rest.get(i + 1))
        .cloned()
        .or_else(|| a.flag.clone());
    let q = p
        .iter()
        .copied()
        .find(|x| Some(x.to_string()) != host_flag && !["smart", "open", "folded"].contains(x))
        .unwrap_or("last");
    if q == "ls" {
        if a.has("--json") {
            let n = a
                .rest
                .iter()
                .position(|x| x == "-n")
                .and_then(|i| a.rest.get(i + 1))
                .and_then(|x| x.parse().ok())
                .unwrap_or(20);
            json(&serde_json::json!(recent_json(&a.cfg, n)));
            return std::process::ExitCode::SUCCESS;
        }
        let v = recent(20);
        for (h, ag, s, t) in v {
            println!(
                "{h}\t{ag}\t{s}\t{}",
                crate::ui::ago(crate::cache::now().saturating_sub(t / 1000))
            );
        }
        return std::process::ExitCode::SUCCESS;
    }
    let fold = Fold::parse(
        &a.rest
            .iter()
            .position(|x| x == "--fold")
            .and_then(|i| a.rest.get(i + 1))
            .cloned()
            .unwrap_or_else(|| a.cfg.get_or("timeline.fold")),
    );
    // last; an agent's own session id (or its start); else a session by name
    let rec = recent(500);
    let pick = if q == "last" {
        rec.iter()
            .find(|(h, ..)| host_flag.as_ref().is_none_or(|f| f == h))
            .cloned()
    } else {
        rec.iter()
            .find(|(h, _, s, _)| s.starts_with(q) && host_flag.as_ref().is_none_or(|f| f == h))
            .cloned()
    };
    let (host, agent, session) = match pick {
        Some((h, ag, s, _)) => (h, ag, s),
        None => {
            let (flag, name) = match q.split_once(':') {
                Some((h, n)) if !h.is_empty() => (Some(h.to_string()), n.to_string()),
                _ => (host_flag.clone(), q.to_string()),
            };
            let (t, s) = match crate::cli::find(&a.cfg, &flag, &name) {
                Ok(x) => x,
                Err(c) => return c,
            };
            let Some(id) = s.native_id.clone() else {
                return fail(&format!(
                    "{} doesn't say its conversation's id yet (aleph timeline ls lists what's read)",
                    s.display()
                ));
            };
            (t.ssh.clone(), s.agent.clone(), id)
        }
    };
    let t = load(&a.cfg, &host, &agent, &session);
    let rows = rows(&t, fold, &HashMap::new(), false);
    // the words, read now from the machine (never kept): --words
    let words = if a.has("--words") && a.cfg.get_or("timeline.words") != "off" {
        crate::usage::steps::words(&host, &agent, &session).unwrap_or_default()
    } else {
        HashMap::new()
    };
    if a.has("--json") {
        let mut v = to_json(&t, &rows);
        if !words.is_empty() {
            v["words"] = serde_json::json!(words
                .iter()
                .map(|(k, w)| (
                    k.clone(),
                    serde_json::Value::String(crate::showcase::text(w).to_string())
                ))
                .collect::<serde_json::Map<String, serde_json::Value>>());
        }
        json(&v);
        return std::process::ExitCode::SUCCESS;
    }
    if rows.is_empty() || t.lanes.iter().all(|l| l.prompts.is_empty()) {
        return fail(&format!(
            "nothing read for {agent} session {session} on {host} yet (aleph usage sync reads the logs)"
        ));
    }
    head(&format!(
        "{} · {} · {}",
        crate::agents::label(&agent),
        host,
        &session[..session.len().min(8)]
    ));
    let tz = crate::ui::utc_offset();
    for r in &rows {
        let clock = {
            let l = (r.start / 1000) as i64 + tz;
            let d = l.rem_euclid(86_400);
            format!("{:02}:{:02}:{:02}", d / 3600, d % 3600 / 60, d % 60)
        };
        let mark = match (r.kind, r.foldable, r.folded) {
            (RowKind::Prompt, ..) => {
                if r.folded {
                    "▸ "
                } else {
                    "┌ "
                }
            }
            (RowKind::PromptEnd, ..) => "└ ",
            (RowKind::Lane, ..) => "⑂ ",
            (RowKind::Turn, ..) => "◆ ",
            (RowKind::Group, ..) => {
                if r.folded {
                    "▸ "
                } else {
                    "▾ "
                }
            }
            (RowKind::Tool, _, true) => "› ",
            (RowKind::Tool, ..) => "› ",
            (RowKind::Compact, ..) => "≋ ",
            (RowKind::Error, ..) => "✗ ",
        };
        let indent = "  ".repeat(r.depth as usize);
        let st = if r.bad {
            crate::ui::fg(crate::ui::c::RED())
        } else if matches!(r.kind, RowKind::Prompt | RowKind::Lane) {
            crate::ui::bold()
        } else {
            Style::new()
        };
        let mut l = vec![
            Span::styled(format!("{clock} "), crate::ui::faint()),
            Span::raw(indent),
            Span::styled(format!("{mark}{}", r.label), st),
            Span::styled(format!("  {}", figures(r)), crate::ui::muted()),
        ];
        if let Some(w) = words.get(&r.key) {
            l.push(Span::styled(
                format!("  {}", crate::showcase::text(w)),
                crate::ui::faint(),
            ));
        }
        out(Line::from(l));
    }
    hint(&format!(
        "{} · --fold open shows every turn · aleph timeline ls",
        figures(&Row {
            id: String::new(),
            depth: 0,
            kind: RowKind::Prompt,
            label: String::new(),
            start: t.start,
            end: t.end,
            sums: t.sums.clone(),
            foldable: false,
            folded: false,
            bad: false,
            lane: String::new(),
            key: String::new(),
        })
    ));
    let _ = USAGE;
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::steps::from_wire;

    fn st(line: &str, agent: &str) -> Step {
        from_wire(line, "b31", agent).unwrap()
    }

    fn req(ts: u64, session: &str, lane: &str, out: u64) -> Record {
        Record {
            ts,
            host: "b31".into(),
            agent: "claude".into(),
            session: session.into(),
            key: format!("k{ts}{lane}"),
            model: "claude-opus-5-5".into(),
            input: 10,
            output: out,
            cache_read: 100,
            lane: lane.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_compaction_shows_under_a_folded_prompt() {
        let tree = Tree {
            lanes: vec![Lane {
                prompts: vec![Prompt {
                    start: 1_000,
                    end: 9_000,
                    compactions: vec![(5_000, 150_000, 30_000)],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let rows = rows(&tree, Fold::Folded, &HashMap::new(), false);
        let kinds: Vec<RowKind> = rows.iter().map(|r| r.kind).collect();
        assert_eq!(kinds, vec![RowKind::Prompt, RowKind::Compact]);
        assert!(rows[1].label.contains("compacted"), "{}", rows[1].label);
    }

    #[test]
    fn a_session_as_a_tree() {
        const B: u64 = 1_792_659_000_000;
        let s = "S";
        let steps = vec![
            st("p\t1792659001000\tS\t\tp1\tu", "claude"),
            st("ks\t1792660002500\tS\t\tt1\tBash\ttool", "claude"),
            st("ke\t1792660004000\tS\t\tt1\tok\t0", "claude"),
            st("ks\t1792660005000\tS\t\tt2\tAgent\tagent", "claude"),
            st("ke\t1792660030000\tS\t\tt2\tok\t0", "claude"),
            st("sl\t\tS\tsub1\tt2\tExplore", "claude"),
            st("p\t1792660006000\tS\tsub1\tq1\ta", "claude"),
            st("ks\t1792660007000\tS\tsub1\tt3\tRead\ttool", "claude"),
            st("ke\t1792660007100\tS\tsub1\tt3\terr\t0", "claude"),
            st("c\t1792660040000\tS\t\tc1\t150000\t30000", "claude"),
            st("p\t1792660050000\tS\t\tp2\ts", "claude"),
        ];
        let recs = vec![
            req(1792660002, s, "", 20),
            req(1792660005, s, "", 30),
            req(1792660006, s, "sub1", 5),
            req(1792660051, s, "", 7),
        ];
        let t = build("b31", "claude", s, &steps, &recs, &prices::Table::default());
        assert_eq!(t.lanes.len(), 2);
        let main = &t.lanes[0];
        assert_eq!(main.prompts.len(), 2);
        let p1 = &main.prompts[0];
        assert_eq!((p1.by.as_str(), p1.turns.len()), ("u", 2));
        assert_eq!(p1.turns[0].tools[0].name, "Bash");
        assert_eq!(p1.turns[0].tools[0].end, Some(B + 1_004_000));
        assert_eq!(p1.turns[1].tools[0].child.as_deref(), Some("sub1"));
        assert_eq!(p1.compactions, vec![(B + 1_040_000, 150_000, 30_000)]);
        let sub = &t.lanes[1];
        assert_eq!(
            (sub.kind.as_str(), sub.prompts[0].by.as_str()),
            ("Explore", "a")
        );
        assert_eq!(sub.sums.errors, 1);
        assert_eq!(t.sums.requests, 4);
        assert!(t.sums.cost > 0.0 && t.sums.estimated);
        // drawn, opened: the subagent under the call that started it
        let r = rows(&t, Fold::Open, &HashMap::new(), false);
        let labels: Vec<&str> = r.iter().map(|x| x.label.as_str()).collect();
        let at = |l: &str| labels.iter().position(|x| x.starts_with(l)).unwrap();
        assert!(at("Agent") < at("prompt 1 · an agent"), "{labels:#?}");
        assert!(labels.contains(&"Read · failed after 0.1s"), "{labels:#?}");
        assert!(
            labels
                .iter()
                .any(|l| l.starts_with("compacted · 150k → 30k")),
            "{labels:#?}"
        );
        // folded by session: one line for the conversation, with what ran in it
        let mut by_session = HashMap::new();
        by_session.insert("L".to_string(), false);
        let r = rows(&t, Fold::Open, &by_session, false);
        assert_eq!(
            r.len(),
            1,
            "{:#?}",
            r.iter().map(|x| &x.label).collect::<Vec<_>>()
        );
        assert_eq!(
            r[0].label,
            format!("this session · 2 prompts · {} steps", main.sums.steps())
        );
        assert!(r[0].folded && r[0].foldable);
        // folded by prompt: each with the steps that ran under it
        let r = rows(&t, Fold::Folded, &HashMap::new(), false);
        let p1row = r.iter().find(|x| x.kind == RowKind::Prompt).unwrap();
        assert_eq!(
            p1row.label,
            format!("prompt 1 · you · {} steps", p1.sums.steps())
        );
        // smart: finished prompts closed, except one with an error in it (its subagent's? no: its own)
        let r = rows(&t, Fold::Smart, &HashMap::new(), true);
        let p1row = r.iter().find(|x| x.kind == RowKind::Prompt).unwrap();
        assert!(p1row.folded, "prompt 1 finished cleanly: closed");
        let last = r.iter().rev().find(|x| x.kind == RowKind::Prompt).unwrap();
        assert!(!last.folded, "the running prompt stays open");
        // a click opens it
        let mut tg = HashMap::new();
        tg.insert(p1row.id.clone(), true);
        assert!(rows(&t, Fold::Smart, &tg, true).len() > r.len());
        // folded: one line a prompt
        let f = rows(&t, Fold::Folded, &HashMap::new(), false);
        assert!(f
            .iter()
            .filter(|x| x.kind == RowKind::Prompt)
            .all(|x| x.folded));
    }

    #[test]
    fn codex_calls_belong_to_the_request_after_them() {
        const B: u64 = 1_792_659_000_000;
        let steps = vec![
            st("p\t1792659001000\tX\t\tturn1\tu", "codex"),
            st("ks\t1792659002000\tX\t\tc1\tshell\ttool", "codex"),
            st("ke\t1792659002100\tX\t\tc1\tok\t0", "codex"),
            st("ke\t1792659002200\tX\t\tc1\terr\t1500", "codex"),
        ];
        let mut r = req(1792659003, "X", "", 10);
        r.agent = "codex".into();
        let t = build("b31", "codex", "X", &steps, &[r], &prices::Table::default());
        let turn = &t.lanes[0].prompts[0].turns[0];
        assert_eq!(turn.tools.len(), 1);
        assert_eq!(
            (turn.tools[0].status.as_str(), turn.tools[0].end),
            ("err", Some(B + 3_500))
        );
        assert_eq!(turn.sums.errors, 1);
    }

    #[test]
    fn repeats_fold() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            group(&s(&["Read", "Read", "Read", "Edit"])),
            vec![Group::Run(0, 3), Group::One(3)]
        );
        assert_eq!(
            group(&s(&[
                "think", "Bash", "think", "Bash", "think", "Bash", "Edit"
            ])),
            vec![Group::Repeat(0, 2, 3), Group::One(6)]
        );
        assert_eq!(
            group(&s(&["Read", "Read", "Edit"])),
            vec![Group::One(0), Group::One(1), Group::One(2)]
        );
        assert_eq!(
            group(&s(&["a", "b", "c", "a", "b", "c"])),
            vec![Group::Repeat(0, 3, 2)]
        );
        assert_eq!(group(&[]), Vec::<Group>::new());
        assert_eq!(short_model("claude-opus-4-5-20251101"), "opus-4-5");
        assert_eq!(short_model("anthropic/claude-sonnet-4.5"), "sonnet-4.5");
        assert_eq!(short_model("qwen/qwen3-coder:free"), "qwen3-coder free");
        assert_eq!(short_model("gemini-3.8-flash-n"), "gemini-3.8-flash-n");
    }

    #[test]
    fn tight_figures_keep_the_money() {
        let r = Row {
            id: "p".into(),
            depth: 0,
            kind: RowKind::Prompt,
            label: "prompt 1".into(),
            start: 0,
            end: 13_800,
            sums: Sums {
                input: 2_000,
                output: 400,
                cost: 0.1,
                requests: 2,
                tools: 5,
                ..Default::default()
            },
            foldable: true,
            folded: false,
            bad: false,
            lane: String::new(),
            key: String::new(),
        };
        let all = figures(&r);
        assert!(
            all.contains("2 req · 5 tools") && all.ends_with("$0.10"),
            "{all}"
        );
        // the counts go first, then the tokens, then the time
        assert_eq!(figures_in(&r, 30), "13.8s · 2.0k→400 · $0.10");
        assert_eq!(figures_in(&r, 14), "13.8s · $0.10");
        assert_eq!(figures_in(&r, 5), "$0.10");
    }
}
