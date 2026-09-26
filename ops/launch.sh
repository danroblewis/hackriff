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
#   ops/launch.sh explorer --window 3h   # explorer window in tmux 'explore' (T-923): the agent from
#                                        # .claude/agents/explorer.md, run by ops/explorer-window.sh,
#                                        # which takes the radio lock and releases it at window end,
#                                        # on exit and on crash. `--dry-run` validates and prints.
#
# Any extra args after the role are passed straight to `claude` (e.g. --resume <id>).
set -euo pipefail
REPO=/Users/daniellewis/hackriff
ROLE="${1:?usage: ops/launch.sh <supervisor|coordinator|pipeline-manager|explorer> [extra claude args...]}"; shift || true
RF="$REPO/.claude/roles/$ROLE.md"
# The explorer is an agent definition, not a role (T-923); its window script runs it with `--agent`.
[ "$ROLE" = explorer ] && RF="$(cd "$(dirname "$0")/.." && pwd)/.claude/agents/explorer.md"
[ -f "$RF" ] || { echo "no role file: $RF"; exit 1; }

# The knob store (`just knobs`): a role session inherits it so anything it starts by hand reads the
# same values the runners do. It is PREFIXED ONTO THE TYPED COMMAND, like HACKRIFF_ROLE below -
# exporting it here reaches nothing: a tmux pane's environment comes from the tmux SERVER, not from
# the client that ran new-session (reviewed and measured 2026-09-23). A pre-set process variable
# is left out of the prefix, so the environment still wins over the store.
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
KNOBPREFIX=""
if [ -f "$S/env" ]; then
  while IFS='=' read -r k v || [ -n "$k" ]; do
    k="${k//$'\r'/}"; k="${k#"${k%%[![:space:]]*}"}"; k="${k%"${k##*[![:space:]]}"}"
    v="${v//$'\r'/}"; v="${v#"${v%%[![:space:]]*}"}"; v="${v%"${v##*[![:space:]]}"}"
    case "$k" in ''|'#'*) continue ;; esac
    [[ "$k" =~ ^[A-Z][A-Z0-9_]*$ ]] || continue
    [ -z "${!k+x}" ] && KNOBPREFIX="$KNOBPREFIX$k=$(printf '%q' "$v") "
  done < "$S/env"
fi

case "$ROLE" in
  coordinator)      SESSION=dev;   MODEL=opus; EFFORT=high ;;
  supervisor)       SESSION=super; MODEL=opus; EFFORT=high ;;
  # The pipeline manager (2026-09-23): owns throughput, ticks every 30 min, one instance (invariant 22).
  pipeline-manager) SESSION=flow;  MODEL=opus; EFFORT=high ;;
  # The explorer (T-923): one bounded window holding the radio lock, one instance.
  explorer)         SESSION=explore; MODEL=opus; EFFORT=high ;;
  *) echo "unknown role '$ROLE' (expected supervisor|coordinator|pipeline-manager|explorer)"; exit 1 ;;
esac

# The explorer's pane runs ops/explorer-window.sh, not claude directly: the window script owns the
# radio lock's take/release trap and the window-end timer, and starts claude itself. Its arguments
# are validated here (by the script's own --dry-run) before any tmux session exists.
PANE_CMD="claude --model $MODEL --effort $EFFORT --dangerously-skip-permissions --append-system-prompt-file '$RF'"
PANE_MATCH=claude
if [ "$ROLE" = explorer ]; then
  WINDOW=3h; DRY=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --window) WINDOW="${2:?--window needs a value (e.g. 3h)}"; shift 2 ;;
      --window=*) WINDOW="${1#--window=}"; shift ;;
      --dry-run) DRY=1; shift ;;
      *) echo "explorer: unknown argument '$1' (usage: ops/launch.sh explorer --window 3h [--dry-run])"; exit 1 ;;
    esac
  done
  if tmux has-session -t "$SESSION" 2>/dev/null; then
    echo "tmux session '$SESSION' already exists - one explorer at a time (attach, or wait for its window to end)."
    exit 1
  fi
  "$(dirname "$0")/explorer-window.sh" --window "$WINDOW" --dry-run || exit 1
  [ "$DRY" = 1 ] && exit 0
  PANE_CMD="bash '$REPO/ops/explorer-window.sh' --window '$WINDOW'"
  PANE_MATCH=explorer-window
fi

if tmux has-session -t "$SESSION" 2>/dev/null; then
  echo "tmux session '$SESSION' already exists — kill it first (tmux kill-session -t $SESSION) or attach."
  exit 1
fi

# The session id this launch runs as (user ask 2026-09-25 09:45): the dashboard names a role ONLY
# from $HACKRIFF_OPS/role-session/<role>, and decides it is live from a process carrying this id.
# A fresh launch picks the id itself (`--session-id`); a resume keeps the resumed id (`--resume
# <id>` / `-r <id>` reuse it), so that is what is written. `--continue` and a bare `-r` (the
# picker) resume an id this script cannot know: the file is left as it was, and it says so.
SID=""; RESUMING=0; PREV=""
for a in "$@"; do
  case "$PREV" in --resume|-r) case "$a" in -*) ;; *) SID="$a" ;; esac ;; esac
  case "$a" in
    --resume=*) RESUMING=1; SID="${a#--resume=}" ;;
    --resume|-r|--continue|-c) RESUMING=1 ;;
  esac
  PREV="$a"
done
if [ "$RESUMING" = 0 ]; then
  SID="$(uuidgen | tr '[:upper:]' '[:lower:]')"
  # the explorer's pane is ops/explorer-window.sh, which passes it on to the claude it starts
  PANE_CMD="$PANE_CMD --session-id $SID"
fi

EXTRA="$*"
# The role session is bounded too, descendants included (its Agent-tool subagents, their cargo
# and hk serve runs): the HiGarfield cpulimit fork at ROLE_CPU_PCT (default 800 = 8 cores of the
# 28; the merge gate keeps its 14 and the work runner's workers their 12). No QoS clamp here - the
# role session is interactive and stays on P-cores; the ceiling alone is the bound. Without the
# fork binary the session runs unbounded and says so.
#
# The limiter ATTACHES to the session by pid (`-p`), from outside the pane; it never WRAPS it.
# Wrapped (`cpulimit -i -- claude`, 2026-09-23), the fork runs its child in a new process group,
# which is not the pane's foreground group, so claude's first read of the terminal stopped it
# with SIGTTIN: state T for four minutes, no prompt, and SIGCONT did not hold (every read stops
# it again). Reproduced with a stdin reader: PGID != TPGID, state T. The session is instead
# `exec`ed into the pane - it IS the pane's process, in the foreground group - and the limiter is
# started below on the pane's pid (verified: a real session drew its prompt at `Ss+` with the
# limiter attached, and the limiter exits with its target). What it bounds: the session's
# DESCENDANTS - every Bash-tool shell runs in its own process group and the fork stops each pid,
# so subagent cargo/nextest/e2e runs are held to the ceiling. Not claude's own node process: tmux
# waits on its pane child with WUNTRACED and SIGCONTs a stopped pane group at once (reviewer,
# measured on tmux 3.6a), so node's CPU counts against the 800 % but is not throttled, and the
# children are throttled harder to make up for it. `remain-on-exit` keeps the pane readable if
# the exec fails or claude exits, instead of the session vanishing with its error.
CPULIMIT_BIN="${HACKRIFF_OPS:-$HOME/.hackriff-ops}/bin/cpulimit"
[ -x "$CPULIMIT_BIN" ] || CPULIMIT_BIN="$(command -v cpulimit || true)"
if [ -n "$CPULIMIT_BIN" ] && ! "$CPULIMIT_BIN" --help 2>&1 | grep -q include-children; then
  CPULIMIT_BIN=""
fi
[ -n "$CPULIMIT_BIN" ] || echo "warning: no cpulimit fork found (see ops/README.md) - launching $ROLE unbounded" >&2
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
tmux set-option -t "$SESSION" remain-on-exit on >/dev/null
tmux send-keys -t "$SESSION" -l \
  "exec env HACKRIFF_ROLE=$ROLE $KNOBPREFIX $PANE_CMD $EXTRA"
tmux send-keys -t "$SESSION" Enter
if [ -n "$SID" ]; then
  mkdir -p "$S/role-session"
  printf '%s\n' "$SID" > "$S/role-session/$ROLE"
  # coordinator-session is the older pointer (ops/worklog.py seeds its registry from it)
  [ "$ROLE" = coordinator ] && printf '%s\n' "$SID" > "$S/coordinator-session"
  echo "  session id: $SID  ($S/role-session/$ROLE)"
else
  echo "warning: resumed with no session id on the command line - $S/role-session/$ROLE left as it was" >&2
fi
if [ -n "$CPULIMIT_BIN" ]; then
  # After the exec the pane's pid is claude itself; wait for that before attaching, so a launch
  # that never reached claude says so instead of claiming a bound.
  PANE_PID="$(tmux display -p -t "$SESSION" '#{pane_pid}')"
  for _ in $(seq 1 40); do
    ps -o command= -p "$PANE_PID" 2>/dev/null | grep -q "$PANE_MATCH" && break
    sleep 0.25
  done
  if ps -o command= -p "$PANE_PID" 2>/dev/null | grep -q "$PANE_MATCH"; then
    nohup "$CPULIMIT_BIN" -l "${ROLE_CPU_PCT:-800}" -i -p "$PANE_PID" >/dev/null 2>&1 &
    disown
    echo "  bound: cpulimit -l ${ROLE_CPU_PCT:-800} -i -p $PANE_PID (limiter pid $!)"
  else
    echo "warning: pane $PANE_PID is not running $PANE_MATCH after 10 s - $ROLE launched unbounded" >&2
  fi
fi
echo "launched '$ROLE' in tmux session '$SESSION' (model=$MODEL effort=$EFFORT)"
echo "  role prompt: $RF  (+ root CLAUDE.md invariants)"
echo "  attach: tmux attach -t $SESSION"
