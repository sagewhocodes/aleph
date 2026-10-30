# The timeline

What one agent conversation did, as a tree: **prompts → turns → tool calls**, with subagents as lanes of
their own. Each row has its time, tokens in → out and cost.

```sh
aleph timeline                     # the latest conversation
aleph timeline ls                  # the latest ones
aleph timeline 3f2a9c1e --fold open --words
aleph timeline my-session --json
```

**Where it comes from:** the same log reads as [usage](analytics.md). Only ids, tool names, times and numbers
are kept (`timeline.keep`, 120 days), never what was said.

**Words, live only:** a prompt's first line and a tool's target (a file, a command) are read from the
machine only while you look, with secrets masked, and never saved (`timeline.words = off` turns them off).

**Folding** (`timeline.fold`): `smart` (default) folds repeats ("Read + 7 more") and finished prompts, and
keeps errors and costly turns open; `open` folds only repeats; `folded` folds everything.

**In the dashboard:**

- `G` in Fleet puts the selected session's timeline in the right panel.
- Focus shows it at the side.
- Monitor › Timeline lists earlier conversations by project, with the selected one's tree beside it.
  `M` lists one machine, `C` hides the tree, `O` groups by machine, project, agent or account, and `F` narrows it to
  any of those.

The wheel scrolls it, a click folds a row. `[` `]` jump between prompts, `e` to the next error, `f`
follows the end, `w` shows the words.
