# Changelog

## 2.1.0 — in progress

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
