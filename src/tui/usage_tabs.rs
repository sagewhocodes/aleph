//! Usage's screens (`usage.layout`): five tabs — Overview, Breakdown,
//! Trends, Turns & sessions, Insights (`v` / `V`, or a click) — or all of it
//! as one long page (j/k scroll it), or a pivot you build (rows `b`, split
//! `s`, measure `n`, chart `c`). `L` cycles the three. The breakup and the
//! daily columns split by what the rows are (`b`, or `g`).

use super::charts::{self, Part};
use super::Dash;
use crate::ui::{self, c};
use crate::usage::dash::{self, Data};
use crate::usage::{By, Row};
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

pub(crate) const TABS: [&str; 5] = [
    "Overview",
    "Breakdown",
    "Trends",
    "Turns & sessions",
    "Insights",
];
pub(crate) const LAYOUTS: [&str; 3] = ["tabs", "page", "pivot"];
/// What a row's split under it can be (the Breakdown tree, the pivot: `s`).
pub(crate) const SPLITS: [(By, &str); 4] = [
    (By::Account, "account"),
    (By::Agent, "agent"),
    (By::Host, "machine"),
    (By::Project, "project"),
];
/// The pivot's measures (`n`) and charts (`c`).
pub(crate) const MEASURES: [&str; 7] = [
    "cost", "tokens", "requests", "prompts", "sessions", "active", "$/prompt",
];
pub(crate) const CHARTS: [&str; 3] = ["bars", "columns", "table"];

/// The view's own state.
#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub tab: usize,
    /// 0 tabs, 1 page, 2 pivot (`usage.layout`, `L`)
    pub layout: Option<usize>,
    /// the pivot's split (`s`), measure (`n`), chart (`c`)
    pub pivot_split: usize,
    pub measure: usize,
    pub chart: usize,
    /// the long page: the first section shown
    pub scroll: usize,
}

impl View {
    pub fn layout(&self, d: &crate::config::Config) -> usize {
        self.layout.unwrap_or_else(|| {
            LAYOUTS
                .iter()
                .position(|x| *x == d.get_or("usage.layout"))
                .unwrap_or(0)
        })
    }
}

fn money(v: f64) -> String {
    let m = crate::timeline::money(v);
    if m.is_empty() {
        "$0".into()
    } else {
        m
    }
}

fn panel(f: &mut Frame, r: Rect, title: &str) -> Rect {
    let block = ui::panel(
        Line::from(Span::styled(format!(" {title} "), ui::bold())),
        false,
    );
    let inner = block.inner(r);
    f.render_widget(block, r);
    inner
}

fn lines(f: &mut Frame, r: Rect, v: Vec<Line<'static>>) {
    f.render_widget(Paragraph::new(v), r);
}

/// ▲ 18% (more: the accent's warm side), ▼ 7%, = against the period before.
fn change(now: f64, before: f64) -> Span<'static> {
    if before <= 0.0 {
        return Span::styled(if now > 0.0 { " new" } else { "" }, ui::faint());
    }
    let p = 100.0 * (now - before) / before;
    if p.abs() < 0.5 {
        Span::styled(" =", ui::faint())
    } else if p > 0.0 {
        Span::styled(format!(" ▲{p:.0}%"), ui::fg(c::PEACH()))
    } else {
        Span::styled(format!(" ▼{:.0}%", -p), ui::fg(c::GREEN()))
    }
}

/// The tab strip (clickable).
pub(crate) fn strip(d: &mut Dash, f: &mut Frame, r: Rect) {
    let v = d.usage.view.clone();
    let mut x = r.x;
    let mut spans = Vec::new();
    for (i, t) in TABS.iter().enumerate() {
        let text = format!(" {t} ");
        let w = ui::width(&text) as u16;
        if x + w > r.x + r.width {
            break;
        }
        d.hits.chips.push((
            Rect::new(x, r.y, w, 1),
            super::Chip::Usage(super::usage_filters::UChip::Tab(i)),
        ));
        spans.push(Span::styled(
            text,
            if i == v.tab {
                Style::new()
                    .fg(c::BASE())
                    .bg(c::ACCENT())
                    .add_modifier(Modifier::BOLD)
            } else {
                ui::muted()
            },
        ));
        spans.push(Span::raw(" "));
        x += w + 1;
    }
    spans.push(Span::styled("  v next · L layout", ui::faint()));
    f.render_widget(Paragraph::new(Line::from(spans)), r);
}

/// Six numbers, each against the period before, with its days as a sparkline.
fn kpis(f: &mut Frame, r: Rect, data: &Data) {
    let t = &data.total;
    let p = &data.prev;
    let m = &data.more;
    let spark = |k: &str| -> Vec<f64> {
        let v = m.per_day.get(k).cloned().unwrap_or_default();
        let max = v.iter().copied().fold(0.0, f64::max);
        v.iter()
            .map(|x| if max > 0.0 { x / max } else { 0.0 })
            .collect()
    };
    let cost_days: Vec<f64> = {
        let v: Vec<f64> = m.daily.iter().map(|d| d.2.iter().sum()).collect();
        let max = v.iter().copied().fold(0.0, f64::max);
        v.iter()
            .map(|x| if max > 0.0 { x / max } else { 0.0 })
            .collect()
    };
    let items: Vec<(&str, String, Span<'static>, Vec<f64>)> = vec![
        (
            "cost",
            dash::row_money(t),
            change(t.cost(), p.cost()),
            cost_days,
        ),
        (
            "tokens",
            dash::tokens(t.tokens()),
            change(t.tokens() as f64, p.tokens() as f64),
            spark("tokens"),
        ),
        (
            "requests",
            t.requests.to_string(),
            change(t.requests as f64, p.requests as f64),
            spark("requests"),
        ),
        (
            "prompts",
            t.prompts.to_string(),
            change(t.prompts as f64, p.prompts as f64),
            spark("prompts"),
        ),
        (
            "sessions",
            t.sessions.to_string(),
            change(t.sessions as f64, p.sessions as f64),
            spark("sessions"),
        ),
        (
            "active",
            ui::dur(t.active_secs()),
            change(t.active_secs() as f64, p.active_secs() as f64),
            spark("active"),
        ),
    ];
    let per = if r.width >= 120 {
        6
    } else if r.width >= 60 {
        3
    } else {
        2
    };
    let rows = items.len().div_ceil(per);
    let row_rs = Layout::vertical(vec![Constraint::Length(3); rows]).split(r);
    for (ri, chunk) in items.chunks(per).enumerate() {
        let Some(rr) = row_rs.get(ri) else { break };
        let cols = Layout::horizontal(vec![Constraint::Ratio(1, per as u32); per]).split(*rr);
        for (i, (name, val, ch, sp)) in chunk.iter().enumerate() {
            let w = cols[i].width.saturating_sub(1) as usize;
            lines(
                f,
                cols[i],
                vec![
                    Line::from(Span::styled(name.to_string(), ui::faint())),
                    Line::from(vec![
                        Span::styled(val.clone(), ui::bold().fg(c::ACCENT())),
                        ch.clone(),
                    ]),
                    Line::from(ui::spark(sp, w.min(24), c::ACCENT())),
                ],
            );
        }
    }
}

/// The cost broken down by what the rows are (`b`): the bar and its legend.
fn breakup(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let m = &data.more;
    let split = super::usage::by_name(d);
    let inner = panel(
        f,
        r,
        &format!("{} · by {split} (b)", dash::row_money(&data.total)),
    );
    if inner.height == 0 {
        return;
    }
    let parts: Vec<Part> = m
        .breakup
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, (_, l, cost, _))| Part {
            name: l.clone(),
            value: *cost,
            color: charts::series(i),
        })
        .collect();
    let mut v = vec![charts::share_bar(&parts, inner.width as usize)];
    v.extend(charts::legend(&parts, inner.width as usize, &|x| money(x)));
    lines(f, inner, v);
}

/// Cost a day, stacked by what the rows are (`b`).
fn daily(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let m = &data.more;
    let split = super::usage::by_name(d);
    let inner = panel(f, r, &format!("cost by day · by {split}"));
    let cols: Vec<(String, Vec<f64>)> = m
        .daily
        .iter()
        .map(|(_, l, v)| (l.clone(), v.clone()))
        .collect();
    let colors: Vec<Color> = (0..m.series.len()).map(charts::series).collect();
    let mut v = charts::columns(
        &cols,
        &colors,
        inner.width as usize,
        inner.height.saturating_sub(1) as usize,
        &|x| money(x),
    );
    let legend: Vec<Span<'static>> = m
        .series
        .iter()
        .enumerate()
        .flat_map(|(i, (_, l))| {
            vec![
                Span::styled(
                    format!("{} ", if c::mono() { charts::glyph(i) } else { "■" }),
                    Style::new().fg(charts::series(i)),
                ),
                Span::styled(format!("{l}  "), ui::muted()),
            ]
        })
        .collect();
    v.push(Line::from(legend));
    lines(f, inner, v);
}

fn heat(f: &mut Frame, r: Rect, data: &Data) {
    let inner = panel(f, r, "when · hour × weekday");
    let h: [[f64; 24]; 7] =
        std::array::from_fn(|d| std::array::from_fn(|x| data.heat[d][x] as f64));
    lines(f, inner, charts::week_hours(&h, inner.width as usize));
}

fn calendar(f: &mut Frame, r: Rect, data: &Data) {
    let inner = panel(f, r, "the last 12 weeks");
    let today = ui::local_day(crate::cache::now());
    let weeks = ((inner.width as usize).saturating_sub(3) / 2).clamp(1, 12);
    lines(
        f,
        inner,
        charts::calendar(&data.more.calendar, today, weeks),
    );
}

/// Overview: the numbers, the breakup, cost by day, when.
fn overview(d: &mut Dash, f: &mut Frame, r: Rect, data: &Data) {
    let kh = if r.width >= 120 {
        3
    } else if r.width >= 60 {
        6
    } else {
        9
    };
    let lh = (data
        .more
        .breakup
        .len()
        .min(8)
        .div_ceil(if r.width >= 120 { 3 } else { 1 }) as u16
        + 3)
    .min(8);
    let [k, b, rest] = Layout::vertical([
        Constraint::Length(kh),
        Constraint::Length(lh),
        Constraint::Min(4),
    ])
    .areas(r);
    kpis(f, k, data);
    breakup(d, f, b, data);
    if rest.width >= 110 {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)])
                .areas(rest);
        daily(d, f, left, data);
        // the heatmap and calendar take what they draw; what's left of a tall
        // screen goes to the costliest work
        let [h, cal, more] = Layout::vertical([
            Constraint::Length(10),
            Constraint::Length(9),
            Constraint::Min(0),
        ])
        .areas(right);
        heat(f, h, data);
        calendar(f, cal, data);
        if more.height >= 4 {
            costliest(f, more, data);
        }
    } else {
        daily(d, f, rest, data);
    }
}

/// The costliest sessions in this range, as many as fit.
fn costliest(f: &mut Frame, r: Rect, data: &Data) {
    let inner = panel(f, r, "the costliest sessions");
    lines(f, inner, session_rows(&data.more.stats));
}

/// A line per costly session: what it cost, its agent, machine, time, prompts, project.
fn session_rows(st: &crate::usage::Stats) -> Vec<Line<'static>> {
    st.top_sessions
        .iter()
        .map(|s| {
            let mut p = s.key.splitn(3, ':');
            let (h, a) = (p.next().unwrap_or(""), p.next().unwrap_or(""));
            Line::from(vec![
                Span::styled(format!("{:>8} ", money(s.cost())), ui::fg(c::ACCENT())),
                Span::styled(format!("{:<12} ", crate::agents::label(a)), ui::bold()),
                Span::styled(format!("{h:<8} "), ui::muted()),
                Span::styled(format!("{:<7} ", ui::dur(s.active_secs())), ui::faint()),
                Span::styled(
                    format!(
                        "{} pr · {}",
                        s.prompts,
                        s.projects.first().cloned().unwrap_or_default()
                    ),
                    ui::faint(),
                ),
            ])
        })
        .collect()
}

/// A breakdown row's numbers.
fn row_line(
    name: String,
    name_w: usize,
    r: &Row,
    total: f64,
    w: usize,
    st: Style,
    sel: bool,
) -> Line<'static> {
    let mut v = vec![
        Span::styled(if sel { "┃ " } else { "  " }, ui::fg(c::ACCENT())),
        Span::styled(ui::pad(&ui::trunc(&name, name_w), name_w), st),
        Span::raw(" "),
    ];
    let bar_w = w.saturating_sub(name_w + 78).min(16);
    if bar_w >= 4 {
        v.extend(ui::bar(
            if total > 0.0 { r.cost() / total } else { 0.0 },
            bar_w,
            c::ACCENT(),
        ));
        v.push(Span::raw(" "));
    }
    v.push(Span::styled(
        format!("{:>9}", dash::row_money(r)),
        ui::fg(c::ACCENT()),
    ));
    v.push(Span::styled(
        format!(
            "  {:>6}→{:<6}",
            dash::tokens(r.input + r.cache_read + r.cache_write()),
            dash::tokens(r.output)
        ),
        ui::muted(),
    ));
    v.push(Span::styled(
        format!(
            " {:>5} req {:>4} pr {:>3} ses",
            r.requests, r.prompts, r.sessions
        ),
        ui::faint(),
    ));
    if w > name_w + 70 {
        v.push(Span::styled(
            format!(
                "  {:>7}/pr {:>7}/ses",
                r.per_prompt().map(money).unwrap_or_else(|| "–".into()),
                r.per_session().map(money).unwrap_or_else(|| "–".into())
            ),
            ui::faint(),
        ));
    }
    let mut l = Line::from(v);
    if sel {
        l = l.style(ui::selected());
    }
    l
}

fn label(by: By, key: &str, data: &Data) -> String {
    match by {
        By::Account => data
            .names
            .get(key)
            .cloned()
            .unwrap_or_else(|| "account unknown".into()),
        By::Agent => crate::agents::label(key),
        By::Project => crate::usage::projects::name_of(key),
        _ if key.is_empty() => "—".into(),
        _ => key.to_string(),
    }
}

/// Breakdown: each row (`b` picks what), its split under it (`s`).
fn breakdown(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let root = super::usage::by_of(d);
    let split = SPLITS[d.usage.view.pivot_split % SPLITS.len()];
    let inner = panel(
        f,
        r,
        &format!(
            "by {} → {} (b · s · enter narrows)",
            root_name(root),
            split.1
        ),
    );
    let w = inner.width as usize;
    let total = data.total.cost();
    let name_w = (w / 3).clamp(12, 34);
    let mut out: Vec<Line> = vec![Line::from(Span::styled(
        format!(
            "  {:<name_w$}            cost      in→out     req   pr ses       $/prompt  $/session",
            ""
        ),
        ui::faint(),
    ))];
    for (i, (row, kids)) in data.more.tree.iter().enumerate() {
        let sel = i == d.usage.sel;
        out.push(row_line(
            label(root, &row.key, data),
            name_w,
            row,
            total,
            w,
            ui::bold(),
            sel,
        ));
        if kids.len() > 1 || kids.iter().any(|k| !k.key.is_empty() && k.key != row.key) {
            for k in kids.iter().take(6) {
                out.push(row_line(
                    format!("  {}", label(split.0, &k.key, data)),
                    name_w,
                    k,
                    total,
                    w,
                    ui::muted(),
                    false,
                ));
            }
        }
    }
    let room = inner.height as usize;
    let first = out
        .len()
        .saturating_sub(room)
        .min(d.usage.sel.saturating_sub(room / 3));
    let shown: Vec<Line> = out.into_iter().skip(first).take(room).collect();
    lines(f, inner, shown);
}

fn root_name(b: By) -> &'static str {
    match b {
        By::Host => "machine",
        _ => b.name(),
    }
}

/// Trends: this period's cost against the one before, the model mix, the days.
fn trends(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let m = &data.more;
    let [top, mid, bot] = Layout::vertical([
        Constraint::Percentage(40),
        Constraint::Percentage(30),
        Constraint::Percentage(30),
    ])
    .areas(r);
    let inner = panel(
        f,
        top,
        "cost so far · this period (accent) and the one before (faint)",
    );
    let mut v = charts::lines(
        &[
            (m.cum_prev.clone(), c::FAINT()),
            (m.cum.clone(), c::ACCENT()),
        ],
        inner.width as usize,
        inner.height.saturating_sub(1) as usize,
        &|x| money(x),
    );
    let now = m.cum.last().copied().unwrap_or(0.0);
    let before = m.cum_prev.last().copied().unwrap_or(0.0);
    v.push(Line::from(vec![
        Span::styled(format!("{} now", money(now)), ui::fg(c::ACCENT())),
        Span::styled(format!(" · {} before", money(before)), ui::faint()),
        change(now, before),
    ]));
    lines(f, inner, v);
    let [ml, mr] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(mid);
    let inner = panel(f, ml, "model mix · share of tokens a day");
    let colors: Vec<Color> = (0..m.models.len()).map(charts::series).collect();
    let mut v = charts::columns(
        &m.model_mix,
        &colors,
        inner.width as usize,
        inner.height.saturating_sub(1) as usize,
        &|x| format!("{:.0}%", x * 100.0),
    );
    v.push(Line::from(
        m.models
            .iter()
            .enumerate()
            .flat_map(|(i, n)| {
                vec![
                    Span::styled("■ ", Style::new().fg(charts::series(i))),
                    Span::styled(format!("{n}  "), ui::muted()),
                ]
            })
            .collect::<Vec<_>>(),
    ));
    lines(f, inner, v);
    let inner = panel(f, mr, "cache hits a day");
    let cache = m.per_day.get("cache").cloned().unwrap_or_default();
    lines(
        f,
        inner,
        charts::lines(
            &[(cache, c::GREEN())],
            inner.width as usize,
            inner.height as usize,
            &|x| format!("{:.0}%", x * 100.0),
        ),
    );
    let [bl, br] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bot);
    let inner = panel(f, bl, "tokens a day");
    let t = m.per_day.get("tokens").cloned().unwrap_or_default();
    lines(
        f,
        inner,
        charts::lines(
            &[(t, c::BLUE())],
            inner.width as usize,
            inner.height as usize,
            &|x| dash::tokens(x as u64),
        ),
    );
    let inner = panel(f, br, "requests and prompts a day");
    let rq = m.per_day.get("requests").cloned().unwrap_or_default();
    let pr = m.per_day.get("prompts").cloned().unwrap_or_default();
    lines(
        f,
        inner,
        charts::lines(
            &[(rq, c::MAUVE()), (pr, c::PEACH())],
            inner.width as usize,
            inner.height as usize,
            &|x| format!("{x:.0}"),
        ),
    );
    let _ = d;
}

/// Turns & sessions: how prompts and sessions spread, and the costliest.
fn turns(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let st = &data.more.stats;
    let [top, bot] =
        Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(r);
    let [a, b, cc] = Layout::horizontal([
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
        Constraint::Ratio(1, 3),
    ])
    .areas(top);
    for (rr, title, dist, fmt) in [
        (
            a,
            "$ a prompt",
            &st.prompt_cost,
            &(|x: f64| money(x)) as &dyn Fn(f64) -> String,
        ),
        (b, "time a prompt", &st.prompt_secs, &|x: f64| {
            ui::dur(x as u64)
        }),
        (cc, "$ a session", &st.session_cost, &|x: f64| money(x)),
    ] {
        let inner = panel(f, rr, &format!("{title} · {} of them", dist.n));
        let mut v = vec![Line::from(Span::styled(
            format!(
                "median {} · p90 {} · p99 {} · most {}",
                fmt(dist.p50),
                fmt(dist.p90),
                fmt(dist.p99),
                fmt(dist.max)
            ),
            ui::muted(),
        ))];
        v.extend(charts::histogram(
            &dist.buckets,
            inner.width as usize,
            inner.height.saturating_sub(1) as usize,
            c::ACCENT(),
            fmt,
        ));
        lines(f, inner, v);
    }
    let [l, rr] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(bot);
    let inner = panel(f, l, "the costliest prompts");
    let mut v = Vec::new();
    for p in &st.top_prompts {
        v.push(Line::from(vec![
            Span::styled(format!("{:>8} ", money(p.cost)), ui::fg(c::ACCENT())),
            Span::styled(
                format!("{:<12} ", crate::agents::label(&p.agent)),
                ui::bold(),
            ),
            Span::styled(format!("{:<8} ", p.host), ui::muted()),
            Span::styled(format!("{:<7} ", ui::dur(p.secs)), ui::faint()),
            Span::styled(
                format!(
                    "{} req · {}",
                    p.requests,
                    crate::usage::projects::name_of(&p.pid)
                ),
                ui::faint(),
            ),
        ]));
    }
    if st.outside.0 > 0 {
        v.push(Line::from(Span::styled(
            format!(
                "{} requests ({}) in no prompt: agents that don't mark their turns",
                st.outside.0,
                money(st.outside.1)
            ),
            ui::faint(),
        )));
    }
    lines(f, inner, v);
    let inner = panel(f, rr, "the costliest sessions");
    lines(f, inner, session_rows(st));
    let _ = d;
}

/// Insights: the lines, the tools, plans.
fn insights(f: &mut Frame, r: Rect, data: &Data) {
    let m = &data.more;
    let [l, rr] =
        Layout::horizontal([Constraint::Percentage(60), Constraint::Percentage(40)]).areas(r);
    let inner = panel(f, l, "insights");
    let mut v: Vec<Line> = m
        .insights
        .iter()
        .map(|(k, x)| {
            Line::from(vec![
                Span::styled(format!("{k:<20}"), ui::faint()),
                Span::styled(x.clone(), ui::muted()),
            ])
        })
        .collect();
    v.extend(super::usage::limit_lines(&data.limits));
    lines(f, inner, v);
    let [t, cal] = Layout::vertical([Constraint::Min(4), Constraint::Length(10)]).areas(rr);
    let inner = panel(f, t, "tools the agents used");
    let most = m.tools.first().map(|x| x.1).unwrap_or(1).max(1);
    let w = inner.width as usize;
    let v: Vec<Line> = m
        .tools
        .iter()
        .take(inner.height as usize)
        .map(|(n, k, bad)| {
            let mut s = vec![
                Span::styled(ui::pad(&ui::trunc(n, 18), 18), ui::bold()),
                Span::raw(" "),
            ];
            s.extend(ui::bar(
                *k as f64 / most as f64,
                w.saturating_sub(34).max(4),
                c::BLUE(),
            ));
            s.push(Span::styled(format!(" {k:>5}"), ui::muted()));
            if *bad > 0 {
                s.push(Span::styled(format!(" {bad} failed"), ui::fg(c::RED())));
            }
            Line::from(s)
        })
        .collect();
    lines(
        f,
        inner,
        if v.is_empty() {
            vec![Line::from(Span::styled(
                "no tool calls read yet",
                ui::faint(),
            ))]
        } else {
            v
        },
    );
    calendar(f, cal, data);
}

/// The pivot: rows (`b`), split (`s`), measure (`n`), chart (`c`).
fn pivot(d: &Dash, f: &mut Frame, r: Rect, data: &Data) {
    let v = &d.usage.view;
    let root = super::usage::by_of(d);
    let split = SPLITS[v.pivot_split % SPLITS.len()];
    let measure = MEASURES[v.measure % MEASURES.len()];
    let chart = CHARTS[v.chart % CHARTS.len()];
    let inner = panel(
        f,
        r,
        &format!(
            "rows {} (b) · split {} (s) · {measure} (n) · {chart} (c)",
            root_name(root),
            split.1
        ),
    );
    let val = |r: &Row| -> f64 {
        match measure {
            "tokens" => r.tokens() as f64,
            "requests" => r.requests as f64,
            "prompts" => r.prompts as f64,
            "sessions" => r.sessions as f64,
            "active" => r.active_secs() as f64,
            "$/prompt" => r.per_prompt().unwrap_or(0.0),
            _ => r.cost(),
        }
    };
    let fmt = |x: f64| -> String {
        match measure {
            "tokens" => dash::tokens(x as u64),
            "active" => ui::dur(x as u64),
            "cost" | "$/prompt" => money(x),
            _ => format!("{x:.0}"),
        }
    };
    let w = inner.width as usize;
    let name_w = (w / 4).clamp(10, 28);
    // the split's keys across every row, biggest first
    let mut keys: Vec<(String, f64)> = Vec::new();
    for (_, kids) in &data.more.tree {
        for k in kids {
            match keys.iter_mut().find(|x| x.0 == k.key) {
                Some(x) => x.1 += val(k),
                None => keys.push((k.key.clone(), val(k))),
            }
        }
    }
    keys.sort_by(|a, b| b.1.total_cmp(&a.1));
    keys.truncate(8);
    let colors: Vec<Color> = (0..keys.len()).map(charts::series).collect();
    let mut out: Vec<Line> = Vec::new();
    match chart {
        "columns" => {
            let cols: Vec<(String, Vec<f64>)> = data
                .more
                .tree
                .iter()
                .map(|(row, kids)| {
                    (
                        ui::trunc(&label(root, &row.key, data), 8),
                        keys.iter()
                            .map(|(k, _)| kids.iter().find(|x| &x.key == k).map(val).unwrap_or(0.0))
                            .collect(),
                    )
                })
                .collect();
            out = charts::columns(
                &cols,
                &colors,
                w,
                inner.height.saturating_sub(1) as usize,
                &fmt,
            );
        }
        "table" => {
            out.push(Line::from(Span::styled(
                format!(
                    "{:<name_w$} {}",
                    "",
                    keys.iter()
                        .map(|(k, _)| format!("{:>12}", ui::trunc(&label(split.0, k, data), 12)))
                        .collect::<String>()
                ),
                ui::faint(),
            )));
            for (row, kids) in &data.more.tree {
                let mut s = vec![
                    Span::styled(
                        ui::pad(&ui::trunc(&label(root, &row.key, data), name_w), name_w),
                        ui::bold(),
                    ),
                    Span::raw(" "),
                ];
                for (k, _) in &keys {
                    let x = kids.iter().find(|x| &x.key == k).map(val).unwrap_or(0.0);
                    s.push(Span::styled(
                        format!("{:>12}", if x > 0.0 { fmt(x) } else { "·".into() }),
                        ui::muted(),
                    ));
                }
                out.push(Line::from(s));
            }
        }
        _ => {
            let max = data
                .more
                .tree
                .iter()
                .map(|(r, _)| val(r))
                .fold(0.0, f64::max);
            for (row, kids) in &data.more.tree {
                let bw = w.saturating_sub(name_w + 12);
                let len = if max > 0.0 {
                    (val(row) / max * bw as f64).round() as usize
                } else {
                    0
                };
                let parts: Vec<Part> = keys
                    .iter()
                    .enumerate()
                    .map(|(i, (k, _))| Part {
                        name: k.clone(),
                        value: kids.iter().find(|x| &x.key == k).map(val).unwrap_or(0.0),
                        color: colors[i],
                    })
                    .collect();
                let mut s = vec![
                    Span::styled(
                        ui::pad(&ui::trunc(&label(root, &row.key, data), name_w), name_w),
                        ui::bold(),
                    ),
                    Span::raw(" "),
                ];
                s.extend(charts::share_bar(&parts, len.max(1)).spans);
                s.push(Span::styled(
                    format!(" {}", fmt(val(row))),
                    ui::fg(c::ACCENT()),
                ));
                out.push(Line::from(s));
            }
        }
    }
    out.push(Line::from(
        keys.iter()
            .enumerate()
            .flat_map(|(i, (k, _))| {
                vec![
                    Span::styled("■ ", Style::new().fg(colors[i])),
                    Span::styled(format!("{}  ", label(split.0, k, data)), ui::muted()),
                ]
            })
            .collect::<Vec<_>>(),
    ));
    lines(f, inner, out);
}

/// The sections of the long page, in order: (title, height).
/// A long-page section shorter than this isn't drawn.
const MIN_SECTION: u16 = 5;

const PAGE: [(&str, u16); 7] = [
    ("numbers", 3),
    ("breakup", 6),
    ("by day", 14),
    ("when", 10),
    ("turns", 16),
    ("trends", 24),
    ("insights", 22),
];

/// Draw the view's body (after the filters' bar and the by chips).
pub(crate) fn draw(d: &mut Dash, f: &mut Frame, r: Rect, data: &Data) {
    match d.usage.view.layout(&d.cfg) {
        1 => {
            let first = d.usage.view.scroll.min(PAGE.len() - 1);
            let mut y = r.y;
            for (name, h) in PAGE.iter().skip(first) {
                let h = if *name == "numbers" && r.width < 120 {
                    6
                } else {
                    *h
                };
                // the section that doesn't fit whole takes what's left when
                // that's enough to read, rather than a blank end of the screen
                let left = (r.y + r.height).saturating_sub(y);
                if left < MIN_SECTION {
                    break;
                }
                let h = h.min(left);
                let rr = Rect::new(r.x, y, r.width, h);
                match *name {
                    "numbers" => kpis(f, rr, data),
                    "breakup" => breakup(d, f, rr, data),
                    "by day" => daily(d, f, rr, data),
                    "when" => heat(f, rr, data),
                    "turns" => turns(d, f, rr, data),
                    "trends" => trends(d, f, rr, data),
                    _ => insights(f, rr, data),
                }
                y += h;
            }
        }
        2 => pivot(d, f, r, data),
        _ => {
            let [s, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(r);
            strip(d, f, s);
            match d.usage.view.tab % TABS.len() {
                0 => overview(d, f, body, data),
                1 => breakdown(d, f, body, data),
                2 => trends(d, f, body, data),
                3 => turns(d, f, body, data),
                _ => insights(f, body, data),
            }
        }
    }
}

/// The view's keys (true: taken).
pub(crate) fn key(d: &mut Dash, k: &crossterm::event::KeyEvent) -> bool {
    use crossterm::event::KeyCode;
    let v = &mut d.usage.view;
    let layout = v.layout(&d.cfg);
    match k.code {
        KeyCode::Char('v') if layout == 0 => v.tab = (v.tab + 1) % TABS.len(),
        KeyCode::Char('V') if layout == 0 => v.tab = (v.tab + TABS.len() - 1) % TABS.len(),
        KeyCode::Char('L') => {
            let n = (layout + 1) % LAYOUTS.len();
            v.layout = Some(n);
            let _ = d.cfg.set("usage.layout", LAYOUTS[n]);
            d.toast(
                super::Level::Info,
                format!("usage as {}", ["tabs", "one long page", "a pivot"][n]),
            );
        }
        // the old key for the charts' split: the same "by" as `b` now
        KeyCode::Char('g') => super::usage::step_by(d, 1),
        KeyCode::Char('s') => {
            v.pivot_split = (v.pivot_split + 1) % SPLITS.len();
            super::usage::reload(d);
        }
        KeyCode::Char('n') if layout == 2 => v.measure = (v.measure + 1) % MEASURES.len(),
        KeyCode::Char('c') if layout == 2 => v.chart = (v.chart + 1) % CHARTS.len(),
        KeyCode::Char('j') | KeyCode::Down if layout == 1 => {
            v.scroll = (v.scroll + 1).min(PAGE.len() - 1)
        }
        KeyCode::Char('k') | KeyCode::Up if layout == 1 => v.scroll = v.scroll.saturating_sub(1),
        _ => return false,
    }
    true
}
