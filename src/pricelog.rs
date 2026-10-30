//! Prices that move. Three public lists, readable without a login (LiteLLM's,
//! models.dev's, OpenRouter's), are read hourly by the hub
//! (`analytics.prices_sync`), and
//! what they say goes into an append-only log, `usage/prices.jsonl`: a line is
//! a model's whole price from a moment on. A change applies only after it was
//! seen, so tokens already spent keep the price they ran at. The built-in table
//! is every model's first line, so an upgrade can't re-price history either.
//!
//! The guard: a change two lists agree on applies at once. One list alone, or a
//! jump over 50%, is held and shown (`aleph usage prices`) until the others
//! catch up or you accept it. `analytics.prices.*` and the costs agents report
//! always win over all of this.
//!
//! A model's price comes from the lists of whoever owns it; a reseller's entry
//! never counts. OpenRouter's own price is kept beside it (`or:<model>`), for
//! requests routed through it and for models nothing else lists.

use crate::prices::{self, normalize, Price, Rates};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const LITELLM: &str = "litellm";
pub const MODELS_DEV: &str = "models.dev";
pub const OPENROUTER: &str = "openrouter";
pub const BUILT_IN: &str = "built-in";
pub const YOU: &str = "you";

/// The lists, in the order a tie between them is settled.
const LISTS: [(&str, &str); 3] = [
    (
        LITELLM,
        "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json",
    ),
    (MODELS_DEV, "https://models.dev/api.json"),
    (OPENROUTER, "https://openrouter.ai/api/v1/models"),
];

/// Who owns what: a list's provider, and the start of the names it owns there
/// ("" all of them). Anything else in a list is someone reselling a model.
const OWNERS: &[(&str, &str)] = &[
    ("anthropic", "claude"),
    ("openai", ""),
    ("gemini", "gemini"),
    ("google", "gemini"),
    ("xai", "grok"),
    ("deepseek", "deepseek"),
    ("moonshot", "kimi"),
    ("moonshotai", "kimi"),
    ("zai", "glm"),
    ("mistral", ""),
    ("minimax", ""),
    ("dashscope", "qwen"),
    ("alibaba", "qwen"),
    ("cohere", "command"),
    ("meta", "llama"),
];

/// A change bigger than this is held, however many lists agree.
const MAX_JUMP: f64 = 0.5;
/// Fewer models than this in an answer: an error page, not a list.
const MIN_MODELS: usize = 3;
const EVERY: u64 = 3600;
const T_FETCH: u64 = 90;

/// What a list says of one set of rates, $/MTok.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Part {
    pub input: f64,
    pub output: f64,
    pub cache_read: Option<f64>,
    pub write_5m: Option<f64>,
    pub write_1h: Option<f64>,
}

/// One model in one list.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Seen {
    /// by [`normalize`]d id
    pub id: String,
    pub base: Part,
    pub long: Option<(u64, Part)>,
    pub fast: Option<f64>,
    pub geo: Option<bool>,
}

fn owned(provider: &str, name: &str) -> bool {
    let name = name.to_lowercase();
    OWNERS
        .iter()
        .any(|(p, start)| *p == provider && name.starts_with(start))
}

/// Six decimals is plenty, and it stops `4e-06 * 1e6` differing from `4`.
fn tidy(x: f64) -> f64 {
    (x * 1e6).round() / 1e6
}

/// A price as a list writes it, a number or a string; never a negative
/// (OpenRouter's -1: "it varies").
fn number(v: &Value) -> Option<f64> {
    let x = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    (x.is_finite() && x >= 0.0).then_some(x)
}

fn per_token(v: &Value) -> Option<f64> {
    number(v).map(|x| tidy(x * 1e6))
}

/// The name as it stands in a list is the id, not a dated copy of it.
fn is_exact(name: &str, id: &str) -> bool {
    name.to_lowercase().replace('.', "-") == id.replace('.', "-")
}

/// One entry per id: the undated one (`gpt-4o`, not `gpt-4o-2024-05-13`, which
/// was dearer), else the latest.
fn one_each(mut raw: Vec<(String, Seen)>) -> Vec<Seen> {
    raw.sort_by(|a, b| a.0.cmp(&b.0));
    let mut by: BTreeMap<String, (bool, Seen)> = BTreeMap::new();
    for (name, s) in raw {
        let exact = is_exact(&name, &s.id);
        match by.get(&s.id) {
            Some((true, _)) if !exact => {}
            _ => {
                by.insert(s.id.clone(), (exact, s));
            }
        }
    }
    by.into_values().map(|x| x.1).collect()
}

/// LiteLLM's `model_prices_and_context_window.json`, the owners' own entries.
pub fn parse_litellm(v: &Value) -> Vec<Seen> {
    let mut raw = Vec::new();
    for (k, e) in v.as_object().into_iter().flatten() {
        let Some(provider) = e.get("litellm_provider").and_then(Value::as_str) else {
            continue;
        };
        let mode = e.get("mode").and_then(Value::as_str).unwrap_or("");
        let name = k.rsplit('/').next().unwrap_or(k);
        if !matches!(mode, "chat" | "responses") || !owned(provider, name) {
            continue;
        }
        let get = |f: &str| e.get(f).and_then(per_token);
        let (Some(input), Some(output)) =
            (get("input_cost_per_token"), get("output_cost_per_token"))
        else {
            continue;
        };
        if input <= 0.0 && output <= 0.0 {
            continue;
        }
        let long = e
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(f, _)| {
                f.strip_prefix("input_cost_per_token_above_")?
                    .strip_suffix("k_tokens")?
                    .parse::<u64>()
                    .ok()
            })
            .min()
            .and_then(|n| {
                let s = format!("above_{n}k_tokens");
                Some((
                    n * 1000,
                    Part {
                        input: get(&format!("input_cost_per_token_{s}"))?,
                        output: get(&format!("output_cost_per_token_{s}"))?,
                        cache_read: get(&format!("cache_read_input_token_cost_{s}")),
                        write_5m: get(&format!("cache_creation_input_token_cost_{s}")),
                        write_1h: get(&format!("cache_creation_input_token_cost_above_1hr_{s}")),
                    },
                ))
            });
        let extra = e.get("provider_specific_entry");
        raw.push((
            name.to_string(),
            Seen {
                id: normalize(k),
                base: Part {
                    input,
                    output,
                    cache_read: get("cache_read_input_token_cost"),
                    write_5m: get("cache_creation_input_token_cost"),
                    write_1h: get("cache_creation_input_token_cost_above_1hr"),
                },
                long,
                fast: extra.and_then(|x| x.get("fast")).and_then(number),
                geo: extra
                    .and_then(|x| x.get("us"))
                    .and_then(number)
                    .map(|x| x > 1.0),
            },
        ));
    }
    one_each(raw)
}

/// models.dev's `api.json`, the owners' providers.
pub fn parse_models_dev(v: &Value) -> Vec<Seen> {
    let part = |c: &Value| -> Option<Part> {
        Some(Part {
            input: c.get("input").and_then(number)?,
            output: c.get("output").and_then(number)?,
            cache_read: c.get("cache_read").and_then(number),
            write_5m: c.get("cache_write").and_then(number),
            write_1h: None,
        })
    };
    let mut raw = Vec::new();
    for (provider, pv) in v.as_object().into_iter().flatten() {
        for (k, m) in pv
            .get("models")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let name = k.rsplit('/').next().unwrap_or(k);
            if !owned(provider, name) {
                continue;
            }
            let Some(c) = m.get("cost") else { continue };
            let Some(base) = part(c) else { continue };
            if base.input <= 0.0 && base.output <= 0.0 {
                continue;
            }
            let tiered = c
                .get("tiers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|t| t.pointer("/tier/type").and_then(Value::as_str) == Some("context"))
                .filter_map(|t| Some((t.pointer("/tier/size")?.as_u64()?, part(t)?)))
                .min_by_key(|x| x.0);
            let long = tiered.or_else(|| {
                c.get("context_over_200k")
                    .and_then(part)
                    .map(|p| (200_000, p))
            });
            raw.push((
                name.to_string(),
                Seen {
                    id: normalize(k),
                    base,
                    long,
                    ..Seen::default()
                },
            ));
        }
    }
    one_each(raw)
}

/// OpenRouter's `/models`: every paid model. A `:variant` is not the model.
pub fn parse_openrouter(v: &Value) -> Vec<Seen> {
    let part = |p: &Value, i: &str, o: &str| -> Option<Part> {
        Some(Part {
            input: p.get(i).and_then(per_token)?,
            output: p.get(o).and_then(per_token)?,
            cache_read: p.get("input_cache_read").and_then(per_token),
            write_5m: p.get("input_cache_write").and_then(per_token),
            write_1h: p.get("input_cache_write_1h").and_then(per_token),
        })
    };
    let mut raw = Vec::new();
    for m in v
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = m.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(pr) = m.get("pricing") else { continue };
        if id.contains(':') {
            continue;
        }
        let Some(base) = part(pr, "prompt", "completion") else {
            continue;
        };
        if base.input <= 0.0 && base.output <= 0.0 {
            continue;
        }
        let long = pr
            .get("overrides")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| {
                Some((
                    o.get("min_prompt_tokens")?.as_u64()?,
                    part(o, "prompt", "completion")?,
                ))
            })
            .min_by_key(|x| x.0);
        raw.push((
            id.rsplit('/').next().unwrap_or(id).to_string(),
            Seen {
                id: normalize(id),
                base,
                long,
                ..Seen::default()
            },
        ));
    }
    one_each(raw)
}

fn parse_list(name: &str, v: &Value) -> Vec<Seen> {
    match name {
        LITELLM => parse_litellm(v),
        MODELS_DEV => parse_models_dev(v),
        _ => parse_openrouter(v),
    }
}

// ------------------------------------------------------------------ merging

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 0.005 * a.abs().max(b.abs()) + 1e-9
}

/// A value and how many lists say it.
type Vote = (f64, u8);

/// The value most lists give; on a tie, the first (`vals` is in order of trust).
fn vote(vals: &[Option<f64>]) -> Option<Vote> {
    let mut groups: Vec<Vote> = Vec::new();
    for v in vals.iter().flatten() {
        match groups.iter_mut().find(|g| close(g.0, *v)) {
            Some(g) => g.1 += 1,
            None => groups.push((*v, 1)),
        }
    }
    let mut best: Option<Vote> = None;
    for g in groups {
        if best.is_none_or(|b| g.1 > b.1) {
            best = Some(g);
        }
    }
    best
}

/// What the lists together say of a model.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Merged {
    pub input: Option<Vote>,
    pub output: Option<Vote>,
    pub cache_read: Option<Vote>,
    pub write_5m: Option<Vote>,
    pub write_1h: Option<Vote>,
    pub long: Option<(u64, Part, u8)>,
    pub fast: Option<f64>,
    pub geo: Option<bool>,
    pub from: Vec<&'static str>,
    /// lists that disagree on input or output: ("openrouter", in, out)
    pub apart: Vec<(&'static str, f64, f64)>,
}

/// A cache write is never cheaper than the input it writes: below that, a list
/// means something else (Google's is storage, DeepSeek's says "not charged").
fn real_write(write: Option<f64>, input: f64) -> Option<f64> {
    write.filter(|w| *w >= input)
}

/// Merge one model across the lists. `said` is in the lists' order; cache rates
/// trust models.dev first, everything else LiteLLM.
pub fn merge(said: &[(&'static str, &Seen)]) -> Merged {
    let in_order = |f: &dyn Fn(&Seen) -> Option<f64>| -> Vec<Option<f64>> {
        said.iter().map(|(_, s)| f(s)).collect()
    };
    let cache_first = |f: &dyn Fn(&Seen) -> Option<f64>| -> Vec<Option<f64>> {
        let mut v: Vec<(&&str, Option<f64>)> = said.iter().map(|(n, s)| (n, f(s))).collect();
        v.sort_by_key(|(n, _)| **n != MODELS_DEV);
        v.into_iter().map(|x| x.1).collect()
    };
    let input = vote(&in_order(&|s| Some(s.base.input)));
    let output = vote(&in_order(&|s| Some(s.base.output)));
    // the same tier: its threshold and its two main rates
    let mut tiers: Vec<(u64, Part, u8)> = Vec::new();
    for (_, s) in said {
        let Some((above, p)) = s.long else { continue };
        match tiers
            .iter_mut()
            .find(|t| t.0 == above && close(t.1.input, p.input) && close(t.1.output, p.output))
        {
            Some(t) => {
                t.2 += 1;
                t.1.cache_read = t.1.cache_read.or(p.cache_read);
                t.1.write_5m = t.1.write_5m.or(p.write_5m);
                t.1.write_1h = t.1.write_1h.or(p.write_1h);
            }
            None => tiers.push((above, p, 1)),
        }
    }
    let mut long: Option<(u64, Part, u8)> = None;
    for t in tiers {
        if long.as_ref().is_none_or(|b| t.2 > b.2) {
            long = Some(t);
        }
    }
    let apart = said
        .iter()
        .filter(|(_, s)| {
            !input.is_some_and(|i| close(i.0, s.base.input))
                || !output.is_some_and(|o| close(o.0, s.base.output))
        })
        .map(|(n, s)| (*n, s.base.input, s.base.output))
        .collect();
    Merged {
        input,
        output,
        cache_read: vote(&cache_first(&|s| s.base.cache_read)),
        write_5m: vote(&cache_first(&|s| real_write(s.base.write_5m, s.base.input))),
        write_1h: vote(&in_order(&|s| real_write(s.base.write_1h, s.base.input))),
        long,
        fast: said.iter().find_map(|(_, s)| s.fast),
        geo: said.iter().find_map(|(_, s)| s.geo),
        from: said.iter().map(|x| x.0).collect(),
        apart,
    }
}

/// Whole rates from a list's part: what it left out comes from `like` (the
/// price it replaces), else cached input at the input price and a 1 h write at
/// Anthropic's 2× where writes cost extra at all.
fn rates_of(p: &Part, like: Option<&Rates>) -> Rates {
    let write_5m = real_write(p.write_5m, p.input).or(like.map(|l| l.cache_write_5m));
    let w5 = write_5m.unwrap_or(p.input);
    let extra = if w5 > p.input { p.input * 2.0 } else { w5 };
    Rates {
        input: p.input,
        output: p.output,
        cache_read: p
            .cache_read
            .or(like.map(|l| l.cache_read))
            .unwrap_or(p.input),
        cache_write_5m: w5,
        cache_write_1h: real_write(p.write_1h, p.input)
            .or(like.map(|l| l.cache_write_1h))
            .unwrap_or(extra),
    }
}

/// A change that didn't apply, and why.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub model: String,
    pub field: String,
    pub have: f64,
    pub says: f64,
    pub why: String,
    /// when it was first held
    pub since: u64,
}

impl Held {
    fn to_json(&self) -> Value {
        json!({"model": self.model, "field": self.field, "have": self.have, "says": self.says,
               "why": self.why, "since": self.since})
    }
    fn from_json(v: &Value) -> Option<Held> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Some(Held {
            model: s("model")?,
            field: s("field")?,
            have: v.get("have")?.as_f64()?,
            says: v.get("says")?.as_f64()?,
            why: s("why")?,
            since: v.get("since").and_then(Value::as_u64).unwrap_or(0),
        })
    }
}

/// One field: what it should be now, and what was held instead. `trusted`: this
/// list's word is enough (OpenRouter on its own route, you accepting).
fn weigh(
    field: &str,
    have: f64,
    said: Option<Vote>,
    trusted: bool,
    force: bool,
    held: &mut Vec<(String, f64, f64, String)>,
) -> f64 {
    let Some((says, votes)) = said else {
        return have;
    };
    if close(have, says) || force {
        return if force { says } else { have };
    }
    let jump = if have > 0.0 {
        (says - have) / have
    } else {
        f64::INFINITY
    };
    let why = if jump.abs() > MAX_JUMP {
        if jump.is_finite() {
            format!("a jump of {:+.0}%", jump * 100.0)
        } else {
            "a jump from nothing".to_string()
        }
    } else if votes < 2 && !trusted {
        "only one list says so".to_string()
    } else {
        return says;
    };
    held.push((field.to_string(), have, says, why));
    have
}

/// A model's price after what the lists say, and what was held. `force` takes
/// everything they say (you accepted it).
fn decide(
    cur: &Price,
    m: &Merged,
    trusted: bool,
    force: bool,
) -> (Price, Vec<(String, f64, f64, String)>) {
    let mut held = Vec::new();
    let mut next = *cur;
    let (t, f) = (trusted, force);
    next.base.input = weigh("input", cur.base.input, m.input, t, f, &mut held);
    next.base.output = weigh("output", cur.base.output, m.output, t, f, &mut held);
    next.base.cache_read = weigh(
        "cache read",
        cur.base.cache_read,
        m.cache_read,
        t,
        f,
        &mut held,
    );
    next.base.cache_write_5m = weigh(
        "cache write 5m",
        cur.base.cache_write_5m,
        m.write_5m,
        t,
        f,
        &mut held,
    );
    next.base.cache_write_1h = weigh(
        "cache write 1h",
        cur.base.cache_write_1h,
        m.write_1h,
        t,
        f,
        &mut held,
    );
    // no write premium, none said: writes follow the input price
    if m.write_5m.is_none() && close(cur.base.cache_write_5m, cur.base.input) {
        next.base.cache_write_5m = next.base.input;
    }
    if m.write_1h.is_none() && close(cur.base.cache_write_1h, cur.base.input) {
        next.base.cache_write_1h = next.base.input;
    }
    if let Some((above, part, votes)) = &m.long {
        let same = cur.long.is_some_and(|(a, r)| {
            a == *above && close(r.input, part.input) && close(r.output, part.output)
        });
        if !same {
            let like = cur.long.map(|x| x.1);
            let was = like.map_or(0.0, |r| r.input);
            let says = weigh(
                "long-context input",
                was,
                Some((part.input, *votes)),
                t,
                f,
                &mut held,
            );
            // a tier nothing priced before is not a jump, just new: two lists will do
            let new_ok = like.is_none() && (*votes >= 2 || t || f);
            if new_ok {
                held.retain(|h| h.0 != "long-context input");
            }
            if new_ok || close(says, part.input) {
                next.long = Some((*above, rates_of(part, like.as_ref())));
            }
        }
    }
    (next, held)
}

fn same_price(a: &Price, b: &Price) -> bool {
    let r = |x: &Rates, y: &Rates| {
        close(x.input, y.input)
            && close(x.output, y.output)
            && close(x.cache_read, y.cache_read)
            && close(x.cache_write_5m, y.cache_write_5m)
            && close(x.cache_write_1h, y.cache_write_1h)
    };
    r(&a.base, &b.base)
        && match (&a.long, &b.long) {
            (Some(x), Some(y)) => x.0 == y.0 && r(&x.1, &y.1),
            (None, None) => true,
            _ => false,
        }
}

// ------------------------------------------------------------------ the log

/// A model's whole price from `from` on.
#[derive(Clone, Debug, PartialEq)]
pub struct Line {
    pub model: String,
    pub from: u64,
    pub price: Price,
    pub src: String,
}

fn rates_json(r: &Rates) -> Value {
    json!([
        r.input,
        r.output,
        r.cache_read,
        r.cache_write_5m,
        r.cache_write_1h
    ])
}

fn rates_from(v: &Value) -> Option<Rates> {
    let a = v.as_array()?;
    let n = |i: usize| a.get(i).and_then(Value::as_f64);
    Some(Rates {
        input: n(0)?,
        output: n(1)?,
        cache_read: n(2)?,
        cache_write_5m: n(3)?,
        cache_write_1h: n(4)?,
    })
}

impl Line {
    fn to_json(&self) -> Value {
        let mut v = json!({"m": self.model, "from": self.from, "p": rates_json(&self.price.base),
                           "src": self.src});
        if let Some((above, r)) = &self.price.long {
            v["long"] = json!([above, rates_json(r)]);
        }
        if let Some(k) = self.price.fast {
            v["fast"] = json!(k);
        }
        if self.price.geo {
            v["geo"] = json!(true);
        }
        v
    }
    fn from_json(v: &Value) -> Option<Line> {
        let long = v.get("long").and_then(|l| {
            let a = l.as_array()?;
            Some((a.first()?.as_u64()?, rates_from(a.get(1)?)?))
        });
        Some(Line {
            model: v.get("m")?.as_str()?.to_string(),
            from: v.get("from")?.as_u64()?,
            price: Price {
                base: rates_from(v.get("p")?)?,
                long,
                fast: v.get("fast").and_then(Value::as_f64),
                geo: v.get("geo").and_then(Value::as_bool).unwrap_or(false),
            },
            src: v
                .get("src")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
    }
}

/// The log, read: each model's lines, oldest first.
#[derive(Debug, Default)]
pub struct Dated {
    by: HashMap<String, Vec<Line>>,
    /// the lists have been read at least once (not just the built-in lines)
    pub synced: bool,
}

impl Dated {
    pub fn parse(text: &str) -> Dated {
        let mut d = Dated::default();
        for l in text.lines() {
            // a torn or hand-broken line is skipped, never fatal
            let Some(line) = serde_json::from_str(l)
                .ok()
                .and_then(|v| Line::from_json(&v))
            else {
                continue;
            };
            d.synced |= line.src != BUILT_IN;
            d.by.entry(line.model.clone()).or_default().push(line);
        }
        for v in d.by.values_mut() {
            v.sort_by_key(|l| l.from);
        }
        d
    }
    pub fn of(&self, id: &str) -> Option<&[Line]> {
        self.by.get(id).map(Vec::as_slice)
    }
    pub fn latest(&self, id: &str) -> Option<&Line> {
        self.by.get(id).and_then(|v| v.last())
    }
    pub fn models(&self) -> impl Iterator<Item = &String> {
        self.by.keys()
    }
    /// A fingerprint of what moved a price at or before `last`: a month whose
    /// records end earlier can't be touched by anything later.
    pub fn stamp_until(&self, last: u64) -> String {
        let (mut n, mut sum) = (0u64, 0u64);
        for l in self.by.values().flatten() {
            if l.from > 0 && l.from <= last {
                n += 1;
                sum = sum.wrapping_add(l.from);
            }
        }
        format!("{n}:{sum}")
    }
}

fn dir() -> PathBuf {
    crate::usage::store::dir()
}

fn log_file() -> PathBuf {
    dir().join("prices.jsonl")
}

fn state_file() -> PathBuf {
    dir().join("prices-state.json")
}

fn list_file(name: &str) -> PathBuf {
    dir().join(format!("prices-list-{name}.json"))
}

/// The log, kept in memory until the file changes.
pub fn load() -> Arc<Dated> {
    type Kept = (std::time::SystemTime, u64, Arc<Dated>);
    static KEPT: Mutex<Option<Kept>> = Mutex::new(None);
    if cfg!(test) {
        return Arc::default();
    }
    let Ok(meta) = std::fs::metadata(log_file()) else {
        return Arc::default();
    };
    let Ok(mt) = meta.modified() else {
        return Arc::default();
    };
    let mut k = KEPT.lock().unwrap();
    if let Some((t, n, d)) = k.as_ref() {
        if *t == mt && *n == meta.len() {
            return d.clone();
        }
    }
    let d = Arc::new(Dated::parse(
        &std::fs::read_to_string(log_file()).unwrap_or_default(),
    ));
    *k = Some((mt, meta.len(), d.clone()));
    d
}

fn append(lines: &[Line]) -> Result<(), String> {
    use std::io::Write;
    if lines.is_empty() {
        return Ok(());
    }
    let mut text = String::new();
    for l in lines {
        text.push_str(&l.to_json().to_string());
        text.push('\n');
    }
    let _ = std::fs::create_dir_all(dir());
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_file())
        .map_err(|e| e.to_string())?;
    f.write_all(text.as_bytes()).map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ one round

fn parts_json(p: &Part) -> Value {
    json!([p.input, p.output, p.cache_read, p.write_5m, p.write_1h])
}

fn parts_from(v: &Value) -> Option<Part> {
    let a = v.as_array()?;
    let opt = |i: usize| a.get(i).and_then(Value::as_f64);
    Some(Part {
        input: opt(0)?,
        output: opt(1)?,
        cache_read: opt(2),
        write_5m: opt(3),
        write_1h: opt(4),
    })
}

fn seen_json(s: &Seen) -> Value {
    json!([
        s.id,
        parts_json(&s.base),
        s.long.map(|(a, p)| json!([a, parts_json(&p)])),
        s.fast,
        s.geo
    ])
}

fn seen_from(v: &Value) -> Option<Seen> {
    let a = v.as_array()?;
    Some(Seen {
        id: a.first()?.as_str()?.to_string(),
        base: parts_from(a.get(1)?)?,
        long: a.get(2).and_then(|l| {
            let x = l.as_array()?;
            Some((x.first()?.as_u64()?, parts_from(x.get(1)?)?))
        }),
        fast: a.get(3).and_then(Value::as_f64),
        geo: a.get(4).and_then(Value::as_bool),
    })
}

fn read_list(name: &str) -> Vec<Seen> {
    std::fs::read_to_string(list_file(name))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| {
            v.as_array()
                .map(|a| a.iter().filter_map(seen_from).collect())
        })
        .unwrap_or_default()
}

/// One list per name, in [`LISTS`]' order.
pub type Lists = Vec<(&'static str, Vec<Seen>)>;

pub fn read_lists() -> Lists {
    LISTS.iter().map(|(n, _)| (*n, read_list(n))).collect()
}

/// The lists that disagree with the others about a model.
pub fn apart(lists: &Lists, id: &str) -> Vec<(&'static str, f64, f64)> {
    let said: Vec<(&'static str, &Seen)> = lists
        .iter()
        .filter_map(|(n, l)| l.iter().find(|s| s.id == id).map(|s| (*n, s)))
        .collect();
    merge(&said).apart
}

/// How many models have a change held (read at most every half minute: Usage
/// draws this every frame).
pub fn held_models() -> usize {
    static KEPT: Mutex<Option<(std::time::Instant, usize)>> = Mutex::new(None);
    if cfg!(test) {
        return 0;
    }
    let mut k = KEPT.lock().unwrap();
    if let Some((t, n)) = *k {
        if t.elapsed() < Duration::from_secs(30) {
            return n;
        }
    }
    let mut models: Vec<String> = State::load().held.into_iter().map(|h| h.model).collect();
    models.sort_unstable();
    models.dedup();
    *k = Some((std::time::Instant::now(), models.len()));
    models.len()
}

/// The lines and the holds one round of lists gives (pure).
pub fn plan(now: u64, dated: &Dated, lists: &Lists) -> (Vec<Line>, Vec<Held>) {
    let mut lines = Vec::new();
    let mut held = Vec::new();
    // today's built-in table: every model's first line, once
    let mut latest: HashMap<String, Price> = HashMap::new();
    for (id, price) in prices::builtin_table() {
        match dated.latest(id) {
            Some(l) => {
                latest.insert(id.to_string(), l.price);
            }
            None => {
                latest.insert(id.to_string(), *price);
                lines.push(Line {
                    model: id.to_string(),
                    from: 0,
                    price: *price,
                    src: BUILT_IN.to_string(),
                });
            }
        }
    }
    let mut ids: BTreeMap<String, Vec<(&'static str, &Seen)>> = BTreeMap::new();
    for (name, list) in lists {
        for s in list {
            ids.entry(s.id.clone()).or_default().push((*name, s));
        }
    }
    for (id, said) in &ids {
        let owner_said = said.iter().any(|(n, _)| *n != OPENROUTER);
        let cur = latest
            .get(id)
            .copied()
            .or_else(|| dated.latest(id).map(|l| l.price));
        let m = merge(said);
        match cur {
            // a model nothing priced: from the start, so its earlier requests get
            // an estimate too. OpenRouter's word alone goes under its own name.
            None if owner_said => {
                if let Some(price) = first_price(&m) {
                    lines.push(Line {
                        model: id.clone(),
                        from: 0,
                        price,
                        src: m.from.join("+"),
                    });
                }
            }
            None => {}
            Some(cur) => {
                let (next, h) = decide(&cur, &m, false, false);
                push_held(&mut held, id, h, now);
                if !same_price(&cur, &next) {
                    lines.push(Line {
                        model: id.clone(),
                        from: now,
                        price: next,
                        src: m.from.join("+"),
                    });
                }
            }
        }
    }
    // OpenRouter on its own route: its word is the price, short of a wild jump
    for (name, list) in lists {
        if *name != OPENROUTER {
            continue;
        }
        for s in list {
            let key = format!("or:{}", s.id);
            let m = merge(&[(OPENROUTER, s)]);
            match dated.latest(&key) {
                None => {
                    if let Some(price) = first_price(&m) {
                        lines.push(Line {
                            model: key,
                            from: 0,
                            price,
                            src: OPENROUTER.to_string(),
                        });
                    }
                }
                Some(l) => {
                    let (next, h) = decide(&l.price, &m, true, false);
                    push_held(&mut held, &key, h, now);
                    if !same_price(&l.price, &next) {
                        lines.push(Line {
                            model: key,
                            from: now,
                            price: next,
                            src: OPENROUTER.to_string(),
                        });
                    }
                }
            }
        }
    }
    (lines, held)
}

fn push_held(held: &mut Vec<Held>, model: &str, h: Vec<(String, f64, f64, String)>, now: u64) {
    for (field, have, says, why) in h {
        held.push(Held {
            model: model.to_string(),
            field,
            have,
            says,
            why,
            since: now,
        });
    }
}

fn first_price(m: &Merged) -> Option<Price> {
    let part = Part {
        input: m.input?.0,
        output: m.output?.0,
        cache_read: m.cache_read.map(|x| x.0),
        write_5m: m.write_5m.map(|x| x.0),
        write_1h: m.write_1h.map(|x| x.0),
    };
    let base = rates_of(&part, None);
    Some(Price {
        base,
        long: m
            .long
            .as_ref()
            .map(|(above, p, _)| (*above, rates_of(p, None))),
        fast: m.fast,
        geo: m.geo.unwrap_or(false),
    })
}

/// How the lists stand: when each was last read, what's held.
#[derive(Clone, Debug, Default)]
pub struct State {
    /// when a round last ran
    pub at: u64,
    /// per list: when it was last read, its ETag, the models in it, what went wrong
    pub lists: BTreeMap<String, ListState>,
    pub held: Vec<Held>,
}

#[derive(Clone, Debug, Default)]
pub struct ListState {
    pub read_at: u64,
    pub etag: String,
    pub models: u64,
    pub error: String,
}

impl State {
    pub fn load() -> State {
        let Some(v) = std::fs::read_to_string(state_file())
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return State::default();
        };
        let mut st = State {
            at: v.get("at").and_then(Value::as_u64).unwrap_or(0),
            ..State::default()
        };
        for (k, x) in v
            .get("lists")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            let s = |f: &str| x.get(f).and_then(Value::as_str).unwrap_or("").to_string();
            let n = |f: &str| x.get(f).and_then(Value::as_u64).unwrap_or(0);
            st.lists.insert(
                k.clone(),
                ListState {
                    read_at: n("read_at"),
                    etag: s("etag"),
                    models: n("models"),
                    error: s("error"),
                },
            );
        }
        st.held = v
            .get("held")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Held::from_json).collect())
            .unwrap_or_default();
        st
    }
    fn to_json(&self) -> Value {
        let lists: serde_json::Map<String, Value> = self
            .lists
            .iter()
            .map(|(k, l)| {
                (
                    k.clone(),
                    json!({"read_at": l.read_at, "etag": l.etag, "models": l.models, "error": l.error}),
                )
            })
            .collect();
        json!({"at": self.at, "lists": lists,
               "held": self.held.iter().map(Held::to_json).collect::<Vec<_>>()})
    }
    fn save(&self) {
        let _ = std::fs::create_dir_all(dir());
        let _ = crate::config::write_atomic(&state_file(), self.to_json().to_string().as_bytes());
    }
    /// Held changes that haven't gone away keep the day they were first held.
    fn set_held(&mut self, mut new: Vec<Held>) {
        for h in &mut new {
            if let Some(was) = self
                .held
                .iter()
                .find(|o| o.model == h.model && o.field == h.field && close(o.says, h.says))
            {
                h.since = was.since;
            }
        }
        self.held = new;
    }
}

/// One list's answer: `None` when it hasn't changed since `etag`.
fn fetch(url: &str, etag: &str) -> Result<Option<(Value, String)>, String> {
    if let Some(e) = crate::showcase::refused() {
        return Err(e);
    }
    if cfg!(test) {
        return Err("no network in unit tests".into());
    }
    let mut cfg = crate::openrouter::curl_config(url, None).ok_or("a bad address")?;
    if !etag.is_empty() && !etag.contains(['\n', '\r']) {
        let quoted = etag.replace('\\', "\\\\").replace('"', "\\\"");
        cfg.push_str(&format!("header = \"If-None-Match: {quoted}\"\n"));
    }
    let _ = std::fs::create_dir_all(dir());
    let hdr = dir().join(format!(".prices-headers-{}", std::process::id()));
    let o = crate::proc::run(
        Command::new("curl").args([
            "-sS",
            "-m",
            &T_FETCH.to_string(),
            "--compressed",
            "-H",
            "Accept: application/json",
            "-D",
            &hdr.to_string_lossy(),
            "-w",
            "\\n%{http_code}",
            "-K",
            "-",
        ]),
        Duration::from_secs(T_FETCH + 5),
        Some(cfg.as_bytes()),
    )
    .map_err(|e| format!("curl: {e}"))?;
    let headers = std::fs::read_to_string(&hdr).unwrap_or_default();
    let _ = std::fs::remove_file(&hdr);
    if o.timed_out {
        return Err("no answer in time".into());
    }
    let text = o.text();
    let (body, code) = crate::openrouter::split_status(&text);
    match code {
        304 => Ok(None),
        200 => {
            let v = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
            Ok(Some((v, etag_of(&headers))))
        }
        0 => Err(format!("can't reach it: {}", o.reason())),
        c => Err(format!("answered {c}")),
    }
}

/// The ETag of the last response in curl's `-D` file.
fn etag_of(headers: &str) -> String {
    headers
        .lines()
        .rev()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("etag")
                .then(|| v.trim().to_string())
        })
        .unwrap_or_default()
}

/// What a round did.
#[derive(Clone, Debug, Default)]
pub struct Round {
    /// price changes applied, and models new to the log
    pub changed: Vec<Line>,
    pub added: usize,
    pub held: Vec<Held>,
    pub errors: Vec<(String, String)>,
}

/// Read the lists and write what they say. Off the UI thread: this waits on
/// the network. Sends no login to anyone: all three lists are public.
pub fn sync_now() -> Round {
    let mut st = State::load();
    let now = crate::cache::now();
    let mut round = Round::default();
    for (name, url) in LISTS {
        let ls = st.lists.entry(name.to_string()).or_default();
        match fetch(url, &ls.etag) {
            Ok(None) => {
                ls.read_at = now;
                ls.error.clear();
            }
            Ok(Some((v, etag))) => {
                let list = parse_list(name, &v);
                if list.len() < MIN_MODELS {
                    ls.error = format!("listed only {} models", list.len());
                } else {
                    let text = Value::Array(list.iter().map(seen_json).collect()).to_string();
                    let _ = std::fs::create_dir_all(dir());
                    match crate::config::write_atomic(&list_file(name), text.as_bytes()) {
                        Ok(()) => {
                            ls.read_at = now;
                            ls.etag = etag;
                            ls.models = list.len() as u64;
                            ls.error.clear();
                        }
                        Err(e) => ls.error = e.to_string(),
                    }
                }
            }
            Err(e) => ls.error = e,
        }
        if !ls.error.is_empty() {
            round.errors.push((name.to_string(), ls.error.clone()));
        }
    }
    let dated = Dated::parse(&std::fs::read_to_string(log_file()).unwrap_or_default());
    let (lines, held) = plan(now, &dated, &read_lists());
    if let Err(e) = append(&lines) {
        round.errors.push(("log".to_string(), e));
    }
    st.set_held(held);
    st.at = now;
    st.save();
    round.added = lines.iter().filter(|l| l.from == 0).count();
    round.changed = lines.into_iter().filter(|l| l.from > 0).collect();
    round.held = st.held.clone();
    round
}

/// Apply what's held for `model` from now on. The lists' word, your decision.
pub fn accept(model: &str) -> Result<Line, String> {
    let id = normalize(model);
    let mut st = State::load();
    let key = if st.held.iter().any(|h| h.model == id) {
        id.clone()
    } else {
        format!("or:{id}")
    };
    if !st.held.iter().any(|h| h.model == key) {
        return Err(format!("nothing is held for {id}"));
    }
    let lists = read_lists();
    let bare = key.strip_prefix("or:");
    let said: Vec<(&'static str, &Seen)> = lists
        .iter()
        .filter(|(n, _)| bare.is_none() || *n == OPENROUTER)
        .filter_map(|(n, l)| l.iter().find(|s| s.id == id).map(|s| (*n, s)))
        .collect();
    if said.is_empty() {
        return Err(format!("no list says anything of {id} any more"));
    }
    let dated = Dated::parse(&std::fs::read_to_string(log_file()).unwrap_or_default());
    let cur = dated
        .latest(&key)
        .map(|l| l.price)
        .or_else(|| {
            prices::builtin_table()
                .iter()
                .find(|x| x.0 == id)
                .map(|x| x.1)
        })
        .ok_or_else(|| format!("no price for {id} yet"))?;
    let (next, _) = decide(&cur, &merge(&said), true, true);
    let line = Line {
        model: key.clone(),
        from: crate::cache::now(),
        price: next,
        src: YOU.to_string(),
    };
    append(std::slice::from_ref(&line))?;
    st.held.retain(|h| h.model != key);
    st.save();
    Ok(line)
}

/// The hub's side: a round every hour, off its thread.
#[derive(Default)]
pub struct Sync {
    busy: Arc<AtomicBool>,
    checked: u64,
}

impl Sync {
    pub fn tick(&mut self, cfg: &crate::config::Config) {
        if cfg!(test) || crate::showcase::demo() {
            return;
        }
        let now = crate::cache::now();
        if now < self.checked + 60 {
            return;
        }
        self.checked = now;
        if !crate::usage::enabled(cfg) || !cfg.flag("analytics.prices_sync") {
            return;
        }
        if now < State::load().at + EVERY || self.busy.swap(true, Ordering::SeqCst) {
            return;
        }
        let busy = self.busy.clone();
        let _ = std::thread::Builder::new()
            .name("prices".into())
            .spawn(move || {
                let r = sync_now();
                for (name, e) in &r.errors {
                    crate::log::warn("usage", &format!("prices: {name}: {e}"));
                }
                busy.store(false, Ordering::SeqCst);
            });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: &str, input: f64, output: f64) -> Seen {
        Seen {
            id: id.to_string(),
            base: Part {
                input,
                output,
                ..Part::default()
            },
            ..Seen::default()
        }
    }

    fn lists(l: &[f64; 2], m: &[f64; 2], o: &[f64; 2], id: &str) -> Lists {
        vec![
            (LITELLM, vec![seen(id, l[0], l[1])]),
            (MODELS_DEV, vec![seen(id, m[0], m[1])]),
            (OPENROUTER, vec![seen(id, o[0], o[1])]),
        ]
    }

    /// A log as it stands after these lines.
    fn log_of(lines: &[Line]) -> Dated {
        let text: String = lines.iter().map(|l| format!("{}\n", l.to_json())).collect();
        Dated::parse(&text)
    }

    #[test]
    fn litellm_owners_tiers_and_extras() {
        let v = json!({
            "sample_spec": {"input_cost_per_token": 1},
            "claude-haiku-5-5": {
                "litellm_provider": "anthropic", "mode": "chat",
                "input_cost_per_token": 1e-7, "output_cost_per_token": 5e-7,
                "cache_read_input_token_cost": 1e-8,
                "cache_creation_input_token_cost": 1.25e-7,
                "cache_creation_input_token_cost_above_1hr": 2e-7,
                "input_cost_per_token_above_100k_tokens": 5e-7,
                "output_cost_per_token_above_100k_tokens": 2.5e-6,
                "cache_read_input_token_cost_above_100k_tokens": 5e-8,
                "provider_specific_entry": {"us": 1.1, "fast": 2}
            },
            "bedrock/anthropic.claude-haiku-5-5": {
                "litellm_provider": "bedrock", "mode": "chat",
                "input_cost_per_token": 9e-6, "output_cost_per_token": 9e-6
            },
            "gpt-5-image": {"litellm_provider": "openai", "mode": "image_generation",
                "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6},
            "dashscope/deepseek-v4-pro": {"litellm_provider": "dashscope", "mode": "chat",
                "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6},
            "gpt-4o": {"litellm_provider": "openai", "mode": "chat",
                "input_cost_per_token": 2.5e-6, "output_cost_per_token": 1e-5},
            "gpt-4o-2024-05-13": {"litellm_provider": "openai", "mode": "chat",
                "input_cost_per_token": 5e-6, "output_cost_per_token": 1.5e-5},
            "gemini/gemini-2.5-pro": {"litellm_provider": "gemini", "mode": "chat",
                "input_cost_per_token": 1.25e-6, "output_cost_per_token": 1e-5}
        });
        let l = parse_litellm(&v);
        let ids: Vec<&str> = l.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["claude-haiku-5-5", "gemini-2.5-pro", "gpt-4o"]);
        let h = &l[0];
        assert_eq!(h.base.input, 0.1);
        assert_eq!(h.base.write_1h, Some(0.2));
        let (above, tier) = h.long.unwrap();
        assert_eq!(
            (above, tier.input, tier.cache_read),
            (100_000, 0.5, Some(0.05))
        );
        assert_eq!((h.fast, h.geo), (Some(2.0), Some(true)));
        // the undated entry, not a dearer dated copy
        assert_eq!(l[2].base.input, 2.5);
    }

    #[test]
    fn models_dev_owners_and_tiers() {
        let v = json!({
            "google": {"models": {
                "gemini-2.5-pro": {"cost": {"input": 1.25, "output": 10, "cache_read": 0.125,
                    "tiers": [{"input": 2.5, "output": 15, "cache_read": 0.25,
                               "tier": {"type": "context", "size": 200000}}]}},
                "deep-research-max": {"cost": {"input": 1, "output": 1}}
            }},
            "fireworks": {"models": {"gemini-2.5-pro": {"cost": {"input": 9, "output": 9}}}},
            "anthropic": {"models": {
                "claude-opus-5-5": {"cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}},
                "claude-free": {"cost": {"input": 0, "output": 0}}
            }}
        });
        let l = parse_models_dev(&v);
        let ids: Vec<&str> = l.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["claude-opus-5-5", "gemini-2.5-pro"]);
        assert_eq!(l[0].base.write_5m, Some(5.0));
        assert_eq!(l[1].long.unwrap().0, 200_000);
    }

    #[test]
    fn openrouter_paid_models_only() {
        let v = json!({"data": [
            {"id": "anthropic/claude-sonnet-4.5", "pricing": {"prompt": "0.000003", "completion": "0.000015",
                "input_cache_read": "0.0000003", "input_cache_write": "0.00000375", "input_cache_write_1h": "0.000006",
                "overrides": [{"min_prompt_tokens": 200000, "prompt": "0.000006", "completion": "0.0000225"}]}},
            {"id": "qwen/qwen3-coder:free", "pricing": {"prompt": "0", "completion": "0"}},
            {"id": "mistralai/mistral-medium-3-5:batch", "pricing": {"prompt": "0.000001", "completion": "0.000001"}},
            {"id": "openrouter/auto", "pricing": {"prompt": "-1", "completion": "-1"}},
            {"id": "z-ai/glm-4.6", "pricing": {"prompt": "0.0000006", "completion": "0.0000022"}}
        ]});
        let l = parse_openrouter(&v);
        let ids: Vec<&str> = l.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["claude-sonnet-4-5", "glm-4.6"]);
        assert_eq!(l[0].base.write_1h, Some(6.0));
        assert_eq!(l[0].long.unwrap().1.output, 22.5);
    }

    #[test]
    fn the_first_round_writes_the_table_down() {
        let (lines, held) = plan(1000, &Dated::default(), &Vec::new());
        assert!(held.is_empty());
        assert_eq!(lines.len(), prices::builtin_table().len());
        assert!(lines.iter().all(|l| l.from == 0 && l.src == BUILT_IN));
        // once
        let (again, _) = plan(2000, &log_of(&lines), &Vec::new());
        assert!(again.is_empty());
    }

    #[test]
    fn two_lists_agreeing_apply_from_now() {
        let id = "claude-opus-5-5";
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let (lines, held) = plan(
            2000,
            &d,
            &lists(&[5.0, 25.0], &[5.0, 25.0], &[4.0, 20.0], id),
        );
        assert!(held.is_empty(), "{held:?}");
        let moved: Vec<&Line> = lines.iter().filter(|l| l.model == id).collect();
        assert_eq!(moved.len(), 1);
        assert_eq!((moved[0].from, moved[0].price.base.input), (2000, 5.0));
        // what wasn't said stays what it was
        assert_eq!(moved[0].price.base.cache_write_1h, 8.0);
        assert_eq!(moved[0].price.fast, Some(2.0));
    }

    #[test]
    fn one_list_alone_is_held() {
        let id = "claude-opus-5-5";
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let (lines, held) = plan(
            2000,
            &d,
            &lists(&[5.0, 20.0], &[4.0, 20.0], &[4.0, 20.0], id),
        );
        // two lists say it hasn't moved: nothing at all
        assert!(lines.iter().all(|l| l.model != id));
        assert!(held.iter().all(|h| h.model != id), "{held:?}");
        let l = vec![(LITELLM, vec![seen(id, 5.0, 20.0)])];
        let (lines, held) = plan(2000, &d, &l);
        assert!(lines.iter().all(|l| l.model != id), "not applied");
        let h = held.iter().find(|h| h.model == id).unwrap();
        assert_eq!((h.field.as_str(), h.have, h.says), ("input", 4.0, 5.0));
        assert_eq!(h.why, "only one list says so");
    }

    #[test]
    fn a_wild_jump_is_held_however_many_agree() {
        let id = "claude-opus-5-5";
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let (lines, held) = plan(
            2000,
            &d,
            &lists(&[40.0, 20.0], &[40.0, 20.0], &[40.0, 20.0], id),
        );
        assert!(lines.iter().all(|l| l.model != id), "{lines:?}");
        let h = held.iter().find(|h| h.model == id).unwrap();
        assert_eq!(h.why, "a jump of +900%");
    }

    #[test]
    fn a_model_nothing_priced_counts_from_the_start() {
        let l: Lists = vec![(LITELLM, vec![seen("gpt-9", 2.0, 8.0)])];
        let (lines, held) = plan(2000, &Dated::default(), &l);
        assert!(held.is_empty());
        let n = lines.iter().find(|l| l.model == "gpt-9").unwrap();
        assert_eq!(n.from, 0);
        assert_eq!(
            n.price.base.cache_read, 2.0,
            "no cached price said: the input price"
        );
    }

    #[test]
    fn openrouter_keeps_its_own_price_and_gives_no_plain_one() {
        let l: Lists = vec![(OPENROUTER, vec![seen("glm-4.6", 0.6, 2.2)])];
        let (lines, _) = plan(2000, &Dated::default(), &l);
        assert!(lines.iter().all(|x| x.model != "glm-4.6"));
        let own = lines.iter().find(|x| x.model == "or:glm-4.6").unwrap();
        assert_eq!(own.from, 0);
        // on its own route its word is enough for a small move, not a wild one
        let d = log_of(&lines);
        let l: Lists = vec![(OPENROUTER, vec![seen("glm-4.6", 0.7, 2.2)])];
        let (lines, held) = plan(3000, &d, &l);
        assert!(held.is_empty());
        let n = lines.iter().find(|x| x.model == "or:glm-4.6").unwrap();
        assert_eq!((n.from, n.price.base.input), (3000, 0.7));
        let l: Lists = vec![(OPENROUTER, vec![seen("glm-4.6", 6.0, 2.2)])];
        let (_, held) = plan(3000, &d, &l);
        assert!(held.iter().any(|h| h.model == "or:glm-4.6"));
    }

    #[test]
    fn a_new_long_tier_needs_two_lists() {
        let id = "claude-opus-5-5";
        let tier = Part {
            input: 8.0,
            output: 30.0,
            ..Part::default()
        };
        let with_tier = |s: Seen| Seen {
            long: Some((200_000, tier)),
            ..s
        };
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let one: Lists = vec![(LITELLM, vec![with_tier(seen(id, 4.0, 20.0))])];
        let (lines, held) = plan(2000, &d, &one);
        assert!(lines.iter().all(|l| l.model != id));
        assert!(held.iter().any(|h| h.model == id));
        let two: Lists = vec![
            (LITELLM, vec![with_tier(seen(id, 4.0, 20.0))]),
            (MODELS_DEV, vec![with_tier(seen(id, 4.0, 20.0))]),
        ];
        let (lines, held) = plan(2000, &d, &two);
        assert!(held.iter().all(|h| h.model != id), "{held:?}");
        let n = lines.iter().find(|l| l.model == id).unwrap();
        assert_eq!(n.price.long.unwrap().1.input, 8.0);
    }

    #[test]
    fn a_write_rate_below_the_input_is_not_a_write_rate() {
        let id = "gemini-2.5-pro";
        let storage = |s: Seen| Seen {
            base: Part {
                write_5m: Some(0.375),
                ..s.base
            },
            ..s
        };
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let l: Lists = vec![
            (LITELLM, vec![storage(seen(id, 1.25, 10.0))]),
            (MODELS_DEV, vec![storage(seen(id, 1.25, 10.0))]),
        ];
        let (lines, held) = plan(2000, &d, &l);
        assert!(held.is_empty(), "{held:?}");
        assert!(lines.iter().all(|x| x.model != id));
        // nor on a model new to the log
        let l: Lists = vec![(
            LITELLM,
            vec![Seen {
                base: Part {
                    write_5m: Some(0.0),
                    ..seen("deepseek-x", 0.3, 0.4).base
                },
                ..seen("deepseek-x", 0.3, 0.4)
            }],
        )];
        let (lines, _) = plan(2000, &Dated::default(), &l);
        let n = lines.iter().find(|x| x.model == "deepseek-x").unwrap();
        assert_eq!(n.price.base.cache_write_5m, 0.3);
    }

    #[test]
    fn writes_without_a_premium_follow_the_input() {
        let id = "gpt-5.1";
        let (base, _) = plan(1000, &Dated::default(), &Vec::new());
        let d = log_of(&base);
        let (lines, _) = plan(
            2000,
            &d,
            &lists(&[1.5, 10.0], &[1.5, 10.0], &[1.5, 10.0], id),
        );
        let n = lines.iter().find(|x| x.model == id).unwrap();
        assert_eq!(n.price.base.cache_write_5m, 1.5);
        assert_eq!(n.price.base.cache_write_1h, 1.5);
    }

    #[test]
    fn a_torn_line_is_skipped_and_order_doesnt_matter() {
        let p = prices::builtin_table()[0].1;
        let a = Line {
            model: "m".into(),
            from: 500,
            price: p,
            src: "litellm".into(),
        };
        let b = Line {
            model: "m".into(),
            from: 0,
            price: p,
            src: BUILT_IN.into(),
        };
        let text = format!("{}\n{{\"m\":\"x\n{}\n", a.to_json(), b.to_json());
        let d = Dated::parse(&text);
        assert_eq!(d.of("m").unwrap().len(), 2);
        assert_eq!(d.of("m").unwrap()[0].from, 0);
        assert_eq!(d.latest("m").unwrap().from, 500);
        assert!(d.synced);
        assert_eq!(d.stamp_until(400), "0:0", "a later change can't touch this");
        assert_eq!(d.stamp_until(600), "1:500");
    }

    #[test]
    fn a_line_comes_back_as_it_went_in() {
        for (id, price) in prices::builtin_table() {
            let l = Line {
                model: id.to_string(),
                from: 7,
                price: *price,
                src: "x".into(),
            };
            let v: Value = serde_json::from_str(&l.to_json().to_string()).unwrap();
            assert_eq!(Line::from_json(&v).unwrap(), l, "{id}");
        }
    }

    #[test]
    fn a_seen_comes_back_as_it_went_in() {
        let s = Seen {
            id: "m".into(),
            base: Part {
                input: 1.0,
                output: 2.0,
                cache_read: Some(0.1),
                write_5m: None,
                write_1h: Some(2.0),
            },
            long: Some((
                100_000,
                Part {
                    input: 3.0,
                    output: 4.0,
                    ..Part::default()
                },
            )),
            fast: Some(2.0),
            geo: Some(true),
        };
        assert_eq!(seen_from(&seen_json(&s)).unwrap(), s);
    }

    #[test]
    fn etags_come_from_the_last_response() {
        let h =
            "HTTP/2 301\r\netag: \"old\"\r\n\r\nHTTP/2 200\r\nETag: W/\"new\"\r\nfoo: bar\r\n\r\n";
        // the last one wins, whatever its case
        assert_eq!(etag_of(h), "W/\"new\"");
        assert_eq!(etag_of("HTTP/2 200\r\n\r\n"), "");
    }

    #[test]
    fn held_changes_keep_the_day_they_were_first_held() {
        let was = Held {
            model: "m".into(),
            field: "input".into(),
            have: 1.0,
            says: 2.0,
            why: "x".into(),
            since: 100,
        };
        let mut st = State {
            held: vec![was.clone()],
            ..State::default()
        };
        st.set_held(vec![Held {
            since: 900,
            ..was.clone()
        }]);
        assert_eq!(st.held[0].since, 100);
        // says something else now: a new hold
        st.set_held(vec![Held {
            since: 900,
            says: 3.0,
            ..was
        }]);
        assert_eq!(st.held[0].since, 900);
    }
}
