//! Aleph — coding-agent sessions (Claude Code, Codex, OpenCode, …) inside
//! herdr on your machines: `aleph` (or the `al` shell function); see
//! `aleph help`. The opt-in `claude` wrapper passes Claude Code's own
//! commands through (`brand::wrapper_mode`).

mod actions;
mod adopt;
mod agentcmd;
mod agentinstall;
mod agents;
mod agentsetup;
mod agentwatch;
mod ask;
mod backup;
// staged for the dashboard rewrite: not wired into the UI yet
#[allow(dead_code)]
mod ansi;
mod api;
mod auto;
mod brand;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod browse;
mod cache;
// staged for the dashboard rewrite: not wired into the UI yet
#[allow(dead_code)]
mod chan;
mod cli;
mod clone;
mod closing;
mod completions;
mod config;
mod configcmd;
mod connect;
mod daemon;
mod daemonctl;
mod deploy;
mod devices;
mod doctor;
mod editor;
mod events;
mod expose;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod fleet;
mod fuzzy;
mod handoff;
mod helpsearch;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod herdr;
mod herdrtheme;
mod history;
mod hooks;
mod hostsetup;
mod http;
mod icons;
mod ignore;
mod journal;
mod keep;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod launch;
mod layouts;
mod limits;
mod listen;
mod livestats;
mod log;
mod logo;
mod look;
mod message;
mod moshi;
mod moshibridge;
mod names;
mod net;
mod nodeinstall;
mod notes;
mod notify;
mod onboard;
mod onscreen;
mod openrouter;
mod out;
mod path;
mod permit;
mod pick;
mod presets;
mod pricelog;
mod prices;
mod probe;
mod proc;
mod profile;
mod prompts;
mod queue;
mod race;
mod refresh;
mod remote;
mod restore;
mod router;
mod selfupdate;
mod sentlog;
mod service;
mod showcase;
mod tokens;
// the dashboard places services::panel / summary / Watcher next
mod secrets;
#[allow(dead_code)]
mod services;
mod servicescmd;
mod setup;
mod shortcuts;
mod sock;
mod sshcfg;
mod statelog;
mod statscmd;
mod statshist;
// staged for the dashboard rewrite: not wired into the UI yet
#[allow(dead_code)]
mod stream;
// staged for the dashboard rewrite: not wired into the UI yet
#[allow(dead_code)]
mod telemetry;
mod term;
mod termlaunch;
mod theme;
mod timeline;
mod tomlcfg;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod tui;
mod turncost;
#[allow(dead_code)] // some items are only used by the upcoming dashboard
mod ui;
mod update;
mod urlscheme;
// the dashboard's Monitor › Usage (next) uses the rest of the API
#[allow(dead_code)]
mod usage;
mod watch;
mod watchrules;
mod webpush;
mod whatsnew;
mod wol;
mod workspace;

use cli::Args;
use config::Config;
use out::*;
use ratatui::prelude::*;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::process::ExitCode;
use std::time::Instant;
use ui::c;

/// (command, args, summary, section) — drives dispatch, `help`, and completion.
pub const COMMANDS: &[(&str, &str, &str, &str)] = &[
    ("new", "[@preset] [--dir /abs] [--name n] [--agent k] [--openrouter model] [--worktree] [--tile N] [-m prompt] [--detach [--json]] [-- args]", "start a session in a new herdr tab", "Sessions"),
    #[cfg(feature = "showcase")]
    ("showcase", "on [--demo] | off | status", "demo mode: names, folders, addresses and versions as space references everywhere (or a scripted demo world)", "Setup"),
    ("shell", "[host] [folder] [--detach]", "a plain terminal (no agent) in herdr on a machine, for running commands", "Sessions"),
    ("local", "[agent | shell] [--dir D] [-m prompt] [--detach]", "an agent (or a plain shell) in this computer's own herdr, here", "Sessions"),
    ("-i", "[args…]", "pick a folder on the host (fuzzy search), then start one", "Sessions"),
    ("ls", "[--json | --plain | --watch]", "the master dashboard; plain/JSON/TSV when piped", "Sessions"),
    ("resume", "[name | host:name]", "reattach (never creates a session)", "Sessions"),
    ("focus", "[name | host:name]", "the dashboard with one session fullscreen: messages, queue, approve, split", "Sessions"),
    ("last", "", "new session where you started the last one", "Sessions"),
    ("send", "<name> <text…> [--after idle | 30m | 14:30] [--wait [--until state]… [--timeout 10m]]", "prompt a running session without attaching (or queue it for later)", "Sessions"),
    ("reply", "<event id | host:name> <text…>", "answer the session an Inbox event is about", "Sessions"),
    ("wait", "<name | host:name> [--until working|blocked|done|idle]… [--timeout 10m] [--json]", "wait until a session is free (or in the state you name)", "Sessions"),
    ("approve", "<name | host:name>", "answer its question with the yes option, wherever it is", "Sessions"),
    ("keys", "<name> <key…>", "answer a question: 1 · 2 · enter · esc · up", "Sessions"),
    ("clone", "<name | host:name> --to <machine> [--dir D] [--move] [--yes]", "the same agent (and its arguments) on another machine; --move closes the original", "Sessions"),
    ("rename", "<name> <new>", "rename a session (tab and agent)", "Sessions"),
    ("close", "<name> [--yes] [--worktree] [--now] · --undo [name] · --pending", "stop a session and close its pane after close.grace (5s; the agent runs until then): --undo keeps it, --now skips the wait, --worktree removes its checkout too", "Sessions"),
    ("wall", "[names…]", "tile live sessions from every machine", "Fleet"),
    ("tile", "<name> <name>…", "move sessions into one herdr tab (same machine)", "Fleet"),
    ("hub", "", "herdr's own window with every machine (ctrl+b w)", "Fleet"),
    ("status", "[--short | --json | --watch]", "counts: working · need you · done", "Fleet"),
    ("stats", "[--json]", "CPU · GPU · memory · top processes on every machine", "Fleet"),
    ("services", "[--host H] [--json]", "local LLM servers and AI gateways on every machine (Ollama, LM Studio, vLLM, …)", "Fleet"),
    ("usage", "[24h | 7d | 30d | 6mo | this-week | A..B | all] [--by host|agent|account|model|effort|project|day|hour|session] [--host H] [--agent A] [--account X] [--stats] [--compare] [--json | --csv] | accounts | sources | script | sync | on | off | prune", "tokens, requests, prompts, active time and cost from the agents' own logs", "Fleet"),
    ("peek", "<name | host:name> [-n N] [--ansi] [--json]", "a session's recent output, without attaching", "Sessions"),
    ("timeline", "[name | agent-session-id | last | ls] [--host H] [--fold smart|open|folded] [--words] [--json]", "what an agent did: its prompts, turns, tool calls and subagents, with time, tokens and $ (from its logs, never what was said)", "Sessions"),
    ("note", "[<name | host:name> [text… #tags | --clear]] [--json]", "a note on a session (kept after it ends; Inbox shows and searches them)", "Sessions"),
    ("watch", "[--install | --uninstall | --once]", "notify when a session needs you", "Fleet"),
    ("daemon", "[status [--json] | run | install | uninstall | start | stop | restart | logs | doctor | expose tailscale|cloudflare|off | deploy <host> | pair [--name N] [--scope read|reply|control] | token new <name> [--scope S] | token ls | token revoke <name>] [--dry-run] [--yes]", "the background daemon: its service (launchd / systemd), state, the hub, the API (socket and HTTP), paired devices", "Setup"),
    ("api", "[methods | <method> ['{json}'] | --stdio]", "talk to the daemon's API (scripts, Raycast, ssh vps aleph api --stdio)", "Setup"),
    ("wake", "<machine> [--via <machine>]", "Wake-on-LAN: wake a sleeping machine, sent from another one in the fleet (or this computer)", "Fleet"),
    ("connect", "<machine> [--mosh | --ssh] [-- aleph args…]", "Aleph's dashboard on another machine (a VPS near the fleet), reconnecting when the link drops", "Fleet"),
    ("queue", "[ls | add [host] \"prompt\" [--dir D] [--agent A] [--to host:name] [--new] | rm <id> | clear]", "tasks that idle agents pick up next (or one session, once it's done)", "Fleet"),
    ("auto", "[status | on | off | add <rule> <pattern> | rm <rule> | test <question>]", "auto-approve rules (never: rm -rf, sudo, git push, …)", "Fleet"),
    ("notify", "[test | routes | dnd [30m|on|off] | mute host:name [1h|off]]", "where notifications go: phone (ntfy), Slack, webhook, desktop, herdr, Raycast; quiet hours, do not disturb", "Fleet"),
    ("hooks", "[status | install | remove] [--host H] [--agent claude|codex|opencode] [--yes]", "agents tell Aleph in their own words (opt-in, per machine; a diff and a backup first)", "Fleet"),
    ("logs", "[-f] [--level L] [--since 1h] [--host H] [-n N] [--crash] [--bundle]", "what Aleph did on its own: reconnects, answers sent, failures; --bundle for bug reports", "Setup"),
    ("moshi", "[status | install | test [--ask] | usage | uninstall] [--json]", "Moshi on your phone: fleet cards, approvals as buttons, live jobs, the dashboard one tap away", "Fleet"),
    ("events", "[--json]", "every machine's herdr events, live (needs you · working · done · started · closed)", "Fleet"),
    ("inbox", "[-n N] [--all] [--json] | read [T] | dismiss ID…", "recent events: needed you, finished, started, ended (--all: the archive too)", "Fleet"),
    ("restore", "[--dry-run] [--all] [--yes]", "recreate sessions lost to a reboot or herdr restart", "Fleet"),
    ("backup", "[FILE] [--all] | list | show FILE | restore FILE [--dry-run] [--yes]", "settings, presets, history, notes and session memory in one file (never secrets); restore shows what changes first", "Setup"),
    ("history", "[-n N] [--json]", "sessions you started", "Fleet"),
    ("journal", "[today | yesterday | -N | DATE] [--write] [--json] | path", "what each session did that day, as Markdown for standups and notes: prompts, the agents' final words, files changed, time, tokens and $ (journal.include)", "Fleet"),
    ("recent", "[--json]", "folders by frecency", "Fleet"),
    ("preset", "[ls | show | launch NAME [--var k=v] [--reuse] [--dry-run] | add | save NAME host:session… | dup | rm | export [--generic] | import FILE|URL | validate | edit]", "starting points: one session or several, on one machine or many (aleph.toml [presets.NAME])", "Fleet"),
    ("host", "[list | pick | set | add | setup | key | include | exclude | color]", "machines: default, fleet, colours, key login; setup installs herdr and agents on one", "Hosts"),
    ("doctor", "[--json]", "check binaries, hosts, login, remote tools, shortcuts", "Hosts"),
    ("agents", "[--local] [--all] [--json] [--refresh] | install <agent> | setup [agent…]", "agents on each machine; install one; herdr integrations", "Hosts"),
    ("icons", "", "what logos this terminal can show, with samples and how to get the real ones", "Settings"),
    ("update", "[--check] [--yes] [--json]", "herdr, integrations and agents across the fleet", "Hosts"),
    ("setup", "[step] [--redo] [--plain] [--defaults] | --apply FILE.toml [--dry-run] [--json]", "the setup wizard: machines, herdr, agents, notifications, analytics, the daemon, … (full screen; --plain for line mode)", "Setup"),
    ("splash", "[NAME|theme|random] [--loop] [--ms 3s] [--no-logo] [--end form|fall|warp]", "play a splash full screen (cockpit, classic, singularity, …), outside the dashboard — any key ends it; --loop for recording", "Setup"),
    ("outside", "[ls] | stop MACHINE:PID [--yes] | reopen MACHINE:PID [--yes] [--json]", "agents running outside herdr (a terminal of their own): list them, stop one, or reopen it in herdr — same folder, same conversation, its options", "Sessions"),
    ("edit", "<session | host:/path>", "open a session's folder in your editor (editor setting; another machine's over Remote-SSH)", "Sessions"),
    ("race", "\"prompt\" [--agents claude,codex] [--host H] [--dir D] | ls | show ID", "one prompt to 2–4 agents at once, each in a git worktree of its own; the dashboard shows their diffs side by side", "Sessions"),
    ("handoff", "<session> [--to AGENT] [--summary agent|aleph] [--json]", "hand a session's work to another agent: a hand-off note (its own, or Aleph's from git and its screen), then a new session in the same folder", "Sessions"),
    ("prompt", "[ls] | show NAME | add NAME TEXT… | rm NAME", "the prompt library: saved prompts with {variables}, used with ctrl+p or @name wherever you write one (aleph send b31:hunt @review)", "Sessions"),
    ("profile", "[ls] | use NAME | off | show NAME | set NAME KEY VALUE | unset NAME KEY | save NAME KEY… | edit NAME | rm NAME", "switch whole sets of settings at once (demo, travel, work, your own), layered over yours; off brings yours back", "Setup"),
    ("settings", "[list | get | set | unset | edit | path]", "every option, with defaults", "Setup"),
    ("secret", "[ls | set NAME [VALUE|-] | get NAME | rm NAME | move]", "secrets in the Keychain / Secret Service, used as secret:NAME in settings", "Setup"),
    ("config", "[path | check | schema | docs | edit | export | import <src> | diff <src> | sources [key] | undo]", "aleph.toml: validate, share (secrets masked), import with a preview, see where values come from, step back", "Setup"),
    ("theme", "[show | herdr [--apply] | machines [--apply]]", "colours in herdr's sidebar and per-machine accents", "Setup"),
    ("shortcuts", "[install | ls]", "Warp tab configs + Raycast scripts for every host", "Setup"),
    ("completions", "<zsh | bash | fish>", "shell completion script", "Setup"),
    ("url", "<aleph://…> | install | uninstall | status | links", "aleph:// links for Shortcuts, Stream Deck and scripts: open views and sessions; approve, send, close, new and queue ask first", "Setup"),
    ("open-tab", "[--terminal T] [--dry-run] -- <command…>", "run a command in a new tab of this terminal (Warp, iTerm2, Terminal, Ghostty, kitty, WezTerm)", "Setup"),
    ("self-update", "[--check]", "update from GitHub releases (update_repo)", "Setup"),
    ("whats-new", "[-n N | --all] [--commits] [--json]", "what changed in recent updates (the card the dashboard shows once after one)", "Setup"),
    ("version", "[--json]", "versions of this, the agent it wraps and herdr", "Setup"),
    ("help", "[command]", "this help, or details for one command", "Setup"),
];

/// Commands designed and wired but not built yet. `help`
/// marks them "planned"; running them says so. Remove entries as they land.
pub const PLANNED: &[(&str, &str)] = &[];

fn planned(cmd: &str) -> Option<ExitCode> {
    PLANNED.iter().find(|p| p.0 == cmd).map(|p| {
        err(&format!("aleph {cmd} isn't built yet"));
        hint(&format!(
            "it's designed ({}) — the rest of aleph works",
            p.1
        ));
        ExitCode::from(2)
    })
}

/// Commands that also take aliases.
const ALIASES: &[(&str, &str)] = &[
    ("browse", "-i"),
    ("list", "ls"),
    ("attach", "resume"),
    ("hosts", "host"),
    ("presets", "preset"),
    ("ui", ""),
    ("prefs", "settings"),
    ("selfupdate", "self-update"),
    ("completion", "completions"),
    ("__complete", "__complete"),
];

/// Read-only commands that still make sense with piped stdin.
const PIPE_OK: &[&str] = &[
    "journal",
    "backup",
    "url",
    "whats-new",
    "profile",
    "prompt",
    "handoff",
    "race",
    "edit",
    "outside",
    "clone",
    "wake",
    "daemon",
    "api",
    "icons",
    "note",
    "reply",
    "wait",
    "open-tab",
    "secret",
    "config",
    "logs",
    "moshi",
    "events",
    "queue",
    "auto",
    "notify",
    "approve",
    "stats",
    "services",
    "usage",
    "timeline",
    "peek",
    "help",
    "rename",
    "close",
    "tile",
    "ls",
    "host",
    "doctor",
    "status",
    "history",
    "recent",
    "preset",
    "settings",
    "theme",
    "completions",
    "version",
    "__complete",
    "send",
    "keys",
    "self-update",
    "shortcuts",
    "watch",
    "agents",
    "update",
    "restore",
    "inbox",
    "setup",
];
// The wrapped agent's own flags and subcommands (which always run locally)
// come from the registry and its `--help`: `agents::Wraps`.

fn canonical(cmd: &str) -> Option<&'static str> {
    COMMANDS
        .iter()
        .map(|c| c.0)
        .find(|c| *c == cmd)
        .or_else(|| ALIASES.iter().find(|a| a.0 == cmd).map(|a| a.1))
}

fn main() -> ExitCode {
    term::install_panic_hook();
    let mut args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // started as an agent's own command (the opt-in `claude` function)?
    let wrapper = brand::wrapper_mode();

    // inside a herdr pane the wrapper is the agent, untouched (never nest herdr)
    if wrapper && std::env::var_os("HERDR_ENV").is_some_and(|v| !v.is_empty()) {
        return passthrough(&args);
    }
    // the one-time move from claude-herdr (cheap no-op once done)
    if args.first().and_then(|a| a.to_str()) != Some("__complete") {
        brand::announce(&brand::migrate());
        brand::nudge_old_name();
    }

    // global flags: --host <alias> / -H <alias> / --host=<alias>, --debug,
    // --via <machine> (run it there), --local (not the `daemon` machine)
    let mut flag: Option<String> = None;
    let mut debug = false;
    let mut via: Option<String> = None;
    let mut local = false;
    while let Some(a) = args.first().and_then(|a| a.to_str()).map(str::to_owned) {
        if a == "--via" {
            args.remove(0);
            if args.is_empty() {
                return fail("--via needs a machine (an ssh alias)");
            }
            via = Some(args.remove(0).to_string_lossy().into_owned());
        } else if let Some(h) = a.strip_prefix("--via=") {
            via = Some(h.to_string());
            args.remove(0);
        } else if a == "--local" {
            local = true;
            args.remove(0);
        } else if a == "--host" || a == "-H" {
            args.remove(0);
            if args.is_empty() {
                return fail("--host needs an ssh alias");
            }
            flag = Some(args.remove(0).to_string_lossy().into_owned());
        } else if let Some(h) = a.strip_prefix("--host=") {
            flag = Some(h.to_string());
            args.remove(0);
        } else if a == "--debug" {
            debug = true;
            args.remove(0);
        } else {
            break;
        }
    }
    proc::debug_init(debug);
    log::init(debug, &Config::load().get_or("log.level"));
    // showcase mode: `--host` given a machine's masked name means that machine
    if crate::showcase::on() {
        flag = flag.map(|h| crate::showcase::unmask(&h, &Config::load().fleet()).into_owned());
    }

    let sargs: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let raw = sargs.first().map(String::as_str);
    // `aleph showcase …` (only in a build with the feature)
    if let Some(code) = raw.and_then(|r| crate::showcase::command(r, &sargs[1..])) {
        return code;
    }
    let cmd = raw.and_then(canonical);
    if wrapper {
        // the wrapped agent's own flags and subcommands win, even over a
        // name we also use (`claude config` is Claude Code's)
        if let Some(r) = raw {
            if wraps().owns(&sargs, r) {
                return passthrough(&args);
            }
        }
        // `new --detach` never needs a terminal (scripts, the Raycast extension)
        let detached =
            matches!(cmd, Some("new" | "local")) && sargs.iter().any(|a| a == "--detach");
        if !io::stdin().is_terminal() && !detached && !cmd.is_some_and(|c| PIPE_OK.contains(&c)) {
            return passthrough(&args);
        }
    }

    if let Some(c) = cmd.and_then(planned) {
        return c;
    }
    // a terminal some other program left raw: put back before printing
    term::unstick();
    let mut cfg = Config::load();
    if io::stderr().is_terminal() && cmd != Some("logs") {
        if let Some(p) = log::unseen_crash() {
            warn(&format!(
                "{} crashed last time — aleph logs --crash shows why ({})",
                brand::NAME,
                p.file_name().unwrap_or_default().to_string_lossy()
            ));
        }
    }
    theme::load_custom(&cfg);
    ui::c::init(
        &cfg.get_or("theme"),
        cfg.get("accent").and_then(|a| ui::parse_color(&a)),
    );
    term::set_mouse(cfg.flag("mouse"));
    term::set_keyboard(cfg.get_or("ui.keyboard") != "legacy");
    // herdr's named sessions per machine (host.<a>.session)
    chan::set_sessions(&cfg);
    icons::init(&cfg.get_or("icons"));
    ui::set_scale(&cfg.get_or("ui_scale"));
    ui::set_spinner(&cfg.get_or("ui.spinner"));
    look::configure(&cfg);
    usage::dash::show_cost(cfg.get_or("analytics.cost") != "off");
    theme::assign_machine_colors(&mut cfg);
    // somewhere else: `--via vps`, or `daemon = ssh://vps` for a bare aleph
    // (never again from inside a session it started: ALEPH_REMOTE)
    if let Some(h) = &via {
        let mut rest = sargs.clone();
        if let Some(f) = &flag {
            rest.splice(0..0, ["--host".to_string(), f.clone()]);
        }
        let how = if cfg.get_or("attach_via") == "mosh" {
            connect::Via::Mosh
        } else {
            connect::Via::Ssh
        };
        return connect::run(&cfg, how, h, &rest);
    }
    if cmd.is_none() && raw.is_none() && !local && brand::env("REMOTE").is_none() {
        if let Some((how, h)) = connect::remote_of(&cfg.get_or("daemon")) {
            return connect::run(&cfg, how, &h, &[]);
        }
    }
    let rest = sargs.get(1..).unwrap_or(&[]).to_vec();
    let a = Args {
        cfg,
        flag: flag.clone(),
        rest: &rest,
    };
    let dash = |a: Args, view: tui::View| {
        tui::run_dashboard(
            a.cfg,
            tui::Start {
                view,
                flag: a.flag.clone(),
                wall: a
                    .rest
                    .iter()
                    .filter(|x| !x.starts_with('-'))
                    .cloned()
                    .collect(),
                ..Default::default()
            },
        )
    };
    match cmd {
        None | Some("") => {
            if raw.is_some() && cmd.is_none() {
                return unknown(&sargs);
            }
            if term::interactive() {
                if a.cfg.hosts().is_empty() && a.flag.is_none() && !tui::onboard::finished() {
                    return setup::first_run(a);
                }
                // `aleph ui monitor` (an aleph://open?view= link): that tab
                // first; `aleph ui gpu-watch`: that saved workspace
                if let Some(first) = a.positional().first().map(|s| s.to_string()) {
                    if let Some(view) = tui::View::named(&first) {
                        return tui::run_dashboard(
                            a.cfg,
                            tui::Start {
                                view,
                                flag: a.flag.clone(),
                                ..Default::default()
                            },
                        );
                    }
                    if workspace::find(&a.cfg, &first).is_some() {
                        return tui::run_dashboard(
                            a.cfg,
                            tui::Start {
                                flag: a.flag.clone(),
                                workspace: Some(first),
                                ..Default::default()
                            },
                        );
                    }
                    return fail(&format!(
                        "{first}: not a tab (fleet, monitor, inbox, settings) or a saved workspace"
                    ));
                }
                dash(a, tui::View::Fleet)
            } else {
                help(&a.cfg, flag.as_deref(), None)
            }
        }
        Some("help") if rest.len() > 1 => help(&a.cfg, flag.as_deref(), Some(&rest.join(" "))),
        Some("help") => help(&a.cfg, flag.as_deref(), rest.first().map(String::as_str)),
        Some("new") => cli::new(a),
        Some("local") => cli::local(a),
        Some("shell") => cli::shell(a),
        Some("-i") => browse_cmd(a),
        Some("ls") => cli::ls(a),
        Some("resume") => cli::resume(a),
        Some("focus") => {
            if !term::interactive() {
                return fail("aleph focus needs a terminal");
            }
            let q = a.positional().first().map(|s| s.to_string());
            tui::run_dashboard(
                a.cfg,
                tui::Start {
                    view: tui::View::Fleet,
                    flag: a.flag.clone(),
                    focus: Some(q.unwrap_or_else(|| "1".into())),
                    ..Default::default()
                },
            )
        }
        Some("open-tab") => termlaunch::cmd(a),
        Some("url") => urlscheme::cmd(a),
        Some("whats-new") => whatsnew::cmd(a),
        Some("profile") => profile::cmd(a),
        Some("prompt") => prompts::cmd(a),
        Some("handoff") => handoff::cmd(a),
        Some("race") => race::cmd(a),
        Some("journal") => journal::cmd(a),
        Some("edit") => editor::cmd(a),
        Some("outside") => adopt::cmd(a),
        Some("splash") => tui::splash_cmd(a),
        Some("connect") => connect::cmd(a),
        Some("wake") => wol::cmd(a),
        Some("clone") => clone::cmd(a),
        Some("last") => cli::last(a),
        Some("send") => cli::send(a),
        Some("reply") => cli::reply(a),
        Some("wait") => cli::wait(a),
        Some("keys") => cli::keys(a),
        Some("rename") => cli::rename(a),
        Some("close") => cli::close(a),
        Some("wall") => {
            if !term::interactive() {
                return fail("aleph wall needs a terminal");
            }
            dash(a, tui::View::Wall)
        }
        Some("tile") => cli::tile(a),
        Some("hub") => cli::hub(a),
        Some("status") => cli::status(a),
        Some("stats") => statscmd::stats(a),
        Some("services") => servicescmd::cmd(a),
        Some("usage") => usage::cmd::cmd(a),
        Some("timeline") => timeline::cmd(a),
        Some("peek") => statscmd::peek(a),
        Some("note") => notes::cmd(a),
        Some("approve") => statscmd::approve(a),
        Some("watch") => watch::cmd(a),
        Some("daemon") => daemon::cmd(a),
        Some("api") => daemon::api_cmd(a),
        Some("inbox") => events::cmd(a),
        Some("events") => listen::cmd(a),
        Some("queue") => queue::cmd(a),
        Some("auto") => auto::cmd(a),
        Some("notify") => notify::cmd(a),
        Some("hooks") => hooks::cmd(a),
        Some("moshi") => moshi::cmd(a),
        Some("logs") => log::cmd(a),
        Some("restore") => restore::cmd(a),
        Some("backup") => backup::cmd(a),
        Some("agents") => agentcmd::cmd(a),
        Some("icons") => icons::cmd(&a.cfg),
        Some("update") => update::cmd(a),
        Some("setup") => setup::cmd(a),
        Some("history") => cli::history_cmd(a),
        Some("recent") => cli::recent(a),
        Some("preset") => presets::cmd(a),
        Some("host") => host_cmd(a.cfg, flag, &rest),
        Some("doctor") => doctor_cmd(&a.cfg, &flag, &rest),
        Some("settings") => cli::settings(a),
        Some("config") => configcmd::cmd(a),
        Some("secret") => secrets::cmd(a),
        Some("theme") => herdrtheme::cmd(a),
        Some("shortcuts") => shortcuts::cmd(a),
        Some("completions") => completions::cmd(a),
        Some("self-update") => selfupdate::cmd(a),
        Some("version") => cli::version(rest.iter().any(|x| x == "--json")),
        Some("__complete") => cli::complete(a),
        Some(_) => unknown(&sargs),
    }
}

fn unknown(sargs: &[String]) -> ExitCode {
    let all = sargs.join(" ");
    let w = wraps();
    err(&format!(
        "unknown command '{}'",
        sargs.first().map(String::as_str).unwrap_or("")
    ));
    let local = format!("local {}", w.name());
    let wl = ui::width(&local).max(18);
    say(Line::from(vec![
        Span::styled(
            format!("  {}  ", ui::pad("new remote session", wl)),
            ui::muted(),
        ),
        Span::styled(format!("aleph new -m {}", proc::sh_quote(&all)), ui::bold()),
    ]));
    say(Line::from(vec![
        Span::styled(format!("  {}  ", ui::pad(&local, wl)), ui::muted()),
        Span::styled(format!("command {} {all}", w.exe), ui::bold()),
    ]));
    say(Line::from(vec![
        Span::styled(
            format!("  {}  ", ui::pad("everything else", wl)),
            ui::muted(),
        ),
        Span::styled("aleph help", ui::bold()),
    ]));
    ExitCode::from(2)
}

/// The agent this wrapper stands in for (the `wraps` setting).
fn wraps() -> agents::Wraps {
    agents::Wraps::from_cfg(&Config::load())
}

/// Hand everything over to the wrapped agent (`wraps`), untouched.
fn passthrough(args: &[OsString]) -> ExitCode {
    let w = wraps();
    if crate::brand::env_os("NO_PASSTHROUGH").is_some() {
        err(&format!(
            "passthrough refused: not an Aleph command, it would go to {} (ALEPH_NO_PASSTHROUGH is set)",
            w.name()
        ));
        return ExitCode::from(97);
    }
    match w.path() {
        Some(p) => {
            let e = proc::exec(&p, args);
            err(&format!("couldn't run {}: {e}", p.display()));
            ExitCode::from(127)
        }
        None => {
            err(&format!(
                "{} isn't installed (no `{}` on PATH)",
                w.name(),
                w.exe
            ));
            if let Some(i) = w.install_hint() {
                hint(i);
            }
            hint("aleph settings set wraps <agent> names the agent this wrapper stands in for");
            ExitCode::from(127)
        }
    }
}

// ------------------------------------------------------------------ -i

fn browse_cmd(a: Args) -> ExitCode {
    let Some(t) = cli::target(&a.cfg, &a.flag) else {
        return cli::no_host();
    };
    if !term::interactive() {
        return fail("aleph -i needs a terminal");
    }
    // `-i --agent codex …`: checked before the browser opens, and named in it
    let mut host_args = vec!["--host".to_string(), t.ssh.clone()];
    host_args.extend(a.rest.iter().cloned());
    let mut na = match cli::parse_new(&host_args, &a.cfg) {
        Ok(x) => x,
        Err(e) => return fail(&e),
    };
    let mut tui = match term::Tui::enter() {
        Ok(t) => t,
        Err(e) => return fail(&format!("terminal: {e}")),
    };
    let mut o = browse::BrowseOpts::from_config(&a.cfg, &t);
    o.agent = agents::label(&na.opts.agent);
    match browse::run(&mut tui, &t, o) {
        // the browser has already closed its ssh connection here; stay on screen
        Ok(Some(browse::Picked::New(d))) => {
            na.opts.dir = Some(d);
            cli::fill_names(&mut na.opts);
            let (tui, o) = launch::run(
                tui,
                &t,
                launch::Job::New(na.opts),
                false,
                theme::machine(&a.cfg, &t.ssh),
            );
            cli::launched(&a.cfg, &t, tui, o)
        }
        // t: the same with no agent, a terminal in that folder
        Ok(Some(browse::Picked::Terminal(d))) => {
            na.opts.agent = agents::SHELL.into();
            na.opts.dir = Some(d);
            cli::fill_names(&mut na.opts);
            let (tui, o) = launch::run(
                tui,
                &t,
                launch::Job::New(na.opts),
                false,
                theme::machine(&a.cfg, &t.ssh),
            );
            cli::launched(&a.cfg, &t, tui, o)
        }
        Ok(Some(browse::Picked::Attach(s))) => {
            let (tui, o) = launch::run(
                tui,
                &t,
                launch::Job::Attach {
                    pane: s.pane_id.clone(),
                    label: s.display().to_string(),
                },
                false,
                theme::machine(&a.cfg, &t.ssh),
            );
            cli::launched(&a.cfg, &t, tui, o)
        }
        Ok(None) => ExitCode::SUCCESS,
        Err(e) => {
            drop(tui);
            fail(&format!("folder browser: {e}"))
        }
    }
}

// ------------------------------------------------------------------ doctor

/// `doctor --deep`, printed like the other sections.
fn deep_checks(cfg: &Config, parts: &[doctor::Part], color: bool, width: usize) {
    let _ = cfg;
    let mut lines: Vec<doctor::Line> = Vec::new();
    let sp = Spinner::start("Asking herdr".into());
    doctor::deep(parts, &mut |l| lines.push(l));
    sp.stop();
    let (head, items) = match lines.split_first() {
        Some((h, rest)) => (h.label.clone(), rest.to_vec()),
        None => return,
    };
    println!();
    println!(
        "{}",
        ui::to_ansi(
            &doctor::head_line(&head, "", &doctor::Counts::of(&items)),
            color
        )
    );
    for l in doctor::section_rows(&items, width) {
        println!("{}", ui::to_ansi(&l, color));
    }
}

fn fix_all(cfg: &Config, fixes: &[doctor::Fix]) {
    println!();
    doctor::fix_all(cfg, fixes);
}

fn doctor_cmd(cfg: &Config, flag: &Option<String>, rest: &[String]) -> ExitCode {
    // every machine in the fleet, side by side (`--host H`: just that one)
    let parts = doctor::parts(cfg, flag.as_deref());
    let n_hosts = parts
        .iter()
        .filter(|p| matches!(p, doctor::Part::Host(_)))
        .count();
    if rest.iter().any(|a| a == "--json") {
        let (v, sum) = doctor::report_json(cfg, &parts, &mut |_| {});
        json(&v);
        return if sum.bad > 0 {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        };
    }
    let fancy = io::stdout().is_terminal();
    let color = term::color_out();
    let width = ui::print_width();
    let t0 = Instant::now();
    if fancy {
        out(Line::from(vec![
            Span::styled(
                format!("{} ", brand::MARK),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("aleph doctor", ui::bold()),
            Span::styled(
                match (flag, n_hosts) {
                    (Some(h), _) => format!("  ·  {h}"),
                    (None, 0) => "  ·  no machines yet".to_string(),
                    (None, 1) => "  ·  1 machine".to_string(),
                    (None, n) => format!("  ·  {n} machines, checked side by side"),
                },
                ui::muted(),
            ),
        ]));
    }
    let spinner = Spinner::start("Checking this Mac".into());
    // a machine's sections (its ssh, what's on it, herdr's view) print as one
    // group: its head with a tally, then its checks, problems first
    let mut group: Option<String> = None;
    let mut buf: Vec<doctor::Line> = Vec::new();
    let flush = |buf: &[doctor::Line], spinner: &Spinner| {
        spinner.print(|| {
            if fancy {
                for (name, items) in doctor::groups(buf) {
                    let n = doctor::Counts::of(items.iter().map(|(_, l)| *l));
                    println!();
                    println!(
                        "{}",
                        ui::to_ansi(
                            &doctor::head_line(&name, &doctor::group_facts(&items), &n),
                            color
                        )
                    );
                    for l in doctor::rows(&items, width) {
                        println!("{}", ui::to_ansi(&l, color));
                    }
                }
            } else {
                for l in buf {
                    if l.st == doctor::St::Head {
                        println!("\n{}", l.label);
                        continue;
                    }
                    let m = match l.st {
                        doctor::St::Ok => "ok",
                        doctor::St::Warn => "warn",
                        doctor::St::Bad => "FAIL",
                        _ => "info",
                    };
                    println!("  {m:<4}  {:<14}  {}", l.label, l.text);
                    for f in &l.fix {
                        println!("        hint: {f}");
                    }
                }
            }
        });
    };
    let mut fixes: Vec<doctor::Fix> = Vec::new();
    spinner.set("Checking this computer and every machine".into());
    let sum = doctor::run_fleet(cfg, &parts, &mut |l| {
        if l.st == doctor::St::Head {
            let g = doctor::group_of(&l.label).0.to_string();
            if group.as_ref() != Some(&g) {
                if group.is_some() {
                    flush(&buf, &spinner);
                    buf.clear();
                }
                group = Some(g);
            }
        } else if let Some(f) = &l.action {
            if matches!(l.st, doctor::St::Bad | doctor::St::Warn) && !fixes.contains(f) {
                fixes.push(f.clone());
            }
        }
        buf.push(l);
    });
    if !buf.is_empty() {
        flush(&buf, &spinner);
    }
    spinner.stop();
    println!();
    let secs = t0.elapsed().as_secs_f32();
    let (m, col, text) = match (sum.bad, sum.warn) {
        (0, 0) => ("✓", c::GREEN(), format!("All {} checks passed", sum.ok)),
        (0, w) => (
            "!",
            c::YELLOW(),
            format!(
                "{} passed · {w} warning{}",
                sum.ok,
                if w == 1 { "" } else { "s" }
            ),
        ),
        (b, w) => (
            "✗",
            c::RED(),
            format!(
                "{b} problem{} · {w} warning{} · {} passed",
                if b == 1 { "" } else { "s" },
                if w == 1 { "" } else { "s" },
                sum.ok
            ),
        ),
    };
    out(Line::from(vec![
        Span::styled(
            format!("{m} {text}"),
            ui::fg(col).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" in {secs:.1}s"), ui::faint()),
    ]));
    if rest.iter().any(|a| a == "--deep") {
        deep_checks(cfg, &parts, color, width);
    }
    if rest.iter().any(|a| a == "--fix") {
        fix_all(cfg, &fixes);
    } else if !fixes.is_empty() {
        hint(&format!(
            "aleph doctor --fix carries out {} of these for you, asking first",
            fixes.len()
        ));
    }
    let w = agents::Wraps::from_cfg(cfg);
    if w.subcommands(true).iter().any(|c| c == "doctor") {
        out(Line::from(Span::styled(
            format!("  {}'s own checks: command {} doctor", w.name(), w.exe),
            ui::faint(),
        )));
    }
    if sum.bad > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ------------------------------------------------------------------ host

fn host_cmd(mut cfg: Config, flag: Option<String>, rest: &[String]) -> ExitCode {
    let sub = rest.first().map(String::as_str).unwrap_or("show");
    match sub {
        "show" => {
            let Some(t) = cli::target(&cfg, &flag) else { return cli::no_host() };
            let r = sshcfg::resolve(&t.ssh);
            out(Line::from(vec![
                Span::styled("▌", Style::new().fg(theme::machine(&cfg, &t.ssh))),
                Span::styled(t.ssh.clone(), ui::bold()),
                Span::styled(if t.source == "config" { "  ·  default host".to_string() } else { format!("  ·  from {}", t.source) }, ui::muted()),
            ]));
            let kv = |k: &str, v: String| out(Line::from(vec![Span::styled(format!("  {}", ui::pad(k, 13)), ui::faint()), Span::raw(v)]));
            if let Some(r) = &r {
                kv("connects to", r.short());
                if let Some(p) = &r.proxy {
                    kv("via", p.clone());
                }
            }
            kv("herdr", t.machine.clone());
            kv("fleet", cfg.hosts().join(", "));
            kv("config", doctor::tilde(&cfg.path));
            hint("aleph doctor checks login, remote tools and shortcuts");
            ExitCode::SUCCESS
        }
        "list" | "ls" => {
            let cur = cli::target(&cfg, &flag).map(|t| t.ssh);
            let fleet = cfg.hosts();
            let al = sshcfg::aliases();
            let json_out = rest.iter().any(|a| a == "--json");
            if al.is_empty() && fleet.is_empty() && !json_out {
                say(Line::from(vec![Span::styled("⏺ ", ui::fg(c::ACCENT())), Span::raw(format!("No Host entries in {}", doctor::tilde(&sshcfg::user_config())))]));
                hint("aleph host add <alias> <user@hostname>");
                return ExitCode::SUCCESS;
            }
            let prof = herdr::profiles();
            let mut all = fleet.clone();
            for a in &al {
                if !all.contains(a) {
                    all.push(a.clone());
                }
            }
            let rows: Vec<(String, String, bool)> = all
                .iter()
                .map(|a| (a.clone(), sshcfg::resolve(a).map(|r| r.short()).unwrap_or_else(|| "?".into()), prof.iter().any(|(t, s)| t == a || s == a)))
                .collect();
            if json_out {
                json(&serde_json::Value::Array(
                    rows.iter()
                        .map(|(a, r, h)| serde_json::json!({"alias": a, "address": r, "herdr": h, "default": Some(a) == cur.as_ref(), "fleet": fleet.contains(a), "color": theme::machine_hex(&cfg, a)}))
                        .collect(),
                ));
                return ExitCode::SUCCESS;
            }
            if !io::stdout().is_terminal() {
                for (a, r, h) in &rows {
                    println!("{}{a}\t{r}\t{}\t{}", if Some(a) == cur.as_ref() { "*" } else { "" }, if *h { "herdr" } else { "" }, if fleet.contains(a) { "fleet" } else { "" });
                }
                return ExitCode::SUCCESS;
            }
            let wa = rows.iter().map(|r| ui::width(&r.0)).max().unwrap_or(5).max(5);
            let wt = rows.iter().map(|r| ui::width(&r.1)).max().unwrap_or(11).max(11);
            out(Line::from(vec![
                Span::styled("⏺ ", ui::fg(c::ACCENT())),
                Span::styled(format!("{} ssh host{}", rows.len(), if rows.len() == 1 { "" } else { "s" }), ui::bold()),
                Span::styled(format!("  ·  {} in the fleet", fleet.len()), ui::faint()),
            ]));
            out(Line::from(Span::styled(format!("       {}  {}  herdr  fleet", ui::pad("alias", wa), ui::pad("connects to", wt)), ui::faint())));
            for (a, r, h) in &rows {
                let def = Some(a) == cur.as_ref();
                let inf = fleet.contains(a);
                out(Line::from(vec![
                    Span::styled(if def { "   ● " } else { "     " }, ui::fg(c::ACCENT())),
                    Span::styled(if inf { "▌ " } else { "  " }, Style::new().fg(theme::machine(&cfg, a))),
                    Span::styled(ui::pad(a, wa), if def { ui::bold() } else { Style::new() }),
                    Span::raw("  "),
                    Span::styled(ui::pad(r, wt), ui::muted()),
                    Span::raw("  "),
                    if *h { Span::styled("  ✓  ", ui::fg(c::GREEN())) } else { Span::styled("  –  ", ui::faint()) },
                    Span::raw("  "),
                    if inf { Span::styled("✓", ui::fg(c::GREEN())) } else { Span::styled("–", ui::faint()) },
                ]));
            }
            println!();
            hint("aleph host include <alias> adds a machine to the dashboard · aleph host set <alias> changes the default");
            ExitCode::SUCCESS
        }
        "pick" => {
            if term::interactive() {
                tui::run_dashboard(cfg, tui::Start { view: tui::View::Fleet, flag, machines: true, ..Default::default() })
            } else {
                host_cmd(cfg, flag, &["list".into()])
            }
        }
        "setup" => hostsetup::cmd(cfg, flag, rest),
        "include" | "join" => {
            let Some(alias) = rest.get(1).filter(|a| !a.starts_with('-')) else { return fail("usage: aleph host include <alias> [--here]") };
            let w = if rest.iter().any(|a| a == "--here") { hostsetup::Where::Here } else { hostsetup::Where::Auto };
            let mut v = cfg.hosts();
            if v.contains(alias) {
                ok(&format!("{alias} is already in the fleet"));
                hostsetup::after_join(&mut cfg, alias, w);
                return ExitCode::SUCCESS;
            }
            if !sshcfg::aliases().contains(alias) {
                warn(&format!("'{alias}' isn't a Host in your ssh config — using it as a plain hostname"));
            }
            v.push(alias.clone());
            if cfg.get("ssh_host").is_none() {
                let _ = cfg.set("ssh_host", alias);
            }
            if let Err(e) = cfg.set_hosts(&v) {
                return fail(&format!("couldn't save: {e}"));
            }
            theme::assign_machine_colors(&mut cfg);
            out(Line::from(vec![
                Span::styled("✓ ", ui::fg(c::GREEN()).add_modifier(Modifier::BOLD)),
                Span::styled("▌", Style::new().fg(theme::machine(&cfg, alias))),
                Span::raw(format!("{alias} joins the fleet")),
            ]));
            // no herdr or no agents there: its setup, in a new tab
            hostsetup::after_join(&mut cfg, alias, w);
            ExitCode::SUCCESS
        }
        "exclude" | "rm" | "remove" => {
            let Some(alias) = rest.get(1) else { return fail("usage: aleph host exclude <alias>") };
            if cfg.get("ssh_host").as_deref() == Some(alias.as_str()) {
                return fail("that's the default host — aleph host set <other> first");
            }
            let def = cfg.get("ssh_host");
            let v: Vec<String> = cfg.hosts().into_iter().filter(|h| h != alias && Some(h) != def.as_ref()).collect();
            if let Err(e) = cfg.set_hosts(&v) {
                return fail(&format!("couldn't save: {e}"));
            }
            ok(&format!("{alias} left the fleet (its sessions keep running)"));
            ExitCode::SUCCESS
        }
        "color" | "colour" => {
            let (Some(alias), Some(col)) = (rest.get(1), rest.get(2)) else {
                let names: Vec<&str> = theme::MACHINES.iter().map(|m| m.0).collect();
                return fail(&format!("usage: aleph host color <alias> <#hex | {}>", names.join(" | ")));
            };
            if ui::parse_color(col).is_none() {
                return fail("colours are #rrggbb, #rgb or a palette name (blue, mauve, teal, peach, pink, green, yellow, sky, lavender, flamingo)");
            }
            let _ = cfg.set(&format!("host.{alias}.color"), col);
            out(Line::from(vec![
                Span::styled("▌", Style::new().fg(theme::machine(&cfg, alias))),
                Span::styled(format!("{alias} is now {col}"), ui::bold()),
            ]));
            hint("aleph theme herdr --apply recolours herdr's sidebar too");
            ExitCode::SUCCESS
        }
        "set" => {
            let usage = "usage: aleph host set <ssh-alias> [--machine <herdr-label>]";
            let Some(alias) = rest.get(1) else { return fail(usage) };
            if alias.is_empty() || alias.contains(char::is_whitespace) || alias.starts_with('-') {
                return fail("that doesn't look like an ssh alias");
            }
            let machine = match (rest.get(2).map(String::as_str), rest.get(3)) {
                (Some("--machine"), Some(m)) => m.clone(),
                (None, _) => herdr::machine_for(alias),
                _ => return fail(usage),
            };
            if !sshcfg::aliases().contains(alias) {
                warn(&format!("'{alias}' isn't a Host in your ssh config — using it as a plain hostname"));
            }
            if let Err(e) = cfg.set("ssh_host", alias).and_then(|_| cfg.set("machine", &machine)) {
                return fail(&format!("couldn't write {}: {e}", cfg.path.display()));
            }
            theme::assign_machine_colors(&mut cfg);
            let extra = if machine != *alias { format!(" (herdr machine {machine})") } else { String::new() };
            ok(&format!("Default host is now {alias}{extra}"));
            onboard::run(&mut cfg, alias, &machine);
            ExitCode::SUCCESS
        }
        "add" => {
            let usage = "usage: aleph host add <alias> <[user@]hostname[:port]> [-i <identity-file>] [--no-default] [--here]";
            let (Some(alias), Some(tgt)) = (rest.get(1), rest.get(2)) else { return fail(usage) };
            let mut identity = None;
            let mut make_default = true;
            let mut here = false;
            let mut i = 3;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-i" if i + 1 < rest.len() => {
                        identity = Some(rest[i + 1].clone());
                        i += 2;
                    }
                    "--no-default" => {
                        make_default = false;
                        i += 1;
                    }
                    "--here" => {
                        here = true;
                        i += 1;
                    }
                    _ => return fail(usage),
                }
            }
            let (user, hostname, port) = match sshcfg::valid_alias(alias).and_then(|_| sshcfg::parse_target(tgt)) {
                Ok(x) => x,
                Err(e) => return fail(&e),
            };
            let h = sshcfg::NewHost { alias: alias.clone(), user, hostname, port, identity, jump: None };
            match sshcfg::add_host(&h) {
                Ok(b) => {
                    ok(&format!("Added Host {alias} to {}", doctor::tilde(&sshcfg::user_config())));
                    if let Some(b) = b {
                        say(Line::from(Span::styled(format!("  backup: {}", doctor::tilde(&b)), ui::faint())));
                    }
                }
                Err(e) => return fail(&e),
            }
            if make_default {
                // the default machine (and in the fleet), then the same look as include
                let machine = herdr::machine_for(alias);
                if let Err(e) = cfg.set("ssh_host", alias).and_then(|_| cfg.set("machine", &machine)) {
                    return fail(&format!("couldn't write {}: {e}", cfg.path.display()));
                }
                ok(&format!("Default host is now {alias}"));
            }
            let mut args = vec!["include".to_string(), alias.clone()];
            if here {
                args.push("--here".into());
            }
            host_cmd(cfg, None, &args)
        }
        "key" => {
            let alias = match rest.get(1) {
                Some(a) => a.clone(),
                None => match cli::target(&cfg, &flag) {
                    Some(t) => t.ssh,
                    None => return cli::no_host(),
                },
            };
            if !io::stdin().is_terminal() {
                return fail("key setup is interactive — run it in a terminal");
            }
            match sshcfg::setup_key(&alias) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    err(&e);
                    ExitCode::from(1)
                }
            }
        }
        _ => fail("usage: aleph host [show | list | pick | set <alias> | add <alias> <user@host> | include <alias> | setup [alias] | exclude <alias> | color <alias> <c> | key [alias]]"),
    }
}

// ------------------------------------------------------------------ help

/// Details for `aleph help <command>`.
const DETAILS: &[(&str, &[&str])] = &[
    ("new", &[
        "Starts an agent in a new herdr tab named <agent>_<verb>[:folder] (claude_ignite:px,",
        "codex_tune, …) and attaches.",
        "Names are picked to be fresh across every machine: no live or recent name,",
        "a first letter no other session uses, no look-alikes.",
        "",
        "  --dir /abs        folder on the host (CC_DIR works too)",
        "  --name n          choose the name (lowercase, digits, - _)",
        "  --agent k         codex, opencode, cursor, … (default: the agent setting; aleph agents)",
        "  --openrouter m    start on OpenRouter's model m (qwen/qwen3-coder), the agent's own",
        "                    way: OpenCode, Kilo, Pi, OMP (--model stays the agent's own flag)",
        "  --worktree[=b]    new git worktree on branch b (default <agent>/<name>)",
        "  --tile N          N agents tiled in one tab (1-9)",
        "  -m, --prompt t    first prompt",
        "  @preset           saved starting point (aleph preset add)",
        "  -- args…          passed to the agent",
        "",
        "With an OpenRouter key (aleph secret set openrouter), sessions of the agents in",
        "openrouter.agents get it as OPENROUTER_API_KEY — through herdr's socket only, never on",
        "a command line, on disk or in a log; without the socket it's left out, and new says so.",
        "",
        "  aleph new --dir /srv/px -m \"fix the flaky tests\"",
        "  aleph --host gpu2 new @train --tile 4",
        "  aleph new --agent opencode --openrouter qwen/qwen3-coder",
    ]),
    ("journal", &[
        "A Markdown file per day of what each session did, in ~/.local/share/aleph/journal/",
        "(journal.dir moves it). The daemon (else the dashboard) keeps today's up to date every",
        "half hour and when a session ends, and finishes yesterday's after midnight.",
        "",
        "  journal.include   prompts (sent through Aleph), finals (the agents' last words),",
        "                    files (commits and what isn't committed), cost (time, tokens, $)",
        "  --write           put it together now and save it",
        "  --json            the day as JSON (put together now)",
        "  path              where the files are",
        "",
        "  aleph journal yesterday > standup.md",
        "  aleph journal 2026-10-24 --write",
    ]),
    ("ls", &[
        "In a terminal: the master dashboard — every machine's herdr, grouped and coloured,",
        "with live machine stats, the selected session's question and output.",
        "  --plain    print the table     --watch   reprint every `refresh` seconds",
        "  --json     machine-readable    piped     host, name, status, cwd, pane, since, title (TSV)",
    ]),
    ("wall", &[
        "Live tiles of sessions from any machines (herdr terminal streams).",
        "  arrows move · enter attach (back here on detach) · z zoom · i type into a tile (ctrl+q stops, ctrl+q again goes to Fleet)",
        "  1-9 answer · s send a prompt · b broadcast · L layout · x drop tile · a add · esc back",
        "  aleph wall ignite tune     start with those sessions",
    ]),
    ("watch", &[
        "Polls every machine and notifies when a session needs you (and, with",
        "notify_done=true, when one finishes). Writes status.json for Raycast's menu bar.",
        "  --install     run at login = aleph daemon install (launchd / systemd --user)",
        "  --uninstall   = aleph daemon uninstall    ·    --once   one check, then exit",
    ]),
    ("daemon", &[
        "One daemon per computer polls the fleet, runs the hub (auto-approve, the queue,",
        "notifications) and answers the API on state/daemon.sock (aleph api).",
        "  aleph daemon install     run it at login: launchd dev.aleph.daemon on macOS (KeepAlive),",
        "                           systemd --user aleph.service on Linux (Restart=on-failure;",
        "                           says when linger is off, never turns it on) · --dry-run",
        "  aleph daemon start | stop | restart | uninstall      through launchd / systemd",
        "  aleph daemon status [--json]   service, socket, version, pid, hub, logs, HTTP API",
        "  aleph daemon logs [-f] [-n N] [--service]   its own lines · what launchd / the journal kept",
        "  aleph daemon doctor [--json] [--fix]        the same checks as Settings › Daemon & API",
        "  aleph daemon expose tailscale [--https-port N]   the HTTP API on your tailnet (tailscale serve,",
        "                           1.52+): https://<machine>.<tailnet>.ts.net, your devices only",
        "  aleph daemon expose cloudflare --hostname H      a named tunnel, its DNS route and a service",
        "                           of its own — public, so put Cloudflare Access in front (no quick tunnels)",
        "  aleph daemon expose off [tailscale | cloudflare] | status   takes back only what Aleph added",
        "  aleph daemon deploy <host> [--build host|cross] [--service] [--config copy|link|none]",
        "                           [--with-secrets] [--src DIR] [--dry-run] [--yes]",
        "                           a daemon of its own on that machine: built there (git archive of this",
        "                           source, nice -n 10 cargo build) or here (cargo zigbuild, musl), put in",
        "                           ~/.local/bin/aleph; your settings go too (secrets stay unless",
        "                           --with-secrets); --service installs it there; hub.machine is offered",
        "  aleph daemon run         in this terminal (aleph watch)",
    ]),
    ("agents", &[
        "Every agent herdr supports (Claude Code, Codex, OpenCode, OMP, Pi, Cursor, Devin,",
        "Gemini, Copilot, Droid, Amp, Kiro, Crush, Command Code, Prime Agent, …), per machine",
        "(OS, GPUs): where each one is, its version, herdr integration, whether its sessions",
        "survive a herdr restart and whether herdr's shell finds it. Looks beyond the PATH",
        "(~/.local/bin, ~/.opencode/bin, nvm/fnm node versions, the npm prefix, bun, cargo,",
        "pnpm, asdf/mise, Homebrew, …) and checks --version, so a same-named program or a",
        "broken install isn't taken for the agent. Answers are reused for 10 minutes.",
        "  --host H      one machine        --local   this machine too",
        "  --all         list agents that aren't installed (with how to install them)",
        "  --refresh     look again now     --json    machine-readable",
        "  piped         host, agent, version, integration, persistence, path, on_path, check (TSV)",
        "  aleph agents install <agent> [--host H] [--yes] [--dry-run]",
        "                shows the exact command and what it needs (node/npm, curl, uv, …), asks",
        "                once, streams the output, then checks the agent runs. Never uses sudo.",
        "  aleph agents setup [agent…] [--yes]   install/refresh herdr integrations (asks once)",
    ]),
    ("update", &[
        "Compares every machine's herdr with this machine's (herdr --machine needs the same",
        "version on both ends), outdated herdr integrations, agent versions across machines,",
        "and aleph itself. Offers herdr update on machines that are behind.",
        "  --check   only report     --yes   don't ask     --json   machine-readable",
        "The wrapped agent's own updater: command <agent> update (wraps setting)",
    ]),
    ("profile", &[
        "A profile is profiles/<name>.toml beside aleph.toml: only the settings it names, over",
        "yours, for every process (the daemon, dashboards, commands) until you turn it off.",
        "  aleph profile use demo      showcase on (a showcase build), cockpit, money hidden, …",
        "  aleph profile use travel    slower polls and stats, no LLM-server scans",
        "  aleph profile use work      straight to the fleet, every finished turn on the desktop",
        "  aleph profile off           your own settings again (and showcase as it was)",
        "  aleph profile set demo ui.splash classic · unset · save NAME KEY… · show · edit · rm",
        "Starters are written out on first use, for you to change. ALEPH_PROFILE=NAME: one shell.",
    ]),
    ("url", &[
        "aleph:// links, from Shortcuts (Open URLs), Stream Deck, a note or a script:",
        "  aleph://open?view=monitor            aleph://focus?session=b31:hunt",
        "  aleph://new?host=b31&dir=/srv/x&agent=codex&prompt=…",
        "  aleph://send?session=b31:hunt&text=…    aleph://approve?session=b31:hunt",
        "  aleph://close?session=b31:hunt          aleph://queue?text=…&host=b31",
        "Any page can open a link, so the ones that act always ask first (a dialog naming the",
        "session; approve shows its screen). aleph url install registers the scheme (macOS: a tiny",
        "applet made by osacompile in ~/Applications; Linux: a .desktop file + xdg-mime).",
    ]),
    ("backup", &[
        "One file with what Aleph keeps about you: settings (aleph.toml, profiles; presets and",
        "workspaces are settings), launch history, notes, session memory, the queue, setup answers.",
        "Secrets are left out (secret:NAME references stay; a restore keeps this computer's own).",
        "  aleph backup                 → ~/.local/share/aleph/backups/aleph-<computer>-<time>.json",
        "  aleph backup FILE --all      also the Inbox and the usage records",
        "  aleph backup list · show FILE",
        "  aleph backup restore FILE    shows each file and setting that changes, asks, and saves",
        "                               what it replaces first (--dry-run, --yes)",
    ]),
    ("restore", &[
        "Recreates sessions that disappeared when a machine rebooted or herdr restarted:",
        "same folder, name and agent, continuing the last conversation where the agent can",
        "(claude --continue, codex resume --last, opencode --continue, …). Agents with a herdr",
        "integration are restored by herdr itself and skipped.",
        "  --dry-run   list only      --all   also sessions that ended while the machine stayed up",
        "  --days N    look back N days (3)   --yes   don't ask   --json",
    ]),
    ("queue", &[
        "Tasks for a machine's next idle agent, first in, first out. With --dir only a session in",
        "that folder takes it, with --agent only that agent, --new starts a fresh session instead.",
        "--to host:name queues it for one session: it goes once that agent is done with its reply.",
        "Handed out while aleph watch runs (or the dashboard, when no watch is running).",
        "  aleph queue add b31 \"run the full test suite and fix what fails\" --dir /srv/px",
        "  aleph queue add \"then commit it\" --to b31:hunt       in Focus: i, then tab",
        "  aleph queue ls · rm <id> · clear [host]          dashboard: Q",
    ]),
    ("auto", &[
        "Auto-approve: when a session starts asking, its question is matched against your rules;",
        "a match answers with the question's yes option (never blindly 1). approve_deny always wins",
        "(default: rm -rf, sudo, git push, --force, drop table, …). Every answer is in the inbox.",
        "  aleph auto add tests '*pytest*' · aleph auto on · aleph auto test 'Run pytest?\\n ❯ 1. Yes'",
        "Self-heal (auto_heal=true): herdr is restarted where it stopped and lost sessions restored.",
    ]),
    ("notify", &[
        "Notifications beyond this computer, for the events in notify.remote_on (default: blocked):",
        "  notify.ntfy=https://ntfy.sh/<secret-topic>   phone push (ntfy app)",
        "  notify.slack=<incoming-webhook URL>          notify.webhook=<URL> (the event as JSON)",
        "  aleph notify test   sends a test to each one",
    ]),
    ("inbox", &[
        "Recent events across machines, newest first: ▲ needs you · ✓ finished · + started ·",
        "– ended · ✕ unreachable · ⚡ auto-approved · ≡ task sent · ↻ herdr restarted.",
        "Written by every view that polls the fleet (dashboard, watch, ls). What you archived in",
        "the dashboard's Inbox stays out unless --all.",
        "  -n N   how many (30)    --all   the archive too    --json (archived ones say so)",
        "  piped: TSV (t, host, name, kind, title)",
        "  aleph inbox read [T]        mark everything read (up to unix time T)",
        "  aleph inbox dismiss ID…     archive events by id (the ids in --json)",
    ]),
    ("setup", &[
        "A full-screen wizard in 12 steps; each can be skipped, and a stopped run picks up where it was:",
        "  1 preflight · 2 look & feel · 3 machines · 4 herdr · 5 agents · 6 notifications ·",
        "  7 analytics · 8 automation · 9 daemon & access · 10 integrations · 11 presets · 12 review",
        "Nothing is written before Review (a diff of aleph.toml and the actions); external steps ask first.",
        "  aleph setup <step>         one step (machines, notifications, 3, …)",
        "  aleph setup --defaults     quick start: every default, what's missing here installed, then Aleph",
        "  aleph setup --redo         from your current settings",
        "  aleph setup --apply FILE   declarative: settings, presets, [setup] actions (--dry-run, --json)",
        "  aleph setup --plain        line mode (also without a terminal)",
        "In the dashboard, Settings › S runs the step for that section.",
    ]),
    ("theme", &[
        "  aleph theme                 machine colours and session hues",
        "  aleph theme herdr [--apply]  colour herdr's sidebar: machine labels + session hues",
        "  aleph theme machines [--apply]  give each machine's herdr its own accent colour",
    ]),
    ("services", &[
        "Local LLM servers and AI gateways on each machine: Ollama, LM Studio, llama.cpp, vLLM,",
        "TGI, LiteLLM, LocalAI, Jan, text-generation-webui, KoboldCpp, Open WebUI, SGLang, TabbyAPI,",
        "Aphrodite, mlx-lm, GPT4All, Xinference, Triton, TensorRT-LLM, llama-swap — version, port,",
        "health, models (loaded first) and VRAM. Found by their processes or by HTTP answers that",
        "identify them (never by a port number alone); read-only, nothing is installed.",
        "  --host H     one machine (local = this computer)",
        "  --json       machine-readable (Raycast)    piped   host, service, version, port, health, … (TSV)",
        "  aleph settings set services on             keep them live in the dashboard (Fleet, Monitor)",
        "  aleph settings set service.<id> off        never look for one (services_interval: seconds)",
    ]),
    ("usage", &[
        "Tokens, requests, sessions, active time and cost per machine, agent, model, effort, folder,",
        "day or session, from the agents' own logs: Claude Code, Codex, OpenCode, Pi / OMP, Gemini,",
        "Qwen. A plain sh + awk script reads only what the logs gained since last time and prints ids,",
        "timestamps, models, numbers and folders — never conversation text. Nothing is installed.",
        "  aleph usage                    the last 7 days by model (machines not read lately first)",
        "  aleph usage today --by host    · 30d --by day · all --by session · --since 2026-10-01",
        "  periods: 24h 3d 7d 30d 6mo all · today this-week this-month last-month · 2026-09-01..2026-10-15",
        "  --stats: per prompt and per session (median, p90, the costliest) · --compare: vs the period before",
        "  filters take lists: --agent claude,codex --host b31,gpu2",
        "  --host H · --agent A · --model M · --json · --csv · piped: TSV · --no-sync",
        "  aleph usage accounts           every account the agents were signed in to (two Claude logins,",
        "                                 one ChatGPT account on three machines), each one's 30 days",
        "  --by account · --account a3f2c1|you@work.com|work   (≈: guessed, from before Aleph knew whose)",
        "  aleph usage projects           every project: a repo wherever it was worked on (its remote,",
        "                                 else its first commit); --by project · --project app · --by folder",
        "  aleph usage sync [--full] [--until-done]   read now · sources: what each machine gave",
        "  aleph usage script --host H    the exact script, to read before it runs anywhere",
        "  aleph usage off [--host H] [--agent A] · on · prune [--keep 400d | --host H] [--dry-run]",
        "Cost is what the agent reported, else an estimate (est.) from the price lists, read hourly and",
        "dated so a change never re-prices past usage (analytics.prices_sync):",
        "  aleph usage prices [MODEL] [--all]   each model's rate, where it came from, what's held",
        "  aleph usage prices sync · accept MODEL   read the lists now · take a held change",
        "  aleph settings set analytics.prices.<model> in,out[,cache write 5m,1h,cache read]  ($/MTok)",
        "The daemon (aleph watch) reads every analytics.interval seconds; analytics.max_seconds and",
        "analytics.max_mb bound each read (renice 19), analytics.backfill the first one.",
        "With an OpenRouter key (openrouter.key), the account's own spend from its API goes under",
        "the table and into --json (openrouter: today, week, month, total, credits, the key's limit),",
        "read again after 5 minutes (--no-sync: what's kept).",
    ]),
    ("secret", &[
        "Secrets out of the settings file: a setting says secret:NAME and the value lives in the",
        "macOS Keychain, the Secret Service (secret-tool), or state/secrets.toml (0600) where",
        "neither works. Exports, backups, logs and the journal never show one.",
        "  aleph secret set NAME [VALUE|-]   - or nothing: read it from stdin (nothing in your history)",
        "  aleph secret ls                   every secret:NAME setting, and the names Aleph knows",
        "  aleph secret get NAME · rm NAME · move (ntfy / Slack / webhook / OpenRouter values → secret:…)",
        "Names Aleph knows: openrouter — the OpenRouter API key (openrouter.key points at it): sessions",
        "of openrouter.agents get it as OPENROUTER_API_KEY, Usage shows its spend.",
        "  aleph secret set openrouter       paste the key, then enter and ctrl+d",
    ]),
    ("host", &[
        "  aleph host include <alias>    add a machine to the dashboard (gets a colour); one with",
        "                                no herdr or no agents gets its setup in a new tab (--here: this one)",
        "  aleph host setup [alias]      herdr, then a checklist of agents: installed one after another,",
        "                                each checked; retry / skip / the log on a failure; Aleph is told",
        "                                when it's done   [--agents a,b] [--yes] [--no-herdr] [--no-agents]",
        "  aleph host exclude <alias>    remove it (sessions keep running)",
        "  aleph host color <alias> <c>  #hex or blue/mauve/teal/peach/pink/green/yellow/sky/…",
        "  aleph host set <alias>        change the default (where aleph new goes)",
        "  aleph host add <alias> <user@host[:port]> [-i key] [--here]   new ssh alias, then as include",
        "  aleph host key [alias]        one-time key login setup",
    ]),
];

/// `aleph help <words>` that aren't a command: the commands that do it, best
/// first ("aleph help terminal" → `local … shell`, "kill" → `close`).
fn help_search(q: &str, color: bool) -> ExitCode {
    type Cmd = (&'static str, &'static str, &'static str, &'static str);
    let mut v: Vec<(i32, &Cmd)> = COMMANDS
        .iter()
        .filter_map(|c| {
            let text = format!("{} {} {}", c.0, c.2, c.1);
            helpsearch::rank(q, &[c.0], &text, &[c.3]).map(|s| (s, c))
        })
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.0));
    if v.is_empty() {
        return fail(&format!(
            "no command does '{q}' — aleph help lists them all"
        ));
    }
    for (_, c) in v.iter().take(6) {
        println!(
            "{}",
            ui::to_ansi(
                &Line::from(vec![
                    Span::styled(format!("  aleph {:<12}", c.0), ui::bold()),
                    Span::styled(c.2.to_string(), ui::muted()),
                ]),
                color
            )
        );
    }
    hint(&format!("aleph help {} — how to use it", v[0].1 .0));
    ExitCode::SUCCESS
}

fn help(cfg: &Config, flag: Option<&str>, topic: Option<&str>) -> ExitCode {
    let color = term::color_out();
    let p = |l: Line| println!("{}", ui::to_ansi(&l, color));
    if let Some(tpc) = topic {
        let Some(name) = canonical(tpc) else {
            return help_search(tpc, color);
        };
        let Some(cmd) = COMMANDS.iter().find(|c| c.0 == name) else {
            return fail(&format!("no command '{tpc}'"));
        };
        p(Line::from(vec![
            Span::styled("Usage: ", ui::bold()),
            Span::styled(format!("aleph {}", cmd.0), ui::bold()),
            Span::styled(format!(" {}", cmd.1), ui::muted()),
        ]));
        p(Line::from(Span::raw(format!("  {}", cmd.2))));
        if let Some((_, lines)) = DETAILS.iter().find(|d| d.0 == name) {
            println!();
            for l in *lines {
                p(Line::from(Span::raw(l.to_string())));
            }
        }
        return ExitCode::SUCCESS;
    }

    // rounded welcome box
    let fleet = cfg.hosts();
    let host_line = match cfg.target(flag, |a| a.to_string()) {
        Some(t) => {
            let mut v = vec![Span::styled("  fleet  ", ui::faint())];
            for (i, h) in fleet.iter().enumerate() {
                if i > 0 {
                    v.push(Span::raw(" "));
                }
                v.push(Span::styled("▌", Style::new().fg(theme::machine(cfg, h))));
                v.push(Span::styled(
                    h.clone(),
                    if *h == t.ssh { ui::bold() } else { ui::muted() },
                ));
            }
            Line::from(v)
        }
        None => Line::from(vec![
            Span::styled("  host  ", ui::faint()),
            Span::styled("none yet — aleph host pick", ui::fg(c::YELLOW())),
        ]),
    };
    let body = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", brand::MARK),
                ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
            ),
            Span::styled("aleph", ui::bold()),
            Span::styled(format!(" {}", env!("CARGO_PKG_VERSION")), ui::muted()),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Agent sessions inside herdr, across your machines",
            Style::new(),
        )),
        host_line,
    ];
    let w = body.iter().map(ui::line_width).max().unwrap_or(40) + 6;
    for l in boxed(w.max(60).min(ui::print_width()), body) {
        p(l);
    }
    println!();
    p(Line::from(vec![
        Span::styled("Usage: ", ui::bold()),
        Span::raw("aleph "),
        Span::styled("[--host <alias>] [--debug] [command] [args…]", ui::muted()),
    ]));
    p(Line::from(vec![
        Span::raw("       aleph"),
        Span::styled(
            "              the dashboard: every machine, live stats, tiles",
            ui::muted(),
        ),
    ]));
    let row = |cmd: &str, args: &str, desc: &str| {
        let shown_args = ui::trunc(args, 34usize.saturating_sub(ui::width(cmd)));
        let left = format!("aleph {cmd} {shown_args}");
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("aleph {cmd}"), ui::bold()),
            Span::styled(format!(" {shown_args}"), ui::muted()),
            Span::raw(" ".repeat(44usize.saturating_sub(ui::width(&left)).max(2))),
            Span::raw(desc.to_string()),
        ])
    };
    // one heading per section, in the order sections first appear
    let mut sections: Vec<&str> = Vec::new();
    for (_, _, _, sec) in COMMANDS {
        if !sections.contains(sec) {
            sections.push(sec);
        }
    }
    for sec in sections {
        println!();
        p(Line::from(Span::styled(
            sec.to_string(),
            ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
        )));
        for (cmd, args, desc, _) in COMMANDS.iter().filter(|x| x.3 == sec) {
            if PLANNED.iter().any(|p| p.0 == *cmd) {
                let mut l = row(cmd, args, desc);
                l.spans.push(Span::styled("  (planned)", ui::faint()));
                p(l);
            } else {
                p(row(cmd, args, desc));
            }
        }
    }
    println!();
    p(Line::from(Span::styled(
        "Keys",
        ui::fg(c::ACCENT()).add_modifier(Modifier::BOLD),
    )));
    let keys = |label: &str, items: &[(&str, &str)]| {
        let mut l = vec![Span::styled(
            format!("  {}", ui::pad(label, 10)),
            ui::muted(),
        )];
        l.extend(ui::hints(items).spans.into_iter().skip(1));
        p(Line::from(l));
    };
    keys(
        "dashboard",
        &[
            ("enter", "attach"),
            ("n", "new"),
            ("w", "wall"),
            ("ctrl+k", "jump anywhere"),
            ("?", "all keys"),
        ],
    );
    keys(
        "wall",
        &[
            ("↑↓←→", "move"),
            ("enter", "attach"),
            ("z", "zoom"),
            ("i", "type"),
            ("1-9", "answer"),
        ],
    );
    keys(
        "browser",
        &[
            ("type", "fuzzy search"),
            ("enter", "start"),
            ("→", "open"),
            ("~/ /", "jump"),
            ("ctrl+o", "show all"),
        ],
    );
    keys(
        "herdr",
        &[("ctrl+b q", "detach — the session keeps running")],
    );
    println!();
    p(Line::from(vec![
        Span::styled("config  ", ui::faint()),
        Span::styled(doctor::tilde(&cfg.path), ui::muted()),
        Span::styled("  ·  aleph settings", ui::faint()),
    ]));
    p(prefixed(
        "hint",
        c::BLUE(),
        &passthrough_hint(&agents::Wraps::from_cfg(cfg)),
    ));
    ExitCode::SUCCESS
}

/// `-p, --help, --version, mcp, config, install… run your local Claude Code —
/// so does `command claude` (its own update and agents too)`.
fn passthrough_hint(w: &agents::Wraps) -> String {
    // its first own flag, --help, --version, then a few of its subcommands
    let flags = w.flags();
    let mut shown: Vec<&str> = flags
        .iter()
        .copied()
        .take_while(|f| *f != "-h")
        .take(1)
        .collect();
    shown.extend(["--help", "--version"]);
    shown.extend(w.cmds().iter().take(3));
    // its own subcommands that ours shadow (from the cached `--help`; never runs it)
    let shadowed: Vec<String> = w
        .subcommands(false)
        .into_iter()
        .filter(|c| COMMANDS.iter().any(|x| x.0 == c))
        .take(3)
        .collect();
    let tail = if shadowed.is_empty() {
        String::new()
    } else {
        format!(" (its own {} too)", shadowed.join(", "))
    };
    format!(
        "aleph help <command> for details · {}… run your local {} — so does `command {}`{tail}",
        shown.join(", "),
        w.name(),
        w.exe
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_hint_names_the_wrapped_agent() {
        let h = passthrough_hint(&agents::Wraps::parse("claude"));
        assert!(
            h.contains("-p, --help, --version, mcp, config, install…"),
            "{h}"
        );
        assert!(
            h.contains("run your local Claude Code — so does `command claude`"),
            "{h}"
        );
        let h = passthrough_hint(&agents::Wraps::parse("codex"));
        assert!(
            h.contains("--help, --version…") && h.contains("local Codex"),
            "{h}"
        );
    }

    #[test]
    fn commands_resolve() {
        assert_eq!(canonical("list"), Some("ls"));
        assert_eq!(canonical("attach"), Some("resume"));
        assert_eq!(canonical("ui"), Some(""));
        assert_eq!(canonical("nope"), None);
        for (c, _, _, _) in COMMANDS {
            assert_eq!(canonical(c), Some(*c));
        }
    }
}
