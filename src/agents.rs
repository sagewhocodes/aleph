//! Every coding agent herdr knows how to run, detect or restore.
//!
//! Sources: `herdr agent start --help` (the `--kind` list), `herdr integration
//! status` (the integration names) and https://herdr.dev/docs/agents/.
//!
//! * `kind` — herdr's `agent start --kind` value. herdr then starts the agent's
//!   canonical executable and waits until it's ready. Agents without a kind are
//!   typed into the pane as a plain command (`pane run`).
//! * `integration` — `herdr integration install <name>`: a hook/plugin that
//!   reports the agent's state and session, so herdr restores the same agent
//!   session after its server restarts. That is our session-persistence story;
//!   agents without one are recreated by `aleph restore`.
//! * `needs_integration` — herdr can't tell the agent's state (working, needs
//!   you, done) without the integration.
//! * `self_reports` — the agent reports its state to herdr by itself.
//! * `resume` — arguments that continue the agent's most recent conversation in
//!   the folder (used by `aleph restore`). Only filled in where we're sure.
//! * `logs` — where the agent writes one file per conversation (for "sessions
//!   today" counts). Only filled in where we're sure.
//! * `flags` / `cmds` — the agent CLI's own flags and subcommands that always go
//!   to it when the shell function shadows it (`wraps`, see [`Wraps`]).
//! * `sig` / `strict` / `not` — how a probe tells the agent from a same-named
//!   program by its `--version` output: `sig` is text only the agent prints
//!   (`(Claude Code)`, `codex-cli`), `strict` means it always does, `not` is
//!   text that means another program (Facebook's `hermes` JavaScript engine).
//! * `install` / `alt_install` — how to install it; `aleph agents install`
//!   picks the first whose prerequisites the machine has.
//!
//! Every caller goes through this module instead of special-casing an agent:
//! [`Agent::launch`], [`Agent::resume_args`], [`Agent::detect`],
//! [`Agent::runs_as`], [`Agent::logs`], [`default_agent`], [`id_of`],
//! [`Filter`] (the `agents` setting) and [`Wraps`] (the `wraps` setting).

use crate::config::Config;
use std::path::PathBuf;
use std::time::Duration;

/// One agent herdr supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Agent {
    /// our id: herdr's kind where it has one
    pub id: &'static str,
    pub name: &'static str,
    /// executable names, preferred first
    pub exes: &'static [&'static str],
    /// other names people type (`antigravity`, `cursor-agent`, …)
    pub aliases: &'static [&'static str],
    pub kind: Option<&'static str>,
    pub integration: Option<&'static str>,
    pub needs_integration: bool,
    pub self_reports: bool,
    /// herdr calls its support experimental / less tested
    pub experimental: bool,
    pub resume: &'static [&'static str],
    /// arguments that continue one conversation by its id (`{id}`: herdr's
    /// `agent_session`), from herdr's session-restore table
    pub resume_id: &'static [&'static str],
    /// how to install it
    pub install: &'static str,
    /// another way, when the first one's prerequisites are missing
    pub alt_install: &'static str,
    /// programs its installer needs that a fresh system may lack (Kiro's
    /// unzip, Hermes's git): setup puts them on first
    pub deps: &'static [&'static str],
    /// lowercase text its `--version` prints (any of them)
    pub sig: &'static [&'static str],
    /// its `--version` always prints a `sig`: output without one is another program
    pub strict: bool,
    /// lowercase text in `--version` output that means another program
    pub not: &'static [&'static str],
    /// where it keeps one log file per conversation
    pub logs: Option<Logs>,
    /// its CLI's own flags that always pass through to it (`-p`, `--print`, …)
    pub flags: &'static [&'static str],
    /// the oldest version Aleph is built and tested against (empty: any);
    /// older ones get a warning wherever their version shows
    pub min_version: &'static str,
    /// how an older one moves up to `min_version`
    pub upgrade: &'static str,
    /// its CLI's own subcommands that always pass through to it (`mcp`, …)
    pub cmds: &'static [&'static str],
}

/// Conversation logs: files matching `pattern` anywhere under `$HOME/<dir>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Logs {
    /// relative to the user's home
    pub dir: &'static str,
    /// a `find -name` pattern
    pub pattern: &'static str,
}

impl Logs {
    /// POSIX sh that prints how many logs changed in the last 24 hours, and
    /// nothing at all when the folder doesn't exist (no `'` or `!` inside, so it
    /// fits a `sh -c '…'` too).
    #[allow(dead_code)] // for the telemetry sampler's per-agent "sessions today"
    pub fn count_today_sh(&self) -> String {
        format!(
            "[ -d \"$HOME/{d}\" ] && find \"$HOME/{d}\" -name \"{p}\" -mmin -1440 2>/dev/null | wc -l | tr -d \" \"",
            d = self.dir,
            p = self.pattern
        )
    }
}

/// How herdr knows what an agent is doing (working, needs you, done).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detect {
    /// herdr reads it from the screen; the agent's integration adds session restore
    BuiltIn,
    /// herdr reads it from the screen; there's no integration (no restore by herdr)
    StateOnly,
    /// herdr only knows it once the agent's integration is installed
    NeedsIntegration,
    /// the agent reports it to herdr by itself
    SelfReports,
}

impl Detect {
    /// Stable name for JSON output.
    pub fn as_str(self) -> &'static str {
        match self {
            Detect::BuiltIn => "builtin",
            Detect::StateOnly => "state_only",
            Detect::NeedsIntegration => "needs_integration",
            Detect::SelfReports => "self_reports",
        }
    }
}

/// How an agent starts in a fresh pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Launch {
    /// `herdr agent start --kind <kind>`: herdr runs its canonical executable
    /// and returns once it's ready
    Kind(&'static str),
    /// typed into the pane's shell (`herdr pane run`)
    Command(&'static str),
}

const fn a(id: &'static str, name: &'static str) -> Agent {
    Agent {
        id,
        name,
        exes: &[],
        aliases: &[],
        kind: None,
        integration: None,
        needs_integration: false,
        self_reports: false,
        experimental: false,
        resume: &[],
        resume_id: &[],
        install: "",
        alt_install: "",
        deps: &[],
        sig: &[],
        strict: false,
        not: &[],
        logs: None,
        flags: &[],
        cmds: &[],
        min_version: "",
        upgrade: "",
    }
}

/// The registry, in the order agents are listed.
pub const ALL: &[Agent] = &[
    Agent {
        exes: &["claude"],
        aliases: &["claude-code", "cc"],
        kind: Some("claude"),
        integration: Some("claude"),
        resume: &["--continue"],
        resume_id: &["--resume", "{id}"],
        install: "curl -fsSL https://claude.ai/install.sh | bash",
        // `2.1.306 (Claude Code)`
        sig: &["(claude code)"],
        strict: true,
        logs: Some(Logs {
            dir: ".claude/projects",
            pattern: "*.jsonl",
        }),
        flags: &["-p", "--print", "-c", "--continue", "-r"],
        // (its own `update`, `agents` and `doctor` are shadowed by ours: `command claude update`)
        cmds: &[
            "mcp",
            "config",
            "install",
            "setup-token",
            "migrate-installer",
            "plugin",
            "plugins",
        ],
        ..a("claude", "Claude Code")
    },
    Agent {
        exes: &["codex"],
        kind: Some("codex"),
        integration: Some("codex"),
        resume: &["resume", "--last"],
        resume_id: &["resume", "{id}"],
        install: "npm i -g @openai/codex",
        // `codex-cli 0.149.1`
        sig: &["codex-cli", "codex"],
        strict: true,
        logs: Some(Logs {
            dir: ".codex/sessions",
            pattern: "*.jsonl",
        }),
        ..a("codex", "Codex")
    },
    Agent {
        exes: &["opencode"],
        aliases: &["open-code"],
        kind: Some("opencode"),
        integration: Some("opencode"),
        resume: &["--continue"],
        resume_id: &["--session", "{id}"],
        // OpenCode 2 (its own installer and npm package): sessions in new tables,
        // which is what Aleph reads; the 1.x installer refuses 2.x
        install: "curl -fsSL https://opencode.ai/v2/install | bash",
        alt_install: "npm i -g @opencode/cli",
        min_version: "2.0",
        upgrade: "opencode upgrade --major",
        ..a("opencode", "OpenCode")
    },
    Agent {
        exes: &["omp"],
        aliases: &["oh-my-pi"],
        kind: Some("omp"),
        integration: Some("omp"),
        needs_integration: true,
        install: "curl -fsSL https://omp.sh/install | sh",
        alt_install: "brew install can1357/tap/omp",
        ..a("omp", "OMP (oh-my-pi)")
    },
    Agent {
        exes: &["pi"],
        kind: Some("pi"),
        integration: Some("pi"),
        resume: &["--continue"],
        resume_id: &["--session", "{id}"],
        // the package moved from @mariozechner to @earendil-works
        install: "npm i -g @earendil-works/pi-coding-agent",
        // the CLN library ships an unrelated `pi` (digits of π)
        not: &["cln", "digits"],
        ..a("pi", "Pi")
    },
    Agent {
        // herdr's canonical executable for kind `cursor` is `cursor-agent`
        exes: &["cursor-agent"],
        aliases: &["cursor-agent", "cursor-cli"],
        kind: Some("cursor"),
        integration: Some("cursor"),
        install: "curl https://cursor.com/install -fsS | bash",
        ..a("cursor", "Cursor CLI")
    },
    Agent {
        exes: &["devin"],
        kind: Some("devin"),
        integration: Some("devin"),
        install: "curl -fsSL https://cli.devin.ai/install.sh | bash",
        alt_install: "brew install --cask devin-cli",
        ..a("devin", "Devin CLI")
    },
    Agent {
        exes: &["gemini"],
        aliases: &["gemini-cli"],
        kind: Some("gemini"),
        experimental: true,
        install: "npm i -g @google/gemini-cli",
        ..a("gemini", "Gemini CLI")
    },
    Agent {
        exes: &["agy"],
        aliases: &["antigravity", "antigravity-cli"],
        kind: Some("agy"),
        integration: Some("antigravity-cli"),
        install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        ..a("agy", "Antigravity CLI")
    },
    Agent {
        exes: &["cline"],
        kind: Some("cline"),
        experimental: true,
        install: "npm i -g cline",
        ..a("cline", "Cline CLI")
    },
    Agent {
        exes: &["mastracode"],
        aliases: &["mastra", "mastra-code"],
        kind: Some("mastracode"),
        integration: Some("mastracode"),
        needs_integration: true,
        install: "npm i -g mastracode",
        ..a("mastracode", "Mastra Code")
    },
    Agent {
        exes: &["copilot"],
        aliases: &["github-copilot", "copilot-cli"],
        kind: Some("copilot"),
        integration: Some("copilot"),
        resume_id: &["--resume={id}"],
        install: "npm i -g @github/copilot",
        ..a("copilot", "GitHub Copilot CLI")
    },
    Agent {
        exes: &["kimi"],
        aliases: &["kimi-code", "kimi-cli"],
        kind: Some("kimi"),
        integration: Some("kimi"),
        resume_id: &["--session", "{id}"],
        install: "uv tool install kimi-cli",
        sig: &["kimi"],
        ..a("kimi", "Kimi Code")
    },
    Agent {
        // herdr's canonical executable for kind `kiro` is `kiro-cli`
        exes: &["kiro-cli"],
        aliases: &["kiro-cli"],
        kind: Some("kiro"),
        self_reports: true,
        install: "curl -fsSL https://cli.kiro.dev/install | bash",
        deps: &["unzip"],
        sig: &["kiro"],
        ..a("kiro", "Kiro CLI")
    },
    Agent {
        exes: &["droid"],
        aliases: &["factory"],
        kind: Some("droid"),
        integration: Some("droid"),
        install: "curl -fsSL https://app.factory.ai/cli | sh",
        ..a("droid", "Factory Droid")
    },
    Agent {
        exes: &["amp"],
        kind: Some("amp"),
        install: "npm i -g @sourcegraph/amp",
        ..a("amp", "Amp")
    },
    Agent {
        exes: &["grok"],
        kind: Some("grok"),
        integration: Some("grok"),
        install: "curl -fsSL https://x.ai/cli/install.sh | bash",
        alt_install: "npm i -g @xai-official/grok",
        ..a("grok", "Grok CLI")
    },
    Agent {
        exes: &["hermes"],
        kind: Some("hermes"),
        integration: Some("hermes"),
        resume_id: &["--resume", "{id}"],
        // without its browser (Chromium) and computer-use driver: hundreds of
        // MB more on top of the clone and ffmpeg (which it always fetches);
        // `hermes pm install agent-browser cua-driver` adds them later
        install: "curl -fsSL https://raw.githubusercontent.com/NousResearch/hermes-agent/main/scripts/install.sh | bash -s -- --non-interactive --skip-browser --skip-computer-use",
        deps: &["git"],
        // Facebook's Hermes JavaScript engine is also called `hermes`
        not: &["javascript", "llvm", "bytecode"],
        ..a("hermes", "Hermes Agent")
    },
    Agent {
        exes: &["kilo"],
        aliases: &["kilocode", "kilo-code"],
        kind: Some("kilo"),
        integration: Some("kilo"),
        install: "npm i -g @kilocode/cli",
        ..a("kilo", "Kilo Code CLI")
    },
    Agent {
        exes: &["qodercli"],
        aliases: &["qoder"],
        kind: Some("qodercli"),
        integration: Some("qodercli"),
        install: "curl -fsSL https://qoder.com/install | bash",
        alt_install: "npm i -g @qoder-ai/qodercli",
        ..a("qodercli", "Qoder CLI")
    },
    Agent {
        exes: &["qwen"],
        aliases: &["qwen-code"],
        kind: Some("qwen"),
        integration: Some("qwen"),
        install: "npm i -g @qwen-code/qwen-code",
        ..a("qwen", "Qwen Code")
    },
    Agent {
        exes: &["letta"],
        aliases: &["letta-code"],
        kind: Some("letta"),
        integration: Some("letta"),
        experimental: true,
        install: "npm i -g @letta-ai/letta-code",
        // node-pty builds from source where there's no prebuilt one (arm64 Linux)
        deps: &["make", "g++", "python3"],
        ..a("letta", "Letta Code")
    },
    Agent {
        exes: &["maki"],
        kind: Some("maki"),
        install: "curl -fsSL https://maki.sh/install.sh | sh",
        alt_install: "cargo install --locked --git https://github.com/tontinton/maki.git maki",
        ..a("maki", "Maki")
    },
    Agent {
        exes: &["muse"],
        aliases: &["muse-code"],
        kind: Some("muse"),
        self_reports: true,
        install: "curl -fsSL https://dev.meta.ai/install.sh | bash",
        ..a("muse", "Muse Code")
    },
    Agent {
        exes: &["crush"],
        self_reports: true,
        install: "brew install charmbracelet/tap/crush",
        alt_install: "npm i -g @charmland/crush",
        sig: &["crush"],
        ..a("crush", "Crush")
    },
    Agent {
        // executable name unverified: the npm package is `command-code`
        exes: &["commandcode", "command-code", "cmd"],
        aliases: &["command-code", "commandcode", "cmd"],
        self_reports: true,
        install: "npm i -g command-code",
        ..a("commandcode", "Command Code")
    },
    Agent {
        // npm package `prime-agent` (PrimeIntellect-ai/prime-agent), bin `prime-agent`
        exes: &["prime-agent"],
        aliases: &["prime-agent", "primeintellect"],
        self_reports: true,
        install: "curl -fsSL https://app.primeintellect.ai/prime-agent/install.sh | sh",
        ..a("prime", "Prime Agent")
    },
];

/// Ids, for the `agent` setting's choices (kept in sync with ALL by a test).
pub const IDS: &[&str] = &[
    "claude",
    "codex",
    "opencode",
    "omp",
    "pi",
    "cursor",
    "devin",
    "gemini",
    "agy",
    "cline",
    "mastracode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
    "crush",
    "commandcode",
    "prime",
];

/// What to call a session's agent in messages: the registry's short name
/// (`Codex`, `OpenCode`, `Claude Code`), else the kind capitalised, else
/// `Agent`.
pub fn label(kind: &str) -> String {
    if let Some(a) = find(kind) {
        return a.name.to_string();
    }
    let k = kind.trim();
    if k.is_empty() {
        return "Agent".into();
    }
    let mut ch = k.chars();
    ch.next()
        .map(|f| f.to_uppercase().collect::<String>() + ch.as_str())
        .unwrap_or_else(|| "Agent".into())
}

/// An agent's name, short: `Claude` for Claude Code, `Terminal` for a plain shell.
pub fn short_label(kind: &str) -> String {
    if kind == SHELL {
        return "Terminal".into();
    }
    let l = label(kind);
    let mut s = l.split(" (").next().unwrap_or(&l);
    for p in ["GitHub ", "Factory "] {
        s = s.strip_prefix(p).unwrap_or(s);
    }
    for x in [" CLI", " Code", " Agent"] {
        s = s.strip_suffix(x).unwrap_or(s);
    }
    s.to_string()
}

/// Agents that are one vendor's: which model they're on matters less. Every other
/// one runs on whichever you pick, so a list says which.
const ONE_VENDOR: &[&str] = &["claude", "codex", "gemini", "kimi", "qwen", "grok"];

/// Is the model worth naming beside this agent?
pub fn many_models(kind: &str) -> bool {
    kind != SHELL && !ONE_VENDOR.contains(&kind)
}

/// Not an agent: a herdr tab with just your shell in it (`aleph local shell`).
pub const SHELL: &str = "shell";

/// The name of a machine's standing terminal (`shell.ready`).
pub const READY_SHELL: &str = "ready";

/// Whether an installed version is one Aleph supports (`Agent::min_version`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Support {
    /// at or past the floor
    Yes,
    /// older than the floor: untested, its usage may not be read
    Old,
    /// no floor, or a version that doesn't say (none read, a `0.0.0-…` nightly)
    Unknown,
}

/// `v2.1.2`, `opencode 2.1.2 (abc)`, `2.0` → its numbers, three at least.
fn version_nums(v: &str) -> Option<Vec<u64>> {
    let at = v.find(|c: char| c.is_ascii_digit())?;
    let core: String = v[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut n: Vec<u64> = core
        .split('.')
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap_or(0))
        .collect();
    n.resize(n.len().max(3), 0);
    Some(n)
}

impl Agent {
    pub fn support(&self, version: &str) -> Support {
        if self.min_version.is_empty() {
            return Support::Unknown;
        }
        match (version_nums(version), version_nums(self.min_version)) {
            (Some(v), _) if v.iter().all(|&x| x == 0) => Support::Unknown,
            (Some(v), Some(min)) if v >= min => Support::Yes,
            (Some(_), Some(_)) => Support::Old,
            _ => Support::Unknown,
        }
    }

    /// The warning for a version older than the floor, with how to move up.
    pub fn old_note(&self, version: &str) -> Option<String> {
        (self.support(version) == Support::Old).then(|| {
            format!(
                "{} {version} is older than {}: untested, and its usage isn't tracked properly{}",
                self.name,
                self.min_version,
                if self.upgrade.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", self.upgrade)
                }
            )
        })
    }
}

/// Look an agent up by id, herdr kind, executable, alias or display name.
pub fn find(q: &str) -> Option<&'static Agent> {
    let q = q.trim().to_lowercase();
    if q.is_empty() {
        return None;
    }
    ALL.iter()
        .find(|x| x.id == q)
        .or_else(|| ALL.iter().find(|x| x.kind == Some(q.as_str())))
        .or_else(|| {
            ALL.iter().find(|x| {
                x.exes.contains(&q.as_str())
                    || x.aliases.contains(&q.as_str())
                    || x.name.to_lowercase() == q
            })
        })
}

/// The agent an integration name belongs to (`antigravity-cli` → agy).
pub fn by_integration(name: &str) -> Option<&'static Agent> {
    ALL.iter().find(|x| x.integration == Some(name))
}

/// Closest agent ids for a typo ("did you mean").
pub fn suggest(q: &str) -> Option<&'static str> {
    let q = q.trim().to_lowercase();
    let mut best: Option<(i32, &'static str)> = None;
    for x in ALL {
        let words = std::iter::once(x.id).chain(x.exes.iter().copied());
        for w in words {
            if let Some((s, _)) = crate::fuzzy::score(&q, w) {
                if best.is_none_or(|b| s > b.0) {
                    best = Some((s, x.id));
                }
            }
        }
    }
    best.map(|b| b.1).or_else(|| {
        // no subsequence match: fall back to the closest edit distance
        ALL.iter()
            .map(|x| (edit(&q, x.id), x.id))
            .filter(|(d, _)| *d <= 2)
            .min_by_key(|(d, _)| *d)
            .map(|x| x.1)
    })
}

fn edit(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Validate an `--agent` value: the canonical id, or an error with a hint.
pub fn resolve(q: &str) -> Result<&'static str, String> {
    if q.trim().eq_ignore_ascii_case(SHELL) {
        return Ok(SHELL);
    }
    if let Some(x) = find(q) {
        return Ok(x.id);
    }
    let mut m = format!("unknown agent '{q}'");
    if let Some(s) = suggest(q) {
        m.push_str(&format!(" — did you mean '{s}'?"));
    }
    m.push_str(" (aleph agents lists them)");
    Err(m)
}

impl Agent {
    /// What we call it in messages (`Claude Code`, `Codex`, …).
    pub fn display_name(&self) -> &'static str {
        self.name
    }
    /// Its executable: what's typed into a pane when herdr can't `agent
    /// start` it, and what a probe looks for on the PATH.
    pub fn exe(&self) -> &'static str {
        self.exes.first().copied().unwrap_or(self.id)
    }
    /// How it starts in a fresh pane.
    pub fn launch(&self) -> Launch {
        match self.kind {
            Some(k) => Launch::Kind(k),
            None => Launch::Command(self.exe()),
        }
    }
    /// The shell command line that starts it with `args` (quoted).
    pub fn command_line(&self, args: &[String]) -> String {
        command_line(self.exe(), args)
    }
    /// Arguments that continue its most recent conversation in the folder
    /// (empty when it can't, or we don't know how).
    pub fn resume_args(&self) -> &'static [&'static str] {
        self.resume
    }
    /// Arguments that continue conversation `id` when it's known and the
    /// agent takes one, else its most recent conversation (`resume_args`).
    pub fn resume_for(&self, id: Option<&str>) -> Vec<String> {
        match id.filter(|i| !i.is_empty() && !self.resume_id.is_empty()) {
            Some(i) => self
                .resume_id
                .iter()
                .map(|a| a.replace("{id}", i))
                .collect(),
            None => self.resume.iter().map(|s| s.to_string()).collect(),
        }
    }
    /// How herdr knows its state.
    pub fn detect(&self) -> Detect {
        if self.self_reports {
            Detect::SelfReports
        } else if self.needs_integration {
            Detect::NeedsIntegration
        } else if self.integration.is_some() {
            Detect::BuiltIn
        } else {
            Detect::StateOnly
        }
    }
    /// How herdr learns its state, in a few words.
    pub fn state_note(&self) -> &'static str {
        match self.detect() {
            Detect::SelfReports => "reports its own state",
            Detect::NeedsIntegration => "state needs the integration",
            Detect::BuiltIn => "state detected · integration adds restore",
            Detect::StateOnly if self.experimental => "state detected (less tested)",
            Detect::StateOnly => "state detected",
        }
    }
    /// herdr's integration for it (`herdr integration install <name>`).
    pub fn integration(&self) -> Option<&'static str> {
        self.integration
    }
    /// Is a process called `name` (as `ps` or `/proc` show it) this agent?
    /// Linux truncates process names to 15 characters.
    pub fn runs_as(&self, name: &str) -> bool {
        let n = name.rsplit('/').next().unwrap_or(name);
        !n.is_empty()
            && self
                .exes
                .iter()
                .any(|e| *e == n || (n.len() == 15 && e.starts_with(n)))
    }
    /// Where it keeps its conversation logs, when we know.
    pub fn logs(&self) -> Option<Logs> {
        self.logs
    }
    /// How to install it (a command or a URL).
    pub fn install_hint(&self) -> &'static str {
        self.install
    }
    /// The install hint as something to do for `host` (a URL stays a URL).
    pub fn install_on(&self, host: &str) -> String {
        if self.install.starts_with("http")
            || !self.install.contains(' ')
            || self.install.contains(" (")
        {
            format!("install {}: {}", self.name, self.install)
        } else {
            format!("ssh {host} {}", crate::proc::sh_quote(self.install))
        }
    }
}

/// `exe args…` as one shell command line (arguments quoted).
pub fn command_line(exe: &str, args: &[String]) -> String {
    let mut cmd = exe.to_string();
    for a in args {
        cmd.push(' ');
        cmd.push_str(&crate::proc::sh_quote(a));
    }
    cmd
}

/// The agent a process called `name` belongs to (per-agent process counts).
#[allow(dead_code)] // for the telemetry sampler's per-agent process counts
pub fn by_process(name: &str) -> Option<&'static Agent> {
    ALL.iter().find(|a| a.runs_as(name))
}

/// The default agent for new sessions: the `agent` setting, or the first
/// registry agent when it's unset or unknown.
pub fn default_agent(cfg: &Config) -> &'static Agent {
    find(&cfg.get_or("agent")).unwrap_or(&ALL[0])
}

/// The default agent for new sessions on `host`: `host.<alias>.agent`, else
/// [`default_agent`].
pub fn default_agent_for(cfg: &Config, host: &str) -> &'static Agent {
    cfg.host_get(host, "agent")
        .and_then(|a| find(&a))
        .unwrap_or_else(|| default_agent(cfg))
}

/// The default for `host` as configured (`host.<alias>.agent`, else
/// `agent`), unresolved, so a typo can be reported with a "did you mean".
pub fn configured(cfg: &Config, host: &str) -> String {
    cfg.host_get(host, "agent")
        .unwrap_or_else(|| cfg.get_or("agent"))
}

/// Our id for whatever herdr calls an agent (`claude`, `Claude Code`,
/// `cursor-agent`, …): the registry id when it's one we know, else the first
/// word, lowercased.
pub fn id_of(raw: &str) -> String {
    let t = raw.trim();
    if let Some(a) = find(t) {
        return a.id.to_string();
    }
    let first = t.split_whitespace().next().unwrap_or("");
    match find(first) {
        Some(a) => a.id.to_string(),
        None => first.to_lowercase(),
    }
}

/// Do two agent names mean the same agent?
pub fn same(a: &str, b: &str) -> bool {
    id_of(a) == id_of(b)
}

// ------------------------------------------------------------------ the `agents` setting

/// Which agents' sessions are listed (the `agents` setting): `all`, or a
/// comma-separated list of agent ids.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Filter {
    #[default]
    All,
    Only(Vec<&'static str>),
}

impl Filter {
    pub fn parse(v: &str) -> Result<Filter, String> {
        let mut ids: Vec<&'static str> = Vec::new();
        for w in v.split([',', ' ']).map(str::trim).filter(|w| !w.is_empty()) {
            if w.eq_ignore_ascii_case("all") {
                return Ok(Filter::All);
            }
            match resolve(w)? {
                SHELL => {
                    return Err("shell isn't an agent — use agent ids (claude,codex) or all".into())
                }
                id if !ids.contains(&id) => ids.push(id),
                _ => {}
            }
        }
        Ok(if ids.is_empty() {
            Filter::All
        } else {
            Filter::Only(ids)
        })
    }
    /// The `agents` setting (anything unreadable lists every agent).
    pub fn from_cfg(cfg: &Config) -> Filter {
        Filter::parse(&cfg.get_or("agents")).unwrap_or_default()
    }
    pub fn is_all(&self) -> bool {
        *self == Filter::All
    }
    /// Is a session of `agent` listed?
    pub fn matches(&self, agent: &str) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(v) => {
                let id = id_of(agent);
                v.iter().any(|x| *x == id)
            }
        }
    }
}

// ------------------------------------------------------------------ the `wraps` setting

/// Flags every wrapped CLI understands: they always go to it.
const COMMON_FLAGS: &[&str] = &["-h", "--help", "-v", "--version"];

/// The program the shell function stands in for (the `wraps` setting,
/// `claude` by default): everything that isn't one of our commands — its own
/// flags and subcommands, piped calls, every call inside a herdr pane — goes
/// to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Wraps {
    /// the executable looked up on the PATH
    pub exe: String,
    /// the registry agent, when it is one
    pub agent: Option<&'static Agent>,
}

impl Wraps {
    pub fn from_cfg(cfg: &Config) -> Wraps {
        Wraps::parse(&cfg.get_or("wraps"))
    }
    /// A registry agent (by id, executable, alias or name) → its executable;
    /// anything else is taken as an executable name.
    pub fn parse(v: &str) -> Wraps {
        let v = v.trim();
        match find(v) {
            Some(a) => Wraps {
                exe: if a.exes.contains(&v) {
                    v.to_string()
                } else {
                    a.exe().to_string()
                },
                agent: Some(a),
            },
            None if v.is_empty() => Wraps {
                exe: ALL[0].exe().to_string(),
                agent: Some(&ALL[0]),
            },
            None => Wraps {
                exe: v.to_string(),
                agent: None,
            },
        }
    }
    /// `Claude Code`, or the executable when it isn't a registry agent.
    pub fn name(&self) -> String {
        self.agent
            .map_or_else(|| format!("`{}`", self.exe), |a| a.name.to_string())
    }
    /// The real program: the first `exe` on the PATH that isn't this binary.
    pub fn path(&self) -> Option<PathBuf> {
        crate::proc::real_program(&self.exe)
    }
    /// Flags that always go to it.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut v: Vec<&'static str> = self.agent.map_or(&[][..], |a| a.flags).to_vec();
        v.extend_from_slice(COMMON_FLAGS);
        v
    }
    /// Subcommands that always go to it (besides the ones its `--help` lists).
    pub fn cmds(&self) -> &'static [&'static str] {
        self.agent.map_or(&[], |a| a.cmds)
    }
    pub fn install_hint(&self) -> Option<&'static str> {
        self.agent.map(|a| a.install)
    }
    fn cache_file(&self) -> PathBuf {
        let safe: String = self
            .exe
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        crate::config::cache_dir().join(format!("{safe}-commands"))
    }
    /// Its own subcommands, read from `<exe> --help` at most once a day (so new
    /// ones pass straight through without a release of this tool). With
    /// `fetch=false`, only what's cached (no process is started).
    pub fn subcommands(&self, fetch: bool) -> Vec<String> {
        let f = self.cache_file();
        let fresh = std::fs::metadata(&f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d < Duration::from_secs(86400));
        if fresh || !fetch {
            if let Ok(s) = std::fs::read_to_string(&f) {
                return s.lines().map(String::from).collect();
            }
        }
        if !fetch {
            return vec![];
        }
        let Some(real) = self.path() else {
            return vec![];
        };
        let o = crate::proc::cap(&real.to_string_lossy(), &["--help"], Duration::from_secs(5));
        let v = parse_help(&o.text());
        if !v.is_empty() {
            let _ = crate::config::write_atomic(&f, v.join("\n").as_bytes());
        }
        v
    }
    /// Do these arguments belong to the wrapped program? `cmd` is the first
    /// argument (already known not to be one of our commands).
    pub fn owns(&self, args: &[String], cmd: &str) -> bool {
        let flags = self.flags();
        args.iter().any(|a| flags.contains(&a.as_str()))
            || self.cmds().contains(&cmd)
            || self.subcommands(true).iter().any(|x| x == cmd)
    }
}

/// The `Commands:` section of a CLI's `--help` (commander, clap and cobra
/// layouts): `config`, `mcp`, `doctor|check` → `doctor`, …
pub fn parse_help(s: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut on = false;
    for l in s.lines() {
        let t = l.trim_start();
        if t.starts_with("Commands:") || t.starts_with("Available Commands:") {
            on = true;
            continue;
        }
        if on {
            if !l.starts_with(' ') && !l.trim().is_empty() {
                break;
            }
            if let Some(w) = l.split_whitespace().next() {
                let w = w.split('|').next().unwrap_or(w).trim_end_matches(',');
                if w.chars().all(|c| c.is_ascii_lowercase() || c == '-') && !w.is_empty() {
                    v.push(w.to_string());
                }
            }
        }
    }
    v
}

#[cfg(test)]
pub(crate) mod tests {
    /// An agent whose install is only a web page (none in the registry is).
    pub(crate) fn web_only() -> &'static Agent {
        Box::leak(Box::new(Agent {
            install: "https://example.com/cli",
            alt_install: "",
            ..a("web", "Web Agent")
        }))
    }

    use super::*;

    #[test]
    fn opencode_2_is_the_supported_one() {
        let oc = find("opencode").unwrap();
        assert!(oc.install.contains("opencode.ai/v2/install"));
        assert_eq!(oc.support("2.1.2"), Support::Yes);
        assert_eq!(oc.support("v2.0.0"), Support::Yes);
        assert_eq!(oc.support("opencode 10.0.1"), Support::Yes);
        assert_eq!(oc.support("1.19.4"), Support::Old);
        assert_eq!(oc.support("0.0.0-beta-19507"), Support::Unknown);
        assert_eq!(oc.support(""), Support::Unknown);
        let note = oc.old_note("1.19.4").unwrap();
        assert!(note.contains("older than 2.0") && note.contains("opencode upgrade --major"));
        assert!(oc.old_note("2.1.4").is_none());
        // agents without a floor never warn
        assert_eq!(find("claude").unwrap().support("0.1"), Support::Unknown);
    }

    #[test]
    fn ids_match_registry() {
        let ids: Vec<&str> = ALL.iter().map(|x| x.id).collect();
        assert_eq!(ids, IDS);
        for x in ALL {
            assert!(!x.exes.is_empty(), "{} has no executable", x.id);
            assert!(!x.install.is_empty(), "{} has no install hint", x.id);
            if x.needs_integration {
                assert!(x.integration.is_some());
            }
        }
    }

    #[test]
    fn every_herdr_kind_is_known() {
        for k in [
            "pi",
            "claude",
            "codex",
            "gemini",
            "cursor",
            "devin",
            "agy",
            "cline",
            "omp",
            "mastracode",
            "opencode",
            "copilot",
            "kimi",
            "kiro",
            "droid",
            "amp",
            "grok",
            "hermes",
            "kilo",
            "qodercli",
            "qwen",
            "letta",
            "maki",
            "muse",
        ] {
            assert_eq!(find(k).and_then(|a| a.kind), Some(k), "{k}");
        }
        for i in [
            "claude",
            "codex",
            "copilot",
            "cursor",
            "opencode",
            "pi",
            "omp",
            "droid",
            "devin",
            "kimi",
            "kilo",
            "hermes",
            "qodercli",
            "qwen",
            "letta",
            "mastracode",
            "grok",
            "antigravity-cli",
        ] {
            assert!(by_integration(i).is_some(), "{i}");
        }
        assert_eq!(by_integration("antigravity-cli").unwrap().id, "agy");
    }

    #[test]
    fn short_names_and_who_needs_a_model_named() {
        for (id, short) in [
            ("claude", "Claude"),
            ("codex", "Codex"),
            ("opencode", "OpenCode"),
            ("omp", "OMP"),
            ("gemini", "Gemini"),
            ("copilot", "Copilot"),
            ("droid", "Droid"),
            ("kilo", "Kilo"),
            ("hermes", "Hermes"),
            ("shell", "Terminal"),
        ] {
            assert_eq!(short_label(id), short, "{id}");
        }
        assert!(many_models("opencode") && many_models("pi") && many_models("cursor"));
        assert!(!many_models("claude") && !many_models("codex") && !many_models(SHELL));
    }

    #[test]
    fn lookups() {
        assert_eq!(find("Claude").unwrap().id, "claude");
        assert_eq!(find("cursor-agent").unwrap().id, "cursor");
        assert_eq!(find("kiro-cli").unwrap().id, "kiro");
        assert_eq!(find("antigravity").unwrap().id, "agy");
        assert_eq!(find("OpenCode").unwrap().id, "opencode");
        assert_eq!(find("crush").unwrap().kind, None);
        assert!(find("").is_none() && find("vim").is_none());
        assert_eq!(find("cursor").unwrap().exe(), "cursor-agent");
        assert_eq!(find("codex").unwrap().resume, &["resume", "--last"]);
        assert!(find("gemini").unwrap().resume.is_empty());
    }

    #[test]
    fn did_you_mean() {
        assert_eq!(suggest("clade"), Some("claude"));
        assert_eq!(suggest("opncode"), Some("opencode"));
        assert_eq!(suggest("codx"), Some("codex"));
        assert!(resolve("clade")
            .unwrap_err()
            .contains("did you mean 'claude'"));
        assert_eq!(resolve("cursor-agent"), Ok("cursor"));
    }

    #[test]
    fn shared_interface() {
        let claude = find("claude").unwrap();
        let crush = find("crush").unwrap();
        assert_eq!(claude.launch(), Launch::Kind("claude"));
        assert_eq!(find("cursor").unwrap().launch(), Launch::Kind("cursor"));
        assert_eq!(crush.launch(), Launch::Command("crush"));
        assert_eq!(
            find("codex")
                .unwrap()
                .command_line(&["resume".into(), "a b".into()]),
            "codex resume 'a b'"
        );
        assert_eq!(claude.detect(), Detect::BuiltIn);
        assert_eq!(find("gemini").unwrap().detect(), Detect::StateOnly);
        assert_eq!(find("omp").unwrap().detect(), Detect::NeedsIntegration);
        assert_eq!(crush.detect(), Detect::SelfReports);
        assert_eq!(claude.resume_for(None), vec!["--continue"]);
        assert_eq!(claude.resume_for(Some("5f0c")), vec!["--resume", "5f0c"]);
        assert_eq!(
            find("codex").unwrap().resume_for(Some("019a")),
            vec!["resume", "019a"]
        );
        assert_eq!(
            find("copilot").unwrap().resume_for(Some("x1")),
            vec!["--resume=x1"]
        );
        // no id: copilot can't pick its last one by itself
        assert!(find("copilot").unwrap().resume_for(None).is_empty());
        assert!(find("amp").unwrap().resume_for(Some("x")).is_empty());
        assert!(claude.runs_as("claude") && claude.runs_as("/usr/bin/claude"));
        assert!(!claude.runs_as("node") && !claude.runs_as(""));
        assert_eq!(by_process("cursor-agent").unwrap().id, "cursor");
        assert!(by_process("python").is_none());
        let l = find("codex").unwrap().logs().unwrap();
        assert_eq!(l.dir, ".codex/sessions");
        let sh = l.count_today_sh();
        assert!(sh.contains("-mmin -1440") && !sh.contains('\'') && !sh.contains('!'));
        assert!(find("amp").unwrap().logs().is_none());
        assert_eq!(
            claude.install_on("b31"),
            "ssh b31 'curl -fsSL https://claude.ai/install.sh | bash'"
        );
        assert!(web_only()
            .install_on("b31")
            .starts_with("install Web Agent: http"));
        for x in ALL {
            assert_eq!(x.display_name(), label(x.id));
            assert_eq!(x.integration(), x.integration);
            assert!(!x.install_hint().is_empty());
        }
    }

    #[test]
    fn defaults_and_ids() {
        let cfg = Config::in_memory(&["host.gpu2.agent=codex", "host.x.agent=nope"]);
        assert_eq!(default_agent(&cfg).id, "claude");
        assert_eq!(default_agent_for(&cfg, "gpu2").id, "codex");
        assert_eq!(default_agent_for(&cfg, "b31").id, "claude");
        assert_eq!(default_agent_for(&cfg, "x").id, "claude");
        assert_eq!(configured(&cfg, "x"), "nope");
        let cfg = Config::in_memory(&["agent=opencode"]);
        assert_eq!(default_agent(&cfg).id, "opencode");
        assert_eq!(
            default_agent(&Config::in_memory(&["agent=zzz"])).id,
            ALL[0].id
        );
        assert_eq!(id_of("Claude Code"), "claude");
        assert_eq!(id_of("claude"), "claude");
        assert_eq!(id_of("cursor-agent"), "cursor");
        assert_eq!(id_of("GitHub Copilot CLI"), "copilot");
        assert_eq!(id_of("Claude Code (beta)"), "claude");
        assert_eq!(id_of("Mystery Agent"), "mystery");
        assert!(same("Codex", "codex") && !same("codex", "claude"));
    }

    #[test]
    fn agent_filter() {
        assert_eq!(Filter::parse("all"), Ok(Filter::All));
        assert_eq!(Filter::parse(""), Ok(Filter::All));
        assert_eq!(
            Filter::parse("claude, codex,claude"),
            Ok(Filter::Only(vec!["claude", "codex"]))
        );
        assert_eq!(
            Filter::parse("cursor-agent"),
            Ok(Filter::Only(vec!["cursor"]))
        );
        assert!(Filter::parse("clade").unwrap_err().contains("did you mean"));
        assert!(Filter::parse("shell").is_err());
        let f = Filter::parse("claude").unwrap();
        assert!(f.matches("claude") && f.matches("Claude Code") && !f.matches("codex"));
        assert!(Filter::All.matches("anything") && Filter::All.is_all());
        let cfg = Config::in_memory(&["agents=codex"]);
        assert_eq!(Filter::from_cfg(&cfg), Filter::Only(vec!["codex"]));
        assert!(Filter::from_cfg(&Config::in_memory(&["agents=bogus"])).is_all());
        assert!(Filter::from_cfg(&Config::in_memory(&[])).is_all());
    }

    #[test]
    fn wrapped_program() {
        let w = Wraps::parse("claude");
        assert_eq!(
            (w.exe.as_str(), w.agent.map(|a| a.id)),
            ("claude", Some("claude"))
        );
        assert_eq!(w.name(), "Claude Code");
        // exactly the flags that always went to Claude Code
        let mut f = w.flags();
        f.sort();
        let mut want = vec![
            "-p",
            "--print",
            "-h",
            "--help",
            "-v",
            "--version",
            "-c",
            "--continue",
            "-r",
        ];
        want.sort();
        assert_eq!(f, want);
        assert!(w.cmds().contains(&"mcp") && w.cmds().contains(&"plugins"));
        assert_eq!(Wraps::parse("cursor").exe, "cursor-agent");
        assert_eq!(Wraps::parse("cursor-agent").exe, "cursor-agent");
        let c = Wraps::parse("codex");
        assert_eq!(c.flags(), vec!["-h", "--help", "-v", "--version"]);
        assert!(c.cmds().is_empty());
        let o = Wraps::parse("my-tool");
        assert_eq!((o.exe.as_str(), o.agent), ("my-tool", None));
        assert_eq!(o.name(), "`my-tool`");
        assert_eq!(Wraps::parse("").exe, "claude");
        assert_eq!(Wraps::from_cfg(&Config::in_memory(&[])).exe, "claude");
        assert_eq!(
            Wraps::from_cfg(&Config::in_memory(&["wraps=codex"])).exe,
            "codex"
        );
    }

    #[test]
    fn help_commands_are_parsed() {
        let claude = "Usage: claude [options] [command] [prompt]\n\nOptions:\n  -d, --debug  x\n\nCommands:\n  config         Manage configuration\n  mcp            Configure MCP servers\n  setup-token    Set up a token\n  doctor|check   Check health\n\nExamples:\n  x\n";
        assert_eq!(
            parse_help(claude),
            vec!["config", "mcp", "setup-token", "doctor"]
        );
        // clap (codex)
        let codex = "Codex CLI\n\nUsage: codex [OPTIONS] [PROMPT]\n       codex [OPTIONS] <COMMAND> [ARGS]\n\nCommands:\n  exec        Run Codex non-interactively [aliases: e]\n  login       Manage login\n  mcp         Run as an MCP server\n  help        Print this message\n\nArguments:\n  [PROMPT]\n";
        assert_eq!(parse_help(codex), vec!["exec", "login", "mcp", "help"]);
        // cobra
        let cobra = "Usage:\n  crush [flags]\n\nAvailable Commands:\n  logs        Show logs\n  run         Run a prompt\n\nFlags:\n  -h, --help\n";
        assert_eq!(parse_help(cobra), vec!["logs", "run"]);
        assert!(parse_help("no commands here").is_empty());
    }
}
