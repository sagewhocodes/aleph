//! Resource history longer than a view's own: up to `stats_keep_hours` of every
//! machine's graphs, in memory, in rings of a fixed size. Charts read it by the
//! wall clock ([`apply`]): a graph holds still until its next bucket closes, and
//! leaving the view and coming back finds it as it was.
//!
//! The hub's daemon always samples, one quiet stream a machine every 30 s ([`Keeper`];
//! a daemon without the hub lock never gets that far, in `watch.rs`), and appends what
//! it read to `stats/<machine>.jsonl`, which a dashboard reads into its own rings. So the hours before a dashboard opened are there too, and
//! a deploy loses nothing. With no daemon, history starts when the dashboard does.

use crate::config::Config;
use crate::telemetry::{Stats, Telemetry};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// The graphs' timescales: what `+ -` step through (`stats_window`).
pub const SCALES: [(&str, u64); 7] = [
    ("5m", 300),
    ("15m", 900),
    ("30m", 1800),
    ("1h", 3600),
    ("3h", 3 * 3600),
    ("6h", 6 * 3600),
    ("12h", 12 * 3600),
];

/// The detailed ring: 5 s buckets for an hour.
const FINE: u64 = 5;
const FINE_KEEP: usize = 720;
/// The long ring: 30 s buckets for as long as the machine keeps them.
const COARSE: u64 = 30;
/// How many points a graph is drawn from, about.
const POINTS: u64 = 180;
/// What the daemon samples at.
const KEEP_EVERY: u64 = 30;

/// A timescale by name; anything else is the default.
pub fn scale_secs(name: &str) -> u64 {
    SCALES
        .iter()
        .find(|(n, _)| *n == name)
        .map_or(1800, |(_, s)| *s)
}

/// The next timescale up or down, by name.
pub fn scale_step(name: &str, up: bool) -> &'static str {
    let i = SCALES
        .iter()
        .position(|(n, _)| *n == name)
        .unwrap_or(2)
        .min(SCALES.len() - 1);
    let j = if up {
        (i + 1).min(SCALES.len() - 1)
    } else {
        i.saturating_sub(1)
    };
    SCALES[j].0
}

/// How long a bucket of a graph at this timescale is: whole 5-second steps.
pub fn step_of(span: u64) -> u64 {
    (span / POINTS).max(FINE).div_ceil(FINE) * FINE
}

/// One series at one resolution: consecutive buckets, NaN where nothing was read.
#[derive(Clone, Debug)]
struct Ring {
    step: u64,
    cap: usize,
    first: u64,
    vals: VecDeque<f32>,
    /// the bucket still filling
    open: u64,
    sum: f64,
    n: u32,
}

impl Ring {
    fn new(step: u64, cap: usize) -> Ring {
        Ring {
            step,
            cap: cap.max(1),
            first: 0,
            vals: VecDeque::new(),
            open: 0,
            sum: 0.0,
            n: 0,
        }
    }

    fn add(&mut self, ts: u64, v: f64) {
        if !v.is_finite() {
            return;
        }
        let b = ts / self.step;
        if self.n > 0 && b != self.open {
            self.close();
        }
        self.open = b;
        self.sum += v;
        self.n += 1;
    }

    fn close(&mut self) {
        if self.n > 0 {
            self.put((self.sum / f64::from(self.n)) as f32, self.open);
        }
        self.sum = 0.0;
        self.n = 0;
    }

    /// Fill in a bucket. What's already there stays (a read of its own beats a
    /// read from the log); older than the ring reaches, it's dropped.
    fn put(&mut self, v: f32, b: u64) {
        if self.vals.is_empty() {
            self.first = b;
            self.vals.push_back(v);
            return;
        }
        if b < self.first {
            if self.first - b >= self.cap as u64 {
                return;
            }
            while self.first > b + 1 {
                self.vals.push_front(f32::NAN);
                self.first -= 1;
            }
            self.vals.push_front(v);
            self.first = b;
        } else {
            let i = (b - self.first) as usize;
            if i < self.vals.len() {
                if self.vals[i].is_nan() {
                    self.vals[i] = v;
                }
                return;
            }
            // a gap wider than the ring: nothing of the old is left
            if i - self.vals.len() >= self.cap {
                self.vals.clear();
                self.first = b;
            } else {
                while self.first + (self.vals.len() as u64) < b {
                    self.vals.push_back(f32::NAN);
                }
            }
            self.vals.push_back(v);
        }
        while self.vals.len() > self.cap {
            self.vals.pop_front();
            self.first += 1;
        }
    }

    fn get(&self, b: u64) -> Option<f64> {
        if self.n > 0 && self.open == b {
            return Some(self.sum / f64::from(self.n));
        }
        let i = b.checked_sub(self.first)? as usize;
        self.vals
            .get(i)
            .filter(|v| !v.is_nan())
            .map(|v| f64::from(*v))
    }

    /// The average over `[from, to)`, of what it holds.
    fn avg(&self, from: u64, to: u64) -> Option<f64> {
        let (mut sum, mut n) = (0.0, 0u32);
        let mut b = from / self.step;
        while b * self.step < to {
            if let Some(v) = self.get(b) {
                sum += v;
                n += 1;
            }
            b += 1;
        }
        (n > 0).then(|| sum / f64::from(n))
    }
}

#[derive(Clone, Debug)]
struct Series {
    fine: Ring,
    coarse: Ring,
}

impl Series {
    fn new(keep: usize) -> Series {
        Series {
            fine: Ring::new(FINE, FINE_KEEP),
            coarse: Ring::new(COARSE, keep),
        }
    }

    /// One graph's values at a timescale, oldest first, closed buckets only. Its
    /// length is fixed by the timescale, so the x axis stays where it is. NaN
    /// where nothing was read.
    fn window(&self, now: u64, span: u64) -> Vec<f64> {
        let step = step_of(span);
        let end = now / step;
        let n = span / step;
        (end.saturating_sub(n)..end)
            .map(|i| {
                let (a, b) = (i * step, (i + 1) * step);
                let got = if span <= FINE * FINE_KEEP as u64 {
                    self.fine.avg(a, b).or_else(|| self.coarse.avg(a, b))
                } else {
                    self.coarse.avg(a, b)
                };
                got.unwrap_or(f64::NAN)
            })
            .collect()
    }
}

/// What a window of a machine came to, kept until its next bucket closes.
type Built = (u64, u64, HashMap<String, Vec<f64>>);

#[derive(Default)]
struct Machine {
    keep: usize,
    series: HashMap<String, Series>,
    built: Option<Built>,
    /// the log: how far it's been read, and the last bucket this process wrote
    read: u64,
    wrote: u64,
}

impl Machine {
    fn series(&mut self, key: &str) -> &mut Series {
        let keep = self.keep.max(1);
        self.series
            .entry(key.to_string())
            .or_insert_with(|| Series::new(keep))
    }

    fn set_keep(&mut self, buckets: usize) {
        self.keep = buckets;
        for s in self.series.values_mut() {
            s.coarse.cap = buckets.max(1);
            while s.coarse.vals.len() > s.coarse.cap {
                s.coarse.vals.pop_front();
                s.coarse.first += 1;
            }
        }
    }
}

fn store() -> &'static Mutex<HashMap<String, Machine>> {
    static STORE: OnceLock<Mutex<HashMap<String, Machine>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A machine's long ring, in buckets: `host.<a>.stats_keep_hours`, else
/// `stats_keep_hours`.
pub fn keep_buckets(cfg: &Config, host: &str) -> usize {
    let hours = cfg
        .host_get(host, "stats_keep_hours")
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|h| h.is_finite() && *h >= 0.0)
        .unwrap_or_else(|| cfg.num("stats_keep_hours") as f64);
    (hours * 3600.0 / COARSE as f64) as usize
}

/// The machines with a stream kept for their history: none with `stats = off`, and
/// never a paused one (Aleph doesn't talk to those) or one keeping nothing. Off the
/// hub's computer (`hub.machine`), at most this one: nothing reaches this there today,
/// since only the hub lock's holder keeps history, and this keeps it that way.
pub fn wanted(cfg: &Config, hosts: &[String]) -> Vec<String> {
    if cfg.get_or("stats") == "off" {
        return Vec::new();
    }
    let hub = crate::auto::hub_here(cfg);
    hosts
        .iter()
        .filter(|h| hub || crate::sshcfg::is_local(h))
        .filter(|h| keep_buckets(cfg, h) > 0)
        .filter(|h| cfg.host_get(h, "paused").is_none_or(|v| v != "true"))
        .cloned()
        .collect()
}

/// A sample has just finished: put each series' newest value in the rings.
pub fn record(host: &str, ts: u64, st: &mut Stats) {
    let mut g = store().lock().unwrap();
    let m = g.entry(host.to_string()).or_default();
    if m.keep == 0 {
        m.keep = (12 * 3600 / COARSE) as usize;
    }
    for (key, h) in st.hists_mut() {
        if let Some(v) = h.last() {
            let s = m.series(&key);
            s.fine.add(ts, *v);
            s.coarse.add(ts, *v);
        }
    }
}

/// Swap a snapshot's histories for this timescale's. `5m` keeps the live samples
/// once it has them; a machine with nothing kept (tests, the demo) is left alone.
pub fn apply(host: &str, st: &mut Stats, span: u64, now: u64) {
    if span <= 300 && st.cpu_hist.len() >= 60 {
        return;
    }
    let step = step_of(span);
    let bucket = now / step;
    let mut g = store().lock().unwrap();
    let Some(m) = g.get_mut(host) else { return };
    if m.series.is_empty() {
        return;
    }
    let fresh = m
        .built
        .as_ref()
        .is_some_and(|(s, b, _)| *s == span && *b == bucket);
    if !fresh {
        let all: HashMap<String, Vec<f64>> = m
            .series
            .iter()
            .map(|(k, s)| (k.clone(), s.window(now, span)))
            .filter(|(_, v)| v.iter().any(|x| x.is_finite()))
            .collect();
        m.built = Some((span, bucket, all));
    }
    let Some((_, _, built)) = &m.built else {
        return;
    };
    for (key, h) in st.hists_mut() {
        if let Some(v) = built.get(&key) {
            h.clone_from(v);
        }
    }
    st.period = step as f64;
}

// ------------------------------------------------------------------ the log

fn log_dir() -> PathBuf {
    crate::config::world_state().join("stats")
}

fn log_file(host: &str) -> PathBuf {
    let safe: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    log_dir().join(format!("{safe}.jsonl"))
}

/// One closed 30 s bucket of every series (at 3 decimals of a rate or a fraction,
/// which is what a graph can show).
fn bucket_line(m: &Machine, b: u64) -> Option<String> {
    let mut vals = Map::new();
    for (k, s) in &m.series {
        if let Some(v) = s.coarse.get(b) {
            vals.insert(k.clone(), json!((v * 1000.0).round() / 1000.0));
        }
    }
    (!vals.is_empty()).then(|| json!({"b": b, "v": vals}).to_string())
}

/// Read what the log gained into this machine's rings.
fn read_log(host: &str) {
    let path = log_file(host);
    let Ok(mut f) = std::fs::File::open(&path) else {
        return;
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let from = {
        let mut g = store().lock().unwrap();
        let m = g.entry(host.to_string()).or_default();
        // it was rewritten shorter: from the top (what's already here stays)
        if m.read > len {
            m.read = 0;
        }
        m.read
    };
    if from >= len || f.seek(SeekFrom::Start(from)).is_err() {
        return;
    }
    let mut text = String::new();
    if f.read_to_string(&mut text).is_err() {
        return;
    }
    // only whole lines: the rest comes next time
    let upto = text.rfind('\n').map_or(0, |i| i + 1);
    let mut g = store().lock().unwrap();
    let m = g.entry(host.to_string()).or_default();
    for l in text[..upto].lines() {
        let Ok(v) = serde_json::from_str::<Value>(l) else {
            continue;
        };
        let Some(b) = v.get("b").and_then(Value::as_u64) else {
            continue;
        };
        m.wrote = m.wrote.max(b);
        for (k, x) in v.get("v").and_then(Value::as_object).into_iter().flatten() {
            if let Some(x) = x.as_f64() {
                m.series(k).coarse.put(x as f32, b);
            }
        }
    }
    m.read = from + upto as u64;
    m.built = None;
}

/// Read every machine's log, off the UI thread, at most every 20 s.
pub fn read_logs(cfg: &Config, hosts: &[String]) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    static BUSY: AtomicBool = AtomicBool::new(false);
    if cfg!(test) {
        return;
    }
    {
        let mut l = LAST.lock().unwrap();
        if l.is_some_and(|t| t.elapsed().as_secs() < 20) {
            return;
        }
        *l = Some(std::time::Instant::now());
    }
    if BUSY.swap(true, Ordering::SeqCst) {
        return;
    }
    {
        let mut g = store().lock().unwrap();
        for h in hosts {
            // keeping nothing: the detailed hour, and one bucket of the long ring
            let keep = keep_buckets(cfg, h).max(1);
            g.entry(h.clone()).or_default().set_keep(keep);
        }
    }
    let hosts = hosts.to_vec();
    let _ = std::thread::Builder::new()
        .name("stats-log".into())
        .spawn(move || {
            for h in &hosts {
                read_log(h);
            }
            BUSY.store(false, Ordering::SeqCst);
        });
}

/// Append the buckets closed since the last time, and trim a log grown to twice
/// what its machine keeps.
fn write_log(host: &str) {
    let mut text = String::new();
    let (keep, wrote) = {
        let mut g = store().lock().unwrap();
        let Some(m) = g.get_mut(host) else { return };
        let done = m
            .series
            .values()
            .map(|s| s.coarse.first + s.coarse.vals.len() as u64)
            .max()
            .unwrap_or(0);
        // nothing's been written by this process yet: what the log ended on
        let mut wrote = m.wrote;
        for b in (wrote + 1).max(done.saturating_sub(4))..done {
            if let Some(l) = bucket_line(m, b) {
                text.push_str(&l);
                text.push('\n');
                wrote = b;
            }
        }
        m.wrote = wrote;
        (m.keep.max(1), wrote)
    };
    if text.is_empty() {
        return;
    }
    let path = log_file(host);
    let _ = std::fs::create_dir_all(log_dir());
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(text.as_bytes());
    }
    if wrote.is_multiple_of(240) {
        trim(&path, keep);
    }
}

fn trim(path: &std::path::Path, keep: usize) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    let n = text.lines().count();
    if n > keep * 2 {
        let tail: Vec<&str> = text.lines().skip(n - keep).collect();
        let _ = crate::config::write_atomic(path, format!("{}\n", tail.join("\n")).as_bytes());
    }
}

/// The daemon's side: one quiet stream a machine, always, for its history. This
/// is the one standing connection Aleph keeps without a view asking for it.
#[derive(Default)]
pub struct Keeper {
    tele: HashMap<String, Telemetry>,
    checked: u64,
}

impl Keeper {
    pub fn tick(&mut self, cfg: &Config, hosts: &[String]) {
        if cfg!(test) || crate::showcase::demo() {
            return;
        }
        let now = crate::cache::now();
        if now < self.checked + 5 {
            return;
        }
        self.checked = now;
        let want = wanted(cfg, hosts);
        for h in &want {
            let keep = keep_buckets(cfg, h);
            let mut g = store().lock().unwrap();
            let m = g.entry(h.clone()).or_default();
            let first = m.keep == 0;
            m.set_keep(keep);
            drop(g);
            // where the log left off, before anything's written after it
            if first {
                read_log(h);
            }
        }
        self.tele.retain(|h, _| want.contains(h));
        for h in &want {
            self.tele
                .entry(h.clone())
                .or_insert_with(|| Telemetry::start(h, KEEP_EVERY));
            write_log(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats_with(cpu: f64) -> Stats {
        Stats {
            cpu_hist: vec![cpu],
            ..Stats::default()
        }
    }

    #[test]
    fn scales_step_and_clamp() {
        assert_eq!(scale_secs("30m"), 1800);
        assert_eq!(scale_secs("nonsense"), 1800);
        assert_eq!(scale_step("30m", true), "1h");
        assert_eq!(scale_step("30m", false), "15m");
        assert_eq!(scale_step("12h", true), "12h");
        assert_eq!(scale_step("5m", false), "5m");
    }

    #[test]
    fn steps_are_whole_and_points_bounded() {
        for (_, s) in SCALES {
            let step = step_of(s);
            assert_eq!(step % FINE, 0, "{s}");
            assert!(s / step <= POINTS, "{s}");
            assert_eq!(s % step, 0, "{s}: a whole number of buckets");
        }
    }

    #[test]
    fn a_ring_stays_its_size_however_long_it_runs() {
        let mut r = Ring::new(30, 10);
        for i in 0..10_000u64 {
            r.add(i * 30, i as f64);
        }
        r.close();
        assert_eq!(r.vals.len(), 10);
        assert_eq!(r.get(9_999), Some(9_999.0));
        assert_eq!(r.get(9_990), Some(9_990.0));
        assert_eq!(r.get(9_989), None, "gone");
    }

    #[test]
    fn a_gap_stays_a_gap_and_a_huge_one_clears_the_ring() {
        let mut r = Ring::new(30, 10);
        r.add(0, 1.0);
        r.add(90, 2.0);
        r.close();
        assert_eq!(r.get(0), Some(1.0));
        assert_eq!(r.get(1), None);
        assert_eq!(r.get(3), Some(2.0));
        r.add(30 * 1000, 3.0);
        r.close();
        assert_eq!(r.get(0), None);
        assert_eq!(r.get(1000), Some(3.0));
        assert_eq!(r.vals.len(), 1);
    }

    #[test]
    fn the_log_fills_only_what_a_ring_doesnt_have() {
        let mut r = Ring::new(30, 10);
        r.add(30 * 5, 9.0);
        r.close();
        r.put(1.0, 5);
        assert_eq!(r.get(5), Some(9.0), "its own read stays");
        r.put(2.0, 3);
        assert_eq!(r.get(3), Some(2.0), "earlier: filled in");
        assert_eq!(r.get(4), None);
        r.put(3.0, 4);
        assert_eq!(r.get(4), Some(3.0), "the gap too");
        r.put(4.0, 0);
        assert_eq!(r.first, 0);
        // older than it could ever hold: dropped
        let mut r = Ring::new(30, 10);
        r.add(30 * 100, 1.0);
        r.close();
        r.put(5.0, 50);
        assert_eq!(r.first, 100);
    }

    #[test]
    fn a_graph_holds_still_until_its_bucket_closes() {
        let mut s = Series::new(1440);
        let t0 = 1_800_000_000u64;
        for i in 0..3600u64 {
            s.fine.add(t0 + i, (i % 97) as f64);
            s.coarse.add(t0 + i, (i % 97) as f64);
        }
        let now = t0 + 3600;
        let span = 1800;
        let step = step_of(span);
        let aligned = now / step * step;
        let a = s.window(aligned, span);
        // more samples arrive inside the same bucket: not a dot of it moves
        for i in 0..(step - 1) {
            s.fine.add(aligned + i, 500.0);
            let b = s.window(aligned + i + 1, span);
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(&b) {
                assert!(x == y || (x.is_nan() && y.is_nan()), "{x} {y}");
            }
        }
        // the bucket closes: the next point arrives, and the length holds
        let c = s.window(aligned + step, span);
        assert_eq!(c.len(), a.len());
        assert!((c[c.len() - 1] - 500.0).abs() < 1e-3, "{}", c[c.len() - 1]);
    }

    #[test]
    fn the_length_is_the_timescale_with_gaps_where_nothing_was_read() {
        let mut s = Series::new(1440);
        let now = 1_800_000_000u64;
        // only the last two minutes were read
        for i in 0..120u64 {
            s.fine.add(now - 120 + i, 0.5);
            s.coarse.add(now - 120 + i, 0.5);
        }
        let w = s.window(now, 1800);
        assert_eq!(w.len(), 180);
        assert!(w[0].is_nan(), "half an hour ago: nothing");
        assert!((w[179] - 0.5).abs() < 1e-3);
        // beyond the detailed ring's hour, from the long one
        let w = s.window(now, 12 * 3600);
        assert_eq!(w.len(), 180);
        assert!((w[179] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn older_than_the_detailed_ring_comes_from_the_long_one() {
        let mut s = Series::new(1440);
        let now = 1_800_000_000u64;
        // the daemon's: every 30 s for the last 6 hours
        for i in 0..720u64 {
            s.coarse.put(0.25, (now - 21_600) / COARSE + i);
        }
        let w = s.window(now, 1800);
        assert!(w.iter().filter(|x| x.is_finite()).count() > 170, "{w:?}");
        let w = s.window(now, 6 * 3600);
        assert!(w.iter().filter(|x| x.is_finite()).count() > 170);
    }

    #[test]
    fn memory_is_bounded_on_a_big_machine_at_twelve_hours() {
        let mut m = Machine {
            keep: 1440,
            ..Machine::default()
        };
        let t0 = 1_800_000_000u64;
        // 16 GPUs at four series each, 8 interfaces, 8 disks, and the rest
        let keys: Vec<String> = (0..120).map(|i| format!("s{i}")).collect();
        for t in (0..(13 * 3600u64)).step_by(30) {
            for k in &keys {
                let s = m.series(k);
                s.fine.add(t0 + t, 1.0);
                s.coarse.add(t0 + t, 1.0);
            }
        }
        let cells: usize = m
            .series
            .values()
            .map(|s| s.fine.vals.len() + s.coarse.vals.len())
            .sum();
        assert!(cells <= 120 * (FINE_KEEP + 1440), "{cells}");
        assert!(cells * 4 < 2 * 1024 * 1024, "{} bytes", cells * 4);
    }

    #[test]
    fn a_machine_with_nothing_kept_is_left_alone() {
        let mut st = stats_with(0.5);
        apply("never-recorded", &mut st, 1800, 1_800_000_000);
        assert_eq!(st.cpu_hist, vec![0.5]);
    }

    #[test]
    fn apply_swaps_histories_and_says_the_step() {
        let host = "apply-test";
        let now = 1_800_000_000u64;
        for i in 0..600u64 {
            let mut st = stats_with(0.5);
            record(host, now - 600 + i, &mut st);
        }
        let mut st = stats_with(0.9);
        apply(host, &mut st, 1800, now);
        assert_eq!(st.cpu_hist.len(), 180);
        assert_eq!(
            st.period, 10.0,
            "the time axis counts in the graph's own step"
        );
        assert!((st.cpu_hist[178] - 0.5).abs() < 1e-3);
    }

    #[test]
    fn five_minutes_stays_live_once_it_has_the_samples() {
        let host = "five-test";
        let now = 1_800_000_000u64;
        for i in 0..200u64 {
            record(host, now - 200 + i, &mut stats_with(0.5));
        }
        let mut st = Stats {
            cpu_hist: vec![0.1; 100],
            ..Stats::default()
        };
        apply(host, &mut st, 300, now);
        assert_eq!(st.cpu_hist.len(), 100);
        // just opened, not enough of its own: from the rings
        let mut st = stats_with(0.1);
        apply(host, &mut st, 300, now);
        assert_eq!(st.cpu_hist.len(), 60);
    }

    #[test]
    fn keep_hours_are_per_machine() {
        let cfg = Config::in_memory(&["stats_keep_hours=12", "host.b31.stats_keep_hours=2"]);
        assert_eq!(keep_buckets(&cfg, "gpu2"), 1440);
        assert_eq!(keep_buckets(&cfg, "b31"), 240);
        let off = Config::in_memory(&["stats_keep_hours=0"]);
        assert_eq!(keep_buckets(&off, "gpu2"), 0);
    }

    #[test]
    fn only_machines_it_may_talk_to_have_a_stream() {
        let hosts = ["b31".to_string(), "gpu2".to_string(), "c3".to_string()];
        let cfg = Config::in_memory(&["host.gpu2.paused=true", "host.c3.stats_keep_hours=0"]);
        assert_eq!(wanted(&cfg, &hosts), vec!["b31".to_string()]);
        let off = Config::in_memory(&["stats=off"]);
        assert!(wanted(&off, &hosts).is_empty());
        // not the hub: only itself, however many machines it knows
        let mine = [crate::sshcfg::LOCAL.to_string(), "b31".to_string()];
        let other = Config::in_memory(&["hub.machine=some-other-box-xyz"]);
        assert_eq!(
            wanted(&other, &mine),
            vec![crate::sshcfg::LOCAL.to_string()]
        );
        let hub = Config::in_memory(&[]);
        assert_eq!(wanted(&hub, &mine).len(), 2, "the hub reads them all");
    }

    #[test]
    fn a_shorter_keep_shrinks_what_is_there() {
        let mut m = Machine {
            keep: 1440,
            ..Machine::default()
        };
        let s = m.series("cpu");
        for b in 0..1000 {
            s.coarse.put(1.0, b);
        }
        m.set_keep(100);
        let s = &m.series["cpu"];
        assert_eq!(s.coarse.vals.len(), 100);
        assert_eq!(s.coarse.first, 900);
    }

    #[test]
    fn a_log_line_carries_only_what_was_read() {
        let mut m = Machine {
            keep: 10,
            ..Machine::default()
        };
        m.series("cpu").coarse.put(0.12345, 7);
        m.series("mem").coarse.put(0.5, 8);
        let l = bucket_line(&m, 7).unwrap();
        let v: Value = serde_json::from_str(&l).unwrap();
        assert_eq!(v["b"], 7);
        assert!(
            (v["v"]["cpu"].as_f64().unwrap() - 0.123).abs() < 1e-9,
            "{l}"
        );
        assert!(v["v"].get("mem").is_none());
        assert!(bucket_line(&m, 9).is_none());
    }

    #[test]
    fn host_names_never_leave_the_folder() {
        let p = log_file("../../etc/passwd");
        assert_eq!(p.parent(), Some(log_dir().as_path()));
        assert!(!p.file_name().unwrap().to_string_lossy().contains('/'));
    }
}
