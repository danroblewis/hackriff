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
# Create a shell pane, then type the claude command so the shell expands "$(cat ROLE)"
# into a single --append-system-prompt argument (robust against the role file's newlines/quotes).
tmux new-session -d -s "$SESSION" -x 220 -y 60 -c "$REPO"
tmux send-keys -t "$SESSION" -l \
  "claude --model $MODEL --effort $EFFORT --dangerously-skip-permissions --append-system-prompt \"\$(cat '$RF')\" $EXTRA"
tmux send-keys -t "$SESSION" Enter
echo "launched '$ROLE' in tmux session '$SESSION' (model=$MODEL effort=$EFFORT)"
echo "  role prompt: $RF  (+ root CLAUDE.md invariants)"
echo "  attach: tmux attach -t $SESSION"
