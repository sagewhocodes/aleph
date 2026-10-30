//! Monitor renderers: fuzzed against every size and every kind of `Stats`
//! (empty, rich, absurd), content checks, and an ignored visual-QA dump
//! (`cargo test -- --ignored --nocapture monitor`).

use super::*;
use crate::telemetry::{DiskIo, Gpu, GpuProc, Net, Proc, Stats};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use std::time::{Duration, Instant};

fn wave(n: usize, phase: f64, amp: f64, base: f64) -> Vec<f64> {
    (0..n)
        .map(|i| (base + amp * ((i as f64 / 7.0 + phase).sin())).clamp(0.0, 1.0))
        .collect()
}

fn mk_gpu(i: usize) -> Gpu {
    Gpu {
        index: i.to_string(),
        name: "RTX 4090".into(),
        util: if i == 0 { 0.93 } else { 0.41 },
        mem_used: 20.0e9,
        mem_total: 24.0 * 1024.0 * 1024.0 * 1024.0,
        temp: Some(if i == 0 { 71.0 } else { 58.0 }),
        power: Some(if i == 0 { 384.2 } else { 210.0 }),
        power_limit: Some(450.0),
        fan: Some(60.0),
        bus: format!("00000000:0{}:00.0", i + 1),
        hist: wave(120, i as f64, 0.4, 0.55),
        vendor: "nvidia".into(),
        mem_util: Some(0.45),
        clock_sm: Some(2520.0),
        clock_mem: Some(10501.0),
        clock_sm_max: Some(2850.0),
        pstate: "P2".into(),
        pcie: "gen4 x16".into(),
        enc: Some(0.0),
        dec: Some(0.12),
        temp_mem: Some(64.0),
        throttle: if i == 0 {
            vec!["power cap".into()]
        } else {
            vec![]
        },
        ecc: Some(0),
        mem_hist: wave(120, 2.0, 0.1, 0.8),
        power_hist: wave(120, 0.5, 0.3, 0.6),
        temp_hist: wave(120, 0.5, 0.1, 0.6).iter().map(|v| v * 100.0).collect(),
        ..Default::default()
    }
}

fn proc_(i: usize) -> Proc {
    let names = [
        "python",
        "claude",
        "node",
        "postgres",
        "nginx",
        "rustc",
        "cargo",
        "sshd",
        "kworker/3:1",
        "codex",
    ];
    let name = names[i % names.len()];
    let cmd = match name {
        "python" => format!("/usr/bin/python3 train.py --epochs {i} --batch 64"),
        "node" => "node /usr/lib/node_modules/@openai/codex/bin/codex.js exec".to_string(),
        "kworker/3:1" => String::new(),
        n => format!("/usr/local/bin/{n} --serve"),
    };
    Proc {
        pid: (1000 + i * 37).to_string(),
        user: if i.is_multiple_of(4) {
            "root"
        } else {
            "selene"
        }
        .into(),
        cpu: if i < 3 {
            3.4 - i as f64
        } else {
            0.9 / (i as f64)
        },
        rss: 1.0e8 * (60 - i) as f64,
        name: name.into(),
        state: ["R", "S", "S", "D", "S", "Z"][i % 6].into(),
        threads: (i as u32 % 12) + 1,
        cmd,
        time: 37.0 * i as f64 * i as f64,
        // a small tree: every 5th process is a child of the one before
        ppid: if i.is_multiple_of(5) || i == 0 {
            "1".into()
        } else {
            (1000 + (i - 1) * 37).to_string()
        },
        age: Some(3600 * (i as u64 + 1)),
        avg_cpu: Some(0.1 * (i % 7) as f64),
    }
}

/// 24 cores, 2 GPUs with every field, 60 processes, 4 interfaces, 3 disks, temps.
pub(super) fn rich() -> Stats {
    let cores: Vec<f64> = (0..24).map(|i| ((i * 37 % 100) as f64) / 100.0).collect();
    let gib = 1024.0 * 1024.0 * 1024.0;
    Stats {
        hostname: "gpu-box-01".into(),
        kernel: "Linux 6.8.0-45-generic".into(),
        os: "Ubuntu 22.04.4 LTS".into(),
        cpu_model: "AMD EPYC 7763 64-Core Processor".into(),
        ncpu: 24,
        sockets: 1,
        cpu: 0.54,
        cores,
        cpu_hist: wave(120, 0.0, 0.35, 0.5),
        mem_total: 125.8 * gib,
        mem_used: 41.2 * gib,
        swap_total: 8.0 * gib,
        swap_used: 1.2 * gib,
        mem_hist: wave(120, 1.0, 0.05, 0.33),
        load: [3.12, 2.8, 2.11],
        runnable: "2/412".into(),
        uptime: 3 * 86400 + 4 * 3600,
        net_rx: 12.3e6,
        net_tx: 1.2e6,
        net_hist: wave(120, 0.0, 0.2, 0.3),
        gpus: vec![mk_gpu(0), mk_gpu(1)],
        gpu_procs: vec![
            GpuProc {
                gpu: "0".into(),
                pid: "1000".into(),
                mem: 19.0e9,
                user: "selene".into(),
                name: "python".into(),
                kind: "C".into(),
                sm: Some(0.87),
                mem_util: Some(0.41),
                enc: None,
                dec: None,
                sm_avg: Some(0.79),
                cpu: Some(3.4),
                avg_cpu: Some(2.9),
                age: Some(5 * 3600 + 120),
                cmd: "/usr/bin/python3 train.py --epochs 0 --batch 64".into(),
                dir: String::new(),
                agent: None,
                owner: String::new(),
            },
            GpuProc {
                gpu: "1".into(),
                pid: "1037".into(),
                mem: 8.2e9,
                user: "selene".into(),
                name: "python".into(),
                kind: "C".into(),
                sm: Some(0.33),
                sm_avg: Some(0.4),
                age: Some(600),
                ..Default::default()
            },
            GpuProc {
                gpu: "0".into(),
                pid: "57638".into(),
                mem: 303.0 * 1048576.0,
                user: "root".into(),
                name: "Xorg".into(),
                kind: "G".into(),
                ..Default::default()
            },
        ],
        top: (0..8).map(proc_).collect(),
        disks: vec![
            ("/".into(), 2.0e12, 1.2e12),
            ("/home".into(), 4.0e12, 3.7e12),
            ("/data".into(), 8.0e12, 0.4e12),
        ],
        claude_procs: 3,
        claude_rss: 1.2e9,
        convos_today: Some(41),
        samples: 42,
        updated: Some(Instant::now()),
        err: None,
        has_gpu_tool: true,
        platform: "linux".into(),
        cpu_user: 0.40,
        cpu_system: 0.10,
        cpu_iowait: 0.03,
        cpu_steal: 0.01,
        load_hist: (0..120)
            .map(|i| 2.6 + 1.4 * (i as f64 / 9.0).sin())
            .collect(),
        cpu_temp: Some(64.0),
        temps: vec![
            ("Tctl".into(), 64.0),
            ("nvme Composite".into(), 41.0),
            ("acpitz".into(), 27.8),
        ],
        mem_buffers: 1.1 * gib,
        mem_cached: 80.3 * gib,
        mem_shared: 0.3 * gib,
        mem_available: 83.0 * gib,
        swap_hist: wave(120, 0.0, 0.01, 0.15),
        procs_total: 412,
        procs_running: 2,
        threads_total: 1203,
        users_logged: Some(2),
        nets: ["eth0", "eno2", "ib0", "wg0"]
            .iter()
            .enumerate()
            .map(|(i, n)| Net {
                name: n.to_string(),
                rx: 12.3e6 / (i + 1) as f64,
                tx: 1.2e6 / (i + 1) as f64,
                hist: wave(120, i as f64, 0.4, 0.5)
                    .iter()
                    .map(|v| v * 4.5e7)
                    .collect(),
                rx_hist: wave(120, i as f64, 0.4, 0.5)
                    .iter()
                    .map(|v| v * 4.0e7 / (i + 1) as f64)
                    .collect(),
                tx_hist: wave(120, i as f64 + 1.5, 0.3, 0.4)
                    .iter()
                    .map(|v| v * 5.0e6 / (i + 1) as f64)
                    .collect(),
            })
            .collect(),
        disk_io: ["nvme0n1", "nvme1n1", "sda"]
            .iter()
            .enumerate()
            .map(|(i, n)| DiskIo {
                name: n.to_string(),
                read: 120.0e6 / (i + 1) as f64,
                write: 30.0e6 / (i + 1) as f64,
                busy: if i == 2 {
                    None
                } else {
                    Some(0.45 / (i + 1) as f64)
                },
                hist: wave(120, i as f64, 0.5, 0.5)
                    .iter()
                    .map(|v| v * 2.0e8)
                    .collect(),
                read_hist: wave(120, i as f64, 0.5, 0.5)
                    .iter()
                    .map(|v| v * 1.6e8)
                    .collect(),
                write_hist: wave(120, i as f64 + 2.0, 0.4, 0.3)
                    .iter()
                    .map(|v| v * 4.0e7)
                    .collect(),
            })
            .collect(),
        procs: (0..60).map(proc_).collect(),
        gpu_note: None,
        agent_procs: vec![
            ("claude".into(), 6, 19.0 * gib),
            ("codex".into(), 12, 33.0 * gib),
        ],
        agent_list: vec![],
        sessions_today: vec![("claude".into(), 14), ("codex".into(), 3)],
        period: 1.0,
        cpu_user_hist: wave(120, 0.0, 0.25, 0.38),
        cpu_system_hist: wave(120, 1.0, 0.05, 0.1),
        cpu_iowait_hist: wave(120, 2.0, 0.02, 0.03),
        cpu_temp_hist: wave(120, 0.0, 0.1, 0.62)
            .iter()
            .map(|v| v * 100.0)
            .collect(),
        mem_cache_hist: wave(120, 1.0, 0.03, 0.65),
        net_rx_hist: wave(120, 0.0, 0.4, 0.5).iter().map(|v| v * 4.0e7).collect(),
        net_tx_hist: wave(120, 1.0, 0.3, 0.4).iter().map(|v| v * 5.0e6).collect(),
        cpu_mhz: Some(3412.0),
        cpu_mhz_peak: Some(4890.0),
        cpu_mhz_max: Some(5200.0),
    }
}

/// Every number absurd: NaN, ±inf, negative, huge; strings with control and
/// wide characters; mismatched lengths.
fn weird() -> Stats {
    let mut st = rich();
    st.cpu = f64::NAN;
    st.cpu_user = f64::INFINITY;
    st.cpu_system = -3.0;
    st.cpu_iowait = f64::NAN;
    st.cpu_steal = 7.0;
    st.cores = vec![f64::NAN, f64::INFINITY, -1.0, 2.0, 0.5];
    st.cpu_hist = vec![f64::NAN, f64::INFINITY, -5.0, 1e300];
    st.mem_total = f64::INFINITY;
    st.mem_used = f64::NAN;
    st.mem_buffers = -1.0;
    st.mem_cached = 1e300;
    st.swap_total = -8.0;
    st.swap_used = f64::NAN;
    st.load = [f64::NAN, f64::INFINITY, -1.0];
    st.load_hist = vec![f64::NAN; 500];
    st.uptime = u64::MAX;
    st.ncpu = 0;
    st.net_rx = f64::NEG_INFINITY;
    st.cpu_temp = Some(f64::NAN);
    st.temps = vec![("\x1b[31mevil\tsensor\n".into(), f64::INFINITY)];
    st.hostname = "日本語のホスト名がとても長い\x07".into();
    st.os = "\u{0}\u{1b}[2J".into();
    st.procs_total = usize::MAX;
    st.threads_total = usize::MAX;
    for g in st.gpus.iter_mut() {
        g.util = f64::NAN;
        g.mem_total = 0.0;
        g.mem_used = 1e30;
        g.power = Some(f64::INFINITY);
        g.power_limit = Some(0.0);
        g.temp = Some(-400.0);
        g.clock_sm = Some(f64::NAN);
        g.hist = vec![];
        g.name = "\u{1b}]0;title\u{7}GPU 名前".into();
        g.throttle = vec!["thermal\nslowdown".into(); 9];
        g.ecc = Some(u64::MAX);
    }
    st.procs.iter_mut().for_each(|p| {
        p.cpu = f64::NAN;
        p.rss = f64::NEG_INFINITY;
        p.time = f64::INFINITY;
        p.name = "名前\t\u{1b}x".into();
        p.cmd = "a".repeat(5000);
    });
    st.procs.push(Proc::default());
    st.nets[0].hist = vec![f64::INFINITY; 3];
    st.disk_io[0].busy = Some(f64::NAN);
    st.disks.push(("".into(), f64::NAN, f64::INFINITY));
    st.disks.push(("/x".into(), -1.0, 5.0));
    st.gpu_procs.push(GpuProc {
        gpu: "\n".into(),
        ..Default::default()
    });
    st
}

fn sampling() -> Stats {
    Stats::default()
}

fn broken() -> Stats {
    Stats {
        err: Some("ssh: connect to host b31 port 22: Connection refused".into()),
        ..Stats::default()
    }
}

fn stale() -> Stats {
    let mut st = rich();
    st.err = Some("stats stream ended".into());
    st.updated = Instant::now().checked_sub(Duration::from_secs(60));
    st
}

fn no_gpu() -> Stats {
    let mut st = rich();
    st.gpus.clear();
    st.gpu_procs.clear();
    st.has_gpu_tool = false;
    st.gpu_note = Some("no nvidia-smi or rocm-smi".into());
    st
}

/// The minimum a machine can send: one sample, nothing else.
fn bare() -> Stats {
    Stats {
        samples: 1,
        updated: Some(Instant::now()),
        ..Stats::default()
    }
}

fn many_gpus() -> Stats {
    let mut st = rich();
    st.gpus = (0..8).map(mk_gpu).collect();
    st.cores = (0..128).map(|i| (i % 10) as f64 / 10.0).collect();
    st.ncpu = 128;
    st
}

const SIZES: &[(u16, u16)] = &[
    (0, 0),
    (1, 1),
    (2, 5),
    (5, 2),
    (7, 3),
    (10, 4),
    (16, 8),
    (24, 6),
    (30, 12),
    (40, 10),
    (59, 20),
    (60, 20),
    (80, 24),
    (89, 30),
    (90, 30),
    (100, 30),
    (120, 40),
    (139, 45),
    (140, 45),
    (160, 48),
    (200, 60),
    (220, 50),
    (300, 100),
    (300, 1),
    (3, 100),
];

fn term(w: u16, h: u16) -> Terminal<TestBackend> {
    Terminal::new(TestBackend::new(w, h)).unwrap()
}

fn text(t: &Terminal<TestBackend>) -> String {
    let b = t.backend().buffer();
    let mut s = String::new();
    for y in 0..b.area.height {
        let mut skip = 0;
        for x in 0..b.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let sym = b[(x, y)].symbol();
            skip = ui::width(sym).saturating_sub(1);
            s.push_str(sym);
        }
        s.push('\n');
    }
    s
}

#[derive(Clone, Copy, Debug)]
enum V {
    Fleet,
    Overview,
    /// the overview at a focus, processes on / off
    Focus(Focus, bool),
    Cpu,
    Memory,
    Gpu,
    Procs,
    /// the process table as a flat list
    ProcsFlat,
    Net,
    Compact(u8),
    Strip,
    Header,
}

const ALL_V: [V; 19] = [
    V::Fleet,
    V::Overview,
    V::Focus(Focus::All, false),
    V::Focus(Focus::Cpu, true),
    V::Focus(Focus::Mem, true),
    V::Focus(Focus::Gpu, true),
    V::Focus(Focus::Procs, true),
    V::Focus(Focus::Gpu, false),
    V::Cpu,
    V::Memory,
    V::Gpu,
    V::Procs,
    V::ProcsFlat,
    V::Net,
    V::Compact(0),
    V::Compact(1),
    V::Compact(2),
    V::Strip,
    V::Header,
];

fn draw(v: V, st: &Stats, w: u16, h: u16) -> String {
    draw_at(v, st, w, h, Rect::new(0, 0, w, h))
}

fn draw_at(v: V, st: &Stats, w: u16, h: u16, r: Rect) -> String {
    let mut t = term(w, h);
    let t0 = Instant::now();
    let other = rich();
    let rows = [
        MachineRow {
            alias: "b31",
            color: Color::Rgb(0xd9, 0x77, 0x57),
            stats: Some(st),
            up: true,
            note: "",
        },
        MachineRow {
            alias: "gpu2",
            color: Color::Rgb(0x89, 0xb4, 0xfa),
            stats: Some(&other),
            up: true,
            note: "herdr not running",
        },
        MachineRow {
            alias: "old-box",
            color: Color::Rgb(0xa6, 0xe3, 0xa1),
            stats: None,
            up: false,
            note: "ssh: connection timed out",
        },
        MachineRow {
            alias: "new",
            color: Color::Rgb(0xcb, 0xa6, 0xf7),
            stats: None,
            up: true,
            note: "",
        },
    ];
    t.draw(|f| match v {
        V::Fleet => fleet(f, r, &rows, 1, t0),
        V::Overview => overview(f, r, "b31", Color::Rgb(0xd9, 0x77, 0x57), st, t0),
        V::Focus(fo, on) => overview_body(f, r, st, t0, fo, on),
        V::Cpu => cpu(f, r, st),
        V::Memory => memory(f, r, st),
        V::Gpu => gpu(f, r, st),
        V::Procs | V::ProcsFlat => {
            let mut ui = ProcUi {
                sel: 3,
                tree: matches!(v, V::Procs),
                ..Default::default()
            };
            let n = procs(f, r, st, &mut ui);
            assert!(n <= plist(st).len());
            assert!(ui.sel < n.max(1));
        }
        V::Net => net(f, r, st),
        V::Compact(m) => compact(f, r, "b31", Color::Rgb(0xd9, 0x77, 0x57), st, m, t0),
        V::Strip => {
            let hits = strip(
                f,
                r,
                Sub::Gpu,
                "b31",
                Color::Rgb(0xd9, 0x77, 0x57),
                st.updated,
                "30m",
                &Sub::ALL,
            );
            for (rr, _) in hits {
                assert!(
                    rr.right() <= w && rr.bottom() <= h,
                    "{rr:?} outside {w}×{h}"
                );
            }
        }
        V::Header => header(f, r, "b31", Color::Rgb(0xd9, 0x77, 0x57), st),
    })
    .unwrap();
    text(&t)
}

#[test]
fn a_gap_in_a_history_is_not_a_zero() {
    let (avg, peak) = avg_peak_frac(&[f64::NAN, f64::NAN, 0.5, 0.25]);
    assert!((avg - 0.375).abs() < 1e-9, "{avg}");
    assert_eq!(peak, 0.5);
    let sum = sum_hists([[f64::NAN, 0.5].as_slice(), [f64::NAN, 0.25].as_slice()].into_iter());
    assert!(sum[0].is_nan(), "nothing read by either: still a gap");
    assert_eq!(sum[1], 0.75);
    let one = sum_hists([[f64::NAN, 0.5].as_slice(), [0.25, 0.25].as_slice()].into_iter());
    assert_eq!(one, vec![0.25, 0.75], "one read it: that counts");
    let spark = ui::spark(&[f64::NAN, 1.0], 2, Color::Red);
    assert_eq!(spark.content, " █");
}

#[test]
fn monitor_never_panics() {
    let all = [
        ("empty", sampling()),
        ("broken", broken()),
        ("bare", bare()),
        ("rich", rich()),
        ("weird", weird()),
        ("stale", stale()),
        ("nogpu", no_gpu()),
        ("many", many_gpus()),
    ];
    for (name, st) in &all {
        for &(w, h) in SIZES {
            for v in ALL_V {
                let r = std::panic::catch_unwind(|| draw(v, st, w, h));
                assert!(r.is_ok(), "{v:?} panicked on {name} at {w}×{h}");
            }
        }
    }
}

#[test]
fn monitor_rects_outside_the_frame_are_clipped() {
    let st = rich();
    for v in ALL_V {
        // offset, larger than the frame, and fully outside
        draw_at(v, &st, 80, 24, Rect::new(10, 5, 200, 100));
        draw_at(v, &st, 80, 24, Rect::new(79, 23, 5, 5));
        draw_at(v, &st, 80, 24, Rect::new(500, 500, 10, 10));
        draw_at(
            v,
            &st,
            80,
            24,
            Rect::new(u16::MAX - 3, u16::MAX - 3, 50, 50),
        );
    }
}

#[test]
fn monitor_sampling_and_errors() {
    for v in [
        V::Overview,
        V::Cpu,
        V::Memory,
        V::Gpu,
        V::Net,
        V::Compact(0),
    ] {
        let s = draw(v, &sampling(), 100, 30);
        assert!(s.contains("sampling…"), "{v:?}:\n{s}");
        let s = draw(v, &broken(), 100, 30);
        assert!(s.contains("Connection refused"), "{v:?}:\n{s}");
    }
    let s = draw(V::Overview, &stale(), 160, 48);
    assert!(s.contains("stats stream ended"), "{s}");
}

#[test]
fn monitor_overview_content() {
    let st = rich();
    let s = draw(V::Overview, &st, 160, 48);
    for want in [
        "b31",
        "gpu-box-01",
        "CPU",
        "Memory",
        "RTX 4090",
        "VRAM",
        "Processes",
        "python",
        "eth0",
        "nvme0n1",
        "claude ×",
        "up 3d 4h",
        " Ubuntu 22.04 ",
        "24 threads",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // the core grid fits beside the chart at this size
    assert!(
        s.lines().any(|l| l.contains(" 23 ") && l.contains('%')),
        "core grid:\n{s}"
    );
    // who holds the memory, by program
    assert!(s.contains("python ×"), "{s}");
    // narrow: CPU and memory still there
    let s = draw(V::Overview, &st, 60, 30);
    assert!(s.contains("CPU") && s.contains("Memory"), "{s}");
    // no GPU: said in the header (the GPU view gives the reason)
    let s = draw(V::Overview, &no_gpu(), 160, 48);
    assert!(s.contains("no GPU") && !s.contains("╭ GPU"), "{s}");
}

#[test]
fn monitor_overview_focus() {
    let st = rich();
    let lines_of = |s: &str, pat: &str| s.lines().filter(|l| l.contains(pat)).count();
    // processes off: no process panel, resources take the room
    let s = draw(V::Focus(Focus::All, false), &st, 160, 46);
    assert!(!s.contains("Processes"), "{s}");
    assert!(
        s.contains("Agents"),
        "agents get their own panel then:\n{s}"
    );
    // a focus makes its panel the biggest: count the rows under its title
    // rows under a panel's title down to its bottom border (by column)
    let height = |s: &str, title: &str| -> usize {
        let rows: Vec<Vec<char>> = s.lines().map(|l| l.chars().collect()).collect();
        let pat: Vec<char> = format!("╭ {title} ").chars().collect();
        for (y, row) in rows.iter().enumerate() {
            if let Some(x) = row.windows(pat.len()).position(|w| w == pat.as_slice()) {
                return rows[y + 1..]
                    .iter()
                    .take_while(|l| l.get(x) != Some(&'╰'))
                    .count();
            }
        }
        0
    };
    let all = draw(V::Focus(Focus::All, true), &st, 160, 46);
    let foc = draw(V::Focus(Focus::Cpu, true), &st, 160, 46);
    assert!(height(&foc, "CPU") > height(&all, "CPU"), "{foc}");
    let foc = draw(V::Focus(Focus::Mem, true), &st, 160, 46);
    assert!(height(&foc, "Memory") > height(&all, "Memory"), "{foc}");
    let foc = draw(V::Focus(Focus::Gpu, true), &st, 160, 46);
    assert!(height(&foc, "GPUs") > height(&all, "GPUs"), "{foc}");
    // the GPU focus lists the GPU processes
    assert!(
        foc.contains("VRAM") && lines_of(&foc, "python") >= 1,
        "{foc}"
    );
    let foc = draw(V::Focus(Focus::Procs, true), &st, 160, 46);
    assert!(
        height(&foc, "Processes") > 2 * height(&all, "Processes"),
        "{foc}"
    );
    assert_eq!(Focus::Procs.next(), Focus::All);
}

#[test]
fn monitor_cpu_content() {
    let s = draw(V::Cpu, &rich(), 160, 48);
    for want in [
        "CPU usage",
        "Cores",
        "user",
        "system",
        "iowait",
        "Load average",
        "Temperatures",
        "Tctl",
        "Clock & tasks",
        "3.4 GHz",
        "Top CPU",
        "python",
        "now",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // every core has a meter
    assert!(s.lines().any(|l| l.contains(" 23 ")), "{s}");
    // charts: braille, and a time axis
    assert!(
        s.chars().any(|c| ('\u{2801}'..='\u{28ff}').contains(&c)),
        "{s}"
    );
    assert!(s.contains("−2m"), "{s}");
    let mut st = rich();
    st.cores.clear();
    let s = draw(V::Cpu, &st, 120, 40);
    assert!(s.contains("no per-core data on this machine"), "{s}");
    // 128 cores still render (a grid or the strip) at 160 wide
    let s = draw(V::Cpu, &many_gpus(), 160, 48);
    assert!(s.contains("128 · avg"), "{s}");
    // narrow: stacked
    let s = draw(V::Cpu, &rich(), 80, 30);
    assert!(s.contains("CPU usage") && s.contains("Cores"), "{s}");
}

#[test]
fn monitor_memory_content() {
    let s = draw(V::Memory, &rich(), 160, 48);
    for want in [
        "Memory",
        "used",
        "buffers",
        "cache",
        "free",
        "available",
        "Swap",
        "History",
        "By program",
        "Top memory",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // at most ten processes in the top list
    let top = s
        .lines()
        .skip_while(|l| !l.contains("Top memory"))
        .filter(|l| l.contains(" selene ") || l.contains(" root "))
        .count();
    assert!(top <= 10, "{top}:\n{s}");
    let s = draw(V::Memory, &bare(), 100, 30);
    assert!(s.contains("no memory data"), "{s}");
}

#[test]
fn monitor_gpu_content() {
    let s = draw(V::Gpu, &rich(), 160, 48);
    for want in [
        "GPU 0",
        "GPU 1",
        "RTX 4090",
        "UTIL",
        "VRAM",
        "POWER",
        "TEMP",
        "fan",
        "2520 / 2850 MHz",
        "P2",
        "gen4 x16",
        "GPU processes",
        "python",
        "Xorg",
        "87%",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // one GPU: every nvidia-smi signal in the card, and a legend over the chart
    let mut one = rich();
    one.gpus.truncate(1);
    let s = draw(V::Gpu, &one, 160, 48);
    for want in [
        "PCIe",
        "enc / dec",
        "throttle",
        "power cap",
        "ECC",
        "util avg",
        "━━ util",
        "━━ temp",
        "SM%",
        "UPTIME",
        "5h02m",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    let s = draw(V::Gpu, &no_gpu(), 100, 30);
    assert!(s.contains("no nvidia-smi or rocm-smi"), "{s}");
    let s = draw(V::Gpu, &bare(), 100, 30);
    assert!(s.contains("no GPU (no nvidia-smi or rocm-smi)"), "{s}");
    let s = draw(V::Gpu, &many_gpus(), 160, 48);
    assert!(s.contains("GPU 7") && s.contains("8 GPUs"), "{s}");
}

fn procs_ui(tree: bool) -> ProcUi {
    ProcUi {
        tree,
        ..Default::default()
    }
}

#[test]
fn monitor_procs_table() {
    let st = rich();
    let mut t = term(140, 30);
    let mut ui = procs_ui(false);
    let mut n = 0;
    t.draw(|f| n = procs(f, f.area(), &st, &mut ui)).unwrap();
    assert_eq!(n, 60);
    let s = text(&t);
    for want in [
        "PID", "USER", "CPU%▼", "AVG%", "MEM%", "RSS", "THR", "UPTIME", "TIME+", "COMMAND",
        "python", "train.py", "list", "CPU", "MEM", "SWAP",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // the details line: who it is and its whole command line
    assert!(s.contains("▸ python") && s.contains("pid 1000"), "{s}");
    assert!(s.contains("/usr/bin/python3 train.py --epochs 0"), "{s}");
    // filtered (fuzzy on name + command)
    ui.sort = Sort::Mem;
    ui.filter = "trn".into();
    t.draw(|f| n = procs(f, f.area(), &st, &mut ui)).unwrap();
    let s = text(&t);
    assert!(n > 0 && n < 60, "{n}");
    assert!(s.contains("train.py") && !s.contains("nginx"), "{s}");
    ui.filter = "zzzz".into();
    t.draw(|f| n = procs(f, f.area(), &st, &mut ui)).unwrap();
    assert_eq!(n, 0);
    assert!(text(&t).contains("nothing matches"));
    // the selection stays visible however far down it is
    ui.filter.clear();
    ui.sort = Sort::Pid;
    ui.jump(59);
    t.draw(|f| n = procs(f, f.area(), &st, &mut ui)).unwrap();
    let s = text(&t);
    let last_pid = (1000 + 59 * 37).to_string();
    assert!(s.contains(&last_pid), "{s}");
    assert!(s.contains("60/60"), "{s}");
    // narrow: the command column survives
    let mut t = term(40, 12);
    let mut ui = procs_ui(true);
    t.draw(|f| n = procs(f, f.area(), &st, &mut ui)).unwrap();
    let s = text(&t);
    assert!(s.contains("python") || s.contains("claude"), "{s}");
}

#[test]
fn monitor_procs_tree() {
    let st = rich();
    let ui = procs_ui(true);
    let rows = ptable::tree_text(&st.procs, &ui);
    assert_eq!(rows.len(), 60);
    // chains of five: each process under the one before it
    assert_eq!(rows[0], "python");
    assert!(rows[1].starts_with("└─ "), "{rows:?}");
    assert!(rows[2].starts_with("   └─ "), "{rows:?}");
    // folding the root hides its four descendants
    let mut ui = procs_ui(true);
    ui.collapsed.insert("1000".into());
    let rows = ptable::tree_text(&st.procs, &ui);
    assert_eq!(rows.len(), 56);
    assert!(rows[0].starts_with("▸ "), "{rows:?}");
    // the table shows the guides and the folded count
    let mut t = term(140, 30);
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    let s = text(&t);
    assert!(s.contains("(+4)") && s.contains("tree"), "{s}");
    // keys: → unfolds, ← folds, ← on a leaf goes to its parent, space toggles
    assert_eq!(ui.sel_pid.as_deref(), Some("1000"));
    assert!(ui.has_kids && !ui.open);
    ui.right();
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    assert!(ui.open);
    ui.step(1);
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    assert_eq!(ui.sel_pid.as_deref(), Some("1037"));
    assert_eq!(ui.parent_row, Some(0));
    ui.step(3);
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    // the chain's last process: a leaf; ← goes to its parent
    assert!(!ui.has_kids);
    let before = ui.sel;
    ui.left();
    assert_eq!(ui.sel, before - 1);
    ui.jump(0);
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    ui.toggle();
    assert!(ui.collapsed.contains("1000"));
    ui.toggle();
    assert!(!ui.collapsed.contains("1000"));
    // fold everything
    ui.collapse_all = true;
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    assert_eq!(ui.rows, 12, "one row per chain");
    // a filter keeps the matches' ancestors (dimmed) so the tree stays whole
    let mut ui = procs_ui(true);
    ui.filter = "nginx".into();
    let rows = ptable::tree_text(&st.procs, &ui);
    assert!(rows.iter().any(|r| r.ends_with("nginx")), "{rows:?}");
    assert!(rows[0].ends_with("python"), "{rows:?}");
    // cycles (pid reuse) and self-parents don't loop
    let mut odd = st.procs.clone();
    odd[0].ppid = odd[4].pid.clone();
    odd[5].ppid = odd[5].pid.clone();
    let rows = ptable::tree_text(&odd, &procs_ui(true));
    assert_eq!(rows.len(), 60);
}

#[test]
fn monitor_procs_sorting() {
    let st = rich();
    let order = |sort: Sort| -> Vec<String> {
        let mut v: Vec<&Proc> = st.procs.iter().collect();
        ptable::sort_procs(&mut v, sort);
        v.iter().map(|p| p.pid.clone()).collect()
    };
    let cpu = order(Sort::Cpu);
    assert_eq!(cpu[0], "1000"); // 340%
    let pid = order(Sort::Pid);
    assert_eq!(pid[0], "1000");
    assert_eq!(pid[59], (1000 + 59 * 37).to_string());
    let name = order(Sort::Name);
    let first = st.procs.iter().find(|p| p.pid == name[0]).unwrap();
    assert_eq!(first.name, "cargo");
    let mem = order(Sort::Mem);
    assert_eq!(mem[0], "1000");
    // youngest first
    let age = order(Sort::Age);
    assert_eq!(age[0], "1000");
    let avg = order(Sort::Avg);
    let top = st.procs.iter().find(|p| p.pid == avg[0]).unwrap();
    assert!((top.avg_cpu.unwrap() - 0.6).abs() < 1e-9, "{avg:?}");
    assert_eq!(Sort::Cpu.next(), Sort::Mem);
    assert_eq!(Sort::Name.next(), Sort::Cpu);
    assert_eq!(Sort::Cpu.arrow(false), "▼");
    assert_eq!(Sort::Cpu.arrow(true), "▲");
}

#[test]
fn monitor_net_content() {
    let s = draw(V::Net, &rich(), 160, 48);
    for want in [
        "Throughput",
        "download",
        "upload",
        "Interfaces",
        "eth0",
        "ib0",
        "Disks",
        "nvme0n1",
        "busy",
        "/home",
        "1.1T / 1.8T",
        "free",
        "peak",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    let s = draw(V::Net, &bare(), 100, 30);
    assert!(
        s.contains("no per-interface data") && s.contains("no disk I/O data"),
        "{s}"
    );
}

#[test]
fn monitor_fleet_table() {
    let s = draw(V::Fleet, &rich(), 160, 20);
    for want in [
        "MACHINE",
        "CPU",
        "GPU",
        "VRAM",
        "AGENTS",
        "b31",
        "gpu2",
        "old-box",
        "connection timed out",
        "connecting to stats",
        "herdr not running",
        "3/4 up",
        "NVIDIA",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // room below the table: each machine's history
    let s = draw(V::Fleet, &rich(), 160, 48);
    assert!(s.contains("━━ cpu") && s.contains("━━ gpu"), "{s}");
    assert!(s.matches("━━ mem").count() >= 2, "{s}");
    // narrow: machine + cpu + mem stay
    let s = draw(V::Fleet, &rich(), 50, 10);
    assert!(
        s.contains("b31") && s.contains("CPU") && s.contains("MEM"),
        "{s}"
    );
    // the selected row carries the accent bar
    let mut t = term(120, 12);
    let st = rich();
    let rows = [
        MachineRow {
            alias: "a",
            color: Color::Red,
            stats: Some(&st),
            up: true,
            note: "",
        },
        MachineRow {
            alias: "b",
            color: Color::Blue,
            stats: Some(&st),
            up: true,
            note: "",
        },
    ];
    t.draw(|f| fleet(f, f.area(), &rows, 1, Instant::now()))
        .unwrap();
    let s = text(&t);
    let bar_line = s.lines().find(|l| l.contains("┃")).unwrap_or("");
    assert!(bar_line.contains("▌b"), "{s}");
}

#[test]
fn monitor_strip_targets() {
    let mut t = term(160, 1);
    let mut hits = vec![];
    t.draw(|f| {
        hits = strip(
            f,
            f.area(),
            Sub::Cpu,
            "b31",
            Color::Red,
            Some(Instant::now()),
            "30m",
            &Sub::ALL,
        );
    })
    .unwrap();
    let s = text(&t);
    assert!(
        s.contains("‹ ▌b31 ›") && s.contains("Processes") && s.contains("live"),
        "{s}"
    );
    assert_eq!(hits.len(), Sub::ALL.len());
    assert_eq!(hits[2].1, Sub::Cpu);
    // the CPU target is where "CPU" is drawn
    let x = hits[2].0.x as usize;
    assert_eq!(
        &s.lines().next().unwrap()[..]
            .chars()
            .skip(x)
            .take(3)
            .collect::<String>(),
        "CPU"
    );
    // narrow: still every target, short labels
    let mut t = term(40, 1);
    t.draw(|f| {
        hits = strip(
            f,
            f.area(),
            Sub::Cpu,
            "b31",
            Color::Red,
            None,
            "30m",
            &Sub::ALL,
        );
    })
    .unwrap();
    assert_eq!(hits.len(), Sub::ALL.len());
    assert!(hits.iter().all(|(r, _)| r.right() <= 40));
    // lower case jumps; upper case is for toggles
    assert_eq!(Sub::from_key('g'), Some(Sub::Gpu));
    assert_eq!(Sub::from_key('F'), None);
    assert_eq!(Sub::Net.next(), Sub::Services);
    assert_eq!(Sub::Usage.next(), Sub::Fleet);
    assert_eq!(Sub::Fleet.prev(), Sub::Usage);
    assert_eq!(Sub::from_key('u'), Some(Sub::Usage));
}

#[test]
fn monitor_os_badges() {
    let name = |os: &str, p: &str| distro_of(os, p).name;
    assert_eq!(name("Ubuntu 24.04.3 LTS", "linux"), "Ubuntu");
    assert_eq!(name("Pop!_OS 22.04 LTS", "linux"), "Pop!_OS");
    assert_eq!(name("Linux Mint 21.3", "linux"), "Mint");
    assert_eq!(name("Debian GNU/Linux 12 (bookworm)", "linux"), "Debian");
    assert_eq!(
        name("Fedora Linux 40 (Workstation Edition)", "linux"),
        "Fedora"
    );
    assert_eq!(name("Rocky Linux 9.4 (Blue Onyx)", "linux"), "Rocky");
    assert_eq!(name("Arch Linux", "linux"), "Arch");
    assert_eq!(name("Some New Distro 1", "linux"), "Linux");
    assert_eq!(name("macOS 27.0", "macos"), "macOS");
    assert_eq!(name("", ""), "");
    assert_eq!(os_version("Ubuntu 24.04.3 LTS"), "24.04");
    assert_eq!(os_version("Debian GNU/Linux 12 (bookworm)"), "12");
    assert_eq!(os_version("macOS 27.0"), "27");
    assert_eq!(os_version("Arch Linux"), "");
    // Ubuntu's own colour, the release, a prominent pill
    let b = os_badge("Ubuntu 24.04.3 LTS", "linux", true);
    let pill = b.iter().find(|s| s.content.contains("Ubuntu")).unwrap();
    assert_eq!(pill.content, " Ubuntu 24.04 ");
    assert_eq!(pill.style.bg, Some(Color::Rgb(0xe9, 0x54, 0x20)));
    assert!(pill.style.add_modifier.contains(Modifier::BOLD));
    let b = os_badge("macOS 27.0", "macos", false);
    assert!(b.iter().any(|s| s.content == " macOS "), "{b:?}");
    // the header says the distro once, with its release
    let s = draw(V::Header, &rich(), 220, 1);
    assert_eq!(s.matches("Ubuntu").count(), 1, "{s}");
}

#[test]
fn monitor_header_line() {
    let s = draw(V::Header, &rich(), 220, 1);
    for want in [
        "b31",
        "gpu-box-01",
        " Ubuntu 22.04 ",
        "Linux 6.8.0-45-generic",
        "AMD EPYC 7763",
        "24 threads",
        "NVIDIA",
        "2× RTX 4090",
        "up 3d 4h",
        "load 3.12",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // narrow: the machine and the essentials stay
    let s = draw(V::Header, &rich(), 60, 1);
    assert!(s.contains("b31") && s.contains("up 3d 4h"), "{s}");
}

#[test]
fn monitor_compact_modes() {
    let st = rich();
    let s = draw(V::Compact(0), &st, 80, 18);
    for want in ["CPU", "MEM", "GPU0", "IO", "TEMP", "AGENT", "resources"] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    // resources, not a process list
    assert!(!s.contains("COMMAND"), "{s}");
    // a taller panel fills with the history
    let s = draw(V::Compact(0), &st, 100, 30);
    assert!(s.contains("━━ cpu") || s.contains("now"), "{s}");
    let s = draw(V::Compact(1), &st, 80, 18);
    assert!(s.contains("RTX 4090") && s.contains("VRAM"), "{s}");
    let s = draw(V::Compact(2), &st, 80, 18);
    assert!(s.contains("COMMAND") && s.contains("python"), "{s}");
    let s = draw(V::Compact(4), &no_gpu(), 80, 18);
    assert!(s.contains("no nvidia-smi"), "{s}");
}

#[test]
fn monitor_agents_from_processes() {
    let st = rich();
    let ag = agents_running(&st);
    let claude = ag.iter().find(|a| a.id == "claude").unwrap();
    assert!(claude.n >= 6, "{ag:?}"); // 6 claude procs in the table beat claude_procs = 3
    let codex = ag.iter().find(|a| a.id == "codex").unwrap();
    // `codex` processes + `node …/codex.js`
    assert_eq!(codex.n, 12, "{ag:?}");
    let st = Stats {
        claude_procs: 2,
        claude_rss: 5e8,
        ..Stats::default()
    };
    let ag = agents_running(&st);
    assert_eq!(ag.len(), 1);
    assert_eq!((ag[0].n, ag[0].rss), (2, 5e8));
}

#[test]
fn monitor_helpers() {
    assert_eq!(ptime(3.456), "0:03.46");
    assert_eq!(ptime(125.0), "2:05.00");
    assert_eq!(ptime(3.0 * 3600.0 + 5.0 * 60.0 + 12.0), "3h05:12");
    assert_eq!(ptime(f64::NAN), "0:00.00");
    assert_eq!(pct(f64::NAN), "0%");
    assert_eq!(pct(3.4), "340%");
    assert_eq!(pct1(0.345), "34.5%");
    assert_eq!(by(f64::INFINITY), "0B");
    assert_eq!(age(45), "45s");
    assert_eq!(age(3 * 3600 + 120), "3h02m");
    assert_eq!(age(3 * 86400 + 4 * 3600), "3d4h");
    // meters: exactly `w` cells, a non-zero value shows
    for w in 0..20 {
        for v in [0.0, 0.01, 0.5, 1.0, 7.0, f64::NAN] {
            assert_eq!(spans_w(&gbar(v, w)), w);
        }
    }
    let one: String = gbar(0.01, 10).iter().map(|s| s.content.as_ref()).collect();
    assert!(one.starts_with('■'));
    // the core grid: aligned cells, the fewest columns that fit the height
    let cores = vec![0.5; 24];
    let (lines, gw) = core_grid(&cores, 80, 12).unwrap();
    assert_eq!(lines.len(), 12);
    assert!(lines.iter().all(|l| ui::line_width(l) == gw), "aligned");
    let (lines, _) = core_grid(&cores, 120, 6).unwrap();
    assert_eq!(lines.len(), 6);
    assert!(core_grid(&cores, 10, 1).is_none());
    assert!(core_grid(&[], 80, 12).is_none());
    // many cores still get a grid beside the chart: q7x's 64 threads in the
    // Overview's right 45% are compact meters (no numbers), 128 a heat grid
    let text = |l: &[Line]| -> String {
        l.iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let many = vec![0.3; 64];
    let (lines, gw) = core_grid(&many, 52, 14).unwrap();
    assert!(
        lines.len() <= 14 && gw <= 52,
        "{} rows, {gw} wide",
        lines.len()
    );
    assert!(!text(&lines).contains('%') && text(&lines).contains("63 "));
    let lots = vec![0.3; 128];
    let (lines, gw) = core_grid(&lots, 30, 9).unwrap();
    assert_eq!(lines.len(), 8, "{}", text(&lines));
    assert!(gw <= 30 && text(&lines).starts_with("  0 ") && text(&lines).contains("112 "));
    assert_eq!(ui::line_width(&lines[0]), gw);
    // the names in headers
    assert_eq!(
        short_gpu("RTX PRO 6000 Blackwell Max-Q Workstation Edition"),
        "RTX PRO 6000 Blackwell Max-Q"
    );
    assert_eq!(short_gpu("NVIDIA GeForce RTX 4090"), "RTX 4090");
    let st = Stats {
        cpu_model: "Intel Xeon Gold 6544Y".into(),
        sockets: 2,
        ..Stats::default()
    };
    assert_eq!(cpu_title(&st), "2× Intel Xeon Gold 6544Y");
    for n in [0, 1, 7, 8, 9, 24, 64, 129] {
        for w in [0, 1, 5, 8, 9, 10, 17, 18, 36, 80] {
            let cores = vec![0.5; n];
            let lines = strip_lines(&cores, w);
            assert_eq!(strip_rows(n, w), lines.len(), "{n} cores in {w}");
            assert!(lines.iter().all(|l| ui::line_width(l) <= w), "{n} in {w}");
        }
    }
    // stacked parts never overflow, whatever the fractions
    let parts = [
        Part {
            frac: 0.7,
            color: Color::Red,
            glyph: "█",
            name: "a",
        },
        Part {
            frac: 0.7,
            color: Color::Red,
            glyph: "▓",
            name: "b",
        },
    ];
    for w in 0..30 {
        assert_eq!(spans_w(&stack(&parts, w)), w);
    }
    let (txt, nl) = cmd_text(&proc_(0));
    assert_eq!(
        txt,
        "python /usr/bin/python3 train.py --epochs 0 --batch 64"
    );
    assert_eq!(nl, 6);
    let mut p = proc_(5);
    p.name = "rustc".into();
    p.cmd = "/usr/local/bin/rustc --serve".into();
    assert_eq!(cmd_text(&p).0, "rustc --serve");
    // kernel threads: just the name
    p.name = "kworker/3:1".into();
    p.cmd = "[kworker/3:1]".into();
    assert_eq!(cmd_text(&p).0, "kworker/3:1");
    // long arguments are shortened, the name kept
    p.name = "chrome".into();
    p.cmd = format!(
        "/opt/google/chrome/chrome --type=renderer --enable-features={} /very/long/path/to/some/profile/directory/x",
        "A".repeat(80)
    );
    let (short, nl) = ptable::short_cmd(&p);
    assert_eq!(nl, 6);
    assert!(
        short.starts_with("chrome --type=renderer --enable-features=AAAAAAAAAA…"),
        "{short}"
    );
    assert!(short.contains("…"), "{short}");
    let a = vec![1.0, 2.0];
    let b = vec![10.0, 20.0, 30.0];
    assert_eq!(
        sum_hists([a.as_slice(), b.as_slice()].into_iter()),
        vec![10.0, 21.0, 32.0]
    );
    assert_eq!(chart::pct_top(0.03), 0.1);
    assert_eq!(chart::pct_top(0.3), 0.5);
    assert_eq!(chart::pct_top(0.99), 1.0);
}

/// No view leaves big empty holes: with data, at most a few blank rows
/// inside the frame (the user's “giant empty areas”). Views whose charts
/// are lines (memory, GPU) are left out: a flat line leaves its chart's
/// rows blank, which isn't wasted layout.
#[test]
fn monitor_no_big_empty_areas() {
    let st = rich();
    for (w, h) in [(200u16, 51u16), (120, 31), (80, 19)] {
        for v in [
            V::Overview,
            V::Focus(Focus::Cpu, true),
            V::Focus(Focus::All, false),
            V::Focus(Focus::Procs, true),
            V::Cpu,
            V::Net,
            V::Procs,
        ] {
            let s = draw(v, &st, w, h);
            // a row is empty when it holds nothing but side borders and
            // spaces (a chart's rows have its y-axis line: an odd `│` count)
            let empty = s
                .lines()
                .filter(|l| {
                    l.chars().all(|c| matches!(c, ' ' | '│')) && l.matches('│').count() % 2 == 0
                })
                .count();
            assert!(
                empty * 100 <= h as usize * 12,
                "{v:?} at {w}×{h}: {empty} empty rows\n{s}"
            );
        }
    }
}

/// Visual QA: `cargo test -- --ignored --nocapture monitor_visual`.
#[test]
#[ignore]
fn monitor_visual() {
    let mut st = rich();
    // MONITOR_CORES=64: a big dual-socket server (q7x), with its long GPU name
    if let Some(n) = std::env::var("MONITOR_CORES")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        st.cores = (0..n).map(|i| ((i * 37 % 100) as f64) / 300.0).collect();
        st.ncpu = n;
        st.sockets = 2;
        st.cpu_model = "Intel Xeon Gold 6544Y".into();
        for g in &mut st.gpus {
            g.name = "RTX PRO 6000 Blackwell Max-Q Workstation Edition".into();
        }
    }
    let only = std::env::var("MONITOR_VIEW").unwrap_or_default();
    for (w, h) in [(200u16, 51u16), (120, 31), (80, 19)] {
        for v in [
            V::Overview,
            V::Focus(Focus::Cpu, true),
            V::Focus(Focus::Procs, true),
            V::Gpu,
            V::Cpu,
            V::Procs,
            V::Memory,
            V::Net,
            V::Fleet,
            V::Compact(0),
            V::Compact(1),
            V::Strip,
            V::Header,
        ] {
            let name = format!("{v:?}");
            if !only.is_empty() && !name.to_lowercase().starts_with(&only.to_lowercase()) {
                continue;
            }
            let (ww, hh) = match v {
                V::Compact(_) => (w.min(110), 24),
                V::Strip | V::Header => (w, 1),
                _ => (w, h),
            };
            println!("──── {name} {ww}×{hh} ────");
            print!("{}", draw(v, &st, ww, hh));
        }
    }
    if matches!(only.as_str(), "weird" | "bare" | "many") {
        let st = match only.as_str() {
            "weird" => weird(),
            "bare" => bare(),
            _ => many_gpus(),
        };
        for v in ALL_V {
            println!("──── {v:?} {only} 160×48 ────");
            print!("{}", draw(v, &st, 160, 48));
        }
    }
}

/// NO_COLOR: meaning must survive without colour (shade glyphs per stacked
/// segment, textures per chart series, reverse-video selection). The palette
/// is a process-wide OnceLock, so the check runs in a child process of this
/// test binary (`MONITOR_PRINT=1` there also prints the renders).
#[test]
fn monitor_no_color() {
    let exe = std::env::current_exe().unwrap();
    let out = std::process::Command::new(exe)
        .args([
            "--exact",
            "tui::monitor_draw::tests::monitor_no_color_child",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("NO_COLOR", "1")
        .env("MONITOR_NO_COLOR_CHILD", "1")
        .output()
        .unwrap();
    let so = String::from_utf8_lossy(&out.stdout);
    let se = String::from_utf8_lossy(&out.stderr);
    if std::env::var_os("MONITOR_PRINT").is_some() {
        print!("{so}");
    }
    assert!(
        out.status.success() && so.contains("1 passed"),
        "{so}\n{se}"
    );
}

#[test]
#[ignore]
fn monitor_no_color_child() {
    if std::env::var_os("MONITOR_NO_COLOR_CHILD").is_none() {
        return; // only meaningful in the child monitor_no_color starts
    }
    ui::c::init("dark", None);
    assert!(ui::c::mono(), "NO_COLOR=1 must select the mono palette");
    let st = rich();
    let ov = draw(V::Overview, &st, 160, 48);
    // the CPU split and memory breakdown keep distinct glyphs
    assert!(ov.contains('▓') && ov.contains('▒'), "{ov}");
    // legends name each series by texture
    let g = draw(V::Gpu, &st, 160, 48);
    assert!(g.contains("╍╍") || g.contains("┅┅"), "{g}");
    let mut t = term(100, 14);
    let mut ui = ProcUi {
        sel: 1,
        tree: false,
        ..Default::default()
    };
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    let b = t.backend().buffer();
    let reversed = (0..b.area.height).any(|y| {
        (0..b.area.width).any(|x| {
            b[(x, y)]
                .modifier
                .contains(ratatui::style::Modifier::REVERSED)
        })
    });
    assert!(reversed, "selection is reverse video under NO_COLOR");
    // palette colours are all the terminal default
    assert_eq!(ui::heat(0.9), Color::Reset);
    if std::env::var_os("MONITOR_PRINT").is_some() {
        for v in [V::Overview, V::Cpu, V::Memory, V::Gpu, V::Net] {
            println!("──── {v:?} NO_COLOR 160×48 ────");
            print!("{}", draw(v, &st, 160, 48));
        }
    }
}

/// `ui_scale` comfortable / large: roomier panels (more padding) must still
/// fit every size and every `Stats`. The scale is process-wide, so this runs
/// in a child process of this test binary, once per scale.
#[test]
fn monitor_scaled() {
    let exe = std::env::current_exe().unwrap();
    for scale in ["1", "2"] {
        let out = std::process::Command::new(&exe)
            .args([
                "--exact",
                "tui::monitor_draw::tests::monitor_scaled_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("MONITOR_SCALE_CHILD", scale)
            .output()
            .unwrap();
        let so = String::from_utf8_lossy(&out.stdout);
        let se = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() && so.contains("1 passed"),
            "scale {scale}:\n{so}\n{se}"
        );
    }
}

#[test]
#[ignore]
fn monitor_scaled_child() {
    let Ok(scale) = std::env::var("MONITOR_SCALE_CHILD") else {
        return; // only meaningful in the child monitor_scaled starts
    };
    ui::set_scale(&scale);
    assert!(ui::scale() >= 1);
    let all = [rich(), weird(), bare(), many_gpus(), no_gpu()];
    for st in &all {
        for &(w, h) in SIZES {
            for v in ALL_V {
                let r = std::panic::catch_unwind(|| draw(v, st, w, h));
                assert!(r.is_ok(), "{v:?} panicked at {w}×{h}, scale {scale}");
            }
        }
    }
    // the content is all still there at the usual sizes
    let st = rich();
    let s = draw(V::Overview, &st, 200, 51);
    for want in [
        "CPU",
        "GPUs",
        "Memory",
        "Network",
        "Disks",
        "Processes",
        "python",
    ] {
        assert!(s.contains(want), "missing {want:?}:\n{s}");
    }
    let mut ui = ProcUi::default();
    let mut t = term(120, 31);
    t.draw(|f| {
        procs(f, f.area(), &st, &mut ui);
    })
    .unwrap();
    assert!(text(&t).contains("COMMAND"));
}

/// Frame cost (the dashboard redraws every ~60 ms):
/// `cargo test --release monitor_perf -- --ignored --nocapture`.
#[test]
#[ignore]
fn monitor_perf() {
    let st = many_gpus();
    let rich = rich();
    let rows: Vec<MachineRow> = (0..12)
        .map(|_| MachineRow {
            alias: "box",
            color: Color::Red,
            stats: Some(&st),
            up: true,
            note: "",
        })
        .collect();
    for v in ALL_V {
        let mut t = term(220, 60);
        let t0 = Instant::now();
        let n = 50;
        for _ in 0..n {
            t.draw(|f| {
                let r = f.area();
                match v {
                    V::Fleet => fleet(f, r, &rows, 3, t0),
                    V::Overview => overview(f, r, "b31", Color::Red, &rich, t0),
                    V::Focus(fo, on) => overview_body(f, r, &rich, t0, fo, on),
                    V::Cpu => cpu(f, r, &rich),
                    V::Memory => memory(f, r, &rich),
                    V::Gpu => gpu(f, r, &st),
                    V::Procs | V::ProcsFlat => {
                        let mut ui = ProcUi {
                            tree: matches!(v, V::Procs),
                            filter: "py".into(),
                            sort: Sort::Mem,
                            ..Default::default()
                        };
                        procs(f, r, &rich, &mut ui);
                    }
                    V::Net => net(f, r, &rich),
                    V::Compact(m) => compact(f, r, "b31", Color::Red, &rich, m, t0),
                    V::Strip => {
                        strip(f, r, Sub::Cpu, "b31", Color::Red, None, "30m", &Sub::ALL);
                    }
                    V::Header => header(f, r, "b31", Color::Red, &rich),
                }
            })
            .unwrap();
        }
        let ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
        println!("{v:?}: {ms:.2} ms/frame at 220×60");
    }
}

#[test]
fn a_gpu_process_names_the_session_it_came_from() {
    let mut st = rich();
    st.gpu_procs[0].owner = "claude_ignite".into();
    let mut t = term(160, 44);
    t.draw(|f| gpu(f, f.area(), &st)).unwrap();
    let s = text(&t);
    assert!(s.contains("◆ claude_ignite python"), "{s}");
}
