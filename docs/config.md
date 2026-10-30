# Aleph settings

Generated from `config::KNOWN` (`ALEPH_WRITE_DOCS=1 cargo test config_docs_are_current`).
Set one with `aleph settings set <key> <value>`; they live in `~/.config/aleph/aleph.toml`
(the TOML column says where). `aleph settings` lists them with their current values, and the
dashboard's Settings tab edits them. Per-machine settings are `host.<alias>.<key>` (`[hosts.<alias>]`).

## General

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `refresh` | `fleet.refresh` | `0.1` (number) | seconds between polls of each machine (0.1 = as fast as it answers; unreachable machines back off) |
| `naming` | `sessions.naming` | `story` (story \| verb \| folder) | how new sessions are named: story = <machine's mood>-<agent's creature> (smoldering-heron: Claude on the peach machine), verb (ignite) or folder (px) |
| `tab_prefix` | `sessions.tab_prefix` | — | tab name prefix; empty = <agent>_ (claude_, codex_, …) |
| `return_to_dashboard` | `sessions.return_to_dashboard` | `true` (on/off) | come back to the dashboard after detaching (ctrl+b q) |
| `enter_opens` | `sessions.enter_opens` | `focus` (focus \| attach \| direct) | enter on a session: Focus (fullscreen here, with messages and splits), attach in herdr (its own terminal view), or straight into the agent |
| `focus.keys` | `sessions.focus_keys` | `agent` (agent \| aleph) | where keys go when Focus opens: agent (straight into it, as in its own terminal — focus.exit, mux_leave or a click outside gives the keys back to Aleph, focus.exit again goes to Fleet; the wheel scrolls its history) or aleph (Aleph's keys first: i for the message bar, I to type into it) |
| `focus.exit` | `sessions.focus_exit` | `ctrl+q` | keys out of Focus (and Tile): while typing into the agent, the first gives the keys back to Aleph (esc is the agent's then: it interrupts its work), the next goes to Fleet; the prefix and then this key sends it to the pane; comma-separated: ctrl+q, f12, alt+q, … |
| `focus.copy_on_select` | `sessions.focus_copy_on_select` | `true` (on/off) | Focus / Tile: text you drag across a pane is copied when you let go, as herdr does. Off: it stays selected until the next key or click, and Cmd+C copies it where the terminal passes Cmd+C on. Option+drag goes to the agent instead |
| `focus.esc_guard` | `sessions.focus_esc_guard` | `true` (on/off) | while typing into an agent, the first esc is held back with a warning (esc stops most agents' work); a second within 3 s goes to the agent, and so do the ones right after it |
| `shell.list` | `sessions.shell_list` | `all` (all \| aleph) | which plain terminals Fleet lists: all (every pane herdr has with no agent in it) or aleph (only those Aleph started) |
| `shell.ready` | `sessions.shell_ready` | `first-use` (first-use \| startup) | each machine's standing terminal, the one ! opens on a machine's row: first-use (made the first time you ask, then kept) or startup (made as soon as the machine answers, which adds a pane there) |
| `close.grace` | `sessions.close_grace` | `5s` | how long a close waits before it happens, wherever it comes from (x, aleph close, Raycast, the web): the agent keeps running until then, and u, aleph close --undo or Undo keep it (5s, 30s, 1m; 0 = at once; at most 1h); aleph close --now skips it once |
| `show_hidden` | `browser.show_hidden` | `false` (on/off) | browser: show dot-folders by default |
| `show_ignored` | `browser.show_ignored` | `false` (on/off) | browser: show package/build folders by default |
| `ignore` | `browser.ignore` | — | browser ignore list edits, e.g. +data,-build |
| `browse_ttl` | `browser.ttl` | `600` (number) | seconds before a cached folder listing is re-fetched |
| `deep_search` | `browser.deep_search` | `3` (number) | browser: folder levels indexed for search (0 = off) |
| `wall_fit` | `tile.fit` | `on` (on \| off) | Terminals sizes each shown session to its tile (like attaching does); off watches it at its own size |
| `inbox.layout` | `inbox.layout` | `overview` (overview \| cards \| table) | the Inbox's layout: overview (KPI tiles, a panel per group, the day's activity), cards (live sessions as cards), table (two lines a row); L switches while you look |
| `inbox.group` | `inbox.group` | `machine` (machine \| day \| agent \| type \| session) | how the Inbox groups what happened (G switches while you look) |
| `inbox.lifecycle` | `inbox.lifecycle` | `false` (on/off) | list sessions starting and ending in the Inbox, and toast a start; off, the Launches filter still has your starts |
| `inbox.hide_ended` | `inbox.hide_ended` | `false` (on/off) | hide alerts whose session has ended (e switches it in the Inbox); off, they're dimmed and listed after the live ones |
| `inbox.auto_archive` | `inbox.auto_archive` | `1d` | a launch whose session has ended goes to the archive by itself after this long (30m, 1d, 7d; off = never); A in the Inbox shows the archive |
| `inbox.needs_ttl` | `inbox.needs_ttl` | `5m` | a needs-you alert goes to the archive after this long, even while its session still waits (the live Needs-you lists still show it); answered or ended ones go at once (off = only those) |
| `inbox.down_grace` | `inbox.down_grace` | `30s` | a machine that stops answering counts as down after this long (a dropped packet never files anything); then the Inbox gets an item only if an agent there was working or waiting on you, and nothing while this computer itself is offline |
| `inbox.machine_back` | `inbox.machine_back` | `close` (close \| lost \| off) | when a machine that was down answers again: close (its outage row gets its end and what happened to the sessions there, and archives itself when nothing was lost), lost (an item only when a session there is gone), off (nothing) |
| `inbox.finished` | `inbox.finished` | `none` (none \| unseen \| unseen-long \| all) | which finished turns the Inbox lists (one row a session a day, with how many): none (the Activity strip and the journal still have them), unseen (not ones that finished on a dashboard's screen in front of you), unseen-long (those, longer than notify.done_min), all |
| `wall_layout` | `tile.layout` | `grid` (grid \| main \| rows \| cols) | tiling layout of the wall |
| `mux_keys` | `tile.keys` | `herdr` (herdr \| tmux) | Focus / Tile prefix key: herdr's own (its config.toml [keys] prefix) or tmux's ctrl+b |
| `mux_prefix` | `tile.prefix` | — | a prefix key of your own for Focus / Tile (e.g. ctrl+a); empty = mux_keys decides |
| `mux_leave` | `tile.leave` | — | more keys that stop typing into a pane in Focus / Tile and give them back to Aleph, besides focus.exit (comma-separated: f12, alt+q, …); a click outside the pane stops it too |
| `wall_max` | `tile.max` | `9` (number) | tiles per wall page |

## Appearance

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `theme` | `ui.theme` | `auto` (auto \| dark \| light \| nord \| catppuccin \| catppuccin-latte \| tokyo-night \| gruvbox \| dracula \| solarized-light \| singularity \| singularity-light \| pulsar \| pulsar-light \| event-horizon \| event-horizon-light \| matrix \| matrix-light \| matrix-mono \| matrix-mono-light \| convergence \| convergence-light \| cyberpunk \| cyberpunk-light) | colour theme: auto (Graphite or Paper, as the terminal is), dark (Graphite), light (Paper), nord, catppuccin, catppuccin-latte, tokyo-night, gruvbox, dracula, solarized-light; the cosmos — singularity (a point becoming everything: white-gold on a violet void), pulsar (cyan beams, magenta flares), event-horizon (accretion amber on true black), matrix (phosphor green, the states keep their colours), matrix-mono (all green: states by brightness and their glyphs ▲ ✓ ✗ ◷), convergence (synapse violet on indigo), each with a -light version; cyberpunk (a neural interface's HUD: electric yellow for the accent and selection, off-red chrome, cyan data, on near-black; HUD-bracket frames, a glitch between views) and cyberpunk-light (the key art's yellow ground, black ink); a theme paints its own background (ui.ground), frames its panels its own way (ui.panels) and has its own spinner, state glyphs and splash (ui.spinner, ui.splash = theme); or one of yours ([themes.<name>] in aleph.toml: base = a theme to start from, then accent, blue, mauve, green, yellow, red, peach, teal, pink, sky, muted, faint, line, sel, text as #rrggbb); the palette's Theme gallery previews them live; NO_COLOR turns colour off |
| `accent` | `ui.accent` | `gold` | accent colour: gold (Aleph's), claude (Claude's orange), a palette name or #rrggbb |
| `mouse` | `ui.mouse` | `true` (on/off) | mouse clicks and scrolling in full-screen views |
| `ui.keyboard` | `ui.keyboard` | `auto` (auto \| legacy) | auto: where the terminal can (kitty's keyboard protocol: Ghostty, kitty, WezTerm, iTerm2, foot, …), every key comes in as itself — ctrl+enter, ctrl+tab, alt+esc — and Focus passes it to the agent as pressed; legacy: the classic encoding (ctrl+enter is enter) |
| `ui.splash` | `ui.splash` | `theme` (theme \| cockpit \| classic \| singularity \| pulsar \| event-horizon \| matrix \| convergence \| cyberdeck \| neon \| random \| off) | what plays as the dashboard starts (any key skips it): theme (the theme's own: cyberpunk's cyberdeck boot, the cosmos themes theirs, the rest the cockpit; it follows the theme), cockpit (an alien ship's bridge dropping out of warp at a black hole, about 2 s), classic (the aleph under a band of light, 1 s), singularity (a point becoming everything: foam, the point, the eruption, young galaxies, 3 s), pulsar (a star keeping time: its beams sweeping, a magnetar flare, spinning up to a millisecond pulsar, 3 s), event-horizon (a black hole close: lensed stars, its disc and shadow, a star torn apart, falling through the horizon, 3 s), matrix (digital rain with your machines in the code, déjà vu, the code freezing and decoding into ALEPH, 3 s), convergence (a network learning, your machines its hubs, spiralling into one brilliant core: the AI singularity, 3 s), cyberdeck (a neural interface booting: its HUD tracing itself, diagnostics, your machines coming online, the sync completing, 3 s), neon (a rain-soaked city at night: your machines as neon signs flickering on, flying traffic, the wet street mirroring it all, 3 s), random (a different one each time), off; aleph splash NAME plays one on its own |
| `ui.splash_ms` | `ui.splash_ms` | `auto` | how long the splash plays: auto (each its own: the cockpit 2 s, classic 1 s, the cosmos 3 s) or 1–8 s (3s, 4500ms); the whole of it stretches, the Aleph logo and its hold come after |
| `ui.splash_end` | `ui.splash_end` | `random` (random \| form \| fall \| warp) | how the cockpit splash ends: form (the particles gather into the ℵ, it flares and bursts), fall (the ship falls into the ℵ, the sky squeezed into a ring), warp (the stars streak out again to the fleet), or random (one of them each time) |
| `ui.splash_logo` | `ui.splash_logo` | `particles` (particles \| off) | how a splash ends: particles (the Aleph logo gathers from the scene's own particles and holds for ui.splash_hold) or off (each ends its own way: the cockpit on its fall or warp, the cosmos on their last image); the palette's Splash gallery previews both (l) |
| `ui.splash_hold` | `ui.splash_hold` | `700ms` | how long the Aleph logo stays up at the end of a splash (e.g. 700ms, 1s; 0 for none; up to 3s) |
| `ui.splash_fx` | `ui.splash_fx` | `all` (list: tunnel, shock, waves, spirals, comet, flares, meteors, constellation, radar, scanlines, glitch, glow) | the cockpit splash's effects, a comma list (all, off, or all but some: all, -glitch): tunnel (warp rings), shock (dropping out), waves (gravitational waves ripple the stars), spirals (matter spiralling in, sparks up the jets), comet, flares, meteors, constellation (your fleet as stars, sessions orbiting), radar, scanlines, glitch (the HUD's holographic flicker), glow (bright stars' halos) |
| `layout.fleet` | `layout.fleet` | — | how Fleet: its list, resources and preview share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main\|top\|bottom\|side; empty: the view's own |
| `layout.monitor` | `layout.monitor` | — | how Monitor's overview: its charts and processes share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main\|top\|bottom\|side; empty: the view's own |
| `layout.inbox` | `layout.inbox` | — | how the Inbox: its list and detail share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main\|top\|bottom\|side; empty: the view's own |
| `layout.settings` | `layout.settings` | — | how Settings: its list and detail share the screen, as p, z, { }, = and L leave it: side N (the side panel's width, %) · top N (the top panel's height, %) · hidden (no side panel) · max main\|top\|bottom\|side; empty: the view's own |
| `ui.spinner` | `ui.spinner` | `theme` (theme \| aleph \| stars \| dots \| line) | the busy spinner: theme (the theme's own: the pulsing ℵ, cyberpunk's glitch, …), aleph (a pulsing ℵ), stars (Claude Code's), dots, line |
| `ui.ground` | `ui.ground` | `theme` (theme \| terminal) | the background: theme (a theme paints its own ground under the whole screen; auto, dark and light leave the terminal's) or terminal (never painted: your terminal's own background, its transparency too) |
| `ui.panels` | `ui.panels` | `theme` (theme \| rounded \| sharp \| heavy \| double \| cut \| brackets) | how panels are framed: theme (each theme its own: rounded, cyberpunk's HUD brackets, …), rounded, sharp, heavy, double, cut (chamfered corners ╱──╲) or brackets (only the corners, a HUD's targeting frame) |
| `ui.ambience` | `ui.ambience` | `subtle` (subtle \| rich \| off) | a theme's living details: subtle (one in the header, e.g. cyberpunk's data readout, and a glitch between views), rich (and its ambience in empty space: Tokyo Night's city rain, the Matrix's code, Nord's snow; Cyberpunk has none), off (still); ui.motion = off stops them too |
| `ui.note_for` | `ui.note_for` | `1s` | how long the status bar shows what a key just did ("processes hidden", "marked read") — 1s, 3s, 500ms |
| `ui.alert_for` | `ui.alert_for` | `6s` | how long the status bar shows a warning or an error |
| `ui.motion` | `ui.motion` | `on` (on \| reduced \| off) | light motion: on (a session row that needs you breathes softly, one that finishes gets a tick), reduced (only the tick), off — only the text's colour moves, never a background; none with NO_COLOR |
| `ui.row_bars` | `ui.row_bars` | `tokens,context` (list: tokens, context) | the tiny bars in each Fleet session row, after its status: tokens (tokens a minute over the last half hour, from its own logs), context (how full its context window is); off for none — rows without numbers keep the room blank, and a list narrower than 96 columns leaves them out |
| `ui.timeline_hours` | `ui.timeline_hours` | `6` (number) | how many hours the session timelines show, 1–48: Fleet's timeline rows (g), the preview's strip and Monitor › Timeline (+ - there) |
| `fleet.timeline` | `fleet.timeline` | `false` (on/off) | Fleet's session rows show each session's states over the last ui.timeline_hours (working, needs you, done, idle, at its limit) under an hour axis, instead of folder, title and usage; g switches it |
| `ui_scale` | `ui.scale` | `comfortable` (compact \| comfortable \| large) | how roomy the dashboard is: padding in panels, margins, row spacing (the font itself is your terminal's: cmd/ctrl +) |
| `icons` | `ui.icons` | `auto` (auto \| nerd \| emoji \| text) | logos for machines, GPUs and agents: auto (real logos when a Nerd Font is installed — set it as your terminal font), nerd, emoji or text |
| `ui.logos` | `ui.logos` | `auto` (auto \| images \| thumbnails \| ascii \| glyphs \| text) | how logos are drawn: auto (images in kitty, Ghostty, iTerm2 and WezTerm, else Nerd Font glyphs, else names), images (block thumbnails where pictures don't show), thumbnails, ascii, glyphs or text — `aleph icons` shows them all |
| `ui.logo_size` | `ui.logo_size` | `medium` (small \| medium \| large) | how big logos are where there's room: small (6×3 cells), medium (8×4) or large (16×8) |
| `herdr_view` | `ui.herdr_view` | `off` (off \| needs-you-first) | while the dashboard runs, herdr's own sidebar lists the sessions that need you first (put back when you quit) |
| `herdr_tokens` | `ui.herdr_tokens` | `off` (on/off) | put each machine's CPU, memory and GPU load on its agent panes as herdr tokens ($cpu $mem $gpu $gput) for herdr's sidebar, every 30 s while the dashboard has its stats |
| `ui.whats_new` | `ui.whats_new` | `on` (on \| off) | after an update, the dashboard shows what changed since you last opened it, once (on), or not (off; aleph whats-new and the palette's What's new still show it) |
| `tour` | `ui.tour` | — ( \| done) | the dashboard's four-card tour after setup: done once seen (empty shows it again after aleph setup) |

## Machines

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `ssh_host` | `fleet.default` | — | default ssh alias |
| `machine` | `fleet.herdr_machine` | — | herdr machine label for the default host |
| `hosts` | `fleet.machines` | — | machines shown together (comma-separated); empty = the default host |
| `stats` | `stats.mode` | `on` (on \| selected \| off) | live machine stats (CPU, GPU, memory) in the dashboard |
| `stats_interval` | `stats.interval` | `1` (number) | seconds between machine stat samples |
| `services` | `services.watch` | `off` (off \| on) | watch for local LLM servers and AI gateways (Ollama, LM Studio, vLLM, …) on every machine; service.<id>=off skips one |
| `services_interval` | `services.interval` | `30` (number) | seconds between LLM-service scans of each machine |
| `local` | `fleet.local` | `true` (on/off) | show this computer's own herdr as the `local` machine (when herdr is installed here) |
| `fleet.group` | `fleet.group` | `tag` (tag \| none) | group Fleet's machines by their first tag (host.<machine>.tags; e to edit), with a heading per tag |

## Agents

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `agent` | `sessions.agent` | `claude` (claude \| codex \| opencode \| omp \| pi \| cursor \| devin \| gemini \| agy \| cline \| mastracode \| copilot \| kimi \| kiro \| droid \| amp \| grok \| hermes \| kilo \| qodercli \| qwen \| letta \| maki \| muse \| crush \| commandcode \| prime) | default agent for new sessions (aleph agents lists them) |
| `agents` | `fleet.show_agents` | `all` | which agents' sessions are listed: all, or agent ids (claude,codex) |
| `agents.install_timeout` | `agents.install_timeout` | `3600` (number) | seconds an agent's installer may run (setup and aleph agents install) before it's stopped — then Aleph checks whether the agent is there anyway; long enough for a big clone and a large download on a slow link, and x on setup's install screen stops one sooner |
| `agents.install_quiet` | `agents.install_quiet` | `60` (number) | seconds of silence after an agent's installer asks something (a line ending in ?, [y/N], a login menu) before it's stopped — installers run with no terminal, so nothing can answer; a silent clone or build isn't a question and goes on |
| `agents.node` | `agents.node` | `auto` (auto \| off) | an agent that installs with npm, on a machine without Node.js (or with one too old): auto puts the current Node.js LTS there first — its newest release, sha256-checked, in your home folder, no sudo; off says how instead |
| `wraps` | `wrapper.wraps` | `claude` | the agent your shell function stands in for: its own flags and subcommands (-p, mcp, …), piped calls and calls inside herdr panes go to it |
| `agent_wrapper` | `agents.wrapper` | — | a command every agent starts under, like a sandbox (`nono run --`, `fence`): typed as HERDR_AGENT=<agent> <wrapper> <agent> …, so herdr still knows the agent (host.<a>.agent_wrapper for one machine) |
| `handoff.summary` | `handoff.summary` | `agent` (agent \| aleph) | who writes a hand-off note (h in Fleet, aleph handoff): agent (the session's own agent, asked with the prompt library's handoff, into ~/.cache/aleph/handoff/ on its machine, never the repo) or aleph (no agent: the task, git's status and diff stat, recent commits, the end of its screen) |
| `handoff.fallback` | `handoff.fallback` | `true` (on/off) | with handoff.summary = agent: when the agent is at its usage limit, or doesn't write its note within 6 minutes, Aleph writes it instead (off: the hand-off stops) |
| `handoff.to` | `handoff.to` | — | the agent a hand-off goes to first (empty: Codex from Claude Code, Claude Code from anything else); h in Fleet lists every agent on the machine |
| `race.agents` | `race.agents` | `claude,codex` | the agents a race runs, 2 to 4, a comma list (aleph race --agents overrides it); each gets the same prompt in a git worktree of its own |
| `openrouter.key` | `openrouter.key` | `secret:openrouter` | your OpenRouter API key, kept in the Keychain / Secret Service (aleph secret set openrouter, or setup's Agents step); off = none. Sessions of openrouter.agents get it as OPENROUTER_API_KEY (only through herdr's socket: never on a command line, on disk or in a log), and Usage shows its spend |
| `openrouter.agents` | `openrouter.agents` | `opencode,kilo,pi,omp,crush,hermes,maki` | the agents whose sessions get the OpenRouter key: agent ids (the ones that read OPENROUTER_API_KEY themselves), all (-id leaves one out), or off. OpenCode, Kilo, Pi and OMP also get a model picker in the new-session form |
| `openrouter.models` | `openrouter.models` | `all` (all \| free \| paid) | which of OpenRouter's models to use: all, only the free ones ($0, the :free variants), or only paid ones. The new-session form's model picker lists just those, and OpenCode sessions Aleph starts get the same as their own config (OPENCODE_CONFIG_CONTENT: provider.openrouter.whitelist / blacklist; your opencode.json is never touched) |

## Notifications

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `notify` | `notify.desktop` | `true` (on/off) | aleph watch: notify when a session needs you |
| `notify.subagents` | `notify.subagents` | `false` (on/off) | notify when an agent wakes because a subagent reported back while others still work; off, only the last report counts as the session finishing |
| `notify.commands` | `notify.commands` | `30` (number) | a command in a terminal that ran this many seconds notifies when it finishes (0: never). Things you leave running and end yourself, such as vim, ssh or htop, never do. herdr gives no exit code, so it says finished, not failed |
| `notify_done` | `notify.desktop_done` | `false` (on/off) | desktop notifications for every finished turn, not only the long ones (notify.done_min); never for a session on screen in a dashboard in front of you |
| `notify.ntfy` | `notify.ntfy` | — | phone notifications: an ntfy topic URL, e.g. https://ntfy.sh/<secret-topic> |
| `notify.slack` | `notify.slack` | — | a Slack incoming-webhook URL |
| `notify.webhook` | `notify.webhook` | — | any URL; gets each event as JSON |
| `notify.source` | `notify.source` | `both` (both \| herdr \| agents) | who tells you: herdr's view of each agent, the agents' own hooks (aleph hooks install), or both (the agent's words win when both say it) |
| `notify.desktop.click` | `notify.desktop_click` | `aleph` (aleph \| raycast \| warp) | a click on a desktop notification opens the session in Aleph's Focus (a new terminal tab), Raycast's Aleph inbox, or a Warp tab attached in herdr |
| `notify.dnd` | `notify.dnd` | — | do not disturb: on, or until a time (aleph notify dnd 1h); problems in notify.quiet_pass still come through |
| `notify.quiet` | `notify.quiet` | — | quiet hours for phone notifications, e.g. 23:00-08:00 (routes in aleph.toml can set their own) |
| `notify.quiet_pass` | `notify.quiet_pass` | `error` | kinds that come through quiet hours, do not disturb and mutes (comma-separated) |
| `notify.include_text` | `notify.include_text` | `summary` (summary \| full \| off) | how much of what an agent said goes into a notification: 140 characters, all of it, or none |
| `notify.escalate_after` | `notify.escalate_after` | — | a question still waiting this long is sent again (e.g. 10m); empty: never |
| `notify.herdr` | `notify.herdr` | `false` (on/off) | questions, limits and problems also show as a toast in herdr's own window here |
| `notify.raycast` | `notify.raycast` | `false` (on/off) | refresh the Raycast menu bar the moment something happens |
| `notify.raycast.kinds` | `notify.raycast_kinds` | — | what the Raycast menu bar lists: event kinds (blocked, done, limit, error, …), all, or none; empty: the extension's default |
| `notify.raycast.deeplink` | `notify.raycast_deeplink` | `raycast://extensions/aleph/aleph/menu-bar?launchType=background` | the Raycast command notify.raycast refreshes |
| `notify.remote_on` | `notify.remote_on` | `blocked,limit,done` | which events go to ntfy/Slack/webhook: blocked,done,new,gone,error,auto,task,heal,limit or all (a finished turn only after notify.done_min of work) |
| `notify.question_after` | `notify.question_after` | `15s` | a question goes to the phone (ntfy, Moshi, Slack, webhook, web push) only if it's still waiting after this long — answer it at the terminal and the phone stays quiet. 0 = at once |
| `notify.done_min` | `notify.done_min` | `2m` | a finished turn goes to the phone (ntfy, Moshi, Slack, webhook, web push) only after this much work; shorter ones stay in the Inbox, Raycast and on the desktop. 0 = every turn |
| `moshi` | `moshi.mode` | `off` (off \| auto \| on) | Moshi (the phone terminal) through moshi-hook: inbox cards, phone approvals, live jobs — off until you turn it on (aleph moshi install); auto = on when moshi-hook is paired here |
| `moshi.events` | `moshi.events` | `needs_you,limit,error,down,up,done` | what becomes a Moshi card: needs_you,limit,error,down,up,done (done: after notify.done_min of work) |
| `moshi.approvals` | `moshi.approvals` | `true` (on/off) | questions go to the phone as buttons (for sessions remote.allow covers; never rm -rf/sudo/… — approve_deny) |
| `moshi.choices` | `moshi.choices` | `false` (on/off) | the agent's own options as buttons on Moshi questions; off = Approve / Deny, which pick its yes and no options (Moshi's app answers only those so far) |
| `moshi.jobs` | `moshi.jobs` | `true` (on/off) | working sessions show as live rows in Moshi's inbox |
| `moshi.session` | `moshi.session` | `true` (on/off) | keep an `aleph` tmux session running, so Moshi's session picker (and a tap on a card) opens the dashboard |

## Remote control

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `remote.allow` | `remote.allow` | — | sessions phones, Raycast and the web may act on (approve, reply): host:agent globs, e.g. b31:*,*:claude or *:* — empty = notifications only |
| `remote.deny` | `remote.deny` | — | host:agent globs never acted on remotely (wins over remote.allow) |

## Analytics

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `analytics.enabled` | `analytics.enabled` | `true` (on/off) | read the agents' own logs on every machine for tokens, models, time and cost (aleph usage) — only ids, numbers, models and folders leave a machine, never conversation text |
| `analytics.include` | `analytics.include` | — | host:agent globs analytics covers (empty = everything), e.g. b31:*,*:claude |
| `analytics.exclude` | `analytics.exclude` | — | host:agent globs analytics never reads (wins over analytics.include) |
| `analytics.backfill` | `analytics.backfill` | `90d` | how far back the first read goes, by log file age: 90d, 12w, 1y or all |
| `analytics.keep` | `analytics.keep` | `400d` | how long usage records are kept (aleph usage prune): 400d, 2y or all |
| `analytics.interval` | `analytics.interval` | `60` (number) | seconds between the daemon's reads of every machine's logs: tokens, $ and quota left, the same in the menu bar, Raycast and the dashboard (60 at least) |
| `analytics.max_seconds` | `analytics.max_seconds` | `20` (number) | per machine and read: start no new log file after this many seconds (the rest waits) |
| `analytics.max_mb` | `analytics.max_mb` | `64` (number) | per machine and read: read at most this many MB of logs (the rest waits) |
| `analytics.cost` | `analytics.cost` | `on` (on \| off) | show $ estimates next to tokens (list prices per model; analytics.prices.<model> overrides one) |
| `usage.layout` | `analytics.usage_layout` | `tabs` (tabs \| page \| pivot) | Monitor › Usage as tabs (Overview, Breakdown, Trends, Turns & sessions, Insights: v moves), one long page (j/k scroll it) or a pivot you build (rows b, split s, measure n, chart c); L cycles them |
| `timeline.keep` | `timeline.keep` | `120d` | how long the timeline's steps are kept (prompts, tool calls, subagents — names and numbers, never what was said): 120d, 1y or all |
| `timeline.fold` | `timeline.fold` | `smart` (smart \| open \| folded) | how a session's timeline starts: smart (repeats folded, finished prompts closed to a line; the running one, errors and costly steps open), open (only repeats folded), folded (every section to its summary line) |
| `timeline.words` | `timeline.words` | `live` (live \| off) | the timeline's words: live (a prompt's first line and a tool's target, read from the machine only while that session is on screen, secrets masked, never kept) or off (names and numbers only) |
| `analytics.account_label` | `analytics.account_label` | `email` (email \| plan \| nickname) | how an agent's account is named in usage: email (its sign-in and plan: you@work.com · Max 20x), plan (Claude Max 20x · a3f2c1, no email shown), nickname (Claude account 1 · a3f2c1 until you name it); analytics.accounts.<id> names one, whatever this says |
| `analytics.claude_roots` | `analytics.claude_roots` | — | more Claude Code config folders to read on every machine, comma-separated (~/… allowed): $CLAUDE_CONFIG_DIR, ~/.claude, ~/.claude-* and ~/.config/claude-* are found on their own; a machine's own: host.<alias>.claude_roots |
| `analytics.codex_homes` | `analytics.codex_homes` | — | more Codex homes (CODEX_HOME) to read on every machine, comma-separated: $CODEX_HOME, ~/.codex and ~/.codex-* are found on their own; a machine's own: host.<alias>.codex_homes |
| `journal.include` | `journal.include` | `all` (list: prompts, finals, files, cost) | what the session journal (a Markdown file a day: aleph journal, the palette's Journal) says about each session: prompts (the ones sent through Aleph — the dashboard, aleph send, the queue, phone replies, new sessions, hand-offs, races; kept in a small pruned record, secrets masked, only while this is on; text typed straight into herdr isn't seen), finals (each finished turn in the agent's own words, else its title), files (the day's commits in its folder and what isn't committed, by git on each machine), cost (time at work, tokens and $, per session and per turn); all, off (no journal), or all but some (-files) |
| `journal.dir` | `journal.dir` | — | where the session journal's files go (YYYY-MM-DD.md); empty: ~/.local/share/aleph/journal (Aleph's data folder, $XDG_DATA_HOME) |

## Automation

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `auto_approve` | `automation.auto_approve` | `false` (on/off) | answer questions that match an approve.<rule>=<pattern> rule (aleph auto) |
| `approve_deny` | `automation.deny` | `*rm -rf*,*sudo *,*git push*,*--force*,*drop table*,*mkfs*,*dd if=*,*shutdown*,*reboot*` | never auto-approve questions matching these patterns (comma-separated, * wildcards) |
| `approvals.layout` | `automation.approvals_layout` | `list` (list \| cards) | the approval board (ctrl+a): list (who's waiting beside the selected question, as its agent shows it) or cards (each question in a box of its own); v switches it there |
| `approvals.group` | `automation.approvals_group` | `off` (off \| same) | the approval board's list: off (each question on its own row) or same (agents asking the same thing share a row, and one answer goes to each of them — each still read again and checked against the deny list) |
| `auto_heal` | `automation.heal` | `false` (on/off) | restart herdr where it stopped (and restore its sessions) while watch or the dashboard runs |
| `limits.auto_resume` | `automation.auto_resume` | — | host:agent globs (e.g. b31:claude, *:codex) whose sessions carry on by themselves once a usage limit resets — it sends the agent's own resume.<agent> ([automation.resume] in aleph.toml), else limits.resume_prompt |
| `limits.resume_prompt` | `automation.resume_prompt` | `continue` | what auto-resume sends once the limit lifts (an agent's own: resume.<agent>) |
| `hook.pre_new` | `hooks.pre_new` | — | shell command run in the new pane before the agent starts |
| `hook.post_new` | `hooks.post_new` | — | local command run after a session starts (CH_HOST, CH_DIR, CH_NAME, CH_PANE) |

## Daemon & API

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `daemon` | `daemon.at` | `local` | where a bare aleph opens the dashboard: local, or ssh://<machine> / mosh://<machine> (a VPS near the fleet; it reconnects when the link drops; aleph --local stays here) |
| `watch_interval` | `daemon.interval` | `20` (number) | aleph watch: seconds between checks |
| `hub.machine` | `hub.machine` | — | with daemons on several computers: the one (hostname -s) that runs automation — auto-approve, the queue, notifications, Moshi, self-heal; empty = this one |
| `api.http` | `api.http` | `off` (off \| on) | the daemon also serves its API and the web console over HTTP (api.listen; aleph daemon pair adds a phone) — restart the daemon after changing it |
| `api.listen` | `api.listen` | `127.0.0.1:7878` | where the HTTP API listens: this computer only by default (put tailscale serve or cloudflared in front); another address needs api.listen_any |
| `api.listen_any` | `api.listen_any` | `false` (on/off) | allow api.listen on an address other machines reach directly (0.0.0.0, a LAN or tailnet IP) — tokens still apply, but prefer tailscale serve |
| `api.url` | `api.url` | — | the address phones and browsers use for the console (e.g. https://mac.tail1234.ts.net): pairing links use it and its host is allowed |
| `api.hosts` | `api.hosts` | — | more names the console may be reached by (comma-separated, *.example.com allowed); anything else is refused (DNS rebinding) |
| `api.trust_tailscale` | `api.trust_tailscale` | `false` (on/off) | behind tailscale serve: name the tailnet user (Tailscale-User-Login) in the Inbox audit line — for the record only, never instead of a token |
| `api.push_contact` | `api.push_contact` | — | who push services can contact about this sender (mailto: or https:, sent in Web Push's VAPID token); empty: a generic address |

## Integrations

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `terminal` | `sessions.terminal` | `auto` (auto \| warp \| iterm \| terminal \| ghostty \| kitty \| wezterm) | where "Open in herdr" opens its new tab (auto: the terminal Aleph runs in) |
| `editor` | `integrations.editor` | `auto` | what o (open in editor) uses: auto (the first of Cursor, VS Code, Zed on this computer), cursor, code, zed, windsurf, or your own command line with {path}, {host} and {remote} (ssh-remote+<alias>); another machine's folder opens over the editor's Remote-SSH (Zed: ssh://) |
| `raycast_dirs` | `integrations.raycast_dirs` | `~/.raycast-scripts` | Raycast script folders (colon-separated) |

## Network

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `ssh_key` | `ssh.key` | — | the ssh key Aleph uses for key login and the machines it adds (a file in ~/.ssh); empty = ssh's own choice (your ~/.ssh/config, then id_ed25519, id_ecdsa, id_rsa) |
| `ssh_reuse` | `ssh.reuse` | `120` (number) | keep one shared ssh connection per machine open N idle seconds (0 = a new connection every time) |
| `listen` | `fleet.listen` | `true` (on/off) | subscribe to each machine's herdr events, so changes show up the moment they happen |
| `attach_via` | `network.attach_via` | `ssh` (ssh \| mosh) | how attaching reaches a machine: herdr --remote over ssh, or mosh running herdr there (survives roaming and sleep; needs mosh at both ends) |
| `net.probe` | `network.probe` | `1.1.1.1:443` | what the connection indicator checks: host:port, a TCP connect every few seconds (no data sent) |
| `herdr_via` | `ssh.herdr_via` | `ssh` (ssh \| machine) | reach a machine's herdr over our shared ssh connection (fast) or through `herdr --machine` |

## Advanced

| Setting | TOML | Default | What it does |
|---|---|---|---|
| `close.exited` | `sessions.close_exited` | `close` (close \| keep) | when an agent exits by itself: close its pane if Aleph opened it and only its shell is left running (close), or leave the pane as a shell (keep) |
| `log.level` | `log.level` | `info` (error \| warn \| info \| debug) | what goes into the log (aleph logs): info = machines down/up, reconnects, answers sent, failures; debug adds every external command |
| `fleet.board_order` | `fleet.board_order` | `needs` (needs \| working \| recent \| machine) | Fleet's board (the machines list maximised with z): each machine's sessions needs you first, working first, the most recent change first, or in herdr's order; , steps through them there |
| `update_repo` | `updates.repo` | — | GitHub owner/repo that publishes releases, for self-update |
| `update_pubkey` | `updates.pubkey` | — | the minisign public key releases are signed with (RW…): with it, self-update installs only a release whose SHA256SUMS.minisig it verifies |

## Per machine

Set as `host.<alias>.<key>` (`[hosts.<alias>] <key>`), e.g. `aleph settings set host.b31.tags gpu`.

| Key | What it does |
|---|---|
| `color` | the machine's colour in lists, cards and charts (C in Fleet) |
| `tags` | comma-separated tags: Fleet groups by the first (fleet.group); presets can target tag:<name> |
| `mac` | its network card's MAC address, for Wake-on-LAN (W) |
| `machine` | its herdr machine label, when it isn't the ssh alias |
| `session` | the herdr named session Aleph uses there (herdr's --session / HERDR_SESSION: its own server, tabs and socket); empty: the default one |
| `moshi` | what its sessions send to Moshi: auto, full, machine-only or off |
| `hooks` | the agents' own hooks report from there (on / off; aleph hooks install) |
| `paused` | Aleph doesn't poll it (Z in Fleet) |
| `agent` | the agent that starts there by default |
| `args` | arguments agents start with there |
| `dir` | the folder new sessions start in there |
| `agent_wrapper` | the agent_wrapper for that machine only |
| `claude_roots` | more Claude Code config folders to read usage from there (analytics.claude_roots, for that machine) |
| `codex_homes` | more Codex homes to read usage from there (analytics.codex_homes, for that machine) |
