//! ~/.config/aleph/config — a plain `key=value` file (edit freely, or
//! use `aleph config …` / the Settings view / `aleph host …`).
//!
//! Core keys
//!   ssh_host=b31          default ssh alias (from ~/.ssh/config, or a plain hostname)
//!   machine=b31           herdr machine label/id for the default host
//!   hosts=b31,gpu2        machines shown together in the dashboard (default: just ssh_host)
//!   host.<alias>.color    machine colour (#hex or a palette name); auto-assigned once
//!   host.<alias>.machine  herdr machine label for that alias (when it differs)
//!   host.<alias>.dir      folder the browser / `aleph new` start in
//!   host.<alias>.args     default agent args for that host
//!   host.<alias>.agent    default agent there (claude, codex, …)
//!   preset.<name>.<field> saved starting points: host, dir, args, agent, worktree, prompt
//!   key.<action>=<key>    dashboard key overrides
//!   action.<key>=<label>|<command>   custom keys: run <command> in a new tab in the session's folder
//!   launch.<key>=<what>   keys that start something and open it: terminal, terminal [machine:]/folder, or a preset
//!   prompt.<name>         a saved prompt; prompt_tags.<name> its scenarios, prompt_agents.<name> the agents it's for
//!   hook.pre_new / hook.post_new     commands run before the agent starts (remote) / after (local)
//!
//! Resolution order for the target host: `--host` flag > CC_SSH_HOST > CC_MACHINE
//! (legacy; label doubles as ssh host) > config.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    /// the file `set` writes to (`aleph.toml`; a key=value file when
    /// `ALEPH_CONFIG` names one, and in tests)
    pub path: PathBuf,
    store: Store,
    /// lower layers: files the main one includes, in order (later wins)
    under: Vec<(PathBuf, Vec<(String, String)>)>,
    /// the top layer: `ALEPH_PROFILE=<name>` → profiles/<name>.toml
    over: Option<(PathBuf, Vec<(String, String)>)>,
    /// the effective settings, layers merged: (key, value, where from)
    cache: Vec<(String, String, Source)>,
}

/// Where a setting's value comes from (`aleph config sources`).
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Main,
    Include(PathBuf),
    Profile(PathBuf),
}

#[derive(Clone)]
enum Store {
    /// the old `key=value` lines
    Flat(Vec<String>),
    /// `aleph.toml`, edited in place (comments and order survive)
    Toml(toml_edit::DocumentMut),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    /// ssh destination (alias) — used for ssh, `herdr --remote`, the folder browser
    pub ssh: String,
    /// herdr saved-machine selector — used for `herdr --machine`
    pub machine: String,
    /// where the host came from, for display
    pub source: &'static str,
}

/// A documented setting: (key, default, kind, description). Shown by
/// `aleph config list` and the Settings view.
pub struct Known {
    pub key: &'static str,
    pub default: &'static str,
    pub kind: Kind,
    pub help: &'static str,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Text,
    Bool,
    Num,
    Choice(&'static [&'static str]),
    /// a comma list of these (plus `all` / `off` / `any` / `mine` where the
    /// setting's help says so, and `-name` for all but some)
    List(&'static [&'static str]),
}

/// The words a list setting may hold besides its options.
pub const LIST_WORDS: &[&str] = &["all", "off", "none", "any", "mine", "on"];

/// Settings' sections, in order; the doctor's checks map to them too.
pub const SECTIONS: &[&str] = &[
    "General",
    "Appearance",
    "Machines",
    "Agents",
    "Notifications",
    "Remote control",
    "Analytics",
    "Automation",
    "Presets",
    "Daemon & API",
    "Integrations",
    "Network",
    "Keys",
    "Advanced",
    // a showcase build's own (after the product's, so their places never move)
    #[cfg(feature = "showcase")]
    "Showcase",
    "Doctor",
];

/// Settings that exist only per machine (`host.<alias>.<key>`), besides the
/// fleet-wide ones a machine can override.
pub const HOST_KEYS: &[(&str, &str)] = &[
    ("color", "the machine's colour in lists, cards and charts (C in Fleet)"),
    ("tags", "comma-separated tags: Fleet groups by the first (fleet.group); presets can target tag:<name>"),
    ("mac", "its network card's MAC address, for Wake-on-LAN (W)"),
    ("machine", "its herdr machine label, when it isn't the ssh alias"),
    ("session", "the herdr named session Aleph uses there (herdr's --session / HERDR_SESSION: its own server, tabs and socket); empty: the default one"),
    ("moshi", "what its sessions send to Moshi: auto, full, machine-only or off"),
    ("hooks", "the agents' own hooks report from there (on / off; aleph hooks install)"),
    ("paused", "Aleph doesn't poll it (Z in Fleet)"),
    ("stats_keep_hours", "hours of resource graphs it keeps (over stats_keep_hours; 0: only while a view shows them)"),
    ("agent", "the agent that starts there by default"),
    ("args", "arguments agents start with there"),
    ("dir", "the folder new sessions start in there"),
    ("agent_wrapper", "the agent_wrapper for that machine only"),
    ("claude_roots", "more Claude Code config folders to read usage from there (analytics.claude_roots, for that machine)"),
    ("codex_homes", "more Codex homes to read usage from there (analytics.codex_homes, for that machine)"),
];

/// Every known setting as Markdown, by section: docs/config.md.
pub fn docs_markdown() -> String {
    let mut out = String::from(
        "# Aleph settings\n\n\
         Generated from `config::KNOWN` (`ALEPH_WRITE_DOCS=1 cargo test config_docs_are_current`).\n\
         Set one with `aleph settings set <key> <value>`; they live in `~/.config/aleph/aleph.toml`\n\
         (the TOML column says where). `aleph settings` lists them with their current values, and the\n\
         dashboard's Settings tab edits them. Per-machine settings are `host.<alias>.<key>` (`[hosts.<alias>]`).\n",
    );
    for sec in SECTIONS {
        // (a showcase build's own settings stay out: the page is the same from either build)
        let keys: Vec<&Known> = KNOWN
            .iter()
            .filter(|k| section_of(k.key) == *sec && !crate::showcase::own_key(k.key))
            .collect();
        if keys.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n## {sec}\n\n| Setting | TOML | Default | What it does |\n|---|---|---|---|\n"
        ));
        for k in keys {
            let kind = match k.kind {
                Kind::Bool => " (on/off)".to_string(),
                Kind::Num => " (number)".to_string(),
                Kind::Choice(c) => format!(" ({})", c.join(" \\| ")),
                Kind::List(c) => format!(" (list: {})", c.join(", ")),
                Kind::Text => String::new(),
            };
            let def = if k.default.is_empty() {
                "—".to_string()
            } else {
                format!("`{}`", k.default)
            };
            out.push_str(&format!(
                "| `{}` | `{}` | {def}{kind} | {} |\n",
                k.key,
                crate::tomlcfg::to_path(k.key).join("."),
                k.help.replace('|', "\\|").replace('\n', " ")
            ));
        }
    }
    out.push_str(
        "\n## Per machine\n\nSet as `host.<alias>.<key>` (`[hosts.<alias>] <key>`), e.g. `aleph settings set host.b31.tags gpu`.\n\n| Key | What it does |\n|---|---|\n",
    );
    for (k, h) in HOST_KEYS {
        out.push_str(&format!("| `{k}` | {h} |\n"));
    }
    out
}

/// The Settings section a key is shown in.
pub fn section_of(key: &str) -> &'static str {
    match key {
        "refresh"
        | "naming"
        | "tab_prefix"
        | "return_to_dashboard"
        | "enter_opens"
        | "focus.keys"
        | "focus.exit"
        | "focus.esc_guard"
        | "focus.copy_on_select"
        | "close.grace"
        | "shell.list"
        | "shell.ready"
        | "palette.pins"
        | "wall_fit"
        | "wall_layout"
        | "wall_max"
        | "mux_keys"
        | "mux_prefix"
        | "mux_leave"
        | "show_hidden"
        | "show_ignored"
        | "ignore"
        | "browse_ttl"
        | "deep_search" => "General",
        "theme" | "accent" | "mouse" | "ui.keyboard" | "ui.spinner" | "ui.splash"
        | "ui.splash_end" | "ui.ground" | "ui.panels" | "ui.ambience" | "ui.splash_fx"
        | "ui.splash_ms" | "ui.splash_logo" | "ui.splash_hold" | "ui_scale" | "icons"
        | "ui.logos" | "tour" | "ui.logo_size" | "herdr_view" | "herdr_tokens" | "ui.note_for"
        | "ui.alert_for" | "ui.whats_new" => "Appearance",
        "ui.motion" | "ui.row_bars" | "ui.timeline_hours" | "fleet.timeline" => "Appearance",
        k if k.starts_with("layout.") || k.starts_with("workspace.") || k.starts_with("theme.") => {
            "Appearance"
        }
        "ssh_host" | "machine" | "hosts" | "local" | "stats" | "stats_interval"
        | "stats_window" | "stats_keep_hours" | "services" | "services_interval"
        | "fleet.group" => "Machines",
        "agent"
        | "agents"
        | "agents.node"
        | "agents.install_timeout"
        | "agents.install_quiet"
        | "wraps"
        | "agent_wrapper" => "Agents",
        k if k.starts_with("handoff.")
            || k.starts_with("race.")
            || k.starts_with("openrouter.") =>
        {
            "Agents"
        }
        "notify" | "notify_done" => "Notifications",
        "remote.allow" | "remote.deny" => "Remote control",
        "auto_approve" | "approve_deny" | "approvals.layout" | "approvals.group" | "auto_heal"
        | "hook.pre_new" | "hook.post_new" => "Automation",
        "watch_interval" | "hub.machine" | "daemon" => "Daemon & API",
        "terminal" | "raycast_dirs" | "editor" => "Integrations",
        "ssh_reuse" | "ssh_key" | "herdr_via" | "listen" | "net.probe" | "attach_via" => "Network",
        k if k.starts_with("inbox.") => "General",
        k if k.starts_with("notify.") || k.starts_with("moshi") => "Notifications",
        k if k.starts_with("host.") || k.starts_with("service.") => "Machines",
        k if k.starts_with("approve.") || k.starts_with("limits.") => "Automation",
        k if k.starts_with("key.") => "Keys",
        k if k.starts_with("action.") || k.starts_with("launch.") => "Integrations",
        k if k.starts_with("preset.") => "Presets",
        k if k.starts_with("analytics.")
            || k.starts_with("usage.")
            || k.starts_with("timeline.")
            || k.starts_with("journal.") =>
        {
            "Analytics"
        }
        k if k.starts_with("daemon.") || k.starts_with("api.") => "Daemon & API",
        #[cfg(feature = "showcase")]
        k if k.starts_with("showcase.") => "Showcase",
        _ => "Advanced",
    }
}

pub const KNOWN: &[Known] = &[
    Known {
        key: "ssh_host",
        default: "",
        kind: Kind::Text,
        help: "default ssh alias",
    },
    Known {
        key: "machine",
        default: "",
        kind: Kind::Text,
        help: "herdr machine label for the default host",
    },
    Known {
        key: "hosts",
        default: "",
        kind: Kind::Text,
        help: "machines shown together (comma-separated); empty = the default host",
    },
    Known {
        key: "refresh",
        default: "0.1",
        kind: Kind::Num,
        help: "seconds between polls of each machine (0.1 = as fast as it answers; unreachable machines back off)",
    },
    Known {
        key: "stats",
        default: "on",
        kind: Kind::Choice(&["on", "selected", "off"]),
        help: "live machine stats (CPU, GPU, memory) in the dashboard; off: none, and no history kept either",
    },
    Known {
        key: "stats_interval",
        default: "1",
        kind: Kind::Num,
        help: "seconds between machine stat samples",
    },
    Known {
        key: "stats_window",
        default: "30m",
        kind: Kind::Choice(&["5m", "15m", "30m", "1h", "3h", "6h", "12h"]),
        help: "how far back the resource graphs reach (+ - in Monitor)",
    },
    Known {
        key: "stats_keep_hours",
        default: "12",
        kind: Kind::Num,
        help: "hours of resource graphs to keep per machine, sampled by the daemon even with nothing open (0: only while a view shows them; one machine: host.<name>.stats_keep_hours)",
    },
    Known {
        key: "services",
        default: "off",
        kind: Kind::Choice(&["off", "on"]),
        help: "watch for local LLM servers and AI gateways (Ollama, LM Studio, vLLM, …) on every machine; service.<id>=off skips one",
    },
    Known {
        key: "services_interval",
        default: "30",
        kind: Kind::Num,
        help: "seconds between LLM-service scans of each machine",
    },
    Known {
        key: "theme",
        default: "auto",
        kind: Kind::Choice(&[
            "auto",
            "dark",
            "light",
            "nord",
            "catppuccin",
            "catppuccin-latte",
            "tokyo-night",
            "gruvbox",
            "dracula",
            "solarized-light",
            "singularity",
            "singularity-light",
            "pulsar",
            "pulsar-light",
            "event-horizon",
            "event-horizon-light",
            "matrix",
            "matrix-light",
            "matrix-mono",
            "matrix-mono-light",
            "convergence",
            "convergence-light",
            "cyberpunk",
            "cyberpunk-light",
        ]),
        help: "colour theme: auto (Graphite or Paper, as the terminal is), dark (Graphite), light (Paper), nord, catppuccin, catppuccin-latte, tokyo-night, gruvbox, dracula, solarized-light; the cosmos — singularity (a point becoming everything: white-gold on a violet void), pulsar (cyan beams, magenta flares), event-horizon (accretion amber on true black), matrix (phosphor green, the states keep their colours), matrix-mono (all green: states by brightness and their glyphs ▲ ✓ ✗ ◷), convergence (synapse violet on indigo), each with a -light version; cyberpunk (a neural interface's HUD: electric yellow for the accent and selection, off-red chrome, cyan data, on near-black; HUD-bracket frames, a glitch between views) and cyberpunk-light (the key art's yellow ground, black ink); a theme paints its own background (ui.ground), frames its panels its own way (ui.panels) and has its own spinner, state glyphs and splash (ui.spinner, ui.splash = theme); or one of yours ([themes.<name>] in aleph.toml: base = a theme to start from, then accent, blue, mauve, green, yellow, red, peach, teal, pink, sky, muted, faint, line, sel, text as #rrggbb); the palette's Theme gallery previews them live; NO_COLOR turns colour off",
    },
    Known {
        key: "accent",
        default: "gold",
        kind: Kind::Text,
        help: "accent colour: gold (Aleph's), claude (Claude's orange), a palette name or #rrggbb",
    },
    Known {
        key: "mouse",
        default: "true",
        kind: Kind::Bool,
        help: "mouse clicks and scrolling in full-screen views",
    },
    Known {
        key: "ui.keyboard",
        default: "auto",
        kind: Kind::Choice(&["auto", "legacy"]),
        help: "auto: where the terminal can (kitty's keyboard protocol: Ghostty, kitty, WezTerm, iTerm2, foot, …), every key comes in as itself — ctrl+enter, ctrl+tab, alt+esc — and Focus passes it to the agent as pressed; legacy: the classic encoding (ctrl+enter is enter)",
    },
    Known {
        key: "naming",
        default: "story",
        kind: Kind::Choice(&["story", "verb", "folder"]),
        help: "how new sessions are named: story = <machine's mood>-<agent's creature> (smoldering-heron: Claude on the peach machine), verb (ignite) or folder (px)",
    },
    Known {
        key: "tab_prefix",
        default: "",
        kind: Kind::Text,
        help: "tab name prefix; empty = <agent>_ (claude_, codex_, …)",
    },
    Known {
        key: "agent",
        default: "claude",
        kind: Kind::Choice(crate::agents::IDS),
        help: "default agent for new sessions (aleph agents lists them)",
    },
    Known {
        key: "agents",
        default: "all",
        kind: Kind::Text,
        help: "which agents' sessions are listed: all, or agent ids (claude,codex)",
    },
    Known {
        key: "agents.install_timeout",
        default: "3600",
        kind: Kind::Num,
        help: "seconds an agent's installer may run (setup and aleph agents install) before it's stopped — then Aleph checks whether the agent is there anyway; long enough for a big clone and a large download on a slow link, and x on setup's install screen stops one sooner",
    },
    Known {
        key: "agents.install_quiet",
        default: "60",
        kind: Kind::Num,
        help: "seconds of silence after an agent's installer asks something (a line ending in ?, [y/N], a login menu) before it's stopped — installers run with no terminal, so nothing can answer; a silent clone or build isn't a question and goes on",
    },
    Known {
        key: "agents.node",
        default: "auto",
        kind: Kind::Choice(&["auto", "off"]),
        help: "an agent that installs with npm, on a machine without Node.js (or with one too old): auto puts the current Node.js LTS there first — its newest release, sha256-checked, in your home folder, no sudo; off says how instead",
    },
    Known {
        key: "wraps",
        default: "claude",
        kind: Kind::Text,
        help: "the agent your shell function stands in for: its own flags and subcommands (-p, mcp, …), piped calls and calls inside herdr panes go to it",
    },
    Known {
        key: "daemon",
        default: "local",
        kind: Kind::Text,
        help: "where a bare aleph opens the dashboard: local, or ssh://<machine> / mosh://<machine> (a VPS near the fleet; it reconnects when the link drops; aleph --local stays here)",
    },
    Known {
        key: "agent_wrapper",
        default: "",
        kind: Kind::Text,
        help: "a command every agent starts under, like a sandbox (`nono run --`, `fence`): typed as HERDR_AGENT=<agent> <wrapper> <agent> …, so herdr still knows the agent (host.<a>.agent_wrapper for one machine)",
    },
    Known {
        key: "return_to_dashboard",
        default: "true",
        kind: Kind::Bool,
        help: "come back to the dashboard after detaching (ctrl+b q)",
    },
    Known {
        key: "enter_opens",
        default: "focus",
        kind: Kind::Choice(&["focus", "attach", "direct"]),
        help: "enter on a session: Focus (fullscreen here, with messages and splits), attach in herdr (its own terminal view), or straight into the agent",
    },
    Known {
        key: "focus.keys",
        default: "agent",
        kind: Kind::Choice(&["agent", "aleph"]),
        help: "where keys go when Focus opens: agent (straight into it, as in its own terminal — focus.exit, mux_leave or a click outside gives the keys back to Aleph, focus.exit again goes to Fleet; the wheel scrolls its history) or aleph (Aleph's keys first: i for the message bar, I to type into it)",
    },
    Known {
        key: "focus.exit",
        default: "ctrl+q",
        kind: Kind::Text,
        help: "keys out of Focus (and Tile): while typing into the agent, the first gives the keys back to Aleph (esc is the agent's then: it interrupts its work), the next goes to Fleet; the prefix and then this key sends it to the pane; comma-separated: ctrl+q, f12, alt+q, …",
    },
    Known {
        key: "focus.copy_on_select",
        default: "true",
        kind: Kind::Bool,
        help: "Focus / Tile: text you drag across a pane is copied when you let go, as herdr does. Off: it stays selected until the next key or click, and Cmd+C copies it where the terminal passes Cmd+C on. Option+drag goes to the agent instead",
    },
    Known {
        key: "focus.esc_guard",
        default: "true",
        kind: Kind::Bool,
        help: "while typing into an agent, the first esc is held back with a warning (esc stops most agents' work); a second within 3 s goes to the agent, and so do the ones right after it",
    },
    Known {
        key: "palette.pins",
        default: "",
        kind: Kind::Text,
        help: "the sessions pinned on top of ctrl+k, up to three, as machine:name (ctrl+p there pins and unpins)",
    },
    Known {
        key: "shell.list",
        default: "all",
        kind: Kind::Choice(&["all", "aleph"]),
        help: "which plain terminals Fleet lists: all (every pane herdr has with no agent in it) or aleph (only those Aleph started)",
    },
    Known {
        key: "shell.ready",
        default: "first-use",
        kind: Kind::Choice(&["first-use", "startup"]),
        help: "each machine's standing terminal, the one ! opens on a machine's row: first-use (made the first time you ask, then kept) or startup (made as soon as the machine answers, which adds a pane there)",
    },
    Known {
        key: "close.exited",
        default: "close",
        kind: Kind::Choice(&["close", "keep"]),
        help: "when an agent or terminal exits by itself: its window closes and you go back where you came from, and its pane closes if Aleph opened it and only its shell is left running (close); or it all stays, saying it ended (keep)",
    },
    Known {
        key: "close.grace",
        default: "5s",
        kind: Kind::Text,
        help: "how long a close waits before it happens, wherever it comes from (x, aleph close, Raycast, the web): the agent keeps running until then, and u, aleph close --undo or Undo keep it (5s, 30s, 1m; 0 = at once; at most 1h); aleph close --now skips it once",
    },
    Known {
        key: "terminal",
        default: "auto",
        kind: Kind::Choice(&["auto", "warp", "iterm", "terminal", "ghostty", "kitty", "wezterm"]),
        help: "where \"Open in herdr\" opens its new tab (auto: the terminal Aleph runs in)",
    },
    Known {
        key: "show_hidden",
        default: "false",
        kind: Kind::Bool,
        help: "browser: show dot-folders by default",
    },
    Known {
        key: "show_ignored",
        default: "false",
        kind: Kind::Bool,
        help: "browser: show package/build folders by default",
    },
    Known {
        key: "ignore",
        default: "",
        kind: Kind::Text,
        help: "browser ignore list edits, e.g. +data,-build",
    },
    Known {
        key: "browse_ttl",
        default: "600",
        kind: Kind::Num,
        help: "seconds before a cached folder listing is re-fetched",
    },
    Known {
        key: "deep_search",
        default: "3",
        kind: Kind::Num,
        help: "browser: folder levels indexed for search (0 = off)",
    },
    Known {
        key: "notify",
        default: "true",
        kind: Kind::Bool,
        help: "aleph watch: notify when a session needs you",
    },
    Known {
        key: "notify.subagents",
        default: "false",
        kind: Kind::Bool,
        help: "notify when an agent wakes because a subagent reported back while others still work; off, only the last report counts as the session finishing",
    },
    Known {
        key: "notify.commands",
        default: "30",
        kind: Kind::Num,
        help: "a command in a terminal that ran this many seconds notifies when it finishes (0: never). Things you leave running and end yourself, such as vim, ssh or htop, never do. herdr gives no exit code, so it says finished, not failed",
    },
    Known {
        key: "notify_done",
        default: "false",
        kind: Kind::Bool,
        help: "desktop notifications for every finished turn, not only the long ones (notify.done_min); never for a session on screen in a dashboard in front of you",
    },
    Known {
        key: "watch_interval",
        default: "20",
        kind: Kind::Num,
        help: "aleph watch: seconds between checks",
    },
    Known {
        key: "local",
        default: "true",
        kind: Kind::Bool,
        help:
            "show this computer's own herdr as the `local` machine (when herdr is installed here)",
    },
    Known {
        key: "ssh_key",
        default: "",
        kind: Kind::Text,
        help: "the ssh key Aleph uses for key login and the machines it adds (a file in ~/.ssh); empty = ssh's own choice (your ~/.ssh/config, then id_ed25519, id_ecdsa, id_rsa)",
    },
    Known {
        key: "ssh_reuse",
        default: "120",
        kind: Kind::Num,
        help: "keep one shared ssh connection per machine open N idle seconds (0 = a new connection every time)",
    },
    Known {
        key: "auto_approve",
        default: "false",
        kind: Kind::Bool,
        help: "answer questions that match an approve.<rule>=<pattern> rule (aleph auto)",
    },
    Known {
        key: "approve_deny",
        default: crate::auto::DEFAULT_DENY,
        kind: Kind::Text,
        help: "never auto-approve questions matching these patterns (comma-separated, * wildcards)",
    },
    Known {
        key: "approvals.layout",
        default: "list",
        kind: Kind::Choice(&["list", "cards"]),
        help: "the approval board (ctrl+a): list (who's waiting beside the selected question, as its agent shows it) or cards (each question in a box of its own); v switches it there",
    },
    Known {
        key: "approvals.group",
        default: "off",
        kind: Kind::Choice(&["off", "same"]),
        help: "the approval board's list: off (each question on its own row) or same (agents asking the same thing share a row, and one answer goes to each of them — each still read again and checked against the deny list)",
    },
    Known {
        key: "auto_heal",
        default: "false",
        kind: Kind::Bool,
        help: "restart herdr where it stopped (and restore its sessions) while watch or the dashboard runs",
    },
    Known {
        key: "notify.ntfy",
        default: "",
        kind: Kind::Text,
        help: "phone notifications: an ntfy topic URL, e.g. https://ntfy.sh/<secret-topic>",
    },
    Known {
        key: "notify.slack",
        default: "",
        kind: Kind::Text,
        help: "a Slack incoming-webhook URL",
    },
    Known {
        key: "notify.webhook",
        default: "",
        kind: Kind::Text,
        help: "any URL; gets each event as JSON",
    },
    Known {
        key: "limits.auto_resume",
        default: "",
        kind: Kind::Text,
        help: "host:agent globs (e.g. b31:claude, *:codex) whose sessions carry on by themselves once a usage limit resets — it sends the agent's own resume.<agent> ([automation.resume] in aleph.toml), else limits.resume_prompt",
    },
    Known {
        key: "limits.resume_prompt",
        default: "continue",
        kind: Kind::Text,
        help: "what auto-resume sends once the limit lifts (an agent's own: resume.<agent>)",
    },
    Known {
        key: "notify.source",
        default: "both",
        kind: Kind::Choice(&["both", "herdr", "agents"]),
        help: "who tells you: herdr's view of each agent, the agents' own hooks (aleph hooks install), or both (the agent's words win when both say it)",
    },
    Known {
        key: "notify.desktop.click",
        default: "aleph",
        kind: Kind::Choice(&["aleph", "raycast", "warp"]),
        help: "a click on a desktop notification opens the session in Aleph's Focus (a new terminal tab), Raycast's Aleph inbox, or a Warp tab attached in herdr",
    },
    Known {
        key: "notify.dnd",
        default: "",
        kind: Kind::Text,
        help: "do not disturb: on, or until a time (aleph notify dnd 1h); problems in notify.quiet_pass still come through",
    },
    Known {
        key: "notify.quiet",
        default: "",
        kind: Kind::Text,
        help: "quiet hours for phone notifications, e.g. 23:00-08:00 (routes in aleph.toml can set their own)",
    },
    Known {
        key: "notify.quiet_pass",
        default: "error",
        kind: Kind::Text,
        help: "kinds that come through quiet hours, do not disturb and mutes (comma-separated)",
    },
    Known {
        key: "notify.include_text",
        default: "summary",
        kind: Kind::Choice(&["summary", "full", "off"]),
        help: "how much of what an agent said goes into a notification: 140 characters, all of it, or none",
    },
    Known {
        key: "notify.escalate_after",
        default: "",
        kind: Kind::Text,
        help: "a question still waiting this long is sent again (e.g. 10m); empty: never",
    },
    Known {
        key: "notify.herdr",
        default: "false",
        kind: Kind::Bool,
        help: "questions, limits and problems also show as a toast in herdr's own window here",
    },
    Known {
        key: "notify.raycast",
        default: "false",
        kind: Kind::Bool,
        help: "refresh the Raycast menu bar the moment something happens",
    },
    Known {
        key: "notify.raycast.kinds",
        default: "",
        kind: Kind::Text,
        help: "what the Raycast menu bar lists: event kinds (blocked, done, limit, error, …), all, or none; empty: the extension's default",
    },
    Known {
        key: "notify.raycast.deeplink",
        default: "raycast://extensions/aleph/aleph/menu-bar?launchType=background",
        kind: Kind::Text,
        help: "the Raycast command notify.raycast refreshes",
    },
    Known {
        key: "notify.remote_on",
        default: "blocked,limit,done",
        kind: Kind::Text,
        help: "which events go to ntfy/Slack/webhook: blocked,done,new,gone,error,auto,task,heal,limit or all (a finished turn only after notify.done_min of work)",
    },
    Known {
        key: "notify.question_after",
        default: "15s",
        kind: Kind::Text,
        help: "a question goes to the phone (ntfy, Moshi, Slack, webhook, web push) only if it's still waiting after this long — answer it at the terminal and the phone stays quiet. 0 = at once",
    },
    Known {
        key: "notify.done_min",
        default: "2m",
        kind: Kind::Text,
        help: "a finished turn goes to the phone (ntfy, Moshi, Slack, webhook, web push) only after this much work; shorter ones stay in the Inbox, Raycast and on the desktop. 0 = every turn",
    },
    Known {
        key: "hub.machine",
        default: "",
        kind: Kind::Text,
        help: "with daemons on several computers: the one (hostname -s) that runs automation — auto-approve, the queue, notifications, Moshi, self-heal; empty = this one",
    },
    Known {
        key: "api.http",
        default: "off",
        kind: Kind::Choice(&["off", "on"]),
        help: "the daemon also serves its API and the web console over HTTP (api.listen; aleph daemon pair adds a phone) — restart the daemon after changing it",
    },
    Known {
        key: "api.listen",
        default: "127.0.0.1:7878",
        kind: Kind::Text,
        help: "where the HTTP API listens: this computer only by default (put tailscale serve or cloudflared in front); another address needs api.listen_any",
    },
    Known {
        key: "api.listen_any",
        default: "false",
        kind: Kind::Bool,
        help: "allow api.listen on an address other machines reach directly (0.0.0.0, a LAN or tailnet IP) — tokens still apply, but prefer tailscale serve",
    },
    Known {
        key: "api.url",
        default: "",
        kind: Kind::Text,
        help: "the address phones and browsers use for the console (e.g. https://mac.tail1234.ts.net): pairing links use it and its host is allowed",
    },
    Known {
        key: "api.hosts",
        default: "",
        kind: Kind::Text,
        help: "more names the console may be reached by (comma-separated, *.example.com allowed); anything else is refused (DNS rebinding)",
    },
    Known {
        key: "api.trust_tailscale",
        default: "false",
        kind: Kind::Bool,
        help: "behind tailscale serve: name the tailnet user (Tailscale-User-Login) in the Inbox audit line — for the record only, never instead of a token",
    },
    Known {
        key: "api.push_contact",
        default: "",
        kind: Kind::Text,
        help: "who push services can contact about this sender (mailto: or https:, sent in Web Push's VAPID token); empty: a generic address",
    },
    Known {
        key: "log.level",
        default: "info",
        kind: Kind::Choice(&["error", "warn", "info", "debug"]),
        help: "what goes into the log (aleph logs): info = machines down/up, reconnects, answers sent, failures; debug adds every external command",
    },
    Known {
        key: "moshi",
        default: "off",
        kind: Kind::Choice(&["off", "auto", "on"]),
        help: "Moshi (the phone terminal) through moshi-hook: inbox cards, phone approvals, live jobs — off until you turn it on (aleph moshi install); auto = on when moshi-hook is paired here",
    },
    Known {
        key: "moshi.events",
        default: "needs_you,limit,error,down,up,done",
        kind: Kind::Text,
        help: "what becomes a Moshi card: needs_you,limit,error,down,up,done (done: after notify.done_min of work)",
    },
    Known {
        key: "moshi.approvals",
        default: "true",
        kind: Kind::Bool,
        help: "questions go to the phone as buttons (for sessions remote.allow covers; never rm -rf/sudo/… — approve_deny)",
    },
    Known {
        key: "moshi.choices",
        default: "false",
        kind: Kind::Bool,
        help: "the agent's own options as buttons on Moshi questions; off = Approve / Deny, which pick its yes and no options (Moshi's app answers only those so far)",
    },
    Known {
        key: "moshi.jobs",
        default: "true",
        kind: Kind::Bool,
        help: "working sessions show as live rows in Moshi's inbox",
    },
    Known {
        key: "moshi.session",
        default: "true",
        kind: Kind::Bool,
        help: "keep an `aleph` tmux session running, so Moshi's session picker (and a tap on a card) opens the dashboard",
    },
    Known {
        key: "remote.allow",
        default: "",
        kind: Kind::Text,
        help: "sessions phones, Raycast and the web may act on (approve, reply): host:agent globs, e.g. b31:*,*:claude or *:* — empty = notifications only",
    },
    Known {
        key: "remote.deny",
        default: "",
        kind: Kind::Text,
        help: "host:agent globs never acted on remotely (wins over remote.allow)",
    },
    Known {
        key: "wall_fit",
        default: "on",
        kind: Kind::Choice(&["on", "off"]),
        help: "Terminals sizes each shown session to its tile (like attaching does); off watches it at its own size",
    },
    Known {
        key: "ui.splash",
        default: "theme",
        kind: Kind::Choice(&[
            "theme",
            "cockpit",
            "classic",
            "singularity",
            "pulsar",
            "event-horizon",
            "matrix",
            "convergence",
            "cyberdeck",
            "neon",
            "random",
            "off",
        ]),
        help: "what plays as the dashboard starts (any key skips it): theme (the theme's own: cyberpunk's cyberdeck boot, the cosmos themes theirs, the rest the cockpit; it follows the theme), cockpit (an alien ship's bridge dropping out of warp at a black hole, about 1.7 s), classic (the aleph under a band of light, under 1 s), singularity (a point becoming everything: foam, the point, the eruption, young galaxies, 2.6 s), pulsar (a star keeping time: its beams sweeping, a magnetar flare, spinning up to a millisecond pulsar, 2.6 s), event-horizon (a black hole close: lensed stars, its disc and shadow, a star torn apart, falling through the horizon, 2.6 s), matrix (digital rain with your machines in the code, déjà vu, the code freezing and decoding into ALEPH, 2.6 s), convergence (a network learning, your machines its hubs, spiralling into one brilliant core: the AI singularity, 2.6 s), cyberdeck (a neural interface booting: its HUD tracing itself, diagnostics, your machines coming online, the sync completing, 2.6 s), neon (a rain-soaked city at night: your machines as neon signs flickering on, flying traffic, the wet street mirroring it all, 2.6 s), random (a different one each time), off; aleph splash NAME plays one on its own",
    },
    Known {
        key: "ui.splash_ms",
        default: "auto",
        kind: Kind::Text,
        help: "how long the splash plays: auto (each its own, played a little brisk: the cockpit 1.7 s, classic under 1 s, the cosmos 2.6 s) or 1–8 s (3s, 4500ms); the whole of it stretches, the Aleph logo and its hold come after",
    },
    Known {
        key: "ui.splash_end",
        default: "random",
        kind: Kind::Choice(&["random", "form", "fall", "warp"]),
        help: "how the cockpit splash ends: form (the particles gather into the ℵ, it flares and bursts), fall (the ship falls into the ℵ, the sky squeezed into a ring), warp (the stars streak out again to the fleet), or random (one of them each time)",
    },
    Known {
        key: "ui.splash_logo",
        default: "particles",
        kind: Kind::Choice(&["particles", "off"]),
        help: "how a splash ends: particles (the Aleph logo gathers from the scene's own particles and holds for ui.splash_hold) or off (each ends its own way: the cockpit on its fall or warp, the cosmos on their last image); the palette's Splash gallery previews both (l)",
    },
    Known {
        key: "ui.splash_hold",
        default: "700ms",
        kind: Kind::Text,
        help: "how long the Aleph logo stays up at the end of a splash (e.g. 700ms, 1s; 0 for none; up to 3s)",
    },
    Known {
        key: "ui.splash_fx",
        default: "all",
        kind: Kind::List(&[
            "tunnel", "shock", "waves", "spirals", "comet", "flares", "meteors", "constellation",
            "radar", "scanlines", "glitch", "glow",
        ]),
        help: "the cockpit splash's effects, a comma list (all, off, or all but some: all, -glitch): tunnel (warp rings), shock (dropping out), waves (gravitational waves ripple the stars), spirals (matter spiralling in, sparks up the jets), comet, flares, meteors, constellation (your fleet as stars, sessions orbiting), radar, scanlines, glitch (the HUD's holographic flicker), glow (bright stars' halos)",
    },
    Known {
        key: "layout.fleet",
        default: "",
        kind: Kind::Text,
        help: "how Fleet: its list, resources and preview share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main|top|bottom|side; empty: the view's own",
    },
    Known {
        key: "layout.monitor",
        default: "",
        kind: Kind::Text,
        help: "how Monitor's overview: its charts and processes share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main|top|bottom|side; empty: the view's own",
    },
    Known {
        key: "layout.inbox",
        default: "",
        kind: Kind::Text,
        help: "how the Inbox: its list and detail share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main|top|bottom|side; empty: the view's own",
    },
    Known {
        key: "layout.settings",
        default: "",
        kind: Kind::Text,
        help: "how Settings: its list and detail share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main|top|bottom|side; empty: the view's own",
    },
    Known {
        key: "ui.spinner",
        default: "theme",
        kind: Kind::Choice(&["theme", "aleph", "stars", "dots", "line"]),
        help: "the busy spinner: theme (the theme's own: the pulsing ℵ, cyberpunk's glitch, …), aleph (a pulsing ℵ), stars (Claude Code's), dots, line",
    },
    Known {
        key: "ui.ground",
        default: "theme",
        kind: Kind::Choice(&["theme", "terminal"]),
        help: "the background: theme (a theme paints its own ground under the whole screen; auto, dark and light leave the terminal's) or terminal (never painted: your terminal's own background, its transparency too)",
    },
    Known {
        key: "ui.panels",
        default: "theme",
        kind: Kind::Choice(&["theme", "rounded", "sharp", "heavy", "double", "cut", "brackets"]),
        help: "how panels are framed: theme (each theme its own: rounded, cyberpunk's HUD brackets, …), rounded, sharp, heavy, double, cut (chamfered corners ╱──╲) or brackets (only the corners, a HUD's targeting frame)",
    },
    Known {
        key: "ui.ambience",
        default: "subtle",
        kind: Kind::Choice(&["subtle", "rich", "off"]),
        help: "a theme's living details: subtle (one in the header, e.g. cyberpunk's data readout, and a glitch between views), rich (and its ambience in empty space: Tokyo Night's city rain, the Matrix's code, Nord's snow; Cyberpunk has none), off (still); ui.motion = off stops them too",
    },
    Known {
        key: "ui.note_for",
        default: "1s",
        kind: Kind::Text,
        help: "how long the status bar shows what a key just did (\"processes hidden\", \"marked read\") — 1s, 3s, 500ms",
    },
    Known {
        key: "ui.alert_for",
        default: "6s",
        kind: Kind::Text,
        help: "how long the status bar shows a warning or an error",
    },
    Known {
        key: "ui.motion",
        default: "on",
        kind: Kind::Choice(&["on", "reduced", "off"]),
        help: "light motion: on (a session row that needs you breathes softly, one that finishes gets a tick), reduced (only the tick), off — only the text's colour moves, never a background; none with NO_COLOR",
    },
    Known {
        key: "ui.row_bars",
        default: "tokens,context",
        kind: Kind::List(&["tokens", "context"]),
        help: "the tiny bars in each Fleet session row, after its status: tokens (tokens a minute over the last half hour, from its own logs), context (how full its context window is); off for none — rows without numbers keep the room blank, and a list narrower than 96 columns leaves them out",
    },
    Known {
        key: "ui.timeline_hours",
        default: "6",
        kind: Kind::Num,
        help: "how many hours the session timelines show, 1–48: Fleet's timeline rows (g), the preview's strip and Monitor › Timeline (+ - there)",
    },
    Known {
        key: "fleet.timeline",
        default: "false",
        kind: Kind::Bool,
        help: "Fleet's session rows show each session's states over the last ui.timeline_hours (working, needs you, done, idle, at its limit) under an hour axis, instead of folder, title and usage; g switches it",
    },
    Known {
        key: "fleet.board_order",
        default: "needs",
        kind: Kind::Choice(&["needs", "working", "recent", "machine"]),
        help: "Fleet's board (the machines list maximised with z): each machine's sessions needs you first, working first, the most recent change first, or in herdr's order; , steps through them there",
    },
    Known {
        key: "ui_scale",
        default: "comfortable",
        kind: Kind::Choice(&["compact", "comfortable", "large"]),
        help: "how roomy the dashboard is: padding in panels, margins, row spacing (the font itself is your terminal's: cmd/ctrl +)",
    },
    Known {
        key: "icons",
        default: "auto",
        kind: Kind::Choice(&["auto", "nerd", "emoji", "text"]),
        help: "logos for machines, GPUs and agents: auto (real logos when a Nerd Font is installed — set it as your terminal font), nerd, emoji or text",
    },
    Known {
        key: "ui.logos",
        default: "auto",
        kind: Kind::Choice(&["auto", "images", "thumbnails", "ascii", "glyphs", "text"]),
        help: "how logos are drawn: auto (images in kitty, Ghostty, iTerm2 and WezTerm, else Nerd Font glyphs, else names), images (block thumbnails where pictures don't show), thumbnails, ascii, glyphs or text — `aleph icons` shows them all",
    },
    Known {
        key: "ui.logo_size",
        default: "medium",
        kind: Kind::Choice(&["small", "medium", "large"]),
        help: "how big logos are where there's room: small (6×3 cells), medium (8×4) or large (16×8)",
    },
    // a build with the showcase feature only
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.names",
        default: "full",
        kind: Kind::Choice(&["full", "outside-tui", "same-length"]),
        help: "how long stand-ins may be: full (whole names everywhere — the dashboard lays them into its rows, cut with … where a column has no room), outside-tui (whole names in printed output, JSON, Raycast and the web; the dashboard keeps each word's length), same-length (every stand-in as long as the real word)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.universes",
        default: "all",
        kind: Kind::List(&[
            "hhgttg", "trek", "starwars", "odyssey", "borges", "cosmos", "spaceflight", "remedy",
            "valve", "spacegames", "retro", "scifi", "spacefilms",
        ]),
        help: "which worlds names come from, a comma list (or all): hhgttg, trek, starwars, odyssey (2001), borges · cosmos, spaceflight · remedy (Alan Wake, Max Payne, Control), valve (Half-Life, Portal, System Shock, Deus Ex, BioShock, Prey), spacegames (Mass Effect, Halo, Dead Space, Outer Wilds, Homeworld, StarCraft, FTL, Kerbal), retro (Metroid, Doom, Quake, Myst, Metal Gear, Chrono Trigger, Thief) · scifi (Blade Runner, Alien, Terminator, The Matrix, Back to the Future), spacefilms (Interstellar, Dune, Arrival, Contact, The Martian, Moon) — aleph showcase universes lists them",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.theme",
        default: "mixed",
        kind: Kind::Choice(&[
            "mixed", "per-launch", "hhgttg", "trek", "starwars", "odyssey", "borges", "cosmos",
            "spaceflight", "remedy", "valve", "spacegames", "retro", "scifi", "spacefilms",
        ]),
        help: "mixed (every universe that's on, at once), per-launch (one of them, a new one each time the dashboard starts), or one universe always",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.machines",
        default: "ships",
        kind: Kind::List(&["ais", "people", "ships", "places", "things", "mine"]),
        help: "what machines are called, a comma list of kinds: ships (ships & stations), ais (AIs & computers), people, places, things (tech, artefacts, lines everyone knows), any — or mine for only your own words (showcase.words.machines)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.users",
        default: "ais",
        kind: Kind::List(&["ais", "people", "ships", "places", "things", "mine"]),
        help: "what user names are called: ais (AIs & computers), people, ships, places, things, any — or mine for only showcase.words.users",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.things",
        default: "any",
        kind: Kind::List(&["ais", "people", "ships", "places", "things", "mine"]),
        help: "what folders, projects and sessions are called: any (every kind), or a comma list of ais, people, ships, places, things — or mine for only showcase.words.things",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.words.machines",
        default: "",
        kind: Kind::Text,
        help: "your own names for machines, a comma list (spaces become dashes), added to the universes' (or alone with showcase.machines = mine)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.words.users",
        default: "",
        kind: Kind::Text,
        help: "your own names for users, a comma list, added to the universes' (or alone with showcase.users = mine)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.words.things",
        default: "",
        kind: Kind::Text,
        help: "your own names for folders, projects and sessions, a comma list, added to the universes' (or alone with showcase.things = mine)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.flair",
        default: "header,splash,keycast,spotlight,celebrate,hud",
        kind: Kind::List(&["header", "splash", "keycast", "spotlight", "celebrate", "hud"]),
        help: "showcase mode's dress-up, a comma list: header (a gold shimmer, a chip, taglines in the idle footer), splash (the aleph for a second at start), keycast (the keys you press, bottom right — never text you type), spotlight (a caption naming each view, stars in empty ones), celebrate (a supernova where a session finishes, a pulsar's flash where a question is answered, about a second each, over the text, never hiding it), hud (faint cockpit brackets round the content, live readouts at the right of the status bar: machines up, sessions working and waiting, today's spend, a pulsar heartbeat; only ever in spare room); all, or off for none",
    },
    // ---- showcase delight: the cockpit palette
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.palette",
        default: "theme",
        kind: Kind::Choice(&["theme", "cockpit", "normal"]),
        help: "the dashboard's colours in showcase mode: theme (the theme you picked, as it is — normal means the same) or cockpit (deep space over any theme — midnight lines, faint text and selection, nebula teal and rose, Aleph's gold; working blue, needs-you orange, done green, down red and limit violet mean what they always do); dark and light terminals both, NO_COLOR stays plain",
    },
    // ---- showcase delight: sound cues
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sounds",
        default: "off",
        kind: Kind::Choice(&["off", "on"]),
        help: "sound cues in showcase mode: a sound when a session needs you, a turn finishes, a usage limit is hit or a machine goes down (showcase.sound.*), played once per event by whichever process runs the hub — the daemon, else the dashboard — however many windows are open; the demo world's events too; `aleph showcase sound` plays them",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sound.needs",
        default: "Glass",
        kind: Kind::Text,
        help: "the sound when a session needs you: a macOS system sound (Basso, Blow, Bottle, Frog, Funk, Glass, Hero, Morse, Ping, Pop, Purr, Sosumi, Submarine, Tink), a sound file's path, or off; Linux plays the freedesktop theme's nearest (canberra-gtk-play or paplay)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sound.done",
        default: "Hero",
        kind: Kind::Text,
        help: "the sound when a turn finishes: a macOS system sound's name, a sound file's path, or off",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sound.limit",
        default: "Basso",
        kind: Kind::Text,
        help: "the sound when a session stops at its usage limit: a macOS system sound's name, a sound file's path, or off",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sound.down",
        default: "Sosumi",
        kind: Kind::Text,
        help: "the sound when a machine stops answering: a macOS system sound's name, a sound file's path, or off",
    },
    // ---- showcase capture: recordings, posters, your watermark, keycast names
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.keycast",
        default: "names",
        kind: Kind::Choice(&["names", "keys"]),
        help: "what the keycast's keycaps say: names (the key and what it did there — j next session, a attach, ? help) or keys (the keys alone); typed text never shows either way",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.record_key",
        default: "ctrl+r",
        kind: Kind::Text,
        help: "the key that starts and stops recording the dashboard, in any view (ctrl+r: free everywhere outside a terminal you type into; a comma list for more than one); off: only the palette's Showcase: record",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.cast_dir",
        default: if cfg!(target_os = "macos") {
            "~/Movies/Aleph"
        } else {
            "~/Videos/Aleph"
        },
        kind: Kind::Text,
        help: "where recordings go: an MP4 and a GIF each (showcase.video), and the asciinema .cast they're made from when showcase.cast_keep is on; left as it is on Linux, the desktop's own Videos folder (XDG_VIDEOS_DIR) is used",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.video",
        default: "mp4,gif",
        kind: Kind::List(&["mp4", "gif"]),
        help: "what stopping a recording leaves in showcase.cast_dir, a comma list (all, or off): mp4 (H.264, 30 fps, plays anywhere) and gif (15 fps, at most 1280 wide, made from the MP4); made in the background by the ffmpeg already on this computer — nothing is installed, and without it only the .cast is saved; off: the .cast alone",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.video_frame",
        default: "cockpit",
        kind: Kind::Choice(&["cockpit", "plain"]),
        help: "how a recording's video looks: cockpit (the screen in a graphite card like a poster's — gold HUD corners, the ℵ mark, the mode, your watermark — at 1920×1080, the screen fitted inside) or plain (just the terminal, edge to edge, its cells twice a poster's, at most 3840 wide)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.cast_keep",
        default: "false",
        kind: Kind::Bool,
        help: "keep each recording's asciinema .cast beside its MP4 and GIF (aleph showcase render makes them again from it; asciinema play plays it); off: it goes once they're made — and it stays whenever they couldn't be",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.video_font",
        default: "",
        kind: Kind::Text,
        help: "the monospace font a recording's video is drawn with: a .ttf, .otf or .ttc file (a …Regular… file's Bold and Italic siblings are used too); empty: the system's own (Menlo on macOS, fc-match monospace on Linux); symbols it lacks come from the symbol fonts already here",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.cast_handoff",
        default: "pause",
        kind: Kind::Choice(&["pause", "stop"]),
        help: "what a recording does while the dashboard hands the terminal over (herdr attach, the folder browser, a launch card, a prompt): pause (the clock stops, a marker notes it, it goes on when you're back) or stop (it's saved there)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.cast_idle",
        default: "2",
        kind: Kind::Num,
        help: "the longest pause a recording plays back, in seconds — in its MP4 and GIF too (asciinema's idle_time_limit; 0 keeps every pause as long as it was)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.cast_chip",
        default: "true",
        kind: Kind::Bool,
        help: "the ● rec 0:42 chip in the header while recording (it's in the recording too; off keeps it out of both)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.capture_real",
        default: "false",
        kind: Kind::Bool,
        help: "let recordings and posters capture the screen while showcase mode is off — they'd carry your real names, folders and addresses (off: they wait for masked or the demo world, and a recording stops when it goes off)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.poster_dir",
        default: "~/Pictures/Aleph",
        kind: Kind::Text,
        help: "where posters go (the palette's Showcase: poster, aleph showcase snap): an SVG card of the screen, and a PNG beside it when showcase.poster_png can make one",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.poster_frame",
        default: "cockpit",
        kind: Kind::Choice(&["cockpit", "plain"]),
        help: "a poster's frame: cockpit (a graphite card with gold HUD corners, the ℵ mark, the view, the mode and your watermark) or plain (the screen on its background, your watermark in the corner)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.poster_png",
        default: "auto",
        kind: Kind::Choice(&["auto", "rsvg-convert", "qlmanage", "off"]),
        help: "a PNG beside each poster, made by a converter already on this computer: auto (rsvg-convert, else macOS's qlmanage), one of them, or off (the SVG only); nothing is ever installed",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.poster_size",
        default: "160x48",
        kind: Kind::Text,
        help: "the screen aleph showcase snap draws, columns × rows (its --size)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.watermark",
        default: "",
        kind: Kind::Text,
        help: "your own line on a showcase screen and on posters — a name, a handle, a site; it's yours, so it's never masked (empty: none)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.watermark_on",
        default: "tui,raycast,web",
        kind: Kind::List(&["tui", "raycast", "web"]),
        help: "where the watermark shows while showcase mode is on, a comma list: tui (a ribbon set in the rule under the tabs, never over content), raycast (the banner card and the titles), web (the console's banner); posters always carry it",
    },
    // ---- showcase demo flow: the autopilot tour, story mode, time warp, presenter sync
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.tour",
        default: "key",
        kind: Kind::Choice(&["off", "key", "idle"]),
        help: "the autopilot tour, a timed walk through the dashboard for booths, kiosks and recordings — it switches views, moves the selection, opens Focus on a working session and captions each step, and loops; any key or click stops it and does nothing else; it only ever looks (never approves, sends, closes, attaches or types), so it's safe on your masked fleet: off (none), key (started by showcase.tour_key or the palette's Showcase: start the tour), idle (also starts by itself after showcase.tour_idle without a key or a click)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.tour_key",
        default: "ctrl+t",
        kind: Kind::Text,
        help: "the key that starts the autopilot tour, in any view outside a text field or a terminal you type into (ctrl+t: free in every view, beside ctrl+k's palette and ctrl+r's recording; a comma list for more than one, keys as the key map writes them: f8, ctrl+t); off: only the palette's Showcase: start the tour",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.tour_idle",
        default: "2m",
        kind: Kind::Text,
        help: "with showcase.tour = idle: how long without a key or a click before the tour starts by itself (30s, 2m, 5m)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.tour_views",
        default: "fleet,focus,monitor,usage,inbox",
        kind: Kind::List(&[
            "fleet",
            "focus",
            "tile",
            "monitor",
            "overview",
            "cpu",
            "memory",
            "gpu",
            "processes",
            "network",
            "services",
            "usage",
            "inbox",
            "settings",
            "doctor",
        ]),
        help: "the tour's steps, in order, a comma list (all for every one): fleet (every agent on every machine; the selection walks down it), focus (one working agent, full screen), tile (a machine's agents side by side), monitor (the fleet's CPU, GPU and memory), overview (Monitor › Overview: one machine in depth), cpu (Monitor › CPU), memory (Monitor › Memory), gpu (Monitor › GPU), processes (Monitor › Processes), network (Monitor › Network), services (Monitor › Services: local LLM servers), usage (Monitor › Usage: tokens, time and spend), inbox (what needs you, what finished; the selection walks down it), settings (every setting), doctor (Settings › Doctor)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.tour_step",
        default: "8s",
        kind: Kind::Text,
        help: "how long the tour stays on each step (3s at the least)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.story",
        default: "on",
        kind: Kind::Choice(&["on", "off"]),
        help: "story mode, in the demo world: every showcase.story_every its fleet plays its beats through Aleph's own paths — the fleet, the hub's auto-approve and self-heal, the Inbox, desktop banners, the menu bar, celebrations and sounds, Raycast and the web console — the same beat in the same second in every window: on (the beats in showcase.story_beats) or off (the world's own rhythm alone)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.story_every",
        default: "2m",
        kind: Kind::Text,
        help: "how often the demo world's story plays, on the world's clock (1m at the least): with showcase.warp 5, a 2m story plays every 24 s of yours — give it 10m to see it every 2 minutes",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.story_beats",
        default: "all",
        kind: Kind::List(&["ask", "drop", "limit", "finish"]),
        help: "the story's beats, a comma list (all, or off): ask (an agent asks before running a command; every other time the world's own auto-approve rule answers it, in between it waits for you), drop (a machine's herdr stops and the hub's self-heal brings it and its sessions back), limit (a Codex session hits its plan's 5-hour limit, and it resets), finish (a big task finishes); a beat left out leaves that part of the world as it was",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.warp",
        default: "1",
        kind: Kind::Choice(&["1", "2", "5", "10"]),
        help: "time warp, in the demo world: its clock runs this many times as fast, so its sessions' work-ask-finish cycles (11–16 minutes each) and the story fit a short pitch: 1 (real time), 2 (twice as fast), 5 (five times), 10 (ten times); every window agrees, and a change only changes the speed — the world never jumps. What the world keeps (usage, history, the Inbox) stays on your clock",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.sync",
        default: "off",
        kind: Kind::Choice(&["off", "on"]),
        help: "presenter sync: on (Raycast and the web console follow the dashboard you present from — its view, the machine and the session selected, as masked as everything else; the dashboard tells the daemon, so nothing happens without one) or off",
    },
    Known {
        key: "listen",
        default: "true",
        kind: Kind::Bool,
        help: "subscribe to each machine's herdr events, so changes show up the moment they happen",
    },
    Known {
        key: "herdr_view",
        default: "off",
        kind: Kind::Choice(&["off", "needs-you-first"]),
        help: "while the dashboard runs, herdr's own sidebar lists the sessions that need you first (put back when you quit)",
    },
    Known {
        key: "herdr_tokens",
        default: "off",
        kind: Kind::Bool,
        help: "put each machine's CPU, memory and GPU load on its agent panes as herdr tokens ($cpu $mem $gpu $gput) for herdr's sidebar, every 30 s while the dashboard has its stats",
    },
    Known {
        key: "fleet.group",
        default: "tag",
        kind: Kind::Choice(&["tag", "none"]),
        help: "group Fleet's machines by their first tag (host.<machine>.tags; e to edit), with a heading per tag",
    },
    Known {
        key: "inbox.layout",
        default: "overview",
        kind: Kind::Choice(&["overview", "cards", "table"]),
        help: "the Inbox's layout: overview (KPI tiles, a panel per group, the day's activity), cards (live sessions as cards), table (two lines a row); L switches while you look",
    },
    Known {
        key: "inbox.group",
        default: "machine",
        kind: Kind::Choice(&["machine", "day", "agent", "type", "session"]),
        help: "how the Inbox groups what happened (G switches while you look)",
    },
    Known {
        key: "inbox.lifecycle",
        default: "false",
        kind: Kind::Bool,
        help: "list sessions starting and ending in the Inbox, and toast a start; off, the Launches filter still has your starts",
    },
    Known {
        key: "inbox.hide_ended",
        default: "false",
        kind: Kind::Bool,
        help: "hide alerts whose session has ended (e switches it in the Inbox); off, they're dimmed and listed after the live ones",
    },
    Known {
        key: "inbox.auto_archive",
        default: "1d",
        kind: Kind::Text,
        help: "a launch whose session has ended goes to the archive by itself after this long (30m, 1d, 7d; off = never); A in the Inbox shows the archive",
    },
    Known {
        key: "inbox.needs_ttl",
        default: "5m",
        kind: Kind::Text,
        help: "a needs-you alert goes to the archive after this long, even while its session still waits (the live Needs-you lists still show it); answered or ended ones go at once (off = only those)",
    },
    Known {
        key: "inbox.down_grace",
        default: "30s",
        kind: Kind::Text,
        help: "a machine that stops answering counts as down after this long (a dropped packet never files anything); then the Inbox gets an item only if an agent there was working or waiting on you, and nothing while this computer itself is offline",
    },
    Known {
        key: "inbox.machine_back",
        default: "close",
        kind: Kind::Choice(&["close", "lost", "off"]),
        help: "when a machine that was down answers again: close (its outage row gets its end and what happened to the sessions there, and archives itself when nothing was lost), lost (an item only when a session there is gone), off (nothing)",
    },
    Known {
        key: "inbox.finished",
        default: "none",
        kind: Kind::Choice(&["none", "unseen", "unseen-long", "all"]),
        help: "which finished turns the Inbox lists (one row a session a day, with how many): none (the Activity strip and the journal still have them), unseen (not ones that finished on a dashboard's screen in front of you), unseen-long (those, longer than notify.done_min), all",
    },
    Known {
        key: "attach_via",
        default: "ssh",
        kind: Kind::Choice(&["ssh", "mosh"]),
        help: "how attaching reaches a machine: herdr --remote over ssh, or mosh running herdr there (survives roaming and sleep; needs mosh at both ends)",
    },
    Known {
        key: "net.probe",
        default: "1.1.1.1:443",
        kind: Kind::Text,
        help: "what the connection indicator checks: host:port, a TCP connect every few seconds (no data sent)",
    },
    Known {
        key: "herdr_via",
        default: "ssh",
        kind: Kind::Choice(&["ssh", "machine"]),
        help: "reach a machine's herdr over our shared ssh connection (fast) or through `herdr --machine`",
    },
    Known {
        key: "wall_layout",
        default: "grid",
        kind: Kind::Choice(&["grid", "main", "rows", "cols"]),
        help: "tiling layout of the wall",
    },
    Known {
        key: "mux_keys",
        default: "herdr",
        kind: Kind::Choice(&["herdr", "tmux"]),
        help: "Focus / Tile prefix key: herdr's own (its config.toml [keys] prefix) or tmux's ctrl+b",
    },
    Known {
        key: "mux_prefix",
        default: "",
        kind: Kind::Text,
        help: "a prefix key of your own for Focus / Tile (e.g. ctrl+a); empty = mux_keys decides",
    },
    Known {
        key: "mux_leave",
        default: "",
        kind: Kind::Text,
        help: "more keys that stop typing into a pane in Focus / Tile and give them back to Aleph, besides focus.exit (comma-separated: f12, alt+q, …); a click outside the pane stops it too",
    },
    Known {
        key: "wall_max",
        default: "9",
        kind: Kind::Num,
        help: "tiles per wall page",
    },
    Known {
        key: "hook.pre_new",
        default: "",
        kind: Kind::Text,
        help: "shell command run in the new pane before the agent starts",
    },
    Known {
        key: "hook.post_new",
        default: "",
        kind: Kind::Text,
        help: "local command run after a session starts (CH_HOST, CH_DIR, CH_NAME, CH_PANE)",
    },
    Known {
        key: "analytics.enabled",
        default: "true",
        kind: Kind::Bool,
        help: "read the agents' own logs on every machine for tokens, models, time and cost (aleph usage) — only ids, numbers, models and folders leave a machine, never conversation text",
    },
    Known {
        key: "analytics.prices_sync",
        default: "true",
        kind: Kind::Bool,
        help: "read the public price lists hourly (OpenRouter, models.dev, LiteLLM; no login); a change applies from then on, never to past usage",
    },
    Known {
        key: "analytics.include",
        default: "",
        kind: Kind::Text,
        help: "host:agent globs analytics covers (empty = everything), e.g. b31:*,*:claude",
    },
    Known {
        key: "analytics.exclude",
        default: "",
        kind: Kind::Text,
        help: "host:agent globs analytics never reads (wins over analytics.include)",
    },
    Known {
        key: "analytics.backfill",
        default: "90d",
        kind: Kind::Text,
        help: "how far back the first read goes, by log file age: 90d, 12w, 1y or all",
    },
    Known {
        key: "analytics.keep",
        default: "400d",
        kind: Kind::Text,
        help: "how long usage records are kept (aleph usage prune): 400d, 2y or all",
    },
    Known {
        key: "analytics.interval",
        default: "60",
        kind: Kind::Num,
        help: "seconds between the daemon's reads of every machine's logs: tokens, $ and quota left, the same in the menu bar, Raycast and the dashboard (60 at least)",
    },
    Known {
        key: "analytics.max_seconds",
        default: "20",
        kind: Kind::Num,
        help: "per machine and read: start no new log file after this many seconds (the rest waits)",
    },
    Known {
        key: "analytics.max_mb",
        default: "64",
        kind: Kind::Num,
        help: "per machine and read: read at most this many MB of logs (the rest waits)",
    },
    Known {
        key: "analytics.cost",
        default: "on",
        kind: Kind::Choice(&["on", "off"]),
        help: "show $ estimates next to tokens (list prices per model; analytics.prices.<model> overrides one)",
    },
    Known {
        key: "usage.layout",
        default: "tabs",
        kind: Kind::Choice(&["tabs", "page", "pivot"]),
        help: "Monitor › Usage as tabs (Overview, Breakdown, Trends, Turns & sessions, Insights: v moves), one long page (j/k scroll it) or a pivot you build (rows b, split s, measure n, chart c); L cycles them",
    },
    Known {
        key: "timeline.keep",
        default: "120d",
        kind: Kind::Text,
        help: "how long the timeline's steps are kept (prompts, tool calls, subagents — names and numbers, never what was said): 120d, 1y or all",
    },
    Known {
        key: "timeline.fold",
        default: "smart",
        kind: Kind::Choice(&["smart", "open", "folded"]),
        help: "how a session's timeline starts: smart (repeats folded, finished prompts closed to a line; the running one, errors and costly steps open), open (only repeats folded), folded (every section to its summary line)",
    },
    Known {
        key: "timeline.mine",
        default: "false",
        kind: Kind::Bool,
        help: "Monitor › Timeline lists only the machine on its strip (M); ctrl+[ ] moves to another",
    },
    Known {
        key: "timeline.conversation",
        default: "on",
        kind: Kind::Choice(&["on", "off"]),
        help: "Monitor › Timeline shows the selected conversation beside the list (C); G opens it whichever",
    },
    Known {
        key: "timeline.group",
        default: "machine",
        kind: Kind::Choice(&["machine", "project", "agent", "account"]),
        help: "what Monitor › Timeline groups its list by (O)",
    },
    Known {
        key: "timeline.words",
        default: "live",
        kind: Kind::Choice(&["live", "off"]),
        help: "the timeline's words: live (a prompt's first line and a tool's target, read from the machine only while that session is on screen, secrets masked, never kept) or off (names and numbers only)",
    },
    Known {
        key: "analytics.account_label",
        default: "email",
        kind: Kind::Choice(&["email", "plan", "nickname"]),
        help: "how an agent's account is named in usage: email (its sign-in and plan: you@work.com · Max 20x), plan (Claude Max 20x · a3f2c1, no email shown), nickname (Claude account 1 · a3f2c1 until you name it); analytics.accounts.<id> names one, whatever this says",
    },
    Known {
        key: "analytics.claude_roots",
        default: "",
        kind: Kind::Text,
        help: "more Claude Code config folders to read on every machine, comma-separated (~/… allowed): $CLAUDE_CONFIG_DIR, ~/.claude, ~/.claude-* and ~/.config/claude-* are found on their own; a machine's own: host.<alias>.claude_roots",
    },
    Known {
        key: "analytics.codex_homes",
        default: "",
        kind: Kind::Text,
        help: "more Codex homes (CODEX_HOME) to read on every machine, comma-separated: $CODEX_HOME, ~/.codex and ~/.codex-* are found on their own; a machine's own: host.<alias>.codex_homes",
    },
    Known {
        key: "journal.include",
        default: "all",
        kind: Kind::List(&["prompts", "finals", "files", "cost"]),
        help: "what the session journal (a Markdown file a day: aleph journal, the palette's Journal) says about each session: prompts (the ones sent through Aleph — the dashboard, aleph send, the queue, phone replies, new sessions, hand-offs, races; kept in a small pruned record, secrets masked, only while this is on; text typed straight into herdr isn't seen), finals (each finished turn in the agent's own words, else its title), files (the day's commits in its folder and what isn't committed, by git on each machine), cost (time at work, tokens and $, per session and per turn); all, off (no journal), or all but some (-files)",
    },
    Known {
        key: "journal.dir",
        default: "",
        kind: Kind::Text,
        help: "where the session journal's files go (YYYY-MM-DD.md); empty: ~/.local/share/aleph/journal (Aleph's data folder, $XDG_DATA_HOME)",
    },
    Known {
        key: "handoff.summary",
        default: "agent",
        kind: Kind::Choice(&["agent", "aleph"]),
        help: "who writes a hand-off note (h in Fleet, aleph handoff): agent (the session's own agent, asked with the prompt library's handoff, into ~/.cache/aleph/handoff/ on its machine, never the repo) or aleph (no agent: the task, git's status and diff stat, recent commits, the end of its screen)",
    },
    Known {
        key: "handoff.fallback",
        default: "true",
        kind: Kind::Bool,
        help: "with handoff.summary = agent: when the agent is at its usage limit, or doesn't write its note within 6 minutes, Aleph writes it instead (off: the hand-off stops)",
    },
    Known {
        key: "handoff.to",
        default: "",
        kind: Kind::Text,
        help: "the agent a hand-off goes to first (empty: Codex from Claude Code, Claude Code from anything else); h in Fleet lists every agent on the machine",
    },
    Known {
        key: "editor",
        default: "auto",
        kind: Kind::Text,
        help: "what o (open in editor) uses: auto (the first of Cursor, VS Code, Zed on this computer), cursor, code, zed, windsurf, or your own command line with {path}, {host} and {remote} (ssh-remote+<alias>); another machine's folder opens over the editor's Remote-SSH (Zed: ssh://)",
    },
    Known {
        key: "race.agents",
        default: "claude,codex",
        kind: Kind::Text,
        help: "the agents a race runs, 2 to 4, a comma list (aleph race --agents overrides it); each gets the same prompt in a git worktree of its own",
    },
    Known {
        key: "openrouter.key",
        default: "secret:openrouter",
        kind: Kind::Text,
        help: "your OpenRouter API key, kept in the Keychain / Secret Service (aleph secret set openrouter, or setup's Agents step); off = none. Sessions of openrouter.agents get it as OPENROUTER_API_KEY (only through herdr's socket: never on a command line, on disk or in a log), and Usage shows its spend",
    },
    Known {
        key: "openrouter.agents",
        default: crate::openrouter::DEFAULT_AGENTS,
        kind: Kind::Text,
        help: "the agents whose sessions get the OpenRouter key: agent ids (the ones that read OPENROUTER_API_KEY themselves), all (-id leaves one out), or off. OpenCode, Kilo, Pi and OMP also get a model picker in the new-session form",
    },
    Known {
        key: "openrouter.models",
        default: "all",
        kind: Kind::Choice(&["all", "free", "paid"]),
        help: "which of OpenRouter's models to use: all, only the free ones ($0, the :free variants), or only paid ones. The new-session form's model picker lists just those, and OpenCode sessions Aleph starts get the same as their own config (OPENCODE_CONFIG_CONTENT: provider.openrouter.whitelist / blacklist; your opencode.json is never touched)",
    },
    Known {
        key: "ui.whats_new",
        default: "on",
        kind: Kind::Choice(&["on", "off"]),
        help: "after an update, the dashboard shows what changed since you last opened it, once (on), or not (off; aleph whats-new and the palette's What's new still show it)",
    },
    Known {
        key: "tour",
        default: "",
        kind: Kind::Choice(&["", "done"]),
        help: "the dashboard's four-card tour after setup: done once seen (empty shows it again after aleph setup)",
    },
    Known {
        key: "herdr.update_handoff",
        default: "false",
        kind: Kind::Bool,
        help: "aleph update moves a machine's running herdr to the new version live (herdr update --handoff), so sessions carry on without a restart",
    },
    Known {
        key: "update_repo",
        default: "",
        kind: Kind::Text,
        help: "GitHub owner/repo that publishes releases, for self-update",
    },
    Known {
        key: "update_pubkey",
        default: "",
        kind: Kind::Text,
        help: "the minisign public key releases are signed with (RW…): with it, self-update installs only a release whose SHA256SUMS.minisig it verifies",
    },
    Known {
        key: "raycast_dirs",
        default: "~/.raycast-scripts",
        kind: Kind::Text,
        help: "Raycast script folders (colon-separated)",
    },
    // a showcase build's safety nets for live demos (showcase/safety/)
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.screens",
        default: "masked",
        kind: Kind::Choice(&["masked", "redact", "hide"]),
        help: "what agents' screens show while showcase mode masks your real fleet — in Focus and Tile, previews, aleph peek, Raycast and the web console, and notifications that quote one: masked (as they are, names, folders and addresses swapped), redact (the agent's own frame stays readable — borders, status and footer lines, spinners, token counts, model names, its questions and their options — and every word it or you wrote becomes alien script of the same shape), hide (a card: screen hidden for the demo)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.demo_screens",
        default: "masked",
        kind: Kind::Choice(&["masked", "redact", "hide"]),
        help: "the same for the demo world's screens: they're made up, so they show as they are by default; redact or hide to rehearse how a real demo looks",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.screens_keep",
        default: "",
        kind: Kind::Text,
        help: "with screens = redact: more words or phrases that mark a screen line as the agent's own and keep it readable, a comma list (your status line's labels, a tool's banner); case doesn't matter",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.panic_key",
        default: "f12",
        kind: Kind::Text,
        help: "the panic key: in any dashboard view (Focus typing included) it blanks every window at once — dashboards, Raycast and its menu bar, the web console — to a starfield and the ℵ, and again brings them back; aleph showcase panic does it from a shell. It works with showcase on or off and changes nothing real. Keys as the key map writes them (f12, f9, ctrl+4, …), a comma list; off for none",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.clipboard",
        default: "masked",
        kind: Kind::Choice(&["masked", "off", "real"]),
        help: "what a copy puts on the clipboard while showcase mode is on — the dashboard's copy keys, Raycast's copy actions, the web console's Copy buttons: masked (the text as the screen shows it), off (nothing: copies are refused, with a note), real (the real text: careful pasting it where the audience can see)",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.money",
        default: "real",
        kind: Kind::Choice(&["real", "scaled", "hidden"]),
        help: "$ amounts while showcase mode is on — Usage, the header's today, Fleet and machine cards, aleph usage (and --json), the daemon's usage answers for Raycast and the menu bar, the web console: real, scaled (every amount times showcase.money_scale, so totals still add up), hidden ($—). Tokens, quota % and plans stay as they are",
    },
    #[cfg(feature = "showcase")]
    Known {
        key: "showcase.money_scale",
        default: "0.37",
        kind: Kind::Num,
        help: "with money = scaled: what every $ amount is multiplied by (any positive number; 0.37 turns $100 into $37)",
    },
];

pub fn known(key: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.key == key)
}

/// Settings an environment variable wins over: (key, the `ALEPH_` name).
const ENV_OVER: &[(&str, &str)] = &[
    ("refresh", "REFRESH"),
    ("browse_ttl", "BROWSE_TTL"),
    ("ssh_host", "SSH_HOST"),
    ("machine", "MACHINE"),
];

/// `(ALEPH_REFRESH, its value)` when the environment sets `key` for this run.
pub fn env_override(key: &str) -> Option<(String, String)> {
    let (_, n) = ENV_OVER.iter().find(|(k, _)| *k == key)?;
    crate::brand::env(n).map(|v| (format!("ALEPH_{n}"), v))
}

/// Is `v` a valid value for `key`? Known settings are checked by kind (and
/// a few by meaning); anything else is accepted.
pub fn check(key: &str, v: &str) -> Result<(), String> {
    if let Some(id) = key.strip_prefix("service.") {
        return crate::services::check_setting(id, v);
    }
    if let Some(m) = key.strip_prefix("analytics.prices.") {
        if m.is_empty() {
            return Err("analytics.prices.<model> needs a model id".into());
        }
        return crate::prices::parse_override(v)
            .map(|_| ())
            .map_err(|e| format!("analytics.prices.{m}: {e}"));
    }
    if let Some(id) = key.strip_prefix("analytics.accounts.") {
        if id.len() < 6 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "analytics.accounts.{id}: an account's id, as aleph usage accounts shows it (a3f2c1)"
            ));
        }
        return Ok(());
    }
    let Some(k) = known(key) else {
        return Ok(());
    };
    // auto, or a length from 1 to 8 s
    if key == "ui.splash_ms" {
        return match v {
            "" | "auto" => Ok(()),
            _ => match crate::log::parse_duration(v) {
                Some(d) if (1000..=8000).contains(&d.as_millis()) => Ok(()),
                Some(_) => Err(format!("ui.splash_ms {v}: between 1s and 8s")),
                None => Err(format!(
                    "ui.splash_ms {v}: auto, or a length like 3s or 4500ms"
                )),
            },
        };
    }
    // a built-in theme, or one of yours ([themes.<name>], read at start)
    if key == "theme" {
        let yours = crate::ui::c::custom_names();
        return if crate::ui::c::THEMES.iter().any(|t| t.0 == v) || yours.iter().any(|n| n == v) {
            Ok(())
        } else {
            let mut all: Vec<String> = crate::ui::c::THEMES
                .iter()
                .map(|t| t.0.to_string())
                .collect();
            all.extend(yours);
            Err(format!("{v}: not a theme — {}", all.join(", ")))
        };
    }
    if key == "agent" {
        return match crate::agents::resolve(v)? {
            id if id == v && id != crate::agents::SHELL => Ok(()),
            crate::agents::SHELL => Err("the default agent can't be a plain shell".into()),
            id => Err(format!("agent takes an agent id: {id}")),
        };
    }
    if key == "close.grace" && crate::closing::parse_grace(v).is_none() {
        return Err(format!("{v}: a duration like 5s, 30s or 1m (0 = at once)"));
    }
    let ok = match k.kind {
        Kind::Bool => ["true", "false", "1", "0", "on", "off", "yes", "no"].contains(&v),
        Kind::Num => v.parse::<f64>().is_ok_and(|x| x >= 0.0),
        Kind::Choice(c) => c.contains(&v),
        Kind::List(c) => v
            .split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .all(|x| {
                let x = x.strip_prefix('-').unwrap_or(x);
                c.contains(&x) || LIST_WORDS.contains(&x)
            }),
        Kind::Text => true,
    };
    if !ok {
        return Err(format!("bad value for {key}: {v}"));
    }
    match key {
        "agents" => crate::agents::Filter::parse(v).map(|_| ()),
        "openrouter.agents" => {
            for w in v.split(',').map(str::trim).filter(|w| !w.is_empty()) {
                let w = w.strip_prefix('-').unwrap_or(w);
                if !["all", "off", "none"].contains(&w) {
                    crate::agents::resolve(w)?;
                }
            }
            Ok(())
        }
        "openrouter.key" if v.trim().contains(char::is_whitespace) => {
            Err("openrouter.key is secret:openrouter (the keystore), the key itself, or off".into())
        }
        "wraps" if v.trim().is_empty() || v.contains(char::is_whitespace) => {
            Err("wraps is one program name (claude, codex, …)".into())
        }
        "analytics.backfill" | "analytics.keep" | "timeline.keep" => {
            crate::usage::parse_days(v).map(|_| ())
        }
        _ => Ok(()),
    }
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// Expand a leading `~/`.
pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(r) => home().join(r),
        None if p == "~" => home(),
        None => PathBuf::from(p),
    }
}

/// Unit tests never touch the real config, state or cache: without an
/// explicit XDG override they get a private folder per test process.
fn base_dir(var: &str, real: &[&str]) -> PathBuf {
    if let Some(v) = std::env::var_os(var).filter(|v| !v.is_empty()) {
        return PathBuf::from(v);
    }
    if cfg!(test) {
        return std::env::temp_dir()
            .join(format!("aleph-test-{}", std::process::id()))
            .join(var.to_lowercase());
    }
    let mut p = home();
    for r in real {
        p.push(r);
    }
    p
}

/// `$XDG_CACHE_HOME/aleph` (default ~/.cache/aleph).
pub fn cache_dir() -> PathBuf {
    base_dir("XDG_CACHE_HOME", &[".cache"]).join(crate::brand::DIR)
}

/// `$XDG_STATE_HOME/aleph` (default ~/.local/state/aleph).
pub fn state_dir() -> PathBuf {
    base_dir("XDG_STATE_HOME", &[".local", "state"]).join(crate::brand::DIR)
}

/// `$XDG_DATA_HOME/aleph` (default ~/.local/share/aleph): what Aleph makes
/// for you to keep (backups, the journal).
pub fn data_dir() -> PathBuf {
    base_dir("XDG_DATA_HOME", &[".local", "share"]).join(crate::brand::DIR)
}

/// Where what the fleet's world writes lives (its events, history, session
/// memory, usage, queue, notes, …): `state_dir()`, or showcase's demo world
/// sandbox in it, so a made-up fleet never mixes with the real one. Daemon
/// sockets, locks, secrets, devices and setup stay in `state_dir()`.
pub fn world_state() -> PathBuf {
    crate::showcase::sandbox(state_dir())
}

/// `world_state`'s cache side: the fleet snapshot, herdr versions, probe and
/// services caches, status.json.
pub fn world_cache() -> PathBuf {
    crate::showcase::sandbox(cache_dir())
}

/// Write a file atomically (temp + fsync + rename), creating parent folders.
/// The temp name is unique per process *and* call, so threads never collide.
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, data, None)
}

/// `write_atomic`, private to this user (0600): config, secrets, history.
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    write_atomic_mode(path, data, Some(0o600))
}

fn write_atomic_mode(
    path: &std::path::Path,
    data: &[u8],
    mode: Option<u32>,
) -> std::io::Result<()> {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}.{n}", std::process::id()));
    {
        let mut f = fs::File::create(&tmp)?;
        #[cfg(unix)]
        if let Some(m) = mode {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(m))?;
        }
        f.write_all(data)?;
        let _ = f.sync_all();
    }
    fs::rename(tmp, path)
}

/// Read a settings file: TOML when it's a `.toml` file, else key=value lines.
fn read_store(path: &std::path::Path) -> Store {
    let text = fs::read_to_string(path).unwrap_or_default();
    if path.extension().and_then(|e| e.to_str()) == Some("toml") {
        match text.parse::<toml_edit::DocumentMut>() {
            Ok(d) => Store::Toml(d),
            Err(e) => {
                // never write over a file we couldn't read: Aleph runs on
                // defaults and `aleph config check` says what's wrong
                crate::log::error("config", &format!("{}: {e}", path.display()));
                BROKEN.store(true, std::sync::atomic::Ordering::Relaxed);
                Store::Toml(toml_edit::DocumentMut::new())
            }
        }
    } else {
        Store::Flat(text.lines().map(str::to_string).collect())
    }
}

/// Set when `aleph.toml` didn't parse: writes are refused until it's fixed.
static BROKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The settings file didn't parse (writes are refused until it's fixed).
pub fn broken() -> bool {
    BROKEN.load(std::sync::atomic::Ordering::Relaxed)
}

/// The old key=value file → a commented `aleph.toml`, checked to read back to
/// exactly the same settings before anything changes; the old file is kept
/// as `config.bak`.
fn migrate_flat(old: &std::path::Path, new: &std::path::Path) -> Result<(), String> {
    let text = fs::read_to_string(old).map_err(|e| e.to_string())?;
    let c = Config {
        path: old.to_path_buf(),
        store: Store::Flat(text.lines().map(str::to_string).collect()),
        under: Vec::new(),
        over: None,
        cache: Vec::new(),
    };
    let mut want = c.main_entries();
    let out = crate::tomlcfg::render(&want);
    let doc: toml_edit::DocumentMut = out.parse().map_err(|e| format!("{e}"))?;
    let mut back = crate::tomlcfg::entries(&doc);
    want.sort();
    back.sort();
    if back != want {
        return Err("the converted file didn't read back the same".into());
    }
    write_private(new, out.as_bytes()).map_err(|e| e.to_string())?;
    let bak = old.with_file_name("config.bak");
    fs::rename(old, &bak).map_err(|e| e.to_string())?;
    crate::log::info(
        "config",
        &format!(
            "{} settings moved to {} (the old file is {})",
            want.len(),
            new.display(),
            bak.display()
        ),
    );
    Ok(())
}

/// `include = ["~/team/aleph.toml", "presets/*.toml"]`: paths relative to
/// the including file; a `*` in the file name matches like a shell glob.
fn expand_include(base: &std::path::Path, inc: &str) -> Vec<PathBuf> {
    let p = if inc.starts_with('~') {
        expand(inc)
    } else if inc.starts_with('/') {
        PathBuf::from(inc)
    } else {
        base.join(inc)
    };
    let name = p
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("")
        .to_string();
    if !name.contains('*') {
        return vec![p];
    }
    let (pre, suf) = name.split_once('*').unwrap_or((&name, ""));
    let dir = p.parent().map(PathBuf::from).unwrap_or_default();
    let mut v: Vec<PathBuf> = fs::read_dir(&dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|q| {
                    q.file_name()
                        .and_then(|f| f.to_str())
                        .is_some_and(|f| f.starts_with(pre) && f.ends_with(suf))
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// An advisory lock around read-modify-write of the settings file (other
/// Aleph processes change it too) and of small state files (devices, push
/// subscriptions). Gives up after 2 s rather than hang.
pub(crate) struct FileLock(#[allow(dead_code)] Option<fs::File>);

impl FileLock {
    pub(crate) fn take(path: &std::path::Path) -> FileLock {
        let f = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .ok();
        if let Some(f) = &f {
            let t0 = std::time::Instant::now();
            while f.try_lock().is_err() {
                if t0.elapsed() > std::time::Duration::from_secs(2) {
                    return FileLock(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        FileLock(f)
    }
}

/// Keep the version about to be replaced (the last 20) in
/// `state/config-history/`, for `aleph config undo`.
fn keep_history(path: &std::path::Path) {
    let Ok(data) = fs::read(path) else { return };
    let dir = state_dir().join("config-history");
    let name = path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("config");
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _ = write_private(
        &dir.join(format!("{name}.{}.{n}", crate::cache::now())),
        &data,
    );
    if let Ok(rd) = fs::read_dir(&dir) {
        let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        // each file's time read once: another process saving now would change it
        // mid-sort, and a sort whose keys move panics
        v.sort_by_cached_key(|p| p.metadata().and_then(|m| m.modified()).ok());
        while v.len() > 20 {
            let _ = fs::remove_file(v.remove(0));
        }
    }
}

/// The newest saved version of the settings file (`aleph config undo`).
pub fn history_latest() -> Option<PathBuf> {
    let dir = state_dir().join("config-history");
    let mut v: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    v.sort_by_cached_key(|p| p.metadata().and_then(|m| m.modified()).ok());
    v.pop()
}

impl Config {
    /// The config directory (`$XDG_CONFIG_HOME/aleph`).
    pub fn dir() -> PathBuf {
        base_dir("XDG_CONFIG_HOME", &[".config"]).join(crate::brand::DIR)
    }

    /// The main settings file: `ALEPH_CONFIG`, else `aleph.toml` — moving the
    /// old `config` (key=value) file over first, once.
    pub fn path() -> PathBuf {
        if let Some(p) = crate::brand::env_os("CONFIG") {
            return PathBuf::from(p);
        }
        let d = Self::dir();
        let toml = d.join("aleph.toml");
        let old = d.join("config");
        if !toml.exists() && old.exists() {
            // one process converts; the others wait for it and then read the result
            let _l = FileLock::take(&d.join(".migrate.lock"));
            if !toml.exists() && old.exists() {
                if let Err(e) = migrate_flat(&old, &toml) {
                    crate::log::warn(
                        "config",
                        &format!("kept {} (couldn't convert it: {e})", old.display()),
                    );
                    return old;
                }
            }
        }
        if toml.exists() || !old.exists() {
            toml
        } else {
            old
        }
    }

    pub fn load() -> Config {
        let path = Self::path();
        let store = read_store(&path);
        let mut c = Config {
            path,
            store,
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.load_layers();
        c.recache();
        c
    }

    /// `in_memory` with a file of its own: a test that saves settings never
    /// reads another test's back.
    #[cfg(test)]
    pub fn in_memory_own(lines: &[&str]) -> Config {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let mut c = Config::in_memory(lines);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        c.path = std::env::temp_dir().join(format!("chc-own-{}-{n}", std::process::id()));
        let _ = std::fs::remove_file(&c.path);
        c
    }

    #[cfg(test)]
    pub fn in_memory(lines: &[&str]) -> Config {
        let mut c = Config {
            path: std::env::temp_dir().join(format!("chc-mem-{}", std::process::id())),
            store: Store::Flat(lines.iter().map(|s| s.to_string()).collect()),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.recache();
        c
    }

    /// `include = [...]` files (lower) and the `ALEPH_PROFILE` file (higher).
    fn load_layers(&mut self) {
        self.under.clear();
        self.over = None;
        if let Store::Toml(doc) = &self.store {
            let base = self.path.parent().map(PathBuf::from).unwrap_or_default();
            let list: Vec<String> = doc
                .get("include")
                .and_then(|i| i.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            for inc in list {
                for p in expand_include(&base, &inc) {
                    if p == self.path {
                        continue;
                    }
                    if let Ok(t) = fs::read_to_string(&p) {
                        match t.parse::<toml_edit::DocumentMut>() {
                            Ok(d) => self.under.push((p, crate::tomlcfg::entries(&d))),
                            Err(e) => crate::log::warn("config", &format!("{}: {e}", p.display())),
                        }
                    }
                }
            }
        }
        // the profile in use (`aleph profile use`), or ALEPH_PROFILE's
        if let Some(name) = crate::profile::active_name() {
            let p = Self::dir().join("profiles").join(format!("{name}.toml"));
            if let Ok(t) = fs::read_to_string(&p) {
                if let Ok(d) = t.parse::<toml_edit::DocumentMut>() {
                    self.over = Some((p, crate::tomlcfg::entries(&d)));
                }
            }
        }
    }

    /// The main file's own settings.
    fn main_entries(&self) -> Vec<(String, String)> {
        match &self.store {
            Store::Toml(doc) => crate::tomlcfg::entries(doc),
            Store::Flat(lines) => {
                let mut out: Vec<(String, String)> = Vec::new();
                for l in lines {
                    if l.trim_start().starts_with('#') {
                        continue;
                    }
                    if let Some((k, v)) = l.split_once('=') {
                        let (k, v) = (k.trim().to_string(), v.trim().to_string());
                        out.retain(|(x, _)| *x != k);
                        if !v.is_empty() {
                            out.push((k, v));
                        }
                    }
                }
                out
            }
        }
    }

    fn recache(&mut self) {
        let mut c: Vec<(String, String, Source)> = Vec::new();
        let mut put = |k: String, v: String, src: Source| {
            c.retain(|(x, _, _)| *x != k);
            if !v.is_empty() {
                c.push((k, v, src));
            }
        };
        for (p, es) in &self.under {
            for (k, v) in es {
                put(k.clone(), v.clone(), Source::Include(p.clone()));
            }
        }
        for (k, v) in self.main_entries() {
            put(k, v, Source::Main);
        }
        if let Some((p, es)) = &self.over {
            for (k, v) in es {
                put(k.clone(), v.clone(), Source::Profile(p.clone()));
            }
        }
        self.cache = c;
    }

    /// Your own value of a setting: the main file and its includes, without
    /// the profile in use (as written: a `secret:NAME` stays one).
    pub fn get_own(&self, key: &str) -> Option<String> {
        let mut v = None;
        for (_, es) in &self.under {
            if let Some((_, x)) = es.iter().rev().find(|(k, _)| k == key) {
                v = Some(x.clone());
            }
        }
        if let Some((_, x)) = self
            .main_entries()
            .into_iter()
            .rev()
            .find(|(k, _)| k == key)
        {
            v = Some(x);
        }
        v.filter(|x| !x.is_empty())
    }

    /// Changes when any file settings come from does: the main file, its
    /// includes, every profile and the one in use. A long-running process
    /// (the daemon, a dashboard) takes it *before* reading the settings and
    /// compares it now and then, so a change during a read isn't missed.
    pub fn stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut files: Vec<PathBuf> = self.under.iter().map(|(p, _)| p.clone()).collect();
        files.push(self.path.clone());
        files.push(crate::profile::state_file());
        let mut profiles: Vec<PathBuf> = fs::read_dir(crate::profile::dir())
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .collect();
        profiles.sort();
        files.extend(profiles);
        for p in files {
            p.hash(&mut h);
            fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos()
                })
                .hash(&mut h);
        }
        h.finish()
    }

    /// A setting's value; `secret:NAME` comes from the keystore (`secrets.rs`).
    pub fn get(&self, key: &str) -> Option<String> {
        let v = self.get_raw(key)?;
        if v.starts_with("secret:") {
            return crate::secrets::resolve(&v);
        }
        Some(v)
    }

    /// As written in the file (`secret:NAME` stays a reference).
    pub fn get_raw(&self, key: &str) -> Option<String> {
        // showcase's demo world has machines (and a few settings) of its own
        if let Some(v) = crate::showcase::setting(key) {
            return v;
        }
        self.cache
            .iter()
            .rev()
            .find(|(k, _, _)| k == key)
            .map(|(_, v, _)| v.clone())
    }

    /// Where `key`'s value comes from (None = the default).
    pub fn source(&self, key: &str) -> Option<Source> {
        self.cache
            .iter()
            .rev()
            .find(|(k, _, _)| k == key)
            .map(|(_, _, s)| s.clone())
    }

    /// Value or the documented default.
    pub fn get_or(&self, key: &str) -> String {
        self.get(key)
            .or_else(|| known(key).map(|k| k.default.to_string()))
            .unwrap_or_default()
    }

    pub fn flag(&self, key: &str) -> bool {
        matches!(
            self.get_or(key).to_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    }

    /// A whole number (`20`, `20.0` and `20.7` all read as 20; junk is 0).
    pub fn num(&self, key: &str) -> u64 {
        self.get_or(key)
            .trim()
            .parse::<f64>()
            .map(|x| {
                if x.is_finite() && x > 0.0 {
                    x as u64
                } else {
                    0
                }
            })
            .unwrap_or(0)
    }

    /// Every setting currently in effect (layers merged; later wins).
    pub fn entries(&self) -> Vec<(String, String)> {
        self.cache
            .iter()
            .map(|(k, v, _)| (k.clone(), v.clone()))
            .collect()
    }

    /// Keys starting with `prefix`, with the prefix removed.
    pub fn with_prefix(&self, prefix: &str) -> Vec<(String, String)> {
        self.entries()
            .into_iter()
            .filter_map(|(k, v)| k.strip_prefix(prefix).map(|r| (r.to_string(), v)))
            .collect()
    }

    /// Change the main file: under its lock, from what's on disk now (another
    /// process may have changed it), with the old version kept in history.
    fn edit(&mut self, f: impl FnOnce(&mut Store)) -> std::io::Result<()> {
        if BROKEN.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::other(format!(
                "{} has an error — fix it first (aleph config check shows where)",
                self.path.display()
            )));
        }
        let name = self
            .path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("config");
        let _lock = FileLock::take(&self.path.with_file_name(format!(".{name}.lock")));
        if self.path.exists() {
            self.store = read_store(&self.path);
        }
        keep_history(&self.path);
        f(&mut self.store);
        self.save()?;
        self.recache();
        Ok(())
    }

    /// Change the TOML document itself (presets with panes don't fit flat
    /// keys): like `set`, under the lock, from what's on disk, with history.
    pub fn edit_doc(&mut self, f: impl FnOnce(&mut toml_edit::DocumentMut)) -> std::io::Result<()> {
        if !self.is_toml() {
            return Err(std::io::Error::other(
                "this needs aleph.toml — the old key=value file can't hold it (aleph config check moves it)",
            ));
        }
        self.edit(|st| {
            if let Store::Toml(doc) = st {
                f(doc)
            }
        })
    }

    pub fn set(&mut self, key: &str, val: &str) -> std::io::Result<()> {
        if let Some(e) = crate::showcase::refuse_setting(key) {
            return Err(std::io::Error::other(e));
        }
        if self.in_profile(key) {
            return self.edit_profile(key, Some(val));
        }
        self.edit(|st| match st {
            Store::Flat(lines) => {
                lines.retain(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != key));
                if lines.is_empty() {
                    lines.push("# Aleph settings — edit freely, or use `aleph settings …`".into());
                }
                lines.push(format!("{key}={val}"));
            }
            Store::Toml(doc) => crate::tomlcfg::set(doc, key, val),
        })
    }

    pub fn unset(&mut self, key: &str) -> std::io::Result<()> {
        if let Some(e) = crate::showcase::refuse_setting(key) {
            return Err(std::io::Error::other(e));
        }
        if self.in_profile(key) {
            // the profile's value goes; your own (or the default) shows again
            self.edit_profile(key, None)?;
        }
        self.edit(|st| match st {
            Store::Flat(lines) => {
                lines.retain(|l| l.split_once('=').is_none_or(|(k, _)| k.trim() != key));
            }
            Store::Toml(doc) => {
                crate::tomlcfg::unset(doc, key);
            }
        })
    }

    /// Does the profile in use name `key` (so it wins over the main file)?
    pub fn in_profile(&self, key: &str) -> bool {
        self.over
            .as_ref()
            .is_some_and(|(_, es)| es.iter().any(|(k, _)| k == key))
    }

    /// Which profile's file a change to `key` goes to, when one names it.
    pub fn profile_of(&self, key: &str) -> Option<PathBuf> {
        self.over
            .as_ref()
            .filter(|_| self.in_profile(key))
            .map(|(p, _)| p.clone())
    }

    /// Set (or, with None, drop) `key` in the profile in use, which names it:
    /// a change made while a profile is on stays in it, and takes effect.
    fn edit_profile(&mut self, key: &str, val: Option<&str>) -> std::io::Result<()> {
        let Some((path, _)) = self.over.clone() else {
            return Ok(());
        };
        let text = fs::read_to_string(&path).unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = text
            .parse()
            .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;
        match val {
            Some(v) => crate::tomlcfg::set(&mut doc, key, v),
            None => {
                crate::tomlcfg::unset(&mut doc, key);
            }
        }
        write_atomic(&path, doc.to_string().as_bytes())?;
        if let Some((_, es)) = &mut self.over {
            es.retain(|(k, _)| k != key);
            if let Some(v) = val {
                es.push((key.to_string(), v.to_string()));
            }
        }
        self.recache();
        Ok(())
    }

    /// Remove every key under `prefix` (e.g. `preset.px.`).
    pub fn unset_prefix(&mut self, prefix: &str) -> std::io::Result<()> {
        if let Some(e) = crate::showcase::refuse_setting(prefix) {
            return Err(std::io::Error::other(e));
        }
        let keys: Vec<String> = self
            .main_entries()
            .into_iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, _)| k)
            .collect();
        self.edit(|st| match st {
            Store::Flat(lines) => {
                lines.retain(|l| {
                    l.split_once('=')
                        .is_none_or(|(k, _)| !k.trim().starts_with(prefix))
                });
            }
            Store::Toml(doc) => {
                for k in &keys {
                    crate::tomlcfg::unset(doc, k);
                }
            }
        })
    }

    fn save(&self) -> std::io::Result<()> {
        let s = match &self.store {
            Store::Flat(lines) => {
                let mut s = String::new();
                for l in lines {
                    s.push_str(l);
                    s.push('\n');
                }
                s
            }
            Store::Toml(doc) => doc.to_string(),
        };
        // it can hold notification URLs (secrets): this user only
        write_private(&self.path, s.as_bytes())
    }

    /// Is the main file TOML (vs. an old key=value file)?
    pub fn is_toml(&self) -> bool {
        matches!(self.store, Store::Toml(_))
    }

    /// The files this config reads, in precedence order (lowest first).
    pub fn files(&self) -> Vec<(String, PathBuf)> {
        let mut v: Vec<(String, PathBuf)> = self
            .under
            .iter()
            .map(|(p, _)| ("include".to_string(), p.clone()))
            .collect();
        v.push(("main".into(), self.path.clone()));
        if let Some((p, _)) = &self.over {
            v.push(("profile".into(), p.clone()));
        }
        v
    }

    pub fn ttl(&self) -> u64 {
        crate::brand::env("BROWSE_TTL")
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| self.num("browse_ttl").max(1))
    }

    /// Time between polls of a machine in live views (`refresh`, seconds,
    /// fractions allowed; min 0.1). Polls never overlap, so a slow machine is
    /// simply re-polled the moment its last answer arrives.
    pub fn refresh_dur(&self) -> std::time::Duration {
        let secs: f64 = crate::brand::env("REFRESH")
            .and_then(|v| v.parse().ok())
            .or_else(|| self.get_or("refresh").parse().ok())
            .unwrap_or(0.1);
        std::time::Duration::from_secs_f64(secs.max(0.1))
    }

    /// Whole seconds between reprints for printed `--watch` modes (min 1).
    pub fn refresh(&self) -> u64 {
        self.refresh_dur().as_secs().max(1)
    }

    pub fn raycast_dirs(&self) -> Vec<PathBuf> {
        self.get_or("raycast_dirs")
            .split(':')
            .filter(|s| !s.is_empty())
            .map(expand)
            .collect()
    }

    /// Per-host setting `host.<alias>.<field>`.
    pub fn host_get(&self, alias: &str, field: &str) -> Option<String> {
        self.get(&format!("host.{alias}.{field}"))
    }

    /// Machines shown together in the dashboard, in order. Always includes the
    /// default host (first) when one is set.
    pub fn hosts(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        if let Some(d) = self.get("ssh_host") {
            v.push(d);
        }
        for h in self
            .get("hosts")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
        {
            if !v.iter().any(|x| x == h) {
                v.push(h.to_string());
            }
        }
        v
    }

    /// Every machine the fleet shows: `hosts()` plus `local` (this computer's
    /// own herdr) when `local` is on and herdr is installed here. ssh-only tools
    /// (doctor, theme, shortcuts) keep using `hosts()`.
    pub fn fleet(&self) -> Vec<String> {
        let mut v = self.hosts();
        let want = self.flag("local")
            && !v.iter().any(|h| h == crate::sshcfg::LOCAL)
            && crate::proc::which(&crate::herdr::bin()).is_some();
        if want {
            v.push(crate::sshcfg::LOCAL.to_string());
        }
        v
    }

    pub fn set_hosts(&mut self, v: &[String]) -> std::io::Result<()> {
        self.set("hosts", &v.join(","))
    }

    /// Work out which host to talk to. `machine_for` maps an ssh alias to a
    /// herdr machine selector (only consulted when the config can't answer).
    pub fn target(
        &self,
        flag: Option<&str>,
        machine_for: impl Fn(&str) -> String,
    ) -> Option<Target> {
        let env = |k: &str| crate::brand::env(k);
        let (cfg_ssh, cfg_machine) = (self.get("ssh_host"), self.get("machine"));
        let (ssh, source) = if let Some(h) = flag {
            (h.to_string(), "--host")
        } else if let Some(h) = env("SSH_HOST") {
            (h, "ALEPH_SSH_HOST")
        } else if let Some(m) = env("MACHINE") {
            (m, "ALEPH_MACHINE")
        } else {
            (cfg_ssh.clone()?, "config")
        };
        let same_as_cfg = Some(&ssh) == cfg_ssh.as_ref();
        let machine = if let Some(m) = env("MACHINE") {
            m
        } else if let Some(m) = self.host_get(&ssh, "machine") {
            m
        } else if let (true, Some(m)) = (same_as_cfg, cfg_machine) {
            m
        } else if source == "config" {
            ssh.clone()
        } else {
            machine_for(&ssh)
        };
        Some(Target {
            ssh,
            machine,
            source,
        })
    }

    /// Target for a specific alias from the fleet (no env/flag overrides).
    pub fn target_for(&self, alias: &str, machine_for: impl Fn(&str) -> String) -> Target {
        let machine = self
            .host_get(alias, "machine")
            .or_else(|| {
                (self.get("ssh_host").as_deref() == Some(alias))
                    .then(|| self.get("machine"))
                    .flatten()
            })
            .unwrap_or_else(|| machine_for(alias));
        Target {
            ssh: alias.to_string(),
            machine,
            source: "fleet",
        }
    }

    // ------------------------------------------------------------ presets

    /// Every preset as one session (a preset with panes: its first one).
    /// `crate::presets` has the whole model.
    pub fn presets(&self) -> Vec<Preset> {
        if !self.is_toml() {
            return self.flat_presets();
        }
        crate::presets::all(self)
            .iter()
            .map(Preset::from_model)
            .collect()
    }

    pub fn preset(&self, name: &str) -> Option<Preset> {
        if !self.is_toml() {
            return self.flat_preset(name);
        }
        crate::presets::get(self, name).map(|p| Preset::from_model(&p))
    }

    /// The old key=value file's presets (`preset.<name>.<field>`).
    pub fn flat_presets(&self) -> Vec<Preset> {
        let mut names: Vec<String> = Vec::new();
        for (k, _) in self.with_prefix("preset.") {
            if let Some((n, _)) = k.split_once('.') {
                if !names.iter().any(|x| x == n) {
                    names.push(n.to_string());
                }
            }
        }
        names.sort();
        names
            .into_iter()
            .filter_map(|n| self.flat_preset(&n))
            .collect()
    }

    fn flat_preset(&self, name: &str) -> Option<Preset> {
        let g = |f: &str| self.get(&format!("preset.{name}.{f}"));
        let any = ["host", "dir", "args", "agent", "worktree", "prompt"]
            .iter()
            .any(|f| g(f).is_some());
        any.then(|| Preset {
            name: name.to_string(),
            host: g("host"),
            dir: g("dir"),
            args: g("args"),
            agent: g("agent"),
            worktree: g("worktree").is_some_and(|v| v != "0" && v != "false"),
            prompt: g("prompt"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Preset {
    pub name: String,
    pub host: Option<String>,
    pub dir: Option<String>,
    pub args: Option<String>,
    pub agent: Option<String>,
    pub worktree: bool,
    pub prompt: Option<String>,
}

impl Preset {
    /// A preset with panes, as the session its first pane starts.
    pub fn from_model(p: &crate::presets::Preset) -> Preset {
        let first = p.panes.first().cloned().unwrap_or_default();
        let x = |s: &Option<String>| s.as_ref().map(|v| crate::presets::expand(v, &p.vars));
        Preset {
            name: p.name.clone(),
            host: x(&first.host),
            dir: x(&first.dir),
            args: (!first.args.is_empty()).then(|| {
                first
                    .args
                    .iter()
                    .map(|a| crate::proc::sh_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            }),
            agent: x(&first.agent),
            worktree: first.worktree != crate::presets::Worktree::None,
            prompt: x(&first.prompt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_has_a_section() {
        for k in KNOWN {
            let s = section_of(k.key);
            assert!(SECTIONS.contains(&s), "{} → {s}", k.key);
        }
        // the ones that aren't Advanced by accident
        assert_eq!(section_of("notify.ntfy"), "Notifications");
        assert_eq!(section_of("host.b31.color"), "Machines");
        assert_eq!(section_of("key.attach"), "Keys");
        assert_eq!(section_of("log.level"), "Advanced");
    }

    #[test]
    fn values_are_checked() {
        assert!(check("refresh", "5").is_ok() && check("refresh", "x").is_err());
        assert!(check("stats", "maybe").is_err());
        assert!(check("agent", "codex").is_ok());
        assert!(check("agent", "clade")
            .unwrap_err()
            .contains("did you mean 'claude'"));
        assert!(check("agent", "shell").is_err());
        assert!(check("agent", "cursor-agent")
            .unwrap_err()
            .contains("cursor"));
        assert!(check("agents", "all").is_ok() && check("agents", "claude,codex").is_ok());
        assert!(check("agents", "claude,nope").is_err());
        assert!(check("wraps", "codex").is_ok() && check("wraps", "a b").is_err());
        assert!(check("host.b31.anything", "x").is_ok());
        assert_eq!(known("agents").unwrap().default, "all");
        assert_eq!(known("wraps").unwrap().default, "claude");
    }

    #[test]
    fn get_set_roundtrip() {
        let dir = std::env::temp_dir().join(format!("chc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // the old key=value format still works where a file is named explicitly
        let mut c = Config {
            path: dir.join("config"),
            store: Store::Flat(vec![]),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.set("ssh_host", "b31").unwrap();
        c.set("machine", "b31").unwrap();
        c.set("ssh_host", "gpu2").unwrap();
        let text = fs::read_to_string(&c.path).unwrap();
        assert_eq!(
            text.lines().filter(|x| x.starts_with("ssh_host")).count(),
            1
        );
        let l = Config {
            path: c.path.clone(),
            store: read_store(&c.path),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        let mut l = l;
        l.recache();
        assert_eq!(l.get("ssh_host").as_deref(), Some("gpu2"));
        assert_eq!(l.get("machine").as_deref(), Some("b31"));
        // aleph.toml: typed, sectioned, edited in place
        let t = dir.join("aleph.toml");
        fs::write(&t, "# mine\n[ui]\nscale = \"large\"\n").unwrap();
        let mut c = Config {
            path: t.clone(),
            store: read_store(&t),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.recache();
        assert_eq!(c.get("ui_scale").as_deref(), Some("large"));
        c.set("auto_approve", "true").unwrap();
        c.set("host.b31.color", "blue").unwrap();
        c.set("refresh", "0.5").unwrap();
        let text = fs::read_to_string(&t).unwrap();
        assert!(
            text.starts_with("# mine\n[ui]\nscale = \"large\""),
            "{text}"
        );
        assert!(text.contains("auto_approve = true"), "{text}");
        assert!(text.contains("[hosts.b31]\ncolor = \"blue\""), "{text}");
        assert_eq!(c.get("refresh").as_deref(), Some("0.5"));
        assert_eq!(c.num("refresh"), 0);
        c.unset("host.b31.color").unwrap();
        assert!(!fs::read_to_string(&t).unwrap().contains("hosts"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_old_file_moves_to_aleph_toml_once() {
        let dir = std::env::temp_dir().join(format!("chc-mig-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let old = dir.join("config");
        fs::write(
            &old,
            "# comment\nssh_host=b31\nhosts=b31,dex42\nhost.b31.color=blue\nnotify.ntfy=https://ntfy.sh/x\napprove.tests=*pytest*\nauto_approve=true\nodd_key=1\n",
        )
        .unwrap();
        let new = dir.join("aleph.toml");
        migrate_flat(&old, &new).unwrap();
        assert!(!old.exists() && dir.join("config.bak").exists());
        let mut c = Config {
            path: new.clone(),
            store: read_store(&new),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.recache();
        assert_eq!(c.hosts(), vec!["b31", "dex42"]);
        assert_eq!(c.host_get("b31", "color").as_deref(), Some("blue"));
        assert_eq!(c.get("approve.tests").as_deref(), Some("*pytest*"));
        assert!(c.flag("auto_approve"));
        assert_eq!(c.get("odd_key").as_deref(), Some("1"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&new).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn includes_sit_under_the_main_file() {
        let dir = std::env::temp_dir().join(format!("chc-inc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("team")).unwrap();
        fs::write(
            dir.join("team/a.toml"),
            "[ui]\nscale = \"large\"\naccent = \"claude\"\n",
        )
        .unwrap();
        let main = dir.join("aleph.toml");
        fs::write(
            &main,
            "include = [\"team/*.toml\"]\n[ui]\nscale = \"compact\"\n",
        )
        .unwrap();
        let mut c = Config {
            path: main.clone(),
            store: read_store(&main),
            under: Vec::new(),
            over: None,
            cache: Vec::new(),
        };
        c.load_layers();
        c.recache();
        assert_eq!(c.get("ui_scale").as_deref(), Some("compact"));
        assert_eq!(c.get("accent").as_deref(), Some("claude"));
        assert!(matches!(c.source("accent"), Some(Source::Include(_))));
        assert_eq!(c.source("ui_scale"), Some(Source::Main));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn hosts_presets_defaults() {
        let c = Config::in_memory(&[
            "ssh_host=b31",
            "hosts=gpu2, b31 ,lab",
            "preset.px.host=b31",
            "preset.px.dir=/srv/px",
            "preset.px.worktree=1",
            "host.gpu2.machine=GPU box",
        ]);
        assert_eq!(c.hosts(), vec!["b31", "gpu2", "lab"]);
        let p = c.preset("px").unwrap();
        assert_eq!(
            (p.host.as_deref(), p.dir.as_deref(), p.worktree),
            (Some("b31"), Some("/srv/px"), true)
        );
        assert!(c.preset("nope").is_none());
        assert_eq!(c.presets().len(), 1);
        assert_eq!(c.get_or("refresh"), "0.1");
        assert!(c.flag("return_to_dashboard"));
        assert_eq!(c.target_for("gpu2", |a| a.to_string()).machine, "GPU box");
        assert_eq!(c.target_for("b31", |a| a.to_string()).machine, "b31");
    }
    /// docs/config.md is rendered from KNOWN (ALEPH_WRITE_DOCS=1 rewrites it).
    #[test]
    fn config_docs_are_current() {
        let md = docs_markdown();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/config.md");
        if std::env::var_os("ALEPH_WRITE_DOCS").is_some() {
            std::fs::write(&path, &md).unwrap();
        }
        let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            on_disk == md,
            "docs/config.md is out of date: ALEPH_WRITE_DOCS=1 cargo test config_docs_are_current"
        );
    }
}
