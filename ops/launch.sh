#!/usr/bin/env bash
# Launch a top-level role session (supervisor or coordinator) in tmux, IN-ROLE.
#
# The role's operating rules live in .claude/roles/<role>.md and are injected at launch
# via `claude --append-system-prompt`, on top of the root CLAUDE.md (shared invariants)
# that every session loads. Workers are NOT launched here — the coordinator spawns them
# via the Agent tool (subagent_type: worker), which loads .claude/agents/worker.md.
#
#   ops/launch.sh coordinator            # fresh coordinator session in tmux 'dev'
#   ops/launch.sh supervisor             # fresh supervisor session in tmux 'super'
#   ops/launch.sh coordinator --resume <session-id>   # resume an existing conversation, in-role
#
# Any extra args after the role are passed straight to `claude` (e.g. --resume <id>).
set -euo pipefail
REPO=/Users/daniellewis/hackriff
ROLE="${1:?usage: ops/launch.sh <supervisor|coordinator> [extra claude args...]}"; shift || true
RF="$REPO/.claude/roles/$ROLE.md"
[ -f "$RF" ] || { echo "no role file: $RF"; exit 1; }

case "$ROLE" in
  coordinator) SESSION=dev;   MODEL=opus; EFFORT=high ;;
  supervisor)  SESSION=super; MODEL=opus; EFFORT=high ;;
  *) echo "unknown role '$ROLE' (expected supervisor|coordinator)"; exit 1 ;;
esac

if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "tmux session '$SESSION' already exists — kill it first (tmux kill-session -t $SESSION) or attach."
  exit 1
fi

EXTRA="$*"
# The role session is bounded too, descendants included (its Agent-tool subagents, their cargo
# and hk serve runs): the HiGarfield cpulimit fork at ROLE_CPU_PCT (default 800 = 8 cores of the
# 28; the merge gate keeps its 14 and the work runner's workers their 12). No QoS clamp here - the
# role session is interactive and stays on P-cores; the ceiling alone is the bound. Without the
# fork binary the session runs unbounded and says so.
CPULIMIT_BIN="${HACKRIFF_OPS:-$HOME/.hackriff-ops}/bin/cpulimit"
[ -x "$CPULIMIT_BIN" ] || CPULIMIT_BIN="$(command -v cpulimit || true)"
if [ -n "$CPULIMIT_BIN" ] && "$CPULIMIT_BIN" --help 2>&1 | grep -q include-children; then
  CPUWRAP="$CPULIMIT_BIN -l ${ROLE_CPU_PCT:-800} -i --"
else
  echo "warning: no cpulimit fork found (see ops/README.md) - launching $ROLE unbounded" >&2
  CPUWRAP=""
fi
# Create a shell pane, then type the claude command. The role goes in BY FILE
# (--append-system-prompt-file): typing "$(cat ROLE)" into the shell parsed the role's own text -
# backticks, quotes, an `if` - and on 2026-09-22 left the pane stuck at zsh's `if>` continuation
# prompt for three hours, swallowing every merge-runner notice and supervisor relay.
#
# HACKRIFF_ROLE is exported into the session so ops/watchdog.py can NAME this session's owner.
# On macOS it cannot actually read it back (no process on this box can read another's
# environment - measured 2026-09-23), so the watchdog names the role from the role file in
# `--append-system-prompt-file` instead, which is why that argument is passed by path. The
# export costs nothing, is what the session says about itself rather than a guess about its
# command line, and is readable wherever this runs on Linux.
tmux new-session -d -s "$SESSION" -x 220 -y 60 -c "$REPO"
tmux send-keys -t "$SESSION" -l \
  "HACKRIFF_ROLE=$ROLE $CPUWRAP claude --model $MODEL --effort $EFFORT --dangerously-skip-permissions --append-system-prompt-file '$RF' $EXTRA"
tmux send-keys -t "$SESSION" Enter
echo "launched '$ROLE' in tmux session '$SESSION' (model=$MODEL effort=$EFFORT)"
echo "  role prompt: $RF  (+ root CLAUDE.md invariants)"
echo "  attach: tmux attach -t $SESSION"
