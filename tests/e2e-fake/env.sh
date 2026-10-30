# source me: a throwaway world with two fake machines (b31, gpu2), fake ssh +
# herdr on PATH, and the debug (or release) build of aleph.
#   source tests/e2e-fake/env.sh [release]
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO=$(cd "$ROOT/../.." && pwd)
RUN=$ROOT/.run
# the fake herdr socket server (examples/fake_herdr_sock.rs, see fakesock.sh):
# built here with the real cargo, HOME and PATH, before they're swapped out
case $HOME in *e2e-fake/.run*) ;; *) command -v cargo >/dev/null 2>&1 && (cd "$REPO" && cargo build -q --example fake_herdr_sock 2>/dev/null) ;; esac
export ALEPH_FAKE_TOOLS=${CARGO_TARGET_DIR:-$REPO/target}/debug/examples
export ST=$RUN/state RHOME=$RUN/rhome HOME=$RUN/home
export XDG_CONFIG_HOME= XDG_CACHE_HOME=$RUN/cache XDG_STATE_HOME=$RUN/xstate XDG_DATA_HOME=$RUN/xdata ZDOTDIR=
# a socket path is capped near 104 bytes: the fake herdr sockets, and the
# forwards Aleph makes under its cache, get a short private folder when the
# checkout is deep (worktrees)
FW=$RUN; [ ${#RUN} -lt 40 ] || FW=/tmp/aleph-fw-$(id -u)-$(printf %s "$RUN" | cksum | cut -d' ' -f1)
mkdir -p "$FW"; chmod 700 "$FW"
export ALEPH_FAKE_SOCK=$FW/sock XDG_CACHE_HOME=$FW/cache
BUILD=${1:-debug}
mkdir -p "$RUN/bin"; ln -sf "$REPO/target/$BUILD/aleph" "$RUN/bin/aleph"
# aleph on a fake machine's PATH (bin/ssh adds this folder for $ST/aleph_<host>):
# this build, with that machine's own folders (its HOME)
mkdir -p "$RUN/rbin"; export ALEPH_FAKE_RBIN=$RUN/rbin
printf '#!/bin/sh\n# aleph on a fake machine (tests/e2e-fake/env.sh): its own HOME and folders\nXDG_CONFIG_HOME= XDG_CACHE_HOME= XDG_STATE_HOME= XDG_DATA_HOME= exec "%s" "$@"\n' "$RUN/bin/aleph" > "$RUN/rbin/aleph"; chmod +x "$RUN/rbin/aleph"
export PATH="$ROOT/bin:$RUN/bin:/usr/local/bin:/usr/bin:/bin"
export COLORTERM=truecolor TERM=xterm-256color
# any tmux a test starts gets a private server, never the real one
export TMUX_TMPDIR=$RUN/tmux; mkdir -p "$TMUX_TMPDIR"; unset TMUX TMUX_PANE
# a shell inside a herdr pane (this repo is often worked on from one) must
# not leak it in: the wrapper would hand every call to the agent, and herdr
# calls would reach the real server
for v in $(env | sed -n 's/^\(HERDR_[A-Z_]*\)=.*/\1/p'); do unset "$v"; done
# a socket path is capped near 104 bytes: deep checkouts (worktrees) get a short private folder
[ ${#TMUX_TMPDIR} -lt 72 ] || { export TMUX_TMPDIR=/tmp/aleph-snap-$(id -u); mkdir -p "$TMUX_TMPDIR"; chmod 700 "$TMUX_TMPDIR"; }
# secrets go to a private file here, never the real Keychain
export ALEPH_SECRETS_FILE=1
# "open in a new tab" runs bin/fake-tab (logs it; runs it with $ST/tab_run),
# never a real terminal app
export ALEPH_TAB_CMD=$ROOT/bin/fake-tab
# "open in editor" runs bin/fake-editor (it logs), never a real editor app
export ALEPH_EDITOR_CMD="$ROOT/bin/fake-editor {remote} {path}"
# never a browser under the fake HOME: a link opens here
export ALEPH_OPEN_CMD="$ROOT/bin/fake-editor"
