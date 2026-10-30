# Usage and cost

`aleph usage` (and Monitor › Usage) reads the agents' own logs on every machine: Claude Code, Codex,
OpenCode, Pi / OMP, Gemini CLI and Qwen.

**Privacy:** one `sh` + `awk` script per machine prints ids, times, models, folders and numbers, never what
was said. It reads only what the logs gained since last time, within a time and size budget. The store is
local (`~/.local/state/aleph/usage/`). Credentials are never read.

```sh
aleph usage                        # the last 7 days by model
aleph usage 30d --by project       # host | agent | account | model | project | day | hour | session
aleph usage this-month --account work --agent claude,codex
aleph usage 2026-09-01..2026-10-15 --compare
aleph usage --stats                # cost per prompt and per session: median, p90, the costliest
aleph usage accounts · aleph usage projects · aleph usage sources
aleph usage prices                 # each model's rates, where they came from, what's held
```

**Cost** is what the agent reported, else an estimate (marked *est.*; override with
`analytics.prices.<model>`).

**Prices** start from a table built into Aleph. Every hour the daemon reads three public lists: OpenRouter's,
models.dev's and LiteLLM's. No login goes to any of them, and `analytics.prices_sync = false` stops it.

- A change is dated and applies from when it was seen, so past usage keeps the price it ran at.
- A change two lists agree on applies at once. One list alone, or a jump over 50%, is held and shown on Usage, until
  the lists agree or you run `aleph usage prices accept MODEL`.
- Only a model's owner counts, never a reseller. A request routed through OpenRouter (`vendor/model`) uses what
  OpenRouter charges.
- Effort only changes the price where a list prices it as its own model (`<model>-<effort>`). No list does today.

**Accounts:** every request carries the login it was made on, so two Claude or Codex accounts, on one
machine or several, each show their own cost. Name them with `analytics.account_label` (email, plan or a
nickname).

**Projects:** one repo is one project wherever it was worked on: the same git remote on two machines,
worktrees and subfolders included. Rules merge or rename: `analytics.projects.<name>`.

**In the dashboard** (Monitor › Usage): filter with `t` (period), `a` (agents and accounts), `m`
(machines), `p` (projects) or type them with `/`. The screens are Overview, Breakdown, Trends, Turns &
sessions and Insights, or one long page, or a pivot (`L` switches).

**OpenRouter:** with a key (`aleph secret set openrouter`), the account's spend and credits show in Usage
and the header.

Settings: `analytics.enabled`, `analytics.interval`, `analytics.keep`, `analytics.include` / `exclude`.
