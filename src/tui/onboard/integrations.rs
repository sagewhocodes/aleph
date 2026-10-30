//! Step 10, Integrations: Aleph in the tools around it, each opt-in. Tab
//! completion and the opt-in `claude` function in your shell's rc file; the
//! Raycast extension (from Aleph's source, or this version's release file, built
//! and imported, and its menu bar asked for); a companion
//! plugin for this computer's herdr with prefix keys (prefix+a the dashboard,
//! prefix+i the Inbox, prefix+y approve the focused agent); Moshi; and the
//! extras that make notifications and logos look right (terminal-notifier, a
//! Nerd Font) as commands you see before they run.

use super::rows::{Row, St};
use super::{Action, Ev, Hand, Msg, Step, Wiz};
use crate::config;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Facts {
    pub shell: String,
    pub rc: Option<PathBuf>,
    pub completions: bool,
    pub wrapper: bool,
    pub raycast_app: bool,
    pub raycast_src: Option<PathBuf>,
    pub npm: bool,
    pub herdr: bool,
    pub plugin: bool,
    pub notifier: bool,
    pub nerd: Option<bool>,
    pub brew: bool,
}

#[derive(Default)]
pub(crate) struct State {
    pub facts: Option<Facts>,
}

pub(crate) fn enter(w: &mut Wiz) {
    if w.ig.facts.is_none() {
        retry(w);
    }
}

pub(crate) fn retry(w: &mut Wiz) {
    w.spawn("integ", "Looking at your shell, Raycast and herdr", || {
        Msg::Integ(facts())
    });
}

fn exe_word() -> String {
    if crate::proc::which(crate::brand::BIN).is_some() {
        crate::brand::BIN.to_string()
    } else {
        crate::proc::sh_quote(&super::self_exe())
    }
}

fn facts() -> Facts {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let target = crate::setup::completion_target(&shell, &config::home(), &exe_word());
    let (sh, rc, comp) = match &target {
        Some((sh, rc, line)) => {
            let cur = std::fs::read_to_string(rc).unwrap_or_default();
            (
                sh.to_string(),
                Some(rc.clone()),
                crate::setup::with_completions(&cur, line).is_none(),
            )
        }
        None => (String::new(), None, false),
    };
    let zrc = zshrc();
    let wrapper = std::fs::read_to_string(&zrc)
        .map(|t| has_wrapper(&t))
        .unwrap_or(false);
    let home = config::home();
    let herdr_cfg = crate::herdrtheme::herdr_config();
    Facts {
        shell: sh,
        rc,
        completions: comp,
        wrapper,
        raycast_app: [
            PathBuf::from("/Applications/Raycast.app"),
            home.join("Applications/Raycast.app"),
        ]
        .iter()
        .any(|p| p.exists()),
        raycast_src: raycast_src(),
        npm: crate::proc::which("npm").is_some(),
        herdr: crate::proc::which(&crate::herdr::bin()).is_some(),
        plugin: plugin_dir().join("herdr-plugin.toml").exists()
            && std::fs::read_to_string(herdr_cfg).is_ok_and(|t| t.contains(KEYS_BEGIN)),
        notifier: crate::proc::which("terminal-notifier").is_some(),
        nerd: crate::icons::terminal_nerd_font()
            .or(Some(crate::icons::nerd_font_installed()).filter(|x| *x)),
        brew: crate::proc::which("brew").is_some(),
    }
}

fn zshrc() -> PathBuf {
    std::env::var_os("ZDOTDIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(config::home)
        .join(".zshrc")
}

/// The release file holding the Raycast extension's source (`release.yml`).
const RAYCAST_FILE: &str = "aleph-raycast-extension.tar.gz";

/// Where this version's extension goes when it came from its release.
fn fetched_raycast() -> PathBuf {
    config::data_dir()
        .join("raycast-extension")
        .join(env!("CARGO_PKG_VERSION"))
}

/// The Raycast extension's source: next to Aleph when it runs from a checkout, or
/// already fetched for this version.
fn raycast_src() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .take(5)
        .map(|d| d.join("extras/raycast-extension"))
        .chain(std::iter::once(fetched_raycast()))
        .find(|d| d.join("package.json").is_file())
}

/// This version's extension from its release, checksum checked, unpacked into `to`.
fn fetch_raycast(cfg: &config::Config, to: &Path) -> Result<(), String> {
    let tmp = to.with_extension(format!("tmp-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let got = (|| {
        let file = tmp.join(RAYCAST_FILE);
        crate::selfupdate::fetch_own_release_file(cfg, RAYCAST_FILE, &file)?;
        let o = crate::proc::cap(
            "tar",
            &[
                "-xzf",
                &file.to_string_lossy(),
                "-C",
                &tmp.to_string_lossy(),
            ],
            Duration::from_secs(60),
        );
        if !o.ok() {
            return Err(format!("couldn't unpack it: {}", o.reason()));
        }
        let src = tmp.join("raycast-extension");
        if !src.join("package.json").is_file() {
            return Err("the release file has no extension in it".to_string());
        }
        let _ = std::fs::remove_dir_all(to);
        std::fs::rename(&src, to).map_err(|e| e.to_string())
    })();
    let _ = std::fs::remove_dir_all(&tmp);
    got
}

/// Raycast's own link to the extension's menu bar command (`author` and the command's
/// name are in its package.json). Raycast asks you to confirm a link once.
fn menu_bar_link() -> &'static str {
    "raycast://extensions/aleph/aleph/menu-bar?launchType=background"
}

/// Fetch the Raycast extension if it isn't here, build and import it: `npm
/// install`, then `ray develop` — which imports it into Raycast and then watches
/// for edits forever, so it's stopped (ctrl+c's SIGINT) a few seconds after it says
/// it built; the extension stays in Raycast. Then its menu bar is asked for. Each
/// line of output goes to `on_line`; npm's audit, funding and deprecation chatter
/// stays out.
pub(crate) fn apply_raycast(mut on_line: impl FnMut(&str)) -> Result<String, String> {
    use std::cell::Cell;
    use std::process::Command;
    use std::time::{Duration, Instant};
    let src = match raycast_src() {
        Some(p) => p,
        None => {
            on_line("fetching the extension from this version's release");
            let to = fetched_raycast();
            fetch_raycast(&config::Config::load(), &to)?;
            to
        }
    };
    let with_env = |c: &mut Command| {
        c.current_dir(&src)
            .env("NODE_OPTIONS", "--no-deprecation")
            .env("NO_UPDATE_NOTIFIER", "1")
            .env("npm_config_update_notifier", "false");
        if cfg!(feature = "showcase") {
            c.env("ALEPH_SHOWCASE", "1");
        }
    };
    let tail = |o: &crate::proc::Out| {
        let text = super::review::strip(&format!("{}\n{}", o.text(), o.stderr));
        text.lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty() && !l.starts_with("npm warn"))
            .map(String::from)
            .unwrap_or_else(|| o.reason())
    };
    on_line("npm install");
    let mut c = Command::new("npm");
    c.args(["install", "--no-audit", "--no-fund", "--loglevel=error"]);
    with_env(&mut c);
    let o = crate::proc::stream(&mut c, Duration::from_secs(300), None, &mut on_line)
        .map_err(|e| format!("npm: {e}"))?;
    if !o.ok() {
        return Err(format!("npm install: {}", tail(&o)));
    }
    let mut c = Command::new("sh");
    c.args(["-c", "sh scripts/sync-logos.sh && sh scripts/showcase.sh"]);
    with_env(&mut c);
    let o = crate::proc::stream(&mut c, Duration::from_secs(60), None, &mut on_line)
        .map_err(|e| e.to_string())?;
    if !o.ok() {
        return Err(tail(&o));
    }
    on_line("ray develop");
    let built: Cell<Option<Instant>> = Cell::new(None);
    let mut c = Command::new(src.join("node_modules/.bin/ray"));
    c.arg("develop");
    with_env(&mut c);
    let o = crate::proc::stream_until(
        &mut c,
        Duration::from_secs(180),
        None,
        |l| {
            if built.get().is_none() && l.contains("built extension successfully") {
                built.set(Some(Instant::now()));
            }
            on_line(l);
        },
        // a moment for Raycast to pick it up
        || {
            built
                .get()
                .is_some_and(|t| t.elapsed() > Duration::from_secs(4))
        },
    )
    .map_err(|e| format!("ray: {e}"))?;
    if built.get().is_some() {
        on_line("asking Raycast for the menu bar");
        let asked = crate::proc::cap("open", &["-g", menu_bar_link()], Duration::from_secs(10));
        Ok(if asked.ok() {
            "imported into Raycast. It asks once to run the menu bar: confirm it there".into()
        } else {
            "imported into Raycast: search Aleph there, and run its Menu Bar command once".into()
        })
    } else if o.timed_out {
        Err("ray develop didn't finish building in 3 minutes — is Raycast running?".into())
    } else {
        Err(format!("ray develop: {}", tail(&o)))
    }
}

pub(crate) fn plugin_dir() -> PathBuf {
    config::Config::dir().join("herdr-plugin")
}

// ------------------------------------------------------------------ pure edits

/// Does an rc file's Aleph block define the `claude` wrapper?
pub(crate) fn has_wrapper(rc: &str) -> bool {
    block_of(rc).is_some_and(|b| crate::brand::rc_functions(&b).iter().any(|n| n == "claude"))
}

fn block_of(rc: &str) -> Option<String> {
    let lines: Vec<&str> = rc.lines().collect();
    let s = lines
        .iter()
        .position(|l| l.trim() == crate::brand::RC_BEGIN)?;
    let e = lines[s..]
        .iter()
        .position(|l| l.trim() == crate::brand::RC_END)?
        + s;
    Some(lines[s..=e].join("\n"))
}

/// Pure: `rc` with the `claude` function added to Aleph's block (the block
/// is created when there's none). None: it's there already.
pub(crate) fn with_wrapper(rc: &str, exe: &str) -> Option<String> {
    if has_wrapper(rc) {
        return None;
    }
    let lines: Vec<&str> = rc.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == crate::brand::RC_BEGIN);
    let end = start.and_then(|s| {
        lines[s..]
            .iter()
            .position(|l| l.trim() == crate::brand::RC_END)
            .map(|e| e + s)
    });
    match (start, end) {
        (Some(s), Some(e)) => {
            let block = lines[s..=e].join("\n");
            let mut names = crate::brand::rc_functions(&block);
            names.retain(|n| n != crate::brand::ALIAS);
            names.push("claude".into());
            let comp = block.contains("completions zsh");
            let mut out = String::new();
            for l in &lines[..s] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(&crate::brand::rc_block(exe, &names, comp));
            for l in &lines[e + 1..] {
                out.push_str(l);
                out.push('\n');
            }
            Some(out)
        }
        _ => {
            let mut out = rc.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&crate::brand::rc_block(exe, &["claude".to_string()], false));
            Some(out)
        }
    }
}

const KEYS_BEGIN: &str = "# >>> aleph keys >>>";
const KEYS_END: &str = "# <<< aleph keys <<<";

/// The plugin manifest (`herdr-plugin.toml`): one action, approve the
/// focused agent with its own yes (herdr hands it `HERDR_PANE_ID`).
pub(crate) fn plugin_manifest(exe: &str) -> String {
    let approve = format!("\"{exe}\" approve \"local:$HERDR_PANE_ID\"");
    format!(
        "# written by aleph setup (Integrations) — herdr plugin link {dir}\n\
         id = \"aleph\"\nname = \"Aleph\"\nversion = \"{ver}\"\nmin_herdr_version = \"0.9.5\"\nplatforms = [\"linux\", \"macos\"]\n\n\
         [[actions]]\nid = \"approve\"\ntitle = \"Approve (Aleph)\"\ncontexts = [\"workspace\"]\ncommand = [\"sh\", \"-c\", {approve:?}]\n",
        dir = crate::doctor::tilde(&plugin_dir()),
        ver = env!("CARGO_PKG_VERSION"),
    )
}

/// The key bindings for herdr's config.toml (between Aleph's markers):
/// prefix+a the dashboard and prefix+i the Inbox as popups, prefix+y the
/// plugin's approve.
pub(crate) fn keys_block(exe: &str) -> String {
    let dash = format!("\"{exe}\" --local");
    let inbox =
        format!("sh -c '\"{exe}\" --local inbox -n 40; printf \"\\n  enter closes\"; read -r _'");
    format!(
        "{KEYS_BEGIN}\n# written by aleph setup (Integrations): prefix+a Aleph · prefix+i Inbox · prefix+y approve\n\
         [[keys.command]]\nkey = \"prefix+a\"\ntype = \"popup\"\ncommand = {dash:?}\ndescription = \"Aleph: the dashboard\"\n\n\
         [[keys.command]]\nkey = \"prefix+i\"\ntype = \"popup\"\ncommand = {inbox:?}\ndescription = \"Aleph: the Inbox\"\n\n\
         [[keys.command]]\nkey = \"prefix+y\"\ntype = \"plugin_action\"\ncommand = \"aleph.approve\"\ndescription = \"Aleph: approve the focused agent\"\n{KEYS_END}\n"
    )
}

/// Pure: `src` with Aleph's keys block in place of the old one (or added).
pub(crate) fn set_keys_block(src: &str, block: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let s = lines.iter().position(|l| l.trim() == KEYS_BEGIN);
    let e = s.and_then(|s| {
        lines[s..]
            .iter()
            .position(|l| l.trim() == KEYS_END)
            .map(|e| e + s)
    });
    let mut out = String::new();
    match (s, e) {
        (Some(s), Some(e)) => {
            for l in &lines[..s] {
                out.push_str(l);
                out.push('\n');
            }
            out.push_str(block);
            for l in &lines[e + 1..] {
                out.push_str(l);
                out.push('\n');
            }
        }
        _ => {
            out.push_str(src);
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(block);
        }
    }
    out
}

// ------------------------------------------------------------------ applying

fn backup(p: &Path) -> Result<(), String> {
    if p.exists() {
        let mut b = p.as_os_str().to_owned();
        b.push(format!(".bak.aleph.{}", crate::cache::now()));
        std::fs::copy(p, PathBuf::from(b))
            .map_err(|e| format!("backup of {}: {e}", crate::doctor::tilde(p)))?;
    }
    Ok(())
}

pub(crate) fn apply_completions() -> Result<String, String> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let (sh, rc, line) = crate::setup::completion_target(&shell, &config::home(), &exe_word())
        .ok_or_else(|| {
            format!("unknown shell '{shell}' — aleph completions zsh|bash|fish prints the script")
        })?;
    let cur = std::fs::read_to_string(&rc).unwrap_or_default();
    let Some(new) = crate::setup::with_completions(&cur, &line) else {
        return Ok(format!(
            "{sh} completion was already in {}",
            crate::doctor::tilde(&rc)
        ));
    };
    backup(&rc)?;
    config::write_atomic(&rc, new.as_bytes()).map_err(|e| format!("{}: {e}", rc.display()))?;
    Ok(format!(
        "{sh} completion in {} — a new shell has it",
        crate::doctor::tilde(&rc)
    ))
}

pub(crate) fn apply_wrapper() -> Result<String, String> {
    let rc = zshrc();
    let cur = std::fs::read_to_string(&rc).unwrap_or_default();
    let Some(new) = with_wrapper(&cur, &super::self_exe()) else {
        return Ok(format!(
            "the claude function was already in {}",
            crate::doctor::tilde(&rc)
        ));
    };
    backup(&rc)?;
    config::write_atomic(&rc, new.as_bytes()).map_err(|e| format!("{}: {e}", rc.display()))?;
    Ok(format!(
        "claude() in {} — a new shell has it",
        crate::doctor::tilde(&rc)
    ))
}

pub(crate) fn apply_plugin() -> Result<String, String> {
    let exe = super::self_exe();
    let dir = plugin_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    config::write_atomic(
        &dir.join("herdr-plugin.toml"),
        plugin_manifest(&exe).as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let hc = crate::herdrtheme::herdr_config();
    let src = std::fs::read_to_string(&hc).unwrap_or_default();
    let new = set_keys_block(&src, &keys_block(&exe));
    if new != src {
        backup(&hc)?;
        config::write_atomic(&hc, new.as_bytes()).map_err(|e| format!("{}: {e}", hc.display()))?;
    }
    let o = crate::proc::run(
        std::process::Command::new(crate::herdr::bin())
            .args(["plugin", "link"])
            .arg(&dir),
        Duration::from_secs(15),
        None,
    )
    .map_err(|e| format!("herdr plugin link: {e}"))?;
    if !o.ok() {
        return Err(format!("herdr plugin link: {}", o.reason()));
    }
    Ok("herdr plugin linked; prefix+a / i / y in herdr (reload its config)".into())
}

// ------------------------------------------------------------------ rows

pub(crate) fn rows(w: &Wiz) -> Vec<Row> {
    let Some(f) = w.ig.facts.clone() else {
        return vec![Row::check(St::Busy, "integ", "…", "looking".into())];
    };
    let mut v = vec![Row::head("Shell")];
    match &f.rc {
        Some(rc) if f.completions => v.push(Row::check(St::Ok, "comp", "completions", format!("{} · {}", f.shell, crate::doctor::tilde(rc)))),
        Some(rc) => v.push(
            Row::pick("comp", "tab completion", w.plan.has(&Action::Completions))
                .note(format!("{} · adds two lines to {}", f.shell, crate::doctor::tilde(rc)))
                .why("Completes Aleph's commands, sessions and machines with tab. A marked block is added to your rc file (a backup is kept)."),
        ),
        None => v.push(Row::info(format!("No completion for this shell ({}): aleph completions zsh|bash|fish prints the script.", f.shell))),
    }
    if f.wrapper {
        v.push(Row::check(
            St::Ok,
            "wrap",
            "claude function",
            "in ~/.zshrc".into(),
        ));
    } else {
        v.push(
            Row::pick("wrap", "the `claude` function", w.plan.has(&Action::Wrapper))
                .note("`claude` opens Aleph; Claude Code's own commands pass through")
                .why("Opt-in: a claude() function in Aleph's block of ~/.zshrc. `claude` then is Aleph (the dashboard, aleph new, …) while everything Claude Code owns — its flags, its subcommands, piped input, inside herdr panes — goes straight to Claude Code. `al` always works."),
        );
    }
    v.push(Row::head("Raycast"));
    if !cfg!(target_os = "macos") {
        v.push(Row::info("Raycast is a macOS app: its Aleph extension installs on your Mac (aleph setup there, step 10)."));
    } else if !f.raycast_app {
        v.push(Row::info("Raycast isn't installed (raycast.com) — the extension lists sessions, approves and shows a menu bar count."));
    } else if f.npm || f.brew {
        let how = match (&f.raycast_src, f.npm) {
            (Some(src), true) => format!("builds from {}", crate::doctor::tilde(src)),
            (Some(src), false) => {
                format!("Node first, then builds from {}", crate::doctor::tilde(src))
            }
            (None, true) => "fetched from this version's release, checked, then built".to_string(),
            (None, false) => {
                "Node first, then fetched from this version's release and built".to_string()
            }
        };
        v.push(
            Row::pick("raycast", "install the extension and its menu bar", w.plan.has(&Action::Raycast))
                .note(how)
                .why("Runs when you apply: npm installs the extension's packages, then `ray develop` builds it and imports it into Raycast. Aleph stops it once Raycast has it; the extension stays installed."),
        );
    } else {
        v.push(Row::info("The extension builds with Node, and neither Node nor Homebrew is here: install Node (nodejs.org), then come back to this step."));
    }
    v.push(Row::head("herdr"));
    if f.plugin {
        v.push(Row::check(
            St::Ok,
            "plugin",
            "companion",
            "linked · prefix+a / i / y".into(),
        ));
    } else if f.herdr || w.plan.has(&Action::HerdrLocal) {
        // herdr is here, or queued to be (it installs first)
        v.push(
            Row::pick("plugin", "companion plugin + keys", w.plan.has(&Action::HerdrPlugin))
                .note("prefix+a Aleph · prefix+i Inbox · prefix+y approve")
                .why("A small plugin for this computer's herdr (it only runs Aleph) and three keys in ~/.config/herdr/config.toml, between Aleph's markers (a backup is kept): prefix+a opens the dashboard in a popup, prefix+i the Inbox, prefix+y approves the focused agent with its own yes. herdr runs local keys only while a local session is focused."),
        );
    } else {
        v.push(Row::info("herdr isn't installed here (step 1)."));
    }
    v.push(Row::head("Moshi"));
    v.push(Row::info(match &w.nt.moshi {
        Some(Some(s)) if s.paired => "moshi-hook is paired — step 6 sets what reaches the phone.".to_string(),
        Some(Some(_)) => "moshi-hook is here, not paired: moshi-hook pair --token <from the app>, then step 6.".to_string(),
        _ => "Moshi (getmoshi.app) puts the fleet on your phone: cards, approvals, the dashboard one tap away. Its host tool is moshi-hook.".to_string(),
    }));
    if matches!(w.nt.moshi, Some(None) | None) {
        v.push(
            Row::pick("moshi-hook", "install moshi-hook", w.plan.has(&Action::moshi_hook()))
                .note("Moshi's installer (getmoshi.app/install.sh), when you apply")
                .why("moshi-hook is Moshi's host tool: it sends Aleph's cards to the Moshi app on your phone and brings approvals back. Pair it afterwards with moshi-hook pair --token <from the app>."),
        );
    }
    v.push(Row::head("Notifications and logos"));
    if f.notifier {
        v.push(Row::check(
            St::Ok,
            "tn",
            "terminal-notifier",
            "installed".into(),
        ));
    } else if cfg!(target_os = "macos") {
        let q = w.plan.has(&Action::notifier());
        v.push(
            Row::check(
                if q { St::Todo } else { St::Info },
                "tn",
                "terminal-notifier",
                if q {
                    "queued: installs when you apply".into()
                } else {
                    "not installed (desktop notifications fall back to osascript)".into()
                },
            )
            .fix(match (q, f.brew) {
                (true, _) => "unqueue it",
                (false, true) => "queue it",
                (false, false) => "copy the command",
            })
                .why("terminal-notifier shows Aleph's own icon and a click opens the session; without it notifications come from osascript."),
        );
    }
    match f.nerd {
        Some(true) => v.push(Row::check(St::Ok, "font", "Nerd Font", "logos as glyphs".into())),
        _ => v.push({
            let q = w.plan.has(&Action::nerd_font()) || w.plan.has(&Action::FontDownload);
            Row::check(
                if q { St::Todo } else { St::Info },
                "font",
                "Nerd Font",
                if q {
                    "queued: JetBrains Mono Nerd Font installs when you apply".into()
                } else {
                    "none in use — emoji stand in for logos".into()
                },
            )
            .fix(match (q, super::preflight::terminal_elsewhere()) {
                (true, _) => "unqueue it",
                (false, false) => "queue one",
                (false, true) => "copy the command (for the computer your terminal runs on)",
            })
                .why("A Nerd Font has the logos of systems and GPUs as glyphs. After installing, pick it as your terminal's font.")
        }),
    }
    v
}

pub(crate) fn act(w: &mut Wiz, ev: Ev) -> Option<Hand> {
    let f = w.ig.facts.clone()?;
    let toggle = |w: &mut Wiz, a: Action| {
        let on = !w.plan.has(&a);
        w.plan.want(Step::Integrations, a, on);
    };
    match ev {
        Ev::Pick(id) => match id.as_str() {
            "comp" => toggle(w, Action::Completions),
            "wrap" => toggle(w, Action::Wrapper),
            "raycast" => {
                toggle(w, Action::Raycast);
                // it builds with npm: Node goes on first, with what you already use
                let on = w.plan.has(&Action::Raycast);
                let needs_node = w.ig.facts.as_ref().is_some_and(|f| !f.npm && f.brew);
                if needs_node {
                    w.plan.want(Step::Integrations, Action::tool("node"), on);
                }
            }
            "moshi-hook" => toggle(w, Action::moshi_hook()),
            "plugin" => toggle(w, Action::HerdrPlugin),
            _ => {}
        },
        Ev::Fix(id) if id == "tn" || id == "font" => {
            let a = if id == "tn" {
                Action::notifier()
            } else if f.brew {
                Action::nerd_font()
            } else {
                Action::FontDownload
            };
            let elsewhere = id == "font" && super::preflight::terminal_elsewhere();
            if (!f.brew && id == "tn") || elsewhere {
                let cmd = super::review::runs_line(&a);
                let ok = crate::tui::copy(&cmd);
                w.toast(
                    !ok,
                    format!("{}{cmd}", if ok { "copied: " } else { "run: " }),
                );
                return None;
            }
            // queued: it installs when you apply
            let on = !w.plan.has(&a);
            w.toast(
                false,
                if on {
                    format!("queued: {} — it runs when you apply", a.label())
                } else {
                    format!("out of the queue: {}", a.label())
                },
            );
            w.plan.want(Step::Integrations, a, on);
        }
        Ev::After(..) => retry(w),
        _ => {}
    }
    None
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_extension_comes_from_its_release_checked() {
        let t = std::env::temp_dir().join(format!("aleph-ray-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let src = t.join("pack/raycast-extension");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("package.json"), "{}").unwrap();
        let rel = t.join("release");
        std::fs::create_dir_all(&rel).unwrap();
        let file = rel.join(RAYCAST_FILE);
        let o = std::process::Command::new("tar")
            .args(["-czf", &file.to_string_lossy(), "-C"])
            .arg(t.join("pack"))
            .arg("raycast-extension")
            .output()
            .unwrap();
        assert!(o.status.success());
        let o = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "cd '{}' && (shasum -a 256 {RAYCAST_FILE} || sha256sum {RAYCAST_FILE}) > SHA256SUMS",
                rel.display()
            ))
            .output()
            .unwrap();
        assert!(o.status.success());
        let base = format!("file://{}", rel.display());
        std::env::set_var("ALEPH_RELEASE_BASE", &base);
        let to = t.join("v1");
        let cfg = config::Config::in_memory(&[]);
        fetch_raycast(&cfg, &to).unwrap();
        assert!(to.join("package.json").is_file());
        // a file that doesn't match its checksum never unpacks
        std::fs::write(
            rel.join("SHA256SUMS"),
            format!("{}  {RAYCAST_FILE}\n", "0".repeat(64)),
        )
        .unwrap();
        let to2 = t.join("v2");
        let e = fetch_raycast(&cfg, &to2).unwrap_err();
        assert!(e.contains("checksum mismatch"), "{e}");
        assert!(!to2.exists());
        std::env::remove_var("ALEPH_RELEASE_BASE");
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn the_menu_bar_link_names_the_real_command() {
        let pkg: serde_json::Value = serde_json::from_str(include_str!(
            "../../../extras/raycast-extension/package.json"
        ))
        .unwrap();
        let (author, name) = (
            pkg["author"].as_str().unwrap(),
            pkg["name"].as_str().unwrap(),
        );
        let link = menu_bar_link();
        assert!(
            link.starts_with(&format!("raycast://extensions/{author}/{name}/menu-bar")),
            "{link}"
        );
        let cmd = pkg["commands"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "menu-bar")
            .expect("the command");
        assert_eq!(cmd["mode"], "menu-bar");
    }

    use super::*;

    #[test]
    fn the_wrapper_joins_alephs_block_once() {
        let rc = "export A=1\n# >>> aleph >>>\nal() { \"/b/aleph\" \"$@\"; }\n# <<< aleph <<<\nalias x=y\n";
        let out = with_wrapper(rc, "/b/aleph").unwrap();
        assert!(out.starts_with("export A=1\n# >>> aleph >>>\n"), "{out}");
        assert!(
            out.contains("claude() { ALEPH_WRAP=1 \"/b/aleph\" \"$@\"; }"),
            "{out}"
        );
        assert!(out.ends_with("alias x=y\n"), "{out}");
        assert_eq!(out.matches("al() {").count(), 1, "{out}");
        assert!(with_wrapper(&out, "/b/aleph").is_none());
        let fresh = with_wrapper("", "/b/aleph").unwrap();
        assert!(
            fresh.contains("claude() {") && fresh.contains("al() {"),
            "{fresh}"
        );
    }

    #[test]
    fn the_keys_block_is_replaced_not_repeated() {
        let b = keys_block("/b/aleph");
        assert!(
            b.contains("key = \"prefix+a\"") && b.contains("aleph.approve"),
            "{b}"
        );
        let once = set_keys_block("[ui]\naccent = \"#fff\"\n", &b);
        assert!(
            once.starts_with("[ui]\naccent = \"#fff\"\n\n# >>> aleph keys >>>"),
            "{once}"
        );
        let twice = set_keys_block(&once, &b);
        assert_eq!(once, twice);
        // what it writes is TOML herdr can read
        assert!(once.parse::<toml_edit::DocumentMut>().is_ok(), "{once}");
        assert!(plugin_manifest("/b/aleph")
            .parse::<toml_edit::DocumentMut>()
            .is_ok());
    }
}
