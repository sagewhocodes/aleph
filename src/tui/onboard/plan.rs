//! What setup will change, remembered with the step that chose it: settings
//! (flat keys, written through `Config` so comments survive), presets, and
//! actions (things to run: an install, an integration, a service). Nothing
//! here runs anything; Review & apply does, and `state/setup.json` keeps the
//! plan so a run that stops picks up where it was.

use super::Step;
use crate::presets::Preset;
use serde_json::{json, Value};

/// One setting to change (`val: None` = back to its default).
#[derive(Clone, Debug, PartialEq)]
pub struct Set {
    pub step: Step,
    pub key: String,
    pub val: Option<String>,
}

/// Something to run when the plan is applied.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// `herdr machine add <host>`: saves it in herdr, installing (or
    /// updating) herdr there first when needed — herdr asks once more
    HerdrSave { host: String, why: String },
    /// `[server] headless_cols/rows` in the machine's herdr config
    Headless { host: String, cols: u16, rows: u16 },
    /// `herdr integration install <name>` on the machine
    Integration { host: String, name: String },
    /// Aleph's own agent hooks there (`hooks install`; a backup is kept)
    Hooks { host: String },
    /// install an agent there (`aleph agents install`)
    AgentInstall { host: String, agent: String },
    /// the daemon at login (`aleph watch --install`)
    Service,
    /// tab completion in the shell's rc file
    Completions,
    /// the opt-in `claude` shell function (Aleph, with Claude Code's own
    /// commands passed through)
    Wrapper,
    /// the herdr companion plugin and its prefix keys
    HerdrPlugin,
    /// build and import the Raycast extension
    Raycast,
    /// `aleph moshi install`: the bridge on, approvals, live jobs
    MoshiOn,
    /// an `aleph daemon …` subcommand (deploy, expose, pair)
    Daemon { args: Vec<String> },
    /// herdr on this computer (Homebrew, else herdr's own script)
    HerdrLocal,
    /// a Homebrew install on this computer: what it is, and brew's arguments
    Brew { what: String, args: Vec<String> },
    /// a new ssh key (`sshcfg::new_key_path`), no passphrase: nothing to type
    SshKey,
    /// a Nerd Font without Homebrew: JetBrains Mono from the Nerd Fonts
    /// release, into your own fonts folder
    FontDownload,
    /// a system package with the machine's own manager (apt, dnf, pacman):
    /// sudo asks for your password, in the terminal
    SysPkg { mgr: String, pkg: String, ask: bool },
    /// an installer script (Tailscale's, moshi-hook's): what, and its URL
    Script { what: String, url: String },
    /// a ready-to-run program from its release (cloudflared on Linux: it isn't
    /// in the system's lists) into ~/.local/bin, as `name`
    Download {
        what: String,
        url: String,
        name: String,
    },
    /// after the installs: their caches and leftovers cleared (npm's cache,
    /// node-gyp's headers, uv's unused cache, links a failed install left
    /// pointing nowhere), on each machine that had them
    Tidy { hosts: Vec<String> },
}

impl Action {
    /// Stable identity (two equal plans hold each action once).
    pub fn id(&self) -> String {
        match self {
            Action::HerdrSave { host, .. } => format!("herdr-save:{host}"),
            Action::Headless { host, .. } => format!("headless:{host}"),
            Action::Integration { host, name } => format!("integration:{host}:{name}"),
            Action::Hooks { host } => format!("hooks:{host}"),
            Action::AgentInstall { host, agent } => format!("install:{host}:{agent}"),
            Action::Service => "service".into(),
            Action::Completions => "completions".into(),
            Action::Wrapper => "wrapper".into(),
            Action::HerdrPlugin => "herdr-plugin".into(),
            Action::Raycast => "raycast".into(),
            Action::MoshiOn => "moshi".into(),
            Action::Daemon { args } => format!("daemon:{}", args.join(" ")),
            Action::HerdrLocal => "herdr-local".into(),
            Action::Brew { args, .. } => format!("brew:{}", args.join(" ")),
            Action::SshKey => "ssh-key".into(),
            Action::FontDownload => "font-download".into(),
            Action::SysPkg { pkg, .. } => format!("sys:{pkg}"),
            Action::Script { url, .. } => format!("script:{url}"),
            Action::Download { name, .. } => format!("download:{name}"),
            Action::Tidy { .. } => "tidy".into(),
        }
    }

    /// terminal-notifier: desktop notifications with Aleph's icon.
    pub fn notifier() -> Action {
        Action::Brew {
            what: "terminal-notifier".into(),
            args: vec!["install".into(), "terminal-notifier".into()],
        }
    }

    /// A Nerd Font (the logos as glyphs).
    pub fn nerd_font() -> Action {
        Action::Brew {
            what: "JetBrains Mono Nerd Font".into(),
            args: vec![
                "install".into(),
                "--cask".into(),
                "font-jetbrains-mono-nerd-font".into(),
            ],
        }
    }

    /// The system's package manager on Linux (apt, dnf, pacman, zypper,
    /// apk), when there's one.
    pub fn sys_mgr() -> Option<&'static str> {
        ["apt-get", "dnf", "pacman", "zypper", "apk"]
            .into_iter()
            .find(|m| crate::proc::which(m).is_some())
    }

    /// A tool where this computer gets it: Homebrew, else the system's own
    /// manager (sudo asks), else nothing.
    pub fn tool_here(name: &str) -> Option<Action> {
        if crate::proc::which("brew").is_some() {
            return Some(Action::tool(name));
        }
        if name == "tailscale" {
            return Some(Action::Script {
                what: "Tailscale".into(),
                url: "https://tailscale.com/install.sh".into(),
            });
        }
        // not in Debian's or Ubuntu's own lists: Cloudflare's own build
        if name == "cloudflared" && !cfg!(target_os = "macos") {
            return Some(Action::Download {
                what: "cloudflared".into(),
                url: Action::cloudflared_url(std::env::consts::ARCH)?,
                name: "cloudflared".into(),
            });
        }
        if cfg!(target_os = "macos") {
            return None;
        }
        Action::sys_pkg(name)
    }

    /// A system package (Linux): sudo asks unless it needs no password here.
    /// `prog` is the program wanted; its package's name is the manager's.
    pub fn sys_pkg(prog: &str) -> Option<Action> {
        let mgr = Action::sys_mgr()?;
        Some(Action::SysPkg {
            mgr: mgr.into(),
            pkg: Action::pkg_of(mgr, prog),
            ask: !Action::sudo_free(),
        })
    }

    /// The package a program comes in, for a manager (pure).
    pub fn pkg_of(mgr: &str, prog: &str) -> String {
        match (mgr, prog) {
            ("dnf" | "zypper", "g++") => "gcc-c++".into(),
            ("pacman", "g++") => "gcc".into(),
            ("pacman", "python3") => "python".into(),
            (_, p) => p.to_string(),
        }
    }

    /// uv, for the agents that install with it (its own installer: no sudo,
    /// into ~/.local/bin).
    pub fn uv() -> Action {
        Action::Script {
            what: "uv".into(),
            url: "https://astral.sh/uv/install.sh".into(),
        }
    }

    /// Does sudo work without a password here (OrbStack's machines, a
    /// container, a cached ticket)? Then a system package needs no terminal.
    pub fn sudo_free() -> bool {
        crate::proc::cap("sudo", &["-n", "true"], std::time::Duration::from_secs(5)).ok()
    }

    /// The command a system package runs (sudo asks for the password).
    pub fn sys_line(mgr: &str, pkg: &str) -> String {
        match mgr {
            "apt-get" => format!("sudo apt-get install -y {pkg}"),
            "dnf" => format!("sudo dnf install -y {pkg}"),
            "pacman" => format!("sudo pacman -S --noconfirm {pkg}"),
            "zypper" => format!("sudo zypper install -y {pkg}"),
            "apk" => format!("sudo apk add {pkg}"),
            m => format!("sudo {m} install {pkg}"),
        }
    }

    /// The same with no password asked (`sudo -n`), for no terminal.
    pub fn sys_line_quiet(mgr: &str, pkg: &str) -> String {
        Action::sys_line(mgr, pkg).replacen(
            "sudo ",
            "sudo -n env DEBIAN_FRONTEND=noninteractive ",
            1,
        )
    }

    /// Cloudflare's Linux build of cloudflared for a processor (Rust's
    /// `ARCH` names), from its latest release (pure).
    pub fn cloudflared_url(arch: &str) -> Option<String> {
        let a = match arch {
            "x86_64" => "amd64",
            "aarch64" => "arm64",
            "arm" => "arm",
            "x86" => "386",
            _ => return None,
        };
        Some(format!(
            "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-{a}"
        ))
    }

    /// moshi-hook, Moshi's host tool (its own installer).
    pub fn moshi_hook() -> Action {
        Action::Script {
            what: "moshi-hook".into(),
            url: "https://getmoshi.app/install.sh".into(),
        }
    }

    /// A tool from Homebrew (`tailscale` is a cask).
    pub fn tool(name: &str) -> Action {
        let pkg = match name {
            "tailscale" => vec!["--cask", "tailscale"],
            n => vec![n],
        };
        Action::Brew {
            what: name.into(),
            args: std::iter::once("install")
                .chain(pkg)
                .map(String::from)
                .collect(),
        }
    }

    /// What it does, in a few words.
    pub fn label(&self) -> String {
        match self {
            Action::HerdrSave { host, why } if why.is_empty() => {
                format!("save {host} in herdr")
            }
            Action::HerdrSave { host, why } => format!("save {host} in herdr ({why})"),
            Action::Headless { host, cols, rows } => {
                format!("herdr on {host}: new panes {cols}×{rows} while nobody's attached")
            }
            Action::Integration { host, name } => {
                format!("herdr's {name} integration on {host}")
            }
            Action::Hooks { host } => {
                format!("Aleph's agent hooks on {host} (diff shown, backup kept)")
            }
            Action::AgentInstall { host, agent } => {
                format!("install {} on {host}", crate::agents::label(agent))
            }
            Action::Service => "run the daemon at login".into(),
            Action::Completions => "tab completion in your shell".into(),
            Action::Wrapper => {
                "the opt-in `claude` function (Claude Code's own commands pass through)".into()
            }
            Action::HerdrPlugin => "herdr companion plugin + prefix a / i / y keys".into(),
            Action::Raycast => "install the Raycast extension and its menu bar".into(),
            Action::MoshiOn => "Aleph → Moshi: cards, phone approvals, live jobs".into(),
            Action::Daemon { args } => format!("aleph daemon {}", args.join(" ")),
            Action::HerdrLocal => "install herdr on this computer".into(),
            Action::Brew { what, .. } => format!("install {what}"),
            Action::SshKey => "make an ssh key".into(),
            Action::FontDownload => "install JetBrains Mono Nerd Font".into(),
            Action::SysPkg { pkg, .. } => format!("install {pkg}"),
            Action::Script { what, .. } => format!("install {what}"),
            Action::Download { what, .. } => format!("install {what}"),
            Action::Tidy { .. } => "tidy up after the installs".into(),
        }
    }

    /// The other machine it changes (None: this computer).
    pub fn host(&self) -> Option<&str> {
        match self {
            Action::HerdrSave { host, .. }
            | Action::Headless { host, .. }
            | Action::Integration { host, .. }
            | Action::Hooks { host }
            | Action::AgentInstall { host, .. }
                if !crate::sshcfg::is_local(host) =>
            {
                Some(host)
            }
            Action::Daemon { args } if args.first().is_some_and(|a| a == "deploy") => {
                args.get(1).map(String::as_str)
            }
            _ => None,
        }
    }

    /// Does it need the terminal (its own prompts, a password, live output)?
    /// Saving a machine in herdr does only when herdr has to be installed or
    /// updated there (herdr asks before it downloads).
    pub fn tty(&self) -> bool {
        match self {
            Action::HerdrSave { why, .. } => !why.is_empty(),
            // sudo's password (apt and the like; Tailscale's script uses sudo)
            Action::SysPkg { ask, .. } => *ask,
            Action::Script { url, .. } => url.contains("tailscale"),
            Action::Daemon { .. } => true,
            _ => false,
        }
    }

    /// Does it need the internet (a download)?
    pub fn online(&self) -> bool {
        matches!(
            self,
            Action::HerdrSave { .. }
                | Action::AgentInstall { .. }
                | Action::Raycast
                | Action::HerdrLocal
                | Action::Brew { .. }
                | Action::FontDownload
                | Action::SysPkg { .. }
                | Action::Script { .. }
                | Action::Download { .. }
        )
    }

    fn to_json(&self) -> Value {
        match self {
            Action::HerdrSave { host, why } => {
                json!({"do": "herdr-save", "host": host, "why": why})
            }
            Action::Headless { host, cols, rows } => {
                json!({"do": "headless", "host": host, "cols": cols, "rows": rows})
            }
            Action::Integration { host, name } => {
                json!({"do": "integration", "host": host, "name": name})
            }
            Action::Hooks { host } => json!({"do": "hooks", "host": host}),
            Action::AgentInstall { host, agent } => {
                json!({"do": "install", "host": host, "agent": agent})
            }
            Action::Service => json!({"do": "service"}),
            Action::Completions => json!({"do": "completions"}),
            Action::Wrapper => json!({"do": "wrapper"}),
            Action::HerdrPlugin => json!({"do": "herdr-plugin"}),
            Action::Raycast => json!({"do": "raycast"}),
            Action::MoshiOn => json!({"do": "moshi"}),
            Action::Daemon { args } => json!({"do": "daemon", "args": args}),
            Action::HerdrLocal => json!({"do": "herdr-local"}),
            Action::Brew { what, args } => json!({"do": "brew", "what": what, "args": args}),
            Action::SshKey => json!({"do": "ssh-key"}),
            Action::FontDownload => json!({"do": "font-download"}),
            Action::SysPkg { mgr, pkg, ask } => {
                json!({"do": "sys", "mgr": mgr, "pkg": pkg, "ask": ask})
            }
            Action::Script { what, url } => json!({"do": "script", "what": what, "url": url}),
            Action::Download { what, url, name } => {
                json!({"do": "download", "what": what, "url": url, "name": name})
            }
            Action::Tidy { hosts } => json!({"do": "tidy", "hosts": hosts}),
        }
    }

    fn from_json(v: &Value) -> Option<Action> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(String::from);
        let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as u16);
        Some(match v.get("do")?.as_str()? {
            "herdr-save" => Action::HerdrSave {
                host: s("host")?,
                why: s("why").unwrap_or_default(),
            },
            "headless" => Action::Headless {
                host: s("host")?,
                cols: n("cols")?,
                rows: n("rows")?,
            },
            "integration" => Action::Integration {
                host: s("host")?,
                name: s("name")?,
            },
            "hooks" => Action::Hooks { host: s("host")? },
            "install" => Action::AgentInstall {
                host: s("host")?,
                agent: s("agent")?,
            },
            "service" => Action::Service,
            "completions" => Action::Completions,
            "wrapper" => Action::Wrapper,
            "herdr-plugin" => Action::HerdrPlugin,
            "raycast" => Action::Raycast,
            "moshi" => Action::MoshiOn,
            "daemon" => Action::Daemon {
                args: v
                    .get("args")?
                    .as_array()?
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            },
            "herdr-local" => Action::HerdrLocal,
            "brew" => Action::Brew {
                what: s("what")?,
                args: v
                    .get("args")?
                    .as_array()?
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            },
            "ssh-key" => Action::SshKey,
            "font-download" => Action::FontDownload,
            "sys" => Action::SysPkg {
                mgr: s("mgr")?,
                pkg: s("pkg")?,
                ask: v.get("ask").and_then(Value::as_bool).unwrap_or(true),
            },
            "script" => Action::Script {
                what: s("what")?,
                url: s("url")?,
            },
            "download" => Action::Download {
                what: s("what")?,
                url: s("url")?,
                name: s("name")?,
            },
            "tidy" => Action::Tidy {
                hosts: v
                    .get("hosts")?
                    .as_array()?
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect(),
            },
            _ => return None,
        })
    }
}

/// Everything setup will change.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub sets: Vec<Set>,
    pub presets: Vec<(Step, Preset)>,
    pub actions: Vec<(Step, Action)>,
}

impl Plan {
    /// The pending value of `key`: Some(Some(v)) set, Some(None) back to the
    /// default, None untouched.
    pub fn pending(&self, key: &str) -> Option<Option<&str>> {
        self.sets
            .iter()
            .rev()
            .find(|s| s.key == key)
            .map(|s| s.val.as_deref())
    }

    /// Remember a change (replacing an earlier one for the same key).
    pub fn put(&mut self, step: Step, key: &str, val: Option<String>) {
        self.sets.retain(|s| s.key != key);
        self.sets.push(Set {
            step,
            key: key.to_string(),
            val,
        });
    }

    /// Forget the change to `key` (it's what the settings say already).
    pub fn forget(&mut self, key: &str) {
        self.sets.retain(|s| s.key != key);
    }

    pub fn has(&self, a: &Action) -> bool {
        let id = a.id();
        self.actions.iter().any(|(_, x)| x.id() == id)
    }

    /// Add `a` (once), or with `on = false` take it out.
    pub fn want(&mut self, step: Step, a: Action, on: bool) {
        let id = a.id();
        self.actions.retain(|(_, x)| x.id() != id);
        if on {
            self.actions.push((step, a));
        }
    }

    pub fn put_preset(&mut self, step: Step, p: Preset) {
        self.presets.retain(|(_, x)| x.name != p.name);
        self.presets.push((step, p));
    }

    /// Drop everything a step chose (it was skipped).
    pub fn drop_step(&mut self, step: Step) -> usize {
        let n = self.of(step);
        self.sets.retain(|s| s.step != step);
        self.presets.retain(|(s, _)| *s != step);
        self.actions.retain(|(s, _)| *s != step);
        n
    }

    /// How many changes a step made.
    pub fn of(&self, step: Step) -> usize {
        self.sets.iter().filter(|s| s.step == step).count()
            + self.presets.iter().filter(|(s, _)| *s == step).count()
            + self.actions.iter().filter(|(s, _)| *s == step).count()
    }

    pub fn len(&self) -> usize {
        self.sets.len() + self.presets.len() + self.actions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn to_json(&self) -> Value {
        json!({
            "sets": self.sets.iter().map(|s| json!({"step": s.step.id(), "key": s.key, "value": s.val})).collect::<Vec<_>>(),
            "presets": self.presets.iter().map(|(st, p)| json!({"step": st.id(), "toml": crate::presets::to_toml(p)})).collect::<Vec<_>>(),
            "actions": self.actions.iter().map(|(st, a)| {
                let mut v = a.to_json();
                v["step"] = json!(st.id());
                v
            }).collect::<Vec<_>>(),
        })
    }

    pub fn from_json(v: &Value) -> Plan {
        let step = |x: &Value| {
            x.get("step")
                .and_then(Value::as_str)
                .and_then(Step::parse)
                .unwrap_or(Step::Review)
        };
        let list = |k: &str| {
            v.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let mut p = Plan::default();
        for x in list("sets") {
            if let Some(k) = x.get("key").and_then(Value::as_str) {
                p.put(
                    step(&x),
                    k,
                    x.get("value").and_then(Value::as_str).map(String::from),
                );
            }
        }
        for x in list("presets") {
            let Some(t) = x.get("toml").and_then(Value::as_str) else {
                continue;
            };
            if let Ok(doc) = t.parse::<toml_edit::DocumentMut>() {
                for pr in crate::presets::from_doc(&doc, "setup").0 {
                    p.put_preset(step(&x), pr);
                }
            }
        }
        for x in list("actions") {
            if let Some(a) = Action::from_json(&x) {
                p.want(step(&x), a, true);
            }
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_keeps_one_value_per_key_and_round_trips() {
        let mut p = Plan::default();
        p.put(Step::Look, "theme", Some("dark".into()));
        p.put(Step::Look, "theme", Some("light".into()));
        p.put(Step::Notify, "notify.quiet", None);
        p.want(
            Step::Agents,
            Action::Integration {
                host: "gpu2".into(),
                name: "codex".into(),
            },
            true,
        );
        p.want(Step::Integrations, Action::Completions, true);
        p.want(Step::Integrations, Action::Completions, true);
        assert_eq!(p.pending("theme"), Some(Some("light")));
        assert_eq!(p.pending("notify.quiet"), Some(None));
        assert_eq!(p.pending("mouse"), None);
        assert_eq!(p.len(), 4);
        let back = Plan::from_json(&p.to_json());
        assert_eq!(back.sets, p.sets);
        assert_eq!(back.actions, p.actions);
        assert_eq!(p.clone().drop_step(Step::Look), 1);
        let mut q = p.clone();
        q.want(Step::Integrations, Action::Completions, false);
        assert_eq!(q.of(Step::Integrations), 0);
    }

    #[test]
    fn actions_say_where_they_run() {
        let a = Action::Integration {
            host: "gpu2".into(),
            name: "codex".into(),
        };
        assert_eq!(a.host(), Some("gpu2"));
        assert!(!a.tty());
        let l = Action::Integration {
            host: "local".into(),
            name: "codex".into(),
        };
        assert_eq!(l.host(), None);
        // a save alone runs on the install screen; one that installs herdr
        // there asks in the terminal
        assert!(!Action::HerdrSave {
            host: "b31".into(),
            why: String::new()
        }
        .tty());
        assert!(Action::HerdrSave {
            host: "b31".into(),
            why: "installs herdr 0.9.5".into()
        }
        .tty());
        assert_eq!(Action::Service.host(), None);
        let d = Action::Daemon {
            args: vec!["deploy".into(), "vps".into()],
        };
        assert_eq!(d.host(), Some("vps"));
        assert_eq!(Action::from_json(&d.to_json()), Some(d));
        // installs queued from preflight and the integrations step
        for a in [
            Action::HerdrLocal,
            Action::nerd_font(),
            Action::notifier(),
            Action::SshKey,
        ] {
            assert_eq!(Action::from_json(&a.to_json()), Some(a.clone()));
            assert_eq!(a.host(), None);
        }
        assert!(!Action::SshKey.tty() && !Action::HerdrLocal.tty() && !Action::notifier().tty());
        let apt = Action::SysPkg {
            mgr: "apt-get".into(),
            pkg: "git".into(),
            ask: true,
        };
        let free = Action::SysPkg {
            mgr: "apt-get".into(),
            pkg: "git".into(),
            ask: false,
        };
        assert!(!free.tty());
        assert_eq!(
            Action::sys_line_quiet("apt-get", "git"),
            "sudo -n env DEBIAN_FRONTEND=noninteractive apt-get install -y git"
        );
        assert!(apt.tty() && apt.online());
        assert_eq!(
            Action::sys_line("apt-get", "git"),
            "sudo apt-get install -y git"
        );
        assert_eq!(Action::from_json(&apt.to_json()), Some(apt));
        assert!(!Action::moshi_hook().tty());
        assert_eq!(Action::pkg_of("dnf", "g++"), "gcc-c++");
        assert_eq!(Action::pkg_of("apt-get", "g++"), "g++");
        assert_eq!(Action::pkg_of("pacman", "python3"), "python");
        assert_eq!(Action::pkg_of("apt-get", "jq"), "jq");
        // cloudflared on Linux: Cloudflare's own build, no sudo
        assert_eq!(
            Action::cloudflared_url("aarch64").as_deref(),
            Some("https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-arm64")
        );
        assert!(Action::cloudflared_url("x86_64")
            .unwrap()
            .ends_with("-amd64"));
        let dl = Action::Download {
            what: "cloudflared".into(),
            url: Action::cloudflared_url("x86_64").unwrap(),
            name: "cloudflared".into(),
        };
        assert!(!dl.tty() && dl.online());
        assert_eq!(Action::from_json(&dl.to_json()), Some(dl));
        assert_eq!(
            Action::from_json(&Action::FontDownload.to_json()),
            Some(Action::FontDownload)
        );
        assert!(Action::HerdrLocal.online() && Action::nerd_font().online());
        assert_eq!(
            Action::tool("tailscale").id(),
            "brew:install --cask tailscale"
        );
        assert_eq!(Action::notifier().label(), "install terminal-notifier");
        // agents install with their output on the install screen, not in the terminal
        assert!(!Action::AgentInstall {
            host: "b31".into(),
            agent: "claude".into()
        }
        .tty());
    }
}
