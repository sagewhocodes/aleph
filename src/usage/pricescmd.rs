//! `aleph usage prices`: what each model costs now, where that came from, what's
//! held. `sync` reads the lists now; `accept MODEL` takes a held change.

use super::store;
use crate::cli::Args;
use crate::out::{fail, head, hint, json, ok, out, warn};
use crate::pricelog::{self, Held, BUILT_IN};
use crate::prices::{normalize, Price, Table};
use crate::ui::{self, c};
use ratatui::prelude::*;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::process::ExitCode;

pub fn cmd(a: &Args) -> ExitCode {
    let words = a.positional();
    match words.get(1).copied() {
        Some("sync") => sync(a),
        Some("accept") => match words.get(2) {
            Some(m) => accept(m),
            None => fail("usage: aleph usage prices accept MODEL"),
        },
        filter => show(a, filter),
    }
}

fn sync(a: &Args) -> ExitCode {
    let r = pricelog::sync_now();
    if a.has("--json") {
        json(&json!({
            "added": r.added,
            "changed": r.changed.iter().map(|l| l.model.clone()).collect::<Vec<_>>(),
            "held": r.held.iter().map(held_json).collect::<Vec<_>>(),
            "errors": r.errors.iter().map(|(n, e)| json!({"list": n, "error": e})).collect::<Vec<_>>(),
        }));
        return ExitCode::SUCCESS;
    }
    for (name, e) in &r.errors {
        warn(&format!("{name}: {e}"));
    }
    for l in &r.changed {
        ok(&format!(
            "{}: now {} in, {} out ({})",
            shown_id(&l.model),
            money(l.price.base.input),
            money(l.price.base.output),
            l.src
        ));
    }
    if r.added > 0 {
        ok(&format!("{} models added", r.added));
    }
    if r.changed.is_empty() && r.added == 0 && r.errors.is_empty() {
        ok("nothing changed");
    }
    show_held(&r.held);
    ExitCode::SUCCESS
}

fn accept(model: &str) -> ExitCode {
    match pricelog::accept(model) {
        Ok(l) => {
            ok(&format!(
                "{}: {} in, {} out from now on",
                shown_id(&l.model),
                money(l.price.base.input),
                money(l.price.base.output)
            ));
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// `or:x` is what OpenRouter charges for x.
fn shown_id(id: &str) -> String {
    match id.strip_prefix("or:") {
        Some(m) => format!("{m} via OpenRouter"),
        None => id.to_string(),
    }
}

fn money(x: f64) -> String {
    let s = format!("{x:.4}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("${}", if s.is_empty() { "0" } else { s })
}

fn held_json(h: &Held) -> Value {
    json!({"model": h.model, "field": h.field, "have": h.have, "says": h.says,
           "why": h.why, "since": h.since})
}

fn show_held(held: &[Held]) {
    let mut models: Vec<&str> = held.iter().map(|h| h.model.as_str()).collect();
    models.sort_unstable();
    models.dedup();
    for h in held {
        warn(&format!(
            "{}: {} would go from {} to {}: held, {}",
            shown_id(&h.model),
            h.field,
            money(h.have),
            money(h.says),
            h.why
        ));
    }
    if let Some(m) = models.first() {
        let m = m.strip_prefix("or:").unwrap_or(m);
        hint(&format!(
            "take it from now on: aleph usage prices accept {m}"
        ));
    }
}

/// One model's row.
struct Row {
    id: String,
    price: Option<Price>,
    source: String,
    since: u64,
    apart: Vec<(&'static str, f64, f64)>,
}

fn rows(a: &Args, filter: Option<&str>, table: &Table, lists: &pricelog::Lists) -> Vec<Row> {
    let dated = pricelog::load();
    let mut ids: BTreeSet<String> = BTreeSet::new();
    let everything = a.has("--all") || filter.is_some();
    if everything {
        ids.extend(
            dated
                .models()
                .filter(|m| !m.starts_with("or:"))
                .cloned()
                .chain(
                    crate::prices::builtin_table()
                        .iter()
                        .map(|x| x.0.to_string()),
                ),
        );
    } else {
        // what you've used, whatever it's called in the logs
        ids.extend(
            store::rollup(&store::months(), table)
                .iter()
                .filter(|r| !r.model.is_empty())
                .map(|r| normalize(&r.model)),
        );
    }
    if let Some(f) = filter {
        let f = f.to_lowercase();
        ids.retain(|m| m.contains(&f));
        if ids.is_empty() {
            ids.insert(normalize(&f));
        }
    }
    ids.into_iter()
        .map(|id| {
            let latest = dated.latest(&id);
            let source = if table.overridden(&id) {
                "you (analytics.prices)".to_string()
            } else {
                match (latest, table.get(&id)) {
                    (Some(l), _) => l.src.clone(),
                    (None, Some(_)) => BUILT_IN.to_string(),
                    (None, None) => String::new(),
                }
            };
            Row {
                price: table.get(&id),
                source,
                since: latest.map_or(0, |l| l.from),
                apart: pricelog::apart(lists, &id),
                id,
            }
        })
        .collect()
}

fn show(a: &Args, filter: Option<&str>) -> ExitCode {
    let table = Table::from_cfg(&a.cfg);
    let st = pricelog::State::load();
    let lists = pricelog::read_lists();
    let list = rows(a, filter, &table, &lists);
    if a.has("--json") {
        json(&json!({
            "models": list.iter().map(row_json).collect::<Vec<_>>(),
            "held": st.held.iter().map(held_json).collect::<Vec<_>>(),
            "synced": st.at,
            "lists": st.lists.iter().map(|(n, l)| json!({
                "name": n, "read_at": l.read_at, "models": l.models, "error": l.error,
            })).collect::<Vec<_>>(),
        }));
        return ExitCode::SUCCESS;
    }
    if !std::io::stdout().is_terminal() {
        println!("model\tinput\toutput\tcache_read\tcache_write_5m\tcache_write_1h\tlong_above\tsource\tsince");
        for r in &list {
            let (i, o, cr, w5, w1, long) = match &r.price {
                Some(p) => (
                    p.base.input,
                    p.base.output,
                    p.base.cache_read,
                    p.base.cache_write_5m,
                    p.base.cache_write_1h,
                    p.long.map_or(0, |x| x.0),
                ),
                None => (0.0, 0.0, 0.0, 0.0, 0.0, 0),
            };
            println!(
                "{}\t{i}\t{o}\t{cr}\t{w5}\t{w1}\t{long}\t{}\t{}",
                r.id, r.source, r.since
            );
        }
        show_held(&st.held);
        return ExitCode::SUCCESS;
    }
    head(&format!(
        "Prices · $ per million tokens · {} models{}",
        list.len(),
        if a.has("--all") || filter.is_some() {
            ""
        } else {
            " you've used (--all for every one)"
        }
    ));
    let name_w = list
        .iter()
        .map(|r| ui::width(&r.id))
        .max()
        .unwrap_or(5)
        .clamp(5, 34);
    let num = |s: String| format!("{s:>8}");
    out(Line::from(Span::styled(
        format!(
            "{}  {}  {}  {}  {}  {}  {:<12}  {}",
            ui::pad("MODEL", name_w),
            num("IN".into()),
            num("OUT".into()),
            num("READ".into()),
            num("WRITE".into()),
            num("1H WRITE".into()),
            "LONG PROMPTS",
            "SOURCE"
        ),
        ui::faint(),
    )));
    let now = crate::cache::now();
    for r in &list {
        let id = Span::styled(ui::pad(&ui::trunc(&r.id, name_w), name_w), ui::bold());
        let Some(p) = &r.price else {
            out(Line::from(vec![
                id,
                Span::styled("  no price anywhere yet", ui::muted()),
            ]));
            continue;
        };
        let long = p.long.map_or(String::new(), |(above, l)| {
            format!(
                "{}k+ {}",
                above / 1000,
                money(l.input).trim_start_matches('$')
            )
        });
        let when = if r.since > 0 {
            format!(" · since {}", ui::ago(now.saturating_sub(r.since)))
        } else {
            String::new()
        };
        out(Line::from(vec![
            id,
            Span::raw(format!(
                "  {}  {}  {}  {}  {}  {:<12}  ",
                num(short(p.base.input)),
                num(short(p.base.output)),
                num(short(p.base.cache_read)),
                num(short(p.base.cache_write_5m)),
                num(short(p.base.cache_write_1h)),
                long
            )),
            Span::styled(format!("{}{when}", r.source), ui::faint()),
        ]));
        for (name, i, o) in &r.apart {
            out(Line::from(Span::styled(
                format!("  {name} says {} in, {} out", money(*i), money(*o)),
                ui::fg(c::YELLOW()),
            )));
        }
    }
    println!();
    if st.at == 0 {
        hint("the price lists haven't been read yet: aleph usage prices sync");
    }
    for (name, l) in &st.lists {
        let text = if !l.error.is_empty() {
            format!("{name}: {} (kept what it said last)", l.error)
        } else {
            format!(
                "{name}: read {} · {} models",
                ui::ago(now.saturating_sub(l.read_at)),
                l.models
            )
        };
        let style = if l.error.is_empty() {
            ui::faint()
        } else {
            ui::fg(c::RED())
        };
        out(Line::from(Span::styled(text, style)));
    }
    show_held(&st.held);
    ExitCode::SUCCESS
}

fn short(x: f64) -> String {
    money(x).trim_start_matches('$').to_string()
}

fn row_json(r: &Row) -> Value {
    let rates = |x: &crate::prices::Rates| {
        json!({"input": x.input, "output": x.output, "cache_read": x.cache_read,
               "cache_write_5m": x.cache_write_5m, "cache_write_1h": x.cache_write_1h})
    };
    json!({
        "model": r.id,
        "price": r.price.as_ref().map(|p| rates(&p.base)),
        "long": r.price.as_ref().and_then(|p| p.long.as_ref()).map(|(above, l)| {
            json!({"above": above, "rates": rates(l)})
        }),
        "source": r.source,
        "since": r.since,
        "apart": r.apart.iter().map(|(n, i, o)| json!({"list": n, "input": i, "output": o})).collect::<Vec<_>>(),
    })
}
