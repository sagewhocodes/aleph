//! `aleph.toml`: where every flat setting key lives in the TOML file, and the
//! conversions between the two.
//!
//! The rest of Aleph keeps asking for flat keys (`ui_scale`,
//! `host.b31.color`), so nothing outside `config.rs` changes; this file maps
//! them onto readable sections:
//!
//! ```toml
//! [fleet]
//! default = "b31"
//! machines = ["b31", "dex42"]
//!
//! [hosts.b31]
//! color = "blue"
//!
//! [ui]
//! scale = "comfortable"
//! ```
//!
//! A few old root keys would collide with tables (`hosts` vs `[hosts.X]`,
//! `notify` vs `[notify]`, `moshi` vs `[moshi]`), so they get their own names.
//! Keys nobody knows go under `[legacy]` rather than being dropped.

use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

/// Fixed keys: (flat, TOML path). Families (`host.*`, `preset.*`, …) are below.
pub const PATHS: &[(&str, &str)] = &[
    // the fleet
    ("ssh_host", "fleet.default"),
    ("machine", "fleet.herdr_machine"),
    ("hosts", "fleet.machines"),
    ("local", "fleet.local"),
    ("refresh", "fleet.refresh"),
    ("listen", "fleet.listen"),
    ("agents", "fleet.show_agents"),
    // ssh
    ("ssh_reuse", "ssh.reuse"),
    ("ssh_key", "ssh.key"),
    ("herdr_via", "ssh.herdr_via"),
    ("net.probe", "network.probe"),
    ("attach_via", "network.attach_via"),
    ("herdr_view", "ui.herdr_view"),
    ("herdr_tokens", "ui.herdr_tokens"),
    ("fleet.group", "fleet.group"),
    ("inbox.layout", "inbox.layout"),
    ("inbox.group", "inbox.group"),
    ("inbox.hide_ended", "inbox.hide_ended"),
    ("inbox.lifecycle", "inbox.lifecycle"),
    ("inbox.auto_archive", "inbox.auto_archive"),
    ("inbox.needs_ttl", "inbox.needs_ttl"),
    ("inbox.down_grace", "inbox.down_grace"),
    ("inbox.machine_back", "inbox.machine_back"),
    ("inbox.finished", "inbox.finished"),
    // stats and services
    ("stats", "stats.mode"),
    ("stats_interval", "stats.interval"),
    ("services", "services.watch"),
    ("services_interval", "services.interval"),
    // analytics (aleph usage)
    ("analytics.enabled", "analytics.enabled"),
    ("analytics.include", "analytics.include"),
    ("analytics.exclude", "analytics.exclude"),
    ("analytics.backfill", "analytics.backfill"),
    ("analytics.keep", "analytics.keep"),
    ("analytics.interval", "analytics.interval"),
    ("analytics.max_seconds", "analytics.max_seconds"),
    ("analytics.max_mb", "analytics.max_mb"),
    ("analytics.cost", "analytics.cost"),
    ("analytics.account_label", "analytics.account_label"),
    ("usage.layout", "analytics.usage_layout"),
    ("timeline.keep", "timeline.keep"),
    ("timeline.fold", "timeline.fold"),
    ("timeline.words", "timeline.words"),
    ("analytics.claude_roots", "analytics.claude_roots"),
    ("analytics.codex_homes", "analytics.codex_homes"),
    // the session journal (aleph journal)
    ("journal.include", "journal.include"),
    ("journal.dir", "journal.dir"),
    ("tour", "ui.tour"),
    // the look
    ("theme", "ui.theme"),
    ("accent", "ui.accent"),
    ("mouse", "ui.mouse"),
    ("ui.keyboard", "ui.keyboard"),
    ("ui_scale", "ui.scale"),
    ("icons", "ui.icons"),
    ("ui.logos", "ui.logos"),
    ("ui.logo_size", "ui.logo_size"),
    ("ui.spinner", "ui.spinner"),
    ("ui.ground", "ui.ground"),
    ("ui.panels", "ui.panels"),
    ("ui.ambience", "ui.ambience"),
    ("ui.splash", "ui.splash"),
    ("ui.splash_end", "ui.splash_end"),
    ("ui.splash_logo", "ui.splash_logo"),
    ("ui.splash_hold", "ui.splash_hold"),
    ("ui.splash_fx", "ui.splash_fx"),
    ("ui.splash_ms", "ui.splash_ms"),
    ("ui.note_for", "ui.note_for"),
    ("ui.alert_for", "ui.alert_for"),
    ("ui.whats_new", "ui.whats_new"),
    ("editor", "integrations.editor"),
    ("ui.motion", "ui.motion"),
    ("ui.row_bars", "ui.row_bars"),
    ("ui.timeline_hours", "ui.timeline_hours"),
    ("fleet.timeline", "fleet.timeline"),
    ("fleet.board_order", "fleet.board_order"),
    // sessions
    ("naming", "sessions.naming"),
    ("tab_prefix", "sessions.tab_prefix"),
    ("agent", "sessions.agent"),
    ("return_to_dashboard", "sessions.return_to_dashboard"),
    ("enter_opens", "sessions.enter_opens"),
    ("focus.keys", "sessions.focus_keys"),
    ("focus.exit", "sessions.focus_exit"),
    ("focus.esc_guard", "sessions.focus_esc_guard"),
    ("focus.copy_on_select", "sessions.focus_copy_on_select"),
    ("approvals.layout", "automation.approvals_layout"),
    ("approvals.group", "automation.approvals_group"),
    ("close.grace", "sessions.close_grace"),
    ("close.exited", "sessions.close_exited"),
    ("terminal", "sessions.terminal"),
    ("wraps", "wrapper.wraps"),
    ("agent_wrapper", "agents.wrapper"),
    ("agents.node", "agents.node"),
    ("agents.install_timeout", "agents.install_timeout"),
    ("agents.install_quiet", "agents.install_quiet"),
    ("daemon", "daemon.at"),
    // the folder browser
    ("show_hidden", "browser.show_hidden"),
    ("show_ignored", "browser.show_ignored"),
    ("ignore", "browser.ignore"),
    ("browse_ttl", "browser.ttl"),
    ("deep_search", "browser.deep_search"),
    // the daemon, notifications, Moshi
    ("watch_interval", "daemon.interval"),
    ("hub.machine", "hub.machine"),
    ("api.http", "api.http"),
    ("api.listen", "api.listen"),
    ("api.listen_any", "api.listen_any"),
    ("api.url", "api.url"),
    ("api.hosts", "api.hosts"),
    ("api.trust_tailscale", "api.trust_tailscale"),
    ("api.push_contact", "api.push_contact"),
    ("log.level", "log.level"),
    ("notify", "notify.desktop"),
    ("notify_done", "notify.desktop_done"),
    ("notify.ntfy", "notify.ntfy"),
    ("notify.slack", "notify.slack"),
    ("notify.webhook", "notify.webhook"),
    ("notify.remote_on", "notify.remote_on"),
    ("notify.done_min", "notify.done_min"),
    ("notify.subagents", "notify.subagents"),
    ("notify.question_after", "notify.question_after"),
    ("notify.dnd", "notify.dnd"),
    ("notify.source", "notify.source"),
    ("notify.desktop.click", "notify.desktop_click"),
    ("notify.quiet", "notify.quiet"),
    ("notify.quiet_pass", "notify.quiet_pass"),
    ("notify.include_text", "notify.include_text"),
    ("notify.escalate_after", "notify.escalate_after"),
    ("notify.herdr", "notify.herdr"),
    ("notify.raycast", "notify.raycast"),
    ("notify.raycast.deeplink", "notify.raycast_deeplink"),
    ("notify.raycast.kinds", "notify.raycast_kinds"),
    ("moshi", "moshi.mode"),
    ("moshi.events", "moshi.events"),
    ("moshi.approvals", "moshi.approvals"),
    ("moshi.choices", "moshi.choices"),
    ("moshi.jobs", "moshi.jobs"),
    ("moshi.session", "moshi.session"),
    ("remote.allow", "remote.allow"),
    ("remote.deny", "remote.deny"),
    // automation
    ("auto_approve", "automation.auto_approve"),
    ("approve_deny", "automation.deny"),
    ("limits.auto_resume", "automation.auto_resume"),
    ("limits.resume_prompt", "automation.resume_prompt"),
    ("auto_heal", "automation.heal"),
    // the tile view (formerly the wall)
    ("wall_fit", "tile.fit"),
    ("wall_layout", "tile.layout"),
    ("wall_max", "tile.max"),
    ("mux_prefix", "tile.prefix"),
    ("mux_keys", "tile.keys"),
    ("mux_leave", "tile.leave"),
    // hooks, updates, integrations
    ("hook.pre_new", "hooks.pre_new"),
    ("hook.post_new", "hooks.post_new"),
    ("update_repo", "updates.repo"),
    ("update_pubkey", "updates.pubkey"),
    ("raycast_dirs", "integrations.raycast_dirs"),
];

/// Families: (flat prefix, TOML table path). `host.b31.color` → `hosts.b31.color`.
const FAMILIES: &[(&str, &str)] = &[
    ("host.", "hosts"),
    ("preset.", "presets"),
    ("key.", "keys"),
    ("action.", "actions"),
    ("approve.", "automation.rules"),
    // after a usage limit, what each agent is told: resume.<agent> → [automation.resume]
    ("resume.", "automation.resume"),
    ("service.", "services"),
    ("analytics.prices.", "analytics.prices"),
    ("analytics.plans.", "analytics.plans"),
    // an account's nickname: analytics.accounts.<id> → [analytics.accounts]
    ("analytics.accounts.", "analytics.accounts"),
    // a project rule: analytics.projects.<name> → [analytics.projects]
    ("analytics.projects.", "analytics.projects"),
    ("watch.", "watch"),
    // each view's panels: layout.fleet, … → [layout]
    ("layout.", "layout"),
    // saved workspaces: workspace.<name>.<field> → [workspaces.<name>]
    ("workspace.", "workspaces"),
    // your own themes: theme.<name>.<colour> → [themes.<name>]
    ("theme.", "themes"),
    // the prompt library: prompt.<name> → [prompts]
    ("prompt.", "prompts"),
    // hand-offs: handoff.summary, handoff.fallback, handoff.to → [handoff]
    ("handoff.", "handoff"),
    // races: race.agents → [race]
    ("race.", "race"),
    // OpenRouter: openrouter.key (secret:openrouter), openrouter.agents → [openrouter]
    ("openrouter.", "openrouter"),
    // the HTTP API: api.http, api.listen, api.hosts, … → [api]
    ("api.", "api"),
    // a showcase build's own settings → [showcase] (its sounds: [showcase.sound],
    // first so they're found before the table they're in)
    #[cfg(feature = "showcase")]
    ("showcase.sound.", "showcase.sound"),
    #[cfg(feature = "showcase")]
    ("showcase.", "showcase"),
];

/// Section order (and a one-line comment) for files we write from scratch.
pub const SECTIONS: &[(&str, &str)] = &[
    ("fleet", "machines in the dashboard"),
    (
        "hosts",
        "one table per machine: colour, start folder, default agent",
    ),
    ("ui", "the look"),
    ("sessions", "new sessions"),
    ("notify", "notifications"),
    ("moshi", "Moshi on your phone (aleph moshi)"),
    ("remote", "what phones, Raycast and the web may act on"),
    (
        "automation",
        "auto-approve rules (never: rm -rf, sudo, git push, …)",
    ),
    ("daemon", "the background service"),
    ("api", "the HTTP API and web console (aleph daemon pair)"),
    ("hub", "the one computer that runs automation"),
    ("log", "aleph logs"),
    ("ssh", "connections"),
    ("network", "the connection indicator"),
    ("stats", "machine stats"),
    ("services", "local LLM servers and gateways"),
    (
        "analytics",
        "tokens, time and cost from the agents' own logs (aleph usage)",
    ),
    (
        "journal",
        "the session journal: what each session did, a file a day",
    ),
    (
        "openrouter",
        "OpenRouter: the key (in the keystore) and the agents that get it",
    ),
    ("browser", "the folder browser"),
    ("tile", "the tile view"),
    ("wrapper", "the opt-in `claude` wrapper"),
    ("hooks", "commands around new sessions"),
    ("presets", "saved starting points"),
    ("keys", "dashboard key overrides"),
    ("actions", "custom keys: label|command"),
    ("updates", "self-update"),
    ("integrations", "Raycast and friends"),
    ("legacy", "keys Aleph doesn't know (kept, never dropped)"),
];

/// The TOML path for a flat key.
pub fn to_path(flat: &str) -> Vec<String> {
    if let Some((_, p)) = PATHS.iter().find(|(f, _)| *f == flat) {
        return p.split('.').map(String::from).collect();
    }
    for (pre, table) in FAMILIES {
        if let Some(rest) = flat.strip_prefix(pre) {
            let mut v: Vec<String> = table.split('.').map(String::from).collect();
            if matches!(*pre, "host." | "preset." | "workspace." | "theme.") {
                // `host.<alias>.<field>`: aliases may hold dots (10.0.0.49,
                // gpu.lab), the field never does — split at the last dot
                match rest.rsplit_once('.') {
                    Some((name, field)) if !name.is_empty() && !field.is_empty() => {
                        v.push(name.to_string());
                        v.push(field.to_string());
                    }
                    _ => break,
                }
            } else {
                v.push(rest.to_string());
            }
            return v;
        }
    }
    vec!["legacy".into(), flat.to_string()]
}

/// The flat key for a TOML path (None = a path we don't map, e.g. a table).
pub fn from_path(path: &[&str]) -> Option<String> {
    let dotted = path.join(".");
    if let Some((f, _)) = PATHS.iter().find(|(_, p)| *p == dotted) {
        return Some(f.to_string());
    }
    if path.first() == Some(&"legacy") && path.len() == 2 {
        return Some(path[1].to_string());
    }
    for (pre, table) in FAMILIES {
        let t: Vec<&str> = table.split('.').collect();
        if path.len() > t.len() && path[..t.len()] == t[..] {
            let rest = &path[t.len()..];
            return match (*pre, rest.len()) {
                ("host." | "preset." | "workspace." | "theme.", 2) => {
                    Some(format!("{pre}{}.{}", rest[0], rest[1]))
                }
                ("host." | "preset." | "workspace." | "theme.", _) => None,
                (_, 1) => {
                    // `services.watch` / `services.interval` are fixed keys,
                    // found above; anything else in [services] is a service id
                    Some(format!("{pre}{}", rest[0]))
                }
                _ => None,
            };
        }
    }
    None
}

/// The typed TOML value for a flat key's string value.
pub fn typed(flat: &str, v: &str) -> Value {
    if matches!(
        flat,
        "hosts" | "remote.allow" | "remote.deny" | "analytics.include" | "analytics.exclude"
    ) {
        let mut a = Array::new();
        for x in v.split(',').map(str::trim).filter(|x| !x.is_empty()) {
            a.push(x);
        }
        return Value::Array(a);
    }
    match crate::config::known(flat).map(|k| k.kind) {
        Some(crate::config::Kind::Bool) => match v.to_lowercase().as_str() {
            "true" | "1" | "on" | "yes" => true.into(),
            "false" | "0" | "off" | "no" => false.into(),
            _ => v.into(),
        },
        Some(crate::config::Kind::Num) => {
            if let Ok(i) = v.parse::<i64>() {
                i.into()
            } else if let Ok(f) = v.parse::<f64>() {
                f.into()
            } else {
                v.into()
            }
        }
        _ => v.into(),
    }
}

/// A TOML value as the flat string the rest of Aleph reads.
pub fn flat_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.value().clone(),
        Value::Boolean(b) => b.value().to_string(),
        Value::Integer(i) => i.value().to_string(),
        Value::Float(f) => {
            let x = *f.value();
            if x.fract() == 0.0 && x.abs() < 1e15 {
                format!("{}", x as i64)
            } else {
                x.to_string()
            }
        }
        Value::Array(a) => a.iter().map(flat_value).collect::<Vec<_>>().join(","),
        Value::Datetime(d) => d.value().to_string(),
        Value::InlineTable(t) => t.to_string(),
    }
}

/// Every setting in a document, as (flat key, value) pairs.
pub fn entries(doc: &DocumentMut) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk(doc.as_table(), &mut Vec::new(), &mut out);
    out
}

fn walk(t: &Table, path: &mut Vec<String>, out: &mut Vec<(String, String)>) {
    for (k, item) in t.iter() {
        path.push(k.to_string());
        match item {
            Item::Value(Value::InlineTable(it)) => {
                for (k2, v2) in it.iter() {
                    path.push(k2.to_string());
                    push(path, v2, out);
                    path.pop();
                }
            }
            Item::Value(v) => push(path, v, out),
            Item::Table(sub) => walk(sub, path, out),
            _ => {}
        }
        path.pop();
    }
}

fn push(path: &[String], v: &Value, out: &mut Vec<(String, String)>) {
    let refs: Vec<&str> = path.iter().map(String::as_str).collect();
    if refs == ["include"] {
        return; // the include list isn't a setting
    }
    if let Some(flat) = from_path(&refs) {
        out.push((flat, flat_value(v)));
    }
}

/// Set a flat key in a document (creating tables in a tidy order).
pub fn set(doc: &mut DocumentMut, flat: &str, v: &str) {
    let path = to_path(flat);
    let (last, tables) = path.split_last().expect("non-empty path");
    let mut t = doc.as_table_mut();
    for (i, name) in tables.iter().enumerate() {
        if !t.contains_key(name) {
            let mut nt = Table::new();
            // [hosts] / [presets] only hold sub-tables: don't print them empty
            nt.set_implicit(i == 0 && tables.len() > 1);
            t.insert(name, Item::Table(nt));
        }
        let entry = t.get_mut(name).expect("just inserted");
        if !entry.is_table() {
            // a root value where a table belongs (an old flat file): make room
            *entry = Item::Table(Table::new());
        }
        t = entry.as_table_mut().expect("a table");
    }
    t.insert(last, value(typed(flat, v)));
}

/// Remove a flat key (and tables it leaves empty).
pub fn unset(doc: &mut DocumentMut, flat: &str) -> bool {
    let path = to_path(flat);
    let refs: Vec<&str> = path.iter().map(String::as_str).collect();
    remove(doc.as_table_mut(), &refs)
}

fn remove(t: &mut Table, path: &[&str]) -> bool {
    match path {
        [] => false,
        [k] => t.remove(k).is_some(),
        [k, rest @ ..] => {
            let Some(sub) = t.get_mut(k).and_then(Item::as_table_mut) else {
                return false;
            };
            let r = remove(sub, rest);
            if sub.is_empty() {
                t.remove(k);
            }
            r
        }
    }
}

/// A fresh, commented `aleph.toml` holding `entries` (the migration from the
/// old key=value file, and `aleph config export`).
pub fn render(entries: &[(String, String)]) -> String {
    let mut doc = DocumentMut::new();
    // sections in a sensible order, each introduced by a comment
    let order = |flat: &str| -> usize {
        let top = to_path(flat).first().cloned().unwrap_or_default();
        SECTIONS
            .iter()
            .position(|(s, _)| *s == top)
            .unwrap_or(SECTIONS.len())
    };
    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort_by_key(|(k, _)| order(k));
    for (k, v) in sorted {
        set(&mut doc, k, v);
    }
    for (name, about) in SECTIONS {
        if let Some(t) = doc.get_mut(name).and_then(Item::as_table_mut) {
            t.decor_mut().set_prefix(format!("\n# {about}\n"));
        }
    }
    let mut s = String::from(
        "# Aleph's settings. Every key is optional; defaults apply.\n\
         # `aleph settings` lists them all, `aleph config check` validates this file,\n\
         # `aleph config schema` gives your editor completion.\n\
         # Shared settings can live elsewhere: include = [\"~/team/aleph.toml\"]\n",
    );
    s.push('\n');
    s.push_str(doc.to_string().trim_start_matches('\n'));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixed_key_round_trips() {
        for (flat, _) in PATHS {
            let p = to_path(flat);
            let refs: Vec<&str> = p.iter().map(String::as_str).collect();
            assert_eq!(from_path(&refs).as_deref(), Some(*flat), "{flat}");
        }
        // every documented setting has a home outside [legacy]
        for k in crate::config::KNOWN {
            assert_ne!(to_path(k.key)[0], "legacy", "{} has no TOML path", k.key);
        }
    }

    #[test]
    fn families_and_dotted_aliases() {
        assert_eq!(
            to_path("host.10.0.0.49.color"),
            vec!["hosts", "10.0.0.49", "color"]
        );
        assert_eq!(
            from_path(&["hosts", "10.0.0.49", "color"]).as_deref(),
            Some("host.10.0.0.49.color")
        );
        assert_eq!(
            to_path("approve.tests"),
            vec!["automation", "rules", "tests"]
        );
        assert_eq!(to_path("service.ollama"), vec!["services", "ollama"]);
        assert_eq!(
            from_path(&["services", "watch"]).as_deref(),
            Some("services")
        );
        assert_eq!(
            from_path(&["services", "ollama"]).as_deref(),
            Some("service.ollama")
        );
        assert_eq!(
            to_path("analytics.prices.gpt-5.1"),
            vec!["analytics", "prices", "gpt-5.1"]
        );
        assert_eq!(
            from_path(&["analytics", "prices", "gpt-5.1"]).as_deref(),
            Some("analytics.prices.gpt-5.1")
        );
        assert_eq!(
            from_path(&["analytics", "keep"]).as_deref(),
            Some("analytics.keep")
        );
        assert_eq!(to_path("weird_old_key"), vec!["legacy", "weird_old_key"]);
        assert_eq!(
            from_path(&["legacy", "weird_old_key"]).as_deref(),
            Some("weird_old_key")
        );
    }

    #[test]
    fn a_real_config_converts_and_reads_back_the_same() {
        // the shape of a live config (values made up)
        let old: Vec<(String, String)> = [
            ("ssh_host", "b31"),
            ("machine", "b31"),
            ("hosts", "b31,dex42"),
            ("host.b31.color", "blue"),
            ("host.dex42.color", "mauve"),
            ("host.local.color", "teal"),
            ("notify.ntfy", "https://ntfy.sh/secret-topic"),
            ("approve.tests", "*pytest*"),
            ("auto_approve", "true"),
            ("moshi", "on"),
            ("moshi.approvals", "true"),
            ("remote.allow", "*:*"),
            ("refresh", "0.1"),
            ("watch_interval", "20"),
            ("service.ollama", "off"),
            ("key.attach", "enter,o"),
            ("action.g", "git|lazygit"),
            ("some_unknown", "x"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let text = render(&old);
        let doc: DocumentMut = text.parse().expect("valid TOML");
        let mut back = entries(&doc);
        let mut want = old.clone();
        back.sort();
        want.sort();
        assert_eq!(back, want, "\n{text}");
        assert!(text.contains("[fleet]\ndefault = \"b31\""), "{text}");
        assert!(text.contains("machines = [\"b31\", \"dex42\"]"), "{text}");
        assert!(text.contains("[hosts.b31]\ncolor = \"blue\""), "{text}");
        assert!(text.contains("auto_approve = true"), "{text}");
        assert!(text.contains("refresh = 0.1"), "{text}");
    }

    #[test]
    fn set_and_unset_keep_the_rest() {
        let mut doc: DocumentMut = "# mine\n[ui]\nscale = \"large\" # keep\n".parse().unwrap();
        set(&mut doc, "ui_scale", "compact");
        set(&mut doc, "host.b31.color", "blue");
        let s = doc.to_string();
        assert!(s.starts_with("# mine\n[ui]\nscale = \"compact\""), "{s}");
        assert!(s.contains("[hosts.b31]"), "{s}");
        assert!(unset(&mut doc, "host.b31.color"));
        assert!(!doc.to_string().contains("hosts"), "{}", doc);
    }
}
