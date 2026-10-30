# Changelog

## 2.1.0 — in progress

### Cursor's Agent SDK
- **Run and watch agents written as code.** `aleph sdk dev`, `validate` and `run -m "…"` start a project as a session, on any
  machine. It's on Fleet like any other: working while the command runs, with the command on the row, and it tells you when
  it finishes. Its screen opens in Focus. It's also in the `n` form's agent list, and the folder browser marks a project and
  starts one with `S`.
- `aleph sdk status` shows, per machine, its version, whether Node is new enough and whether you're signed in.
  `aleph sdk projects [DIR]` finds them.
- **Signing in stays yours.** `agent-sdk login` opens a browser, and Aleph only asks `agent-sdk whoami` for its exit code,
  never what it prints. Tokens, cost and deployed agents aren't shown yet.
- The package is `@grok/agent-sdk`, as Cursor's docs say. `@cursor/agent-sdk`, the name in the announcement, doesn't exist
  on npm.

### Monitor's Timeline, your way
- **`M` lists only the machine on the strip,** and follows it when you move to another. `C` hides the conversation panel so
  the list has the width (`G` still opens the turn view). Both are saved.
- **`O` organises the list by machine, project, agent or account,** with a heading and count for each, and each row saying
  which machine it's on. `timeline.group` keeps your choice.
- **`F` or `/` narrows it the way Usage does:** any machines, agents, accounts and projects, from one searchable list with
  what each has cost. A chip shows each one; click to drop it, or `x` clears them all.
- A bar on top says what's on, and every part of it is a click.

### Resource graphs
- **Twelve hours of history, by default, for every machine.** The daemon keeps sampling with nothing open, so a graph has
  the hours before you looked. `stats_keep_hours` sets how much, and Settings › Machines has a row per machine to give each
  its own (0: only while a view shows it, and no stream there). They stay in memory at a fixed size, and survive a restart
  because the daemon also writes them to `state/stats/<machine>.jsonl`.
- **`+` and `-` change the timescale in Monitor:** 5 m, 15 m, 30 m by default, 1 h, 3 h, 6 h and 12 h. It says which on the
  top right and the axis counts in it.
- **The graphs don't redraw any more.** A point appears when its time bucket closes and nothing already drawn moves, and
  leaving Monitor and coming back finds the same graph.
- **Time nothing was read stays blank,** in charts and sparklines, and averages skip it instead of counting it as zero.
- This is the one connection Aleph keeps open without a view asking for it: one quiet stream per machine, every 30 s.

### Keys on the timeline
- **`G` opens the turn view on Monitor's Timeline too,** as it does everywhere else, and `z` `Z` fold or expand every
  turn right there, without opening anything first. Your choice carries to the next conversation you move to.
- **Capital letters work in every terminal.** A terminal that sends shift+g as `g` with shift held got the lowercase
  key, so `G` on the timeline opened the GPU page.

### Two windows on one session
- **The window you're using gets the pane.** herdr gives a pane one controller and one size, so a second Aleph window
  opening a session the first still held showed it clipped on the right with empty space below, and it stayed like that.
  Now opening it takes it over and it fills the window. The other window shows "view only", and takes the pane back when
  you press a key, click, or switch to that window.
- A tile resized quickly can no longer lose its own pane to the stream it just replaced.

### Prices
- **Prices stay current, and past usage keeps the price it ran at.** The daemon reads OpenRouter's, models.dev's and
  LiteLLM's public lists every hour, with no login sent to any of them. A change applies from when it was seen, so your
  history never moves, and an upgrade can't re-price it either (`analytics.prices_sync`).
- **The right price for each request:** per model, its long-prompt tier, cache rates, fast mode and effort where a list
  prices one separately. Only a model's owner counts, not a reseller. A request routed through OpenRouter uses what
  OpenRouter charges.
- **A bad update can't skew your costs.** A change two lists agree on applies at once. One list alone, or a jump over 50%,
  is held and shown on Usage until the lists agree or you accept it. Your own `analytics.prices.<model>` and the costs
  agents report always win.
- **`aleph usage prices`** shows each model's rates, where they came from, when they last changed and any list that
  disagrees. `sync` reads the lists now; `accept MODEL` takes a held change.

### The tour
- **A hands-on tour,** with chapters on Fleet, Focus and splits, the Inbox, Usage and `ctrl+k`. Each step waits for the real
  key on your real sessions, with reading cards alongside. Only the key it asks for gets through, so nothing starts, closes or
  is sent while it runs. `ctrl+n` skips a step, `ctrl+x` ends it, and `ctrl+k` plays it again, or any one chapter.

### ctrl+k
- **Before you type:** what you pinned, your sessions by machine, a few actions. Type and everything else is there: every
  command, machines, views, pages, presets, folders.
- **A session is one aligned line:** its name, the agent's icon and short name, the model for agents that run on any model
  (OpenCode and the like), its state, and how long only while it works. What it's been doing shows under the list for the
  one you're on.
- **Words match in any order, each on its own:** `gpu2 claude` and `claude gpu2` list the same sessions.
- **`ctrl+p` pins up to three sessions** on top.
- **In Focus and Tile,** picking a session goes to it, `ctrl+s` adds it as a split, and their own commands and your windows
  by name are all listed.

### Session names
- **A session whose only name is herdr's tab number goes by its folder,** everywhere in Aleph: `train`, `train 2`. In a home
  folder it goes by its agent. Only the name on screen changes: herdr's stays, and still finds it.

### Prompts
- **A library:** `ctrl+p` outside a text box, or in `ctrl+k`. Browse by scenario and agent, read one whole, use it on the
  session you're on, edit, add and delete.
- **Starters** for review, tests, debug, refactor, docs, commit, handoff, explain, plan and security.
- **A prompt can be for certain agents,** and `<name>-<agent>` is that agent's way of doing it: `@review` in a Codex session
  uses `review-codex` when there is one.
- `ctrl+p` works in the new-session form too, and the list in any text box narrows as you type.

### Timeline
- **A turn view:** a conversation's prompts, turns and tool calls on the whole screen, with its keys. `t` in Monitor ›
  Timeline, `G` in Focus and `G` again in Fleet, or click "turn view" on any timeline panel.
- **Fold all, expand all, or one at a time:** `z`, `Z` and enter, or click them on the panel's top line, even while you're
  typing into the agent. Fold all leaves a line per prompt.
- **Compactions show.** They stay visible with their prompt folded, say how far the context shrank, and one in progress
  says so at once. Session strips have a colour of their own for it, and it counts as busy time.

### Leaving things
- **When an agent or a terminal exits, its window closes and you go back where you came from.** It used to leave you on an
  empty pane, or on some other window that happened to be open. `x` on the last pane does the same. `close.exited = keep`
  leaves it on screen, saying it ended.
- A window with nothing in it no longer comes back after a restart, and Tab no longer stops on one.

### Starting things
- **`n` is one flow, for agents and terminals.** Enter on a place moves to the agent, enter on the agent starts it, and `o`
  does the same and opens it in Focus. Nothing is marked and then left waiting for another key. A terminal is a choice beside
  the agents and opens where you picked.
- **The browser says what it's doing.** From `n` it reads "✓ Use this folder" and comes back to you, or starts straight
  away if you'd already settled on an agent. Esc returns to `n` as you left it. More options keeps the folder you picked.
- **`i` says what it starts:** "start Claude Code in a folder", the machine's own default, in the hints and in `ctrl+k`.
  It used to name one agent and could start another: the fleet's default, not that machine's.
- **A start has its row at once,** with a spinner and what it's doing, whichever way you began it. Enter opens it once it's
  up; the selection follows it to the real session, unless you've moved on.
- **A start that fails says why on its row.** `r` tries again with the same options, `x` drops it, and `x` also cancels one
  still running. It no longer leaves an empty tab behind.
- **A typed command is checked.** Agents started by typing into the shell used to report success whatever happened. Now
  "command not found" and the like fail the start. It only counts as failed when the shell says so by name, so a slow
  shell never closes a real tab.
- **`ctrl+t` opens a terminal in a folder you pick,** and `t` does in any folder browser.
- **`launch.<key>` binds your own:** `terminal /srv/train`, `terminal gpu2:/opt/x` or a preset's name. It works in any
  view and opens in Focus. `launch.ctrl+t = off` frees that key.
- Fleet's `/` finds sessions by agent too, and says it's there.
- `n`, `i`, `!`, `D` and the new-session form all start in the background now; none of them takes over the screen.

### Selecting and copying
- **Drag across any tile to select its text.** It's copied when you let go, as herdr does, in every tile rather than
  only the first; a line the terminal wrapped stays one line, and the screen holds still while you drag. Clicks still
  reach the agent, and option+drag goes to it for an agent's own dragging. `focus.copy_on_select` off keeps the selection
  until the next key, and Cmd+C copies it where the terminal passes Cmd+C on.

### Tile layouts
- **`L` locks a layout:** nothing splits, closes, swaps or resizes it, a session ending keeps its tile, and `w` comes back
  to it as you left it.
- **`z` maximises a tile, `z` goes back,** and the status line says so.
- `space` no longer loses what you arranged by hand: your own arrangement is one of the layouts it cycles through.

### Free splits
- **Any pane can hold anything:** `v` and `s` split it, and the picker lists every machine's sessions, a new agent and a
  terminal on any machine.
- **Your arrangement stays.** A split Focus becomes a Tile, `w` comes back to it, and it survives a restart. Marked
  sessions start a fresh one; `&` closes it.
- **`alt+arrows` move between panes while typing,** and the keys go with you. With one pane they're the agent's.

### Terminals
- **Every plain terminal herdr has is listed,** not only those Aleph started (`shell.list = aleph` for the old way). None
  of them is ever restored or closed by Aleph.
- **Each machine has a standing terminal.** `!` on a machine's row opens it, making it the first time; `!` on a session
  opens the terminal already in that folder if there is one. `shell.ready = startup` makes each machine's as soon as it
  answers.
- `!` works in Focus and Tile too, and shows in their hints, Fleet's and `ctrl+k`.
- **A terminal running a command shows as working, named for it,** and its timeline lists what it ran, long ones cut off,
  with how long each took. Commands are kept in memory only.
- **A long command notifies when it finishes** (`notify.commands`, 30 seconds; 0 turns it off), through the Inbox and every
  notification you've set up. Things you leave running and end yourself, such as vim or ssh, never do. herdr gives no exit
  code, so it says finished, not failed.
- A pane herdr couldn't bring back after a restart says why on its row.
- `ctrl+q` in Fleet no longer quits Aleph: one press too many while backing out of Focus would have closed it. Plain `q`
  still does.

### Keys that changed
- **`ctrl+q` is the way out.** While typing into an agent it gives the keys back to Aleph; pressed again it goes to Fleet.
  `ctrl+]` isn't Aleph's any more and reaches the agent. The prefix and then `ctrl+q` sends it to the pane, for a shell
  frozen by `ctrl+s`.
- **`[ ]` step through sessions, on every machine; `ctrl+[ ]` through machines.** `( )` do the machines too, where a
  terminal can't tell `ctrl+[` from esc. While typing the brackets are the agent's: `alt+,` `alt+.` or the prefix and
  `[ ]` step sessions. Monitor keeps `[ ]` for machines, except Timeline's list.
- **Tab cycles the views everywhere:** Fleet, Focus while it's open, Monitor, Inbox, Settings. Monitor's pages are now
  `←` `→`, the Inbox's filters `f` `F`, Settings' sections `[ ]` and the Doctor's arrangement `v`.
- Tile's windows moved to `w` `W`, the timeline tree's prompts to `( )`.

### The remaining fixes
- **Compaction has its own state.** A session compacting shows "compacting" and stays working, and the end of a
  compaction no longer counts as a finished turn.
- **Forms of questions are walked page by page.** The approval board shows which page you're on, its tick boxes and the
  Submit page; Tab and shift+Tab move between pages. A form is one question, so a new page never reads as answered.
  `Y` skips forms, and on the phone a form says to open it in Aleph.
- **The model is on every session row:** name, model, state, folder, then tokens, context, cost and time.
- **Agents outside herdr show their own usage,** read from the conversation they have open, and how long they've run.
  They no longer borrow a herdr session's numbers.
- **When an agent exits by itself, its pane closes** (`close.exited`, default `close`), if nothing but the shell is left.
- **The expanded Fleet names every GPU.** Each has its own line with its whole model. Spare rows show GPU detail, the
  machine, network and disks, and agents and services, and a GPU tool that failed says so.
- Usage's Overview fills a tall screen.
- Started and ended events leave the Inbox and the toasts by default (`inbox.lifecycle` brings them back).
- No ping for this computer: a slow local poll can't mark it degraded.
- No Services page when `services` is off.

## 2.0.2 — 2026-10-30
- **Usage reads on Linux again.** In 2.0.1 a mistake in the log reader made mawk, the default awk on Ubuntu and
  Debian, refuse it, so those machines recorded no new usage while reporting success. Nothing is lost: what they
  logged in the meantime is read after updating.
- A read that fails now says so. Usage shows how each machine was last read, with the reason when it failed.
- Machines with herdr stopped still get their logs read.
- The Inbox's count matches what it lists, and a question that was answered or expired leaves a running dashboard.
- A subagent reporting back no longer notifies while others are still out; the last report does.
  `notify.subagents` turns this off.
- A click on another tile focuses it, and the keys follow.
- `t` in Focus hides the timeline on the first press.
- The folder you choose in `n` is the one the session opens in, even if the machine's workspaces arrive afterwards.
- Cmd keys never type a letter into the agent or run a command in Aleph.

## 2.0.1 — 2026-10-27
- Pausing a machine (`Z`) asks twice, and a paused machine says so everywhere. The daemon and dashboards
  follow a pause or resume at once.
- Codex sessions show their tokens, cost, turns and timeline (Codex 0.164 no longer names its conversation
  to herdr).
- `ctrl+a` opens the approval board while typing into an agent, when something waits.
- Desktop notifications fall back to `osascript` when terminal-notifier isn't allowed.
- Release binaries carry no build paths; `tools/leak-scan.sh` guards the repository and the releases.
- Antigravity CLI: tokens, thinking tokens, cost, turns and timeline, with each request's model.
- OpenCode 2 read in full and installed by default; OpenCode 1 warned.
- Free models count their tokens and show as free; Gemini 3 prices; OpenRouter's prices for paid models
  (with a key).
- `openrouter.models`: all, free or paid only, in the picker and OpenCode's config.
- The model in use in Fleet, Focus and the timeline; `v` folds the timeline by session or prompt.
- Agent installs run inside the dashboard; fresh agents get their herdr integration.
- Usage's `b` drives its charts; `install.sh` puts `aleph` on your PATH.

## 2.0.0 — 2026-10-26
Aleph (formerly claude-herdr): one binary that runs and watches coding agents on all your machines.
- **Dashboard:** Fleet, Focus and Tile (type into agents natively), Monitor, Inbox, Settings, the palette.
- **Approvals:** every waiting question on one board, answered one at a time or all at once; auto-approve
  rules with a deny list.
- **Usage:** tokens, time and cost by account, agent, model, machine and project, from the agents' own logs.
- **Timeline:** what every agent did, as a tree of prompts, turns, tool calls and subagents.
- **Notifications:** desktop, ntfy, Slack, webhooks, Raycast; an Inbox that keeps only what matters.
- **Daemon and API:** a background service, a local API, the web console, deploys to your machines.
- **Looks:** themes, splashes, logos.

## 1.0.0
claude-herdr: a fleet of machines polled in parallel, remote control without attaching (`send`, `keys`,
`close`), live machine stats.

## 0.5.0
A fuzzy folder browser and a session picker.
