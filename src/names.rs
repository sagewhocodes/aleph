//! Names for new sessions.
//!
//! A name is a short verb (`ignite`), used as herdr's agent name and in the tab
//! label `<agent>_ignite[:folder]` (`claude_ignite:px`, `codex_ignite`, or
//! `tab_prefix`). Picking is deliberate rather than random:
//!
//! * never a name running on any machine in the fleet, and not one used in
//!   the last week (history), so an old transcript never looks like a new session;
//! * prefer a first letter no live session has — the palette and `'`-jump can
//!   then reach it with one key;
//! * avoid look-alikes of live names (edit distance ≤ 2, or a shared 4-letter
//!   prefix), so `forge` and `forget` never run side by side;
//! * shorter wins ties; the rest is seeded randomness.
//!
//! When every verb is taken it composes `swift-forge`, then `forge-2`, … — names
//! never run out. `naming=folder` names sessions after their folder instead.
//! All names match herdr's agent-name rule `[a-z][a-z0-9_-]{0,31}`.
//!
//! **Story names** (`naming=story`, the default) say what, who and where:
//! `<mood>-<creature>`, e.g. `smoldering-heron`. The creature is the agent's
//! — Claude's sessions are birds, Codex's are crafts, Gemini's stars,
//! OpenCode's sea creatures, a shell's harbour words — and the mood is a
//! playful verb (in the spirit of Claude Code's spinner) from the machine's
//! set, which follows its colour in the UI (the peach machine's sessions are
//! `smoldering-`, `sizzling-`, `toasting-`…; the blue one's `drifting-`,
//! `bobbing-`…). Uniqueness, fresh initials and look-alike avoidance work as
//! above.

pub const VERBS: &[&str] = &[
    "align", "anchor", "bake", "bend", "bloom", "brew", "bridge", "build", "carve", "chant",
    "chart", "chisel", "churn", "cipher", "coax", "comb", "conjure", "craft", "decode", "delve",
    "dig", "distil", "doodle", "draft", "dream", "drum", "echo", "etch", "fathom", "fizz", "fold",
    "forge", "gather", "glean", "graft", "hammer", "harvest", "hatch", "herd", "hone", "hum",
    "hunt", "ignite", "index", "jam", "jolt", "juggle", "kindle", "knead", "knit", "lasso",
    "launch", "map", "mend", "mesh", "mine", "mold", "mull", "muse", "nest", "noodle", "nudge",
    "orbit", "parse", "pilot", "pivot", "plant", "plot", "polish", "ponder", "probe", "prune",
    "quest", "rally", "refine", "relay", "riff", "ripple", "roam", "sail", "scan", "scout",
    "scribe", "sculpt", "seek", "shape", "shift", "sift", "signal", "simmer", "sketch", "smelt",
    "solve", "sort", "spark", "spin", "splice", "sprint", "sprout", "stack", "steer", "stew",
    "stitch", "summon", "surge", "sweep", "tally", "temper", "thread", "tinker", "toast", "trace",
    "tune", "twist", "unfold", "vault", "vector", "wade", "wander", "weave", "whisk", "whittle",
    "wield", "wrangle", "yield", "zest", "zoom",
];

const ADJECTIVES: &[&str] = &[
    "swift", "quiet", "amber", "brisk", "calm", "deft", "eager", "keen", "lucid", "merry",
    "nimble", "plucky", "rapid", "sunny", "tidy", "vivid", "witty", "bold", "bright", "cosmic",
    "gentle", "jolly", "mellow", "noble", "steady", "lunar", "polar", "velvet",
];

/// Moods: the first word of a story name — a playful verb in the spirit of
/// Claude Code's spinner (`Percolating`, `Noodling`, `Moseying`) and
/// ChatGPT's (`Pondering`, `Mulling`) — one set per machine colour (the
/// `MACHINES` palette names in `theme.rs`), so a name echoes its machine:
/// sea for blue, embers for peach, night for mauve, and so on.
pub const LANDS: &[(&str, &[&str])] = &[
    (
        "blue",
        &[
            "drifting",
            "bobbing",
            "paddling",
            "sloshing",
            "surfing",
            "wading",
            "rippling",
            "snorkeling",
        ],
    ),
    (
        "mauve",
        &[
            "stargazing",
            "daydreaming",
            "pondering",
            "moonwalking",
            "musing",
            "humming",
            "lulling",
            "mulling",
        ],
    ),
    (
        "teal",
        &[
            "chilling",
            "skating",
            "frosting",
            "gliding",
            "shivering",
            "sledding",
            "glistening",
            "snowballing",
        ],
    ),
    (
        "peach",
        &[
            "smoldering",
            "sizzling",
            "toasting",
            "flickering",
            "simmering",
            "kindling",
            "searing",
            "glowing",
        ],
    ),
    (
        "pink",
        &[
            "blossoming",
            "twirling",
            "fluttering",
            "giggling",
            "frolicking",
            "pirouetting",
            "swooning",
            "skipping",
        ],
    ),
    (
        "green",
        &[
            "sprouting",
            "rustling",
            "foraging",
            "noodling",
            "pottering",
            "tending",
            "whittling",
            "mossing",
        ],
    ),
    (
        "yellow",
        &[
            "moseying",
            "basking",
            "meandering",
            "sauntering",
            "percolating",
            "dawdling",
            "ambling",
            "lazing",
        ],
    ),
    (
        "sky",
        &[
            "soaring",
            "breezing",
            "hovering",
            "billowing",
            "whooshing",
            "kiting",
            "swooping",
            "puffing",
        ],
    ),
    (
        "lavender",
        &[
            "conjuring",
            "shimmering",
            "twinkling",
            "beaming",
            "glimmering",
            "spellcasting",
            "wishing",
            "sparkling",
        ],
    ),
    (
        "flamingo",
        &[
            "spelunking",
            "rambling",
            "trekking",
            "echoing",
            "tumbling",
            "scrambling",
            "yodeling",
            "clambering",
        ],
    ),
];

/// Creatures: the second word, by agent family (the agent ids it covers).
pub const CREW: &[(&[&str], &[&str])] = &[
    (
        &["claude"],
        &[
            "heron", "kestrel", "wren", "finch", "owl", "raven", "lark", "egret", "ibis", "osprey",
            "plover", "robin", "tern", "crane", "falcon", "magpie",
        ],
    ),
    (
        &["codex"],
        &[
            "anvil", "lathe", "loom", "quill", "kiln", "chisel", "sextant", "bellows", "spindle",
            "awl", "gimbal", "rivet", "trowel", "plumb",
        ],
    ),
    (
        &["gemini", "agy"],
        &[
            "vega", "lyra", "orion", "rigel", "nova", "sirius", "altair", "deneb", "castor",
            "pollux", "spica", "antares",
        ],
    ),
    (
        &["opencode", "crush"],
        &[
            "orca", "manta", "otter", "squid", "marlin", "nautilus", "urchin", "seal", "narwhal",
            "octopus", "dolphin", "puffin",
        ],
    ),
    (
        &["amp", "droid", "grok"],
        &[
            "volt", "ion", "flux", "arc", "pulse", "quark", "photon", "dynamo", "tesla", "joule",
        ],
    ),
    (
        &[
            "cursor",
            crate::agentsdk::ID,
            "copilot",
            "cline",
            "kilo",
            "kiro",
            "devin",
        ],
        &[
            "trail", "ridge", "pass", "fjord", "delta", "atlas", "beacon", "summit", "bluff",
            "gorge",
        ],
    ),
    (
        &["pi", "omp"],
        &[
            "euler", "gauss", "fermat", "pascal", "kepler", "noether", "turing", "lovelace",
            "riemann", "laplace", "galois", "cantor",
        ],
    ),
    (
        &["kimi", "qwen", "qodercli"],
        &[
            "lotus", "bamboo", "peony", "jasmine", "ginkgo", "orchid", "camellia", "magnolia",
            "wisteria", "hibiscus",
        ],
    ),
    (
        &["hermes", "letta"],
        &[
            "phoenix", "griffin", "pegasus", "sphinx", "hydra", "kraken", "siren", "chimera",
            "wyvern", "basilisk",
        ],
    ),
    (
        &["mastracode", "commandcode", "prime", "maki", "muse"],
        &[
            "opal", "jade", "onyx", "topaz", "garnet", "beryl", "agate", "quartz", "cobalt",
            "jasper", "zircon", "pyrite",
        ],
    ),
    (
        &["shell"],
        &[
            "dock", "pier", "port", "quay", "jetty", "wharf", "buoy", "mooring",
        ],
    ),
];

/// Every other agent's creatures.
const CRITTERS: &[&str] = &[
    "lynx", "badger", "ibex", "yak", "panda", "koala", "fox", "hare", "bison", "gecko", "newt",
    "tapir", "okapi", "lemur", "marmot", "wombat",
];

/// The land words for a machine: by its colour name (`host.<a>.color`),
/// else by its alias.
pub fn land_words(color: Option<&str>, alias: &str) -> &'static [&'static str] {
    if let Some(c) = color {
        if let Some((_, w)) = LANDS.iter().find(|(k, _)| *k == c) {
            return w;
        }
    }
    LANDS[(crate::theme::hash(alias) % LANDS.len() as u64) as usize].1
}

/// The creatures of an agent.
pub fn creatures(agent: &str) -> &'static [&'static str] {
    CREW.iter()
        .find(|(ids, _)| ids.contains(&agent))
        .map_or(CRITTERS, |(_, w)| w)
}

/// A story name for `agent` on a machine whose land is `land`: fresh across
/// the fleet, a creature no live session has where possible (and a fresh
/// initial), not one used lately, never a look-alike of a live name.
pub fn story(agent: &str, land: &[&str], live: &[String], recent: &[String], seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let crew = creatures(agent);
    let creature_of = |n: &str| n.rsplit('-').next().unwrap_or(n).to_string();
    let live_creatures: Vec<String> = live.iter().map(|n| creature_of(n)).collect();
    let initials: Vec<char> = live.iter().filter_map(|n| n.chars().next()).collect();
    let mut best: Vec<(i64, String)> = Vec::new();
    for a in land {
        for c in crew {
            let n = format!("{a}-{c}");
            if live.contains(&n) || !valid(&n) {
                continue;
            }
            let mut s = 0i64;
            if !live_creatures.iter().any(|x| x == c) {
                s += 40;
            }
            if !initials.contains(&a.chars().next().unwrap_or(' ')) {
                s += 20;
            }
            if recent.contains(&n) {
                s -= 60;
            }
            if live.iter().any(|l| confusable(l, &n)) {
                s -= 50;
            }
            s -= n.len() as i64 / 3;
            best.push((s, n));
        }
    }
    if let Some(top) = best.iter().map(|x| x.0).max() {
        let pool: Vec<&String> = best
            .iter()
            .filter(|x| x.0 >= top - 4)
            .map(|x| &x.1)
            .collect();
        return pool[(rng.next() % pool.len() as u64) as usize].clone();
    }
    // every pairing is running: number the first
    let base = format!(
        "{}-{}",
        land.first().copied().unwrap_or("swift"),
        crew.first().copied().unwrap_or("fox")
    );
    numbered(&base, live)
}

/// Is `n` one of Aleph's own story names (`frosting-crane`, `forge`,
/// `swift-forge-2`) rather than one someone typed? (Showcase mode keeps
/// these: they say nothing about anyone.)
#[allow(dead_code)]
pub fn generated(n: &str) -> bool {
    let n = n.trim();
    // a numbered one: `name-2`
    let n = match n.rsplit_once('-') {
        Some((b, d)) if !d.is_empty() && d.bytes().all(|c| c.is_ascii_digit()) => b,
        _ => n,
    };
    let first = |w: &str| ADJECTIVES.contains(&w) || LANDS.iter().any(|(_, l)| l.contains(&w));
    let second = |w: &str| {
        VERBS.contains(&w) || CRITTERS.contains(&w) || CREW.iter().any(|(_, c)| c.contains(&w))
    };
    match n.split('-').collect::<Vec<_>>().as_slice() {
        [v] => VERBS.contains(v),
        [a, b] => first(a) && second(b),
        _ => false,
    }
}

/// Is `n` a valid herdr agent name?
pub fn valid(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 32
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// Lowercase slug usable as a name (`AuroraMix_3.1` → `auroramix-3-1`).
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        let ch = ch.to_ascii_lowercase();
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let mut out = out.trim_matches('-').to_string();
    if out.is_empty() || !out.as_bytes()[0].is_ascii_lowercase() {
        out = format!("s-{out}");
    }
    out.truncate(28);
    out.trim_end_matches('-').to_string()
}

fn lev(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != cb))
                .min(prev[j + 1] + 1)
                .min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

fn confusable(a: &str, b: &str) -> bool {
    a == b
        || lev(a, b) <= 2.min(a.len().min(b.len()) / 2)
        || (a.len() >= 4 && b.len() >= 4 && a[..4] == b[..4])
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Pick a fresh name.
/// * `live` — names of sessions running anywhere in the fleet (verbs, no prefix)
/// * `recent` — names used recently (avoided while alternatives exist)
pub fn pick(live: &[String], recent: &[String], seed: u64) -> String {
    let mut rng = Rng(seed | 1);
    let initials: Vec<char> = live.iter().filter_map(|n| n.chars().next()).collect();
    let score = |v: &str| -> i64 {
        let mut s = 0i64;
        if !initials.contains(&v.chars().next().unwrap_or(' ')) {
            s += 30;
        }
        if live.iter().any(|l| confusable(l, v)) {
            s -= 50;
        }
        if recent.iter().any(|r| r == v) {
            s -= 40;
        }
        s - v.len() as i64
    };
    let free: Vec<&str> = VERBS
        .iter()
        .copied()
        .filter(|v| !live.iter().any(|l| l == v))
        .collect();
    if !free.is_empty() {
        let best = free.iter().map(|v| score(v)).max().unwrap_or(0);
        // anything within 3 points of the best is fair game — keeps variety
        let pool: Vec<&str> = free.into_iter().filter(|v| score(v) >= best - 3).collect();
        return pool[(rng.next() % pool.len() as u64) as usize].to_string();
    }
    // every verb is running somewhere: compose adjective-verb
    for _ in 0..400 {
        let a = ADJECTIVES[(rng.next() % ADJECTIVES.len() as u64) as usize];
        let v = VERBS[(rng.next() % VERBS.len() as u64) as usize];
        let n = format!("{a}-{v}");
        if !live.contains(&n) {
            return n;
        }
    }
    numbered("forge", live)
}

/// `base`, or `base-2`, `base-3`, … whichever is free.
pub fn numbered(base: &str, live: &[String]) -> String {
    if !live.iter().any(|l| l == base) {
        return base.to_string();
    }
    (2..)
        .map(|k| format!("{base}-{k}"))
        .find(|n| !live.iter().any(|l| l == n))
        .unwrap_or_default()
}

/// Name a session after its folder (`naming=folder`).
pub fn from_folder(folder: &str, live: &[String]) -> String {
    numbered(&slug(folder), live)
}

pub fn seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^ ((std::process::id() as u64) << 32)
}

/// The name part of a tab label: `claude_ignite:px` / `codex_ignite` → `ignite`.
pub fn from_label(label: &str) -> String {
    let base = label.split(':').next().unwrap_or(label);
    let base = base.split('+').next().unwrap_or(base);
    match base.split_once('_') {
        Some((_, rest)) if !rest.is_empty() => rest.to_string(),
        _ => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbs_are_valid_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for v in VERBS {
            assert!(valid(v) && v.len() <= 7, "{v}");
            assert!(seen.insert(*v), "duplicate verb {v}");
        }
        assert!(VERBS.len() >= 120);
    }

    #[test]
    fn every_agent_has_its_family() {
        let mut seen = std::collections::HashSet::new();
        for (ids, words) in CREW {
            for w in *words {
                assert!(valid(w) && w.len() <= 12, "{w}");
                assert!(seen.insert(*w), "{w} is in two families");
            }
            for id in *ids {
                assert!(
                    *id == "shell" || crate::agents::find(id).is_some(),
                    "{id} isn't an agent"
                );
            }
        }
        for a in crate::agents::ALL {
            assert!(
                CREW.iter().any(|(ids, _)| ids.contains(&a.id)),
                "{} has no family (CRITTERS)",
                a.id
            );
        }
    }

    #[test]
    fn never_collides_and_scales() {
        let mut live: Vec<String> = vec!["forge".into()];
        for i in 0..400 {
            let n = pick(&live, &[], 0x9e3779b97f4a7c15 ^ i);
            assert!(valid(&n), "{n}");
            assert!(!live.contains(&n), "dup {n}");
            live.push(n);
        }
        assert!(live.iter().any(|n| n.contains('-')));
    }

    #[test]
    fn prefers_fresh_initials_and_avoids_lookalikes() {
        let live: Vec<String> = ["forge", "probe", "scout"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for s in 0..50 {
            let n = pick(&live, &[], s * 31 + 7);
            let ch = n.chars().next().unwrap();
            assert!(!"fps".contains(ch), "{n} reuses a live initial");
            assert!(!live.iter().any(|l| confusable(l, &n)));
        }
        let recent = vec!["ignite".to_string()];
        assert!((0..50).all(|s| pick(&[], &recent, s) != "ignite"));
    }

    #[test]
    fn story_names_say_agent_and_machine() {
        let land = land_words(Some("peach"), "b31");
        assert!(land.contains(&"smoldering"));
        let mut live: Vec<String> = Vec::new();
        for i in 0..30 {
            let n = story("claude", land, &live, &[], 0x51 ^ i);
            assert!(valid(&n) && n.len() <= 32, "{n}");
            assert!(!live.contains(&n), "dup {n}");
            let (l, c) = n.split_once('-').unwrap();
            assert!(land.contains(&l), "{n}: land");
            assert!(creatures("claude").contains(&c), "{n}: creature");
            live.push(n);
        }
        // a codex session on the same machine is a craft
        let n = story("codex", land, &live, &[], 7);
        assert!(creatures("codex").contains(&n.split_once('-').unwrap().1));
        // agents outside the registry get critters; unknown colours pick a land by alias
        assert_eq!(creatures("some-new-agent"), CRITTERS);
        assert!(creatures("hermes").contains(&"phoenix"));
        assert_eq!(
            land_words(None, "gpu2"),
            land_words(Some("#123456"), "gpu2")
        );
        // fresh creatures first while they last
        let live = vec!["smoldering-heron".to_string()];
        assert!((0..40).all(|s| !story("claude", land, &live, &[], s).ends_with("-heron")));
    }

    #[test]
    fn vocabularies_are_clean() {
        let mut all = std::collections::HashSet::new();
        for (_, words) in LANDS {
            assert_eq!(words.len(), 8);
            for w in *words {
                assert!(valid(w) && w.len() <= 12 && w.ends_with("ing"), "{w}");
                assert!(all.insert(*w), "mood {w} twice");
            }
        }
        for (_, words) in CREW {
            for w in *words {
                assert!(valid(w) && w.len() <= 8, "{w}");
                assert!(all.insert(*w), "creature {w} also elsewhere");
            }
        }
        for w in CRITTERS {
            assert!(all.insert(*w), "critter {w} also elsewhere");
        }
    }

    #[test]
    fn folder_names_and_labels() {
        assert_eq!(slug("AuroraMix_3.1"), "auroramix-3-1");
        assert_eq!(slug("2026 runs"), "s-2026-runs");
        assert!(valid(&slug("√√√")));
        let live = vec!["px".to_string(), "px-2".to_string()];
        assert_eq!(from_folder("px", &live), "px-3");
        assert_eq!(from_label("claude_ignite:ox_color_check"), "ignite");
        assert_eq!(from_label("claude_ignite+2:px"), "ignite");
        assert_eq!(from_label("codex_tune:train"), "tune");
        assert_eq!(from_label("opencode_ignite"), "ignite");
        assert_eq!(from_label("shell"), "shell");
    }
}
