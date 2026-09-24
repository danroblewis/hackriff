#!/bin/bash
# PreToolUse(Bash) hook: block full gate/acceptance/workspace-test in SUBAGENTS.
# Rationale: CLAUDE.md — worker agents run targeted tests and HAND BACK; only the
# coordinator gates. /perf metrics showed ~23h of agent time on suites they must not run.
# FAIL-OPEN: any error/unexpected input -> allow (exit 0), so a hook bug never blocks work.
INPUT=$(cat 2>/dev/null) || exit 0
AGENT_ID=$(printf '%s' "$INPUT" | jq -r '.agent_id // empty' 2>/dev/null) || exit 0
CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
CMD=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)
# What would RUN, not the text the command carries (cmd_code.py: heredoc bodies and the quoted
# argument of tmux send-keys / echo / printf / -m stripped). Every pattern below reads CODE. On any
# error CODE is the raw command - stricter, never looser.
CODE=$(printf '%s' "$CMD" | python3 "$(dirname "$0")/cmd_code.py" 2>/dev/null) || CODE="$CMD"
[ -n "$CODE" ] || CODE="$CMD"

deny(){ printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$1" | jq -Rs .)"; exit 0; }

# ---------------------------------------------------------------------------------------------
# MAIN COMMIT GUARD - every session. A commit in the MAIN checkout while the merge runner has a merge
# staged (.git/MERGE_HEAD) or a batch provisionally on main (bulk-in-progress) completes or corrupts
# the runner's merge: 2026-09-22 ea91c27c (a 4-of-44-file "merge"), and 2026-09-24 15:05:04 a board
# note that became a merge commit of task-t899, which LANDED UNGATED. The runner itself is a script,
# not a session, and is never stopped by this. No override: wait for the runner (`just wait-for-gate`).
# ---------------------------------------------------------------------------------------------
if [ -n "$CMD" ] && printf '%s' "$CODE" | grep -Eq '(^|[;&|(]|[[:space:]])git([[:space:]]+(-[Cc][[:space:]]+[^[:space:]]+|--?[A-Za-z][A-Za-z-]*(=[^[:space:]]*)?))*[[:space:]]+(commit|merge|cherry-pick|revert|am|rebase)([[:space:]]|$)'; then
  TGT=$(printf '%s' "$CODE" | sed -nE 's/.*git[[:space:]]+-C[[:space:]]+([^[:space:];&|]+).*/\1/p' | head -1)
  TGT=${TGT:-$CWD}
  TGT=${TGT/#\~/$HOME}
  TOP=$(git -C "$TGT" rev-parse --show-toplevel 2>/dev/null)
  # The main checkout is the one whose .git is a DIRECTORY; a linked worktree's .git is a file.
  if [ -n "$TOP" ] && [ -d "$TOP/.git" ]; then
    OPS="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
    WHY=""
    [ -e "$TOP/.git/MERGE_HEAD" ] && WHY="the merge runner has a merge STAGED in $TOP (.git/MERGE_HEAD)"
    [ -z "$WHY" ] && [ -e "$OPS/bulk-in-progress" ] && WHY="a batch is provisionally merged on main and gating ($OPS/bulk-in-progress)"
    [ -n "$WHY" ] && deny "Not now: $WHY. A commit in main's checkout now becomes part of the runner's merge - on 2026-09-24 15:05 a board note turned into a merge of task-t899 and landed it UNGATED. Wait until the gate ends (\`just wait-for-gate\`, or until .git/MERGE_HEAD and bulk-in-progress are gone), then edit -> check -> commit in one step. Or put the change on a branch and queue it."
  fi
fi

# ---------------------------------------------------------------------------------------------
# LOAD GUARD - applies to EVERY session, coordinator included, because contention is contention.
# On 2026-09-22 a deflaker agent left sixteen `while :; do :; done` shells at 100 % CPU running
# from 14:29 to 16:47, through every merge gate; the work runner's cpulimit bound does not apply
# to a shell whose agent has exited, and no runner tracks a process it did not start. The cheapest
# place to stop that is before it is typed. `HK_ALLOW_LOAD=1` is the override for the one case
# this is wrong about - deliberately loading the box to reproduce a contention bug.
# ---------------------------------------------------------------------------------------------
if [ -n "$CMD" ] && [ "${HK_ALLOW_LOAD:-0}" != "1" ]; then
  # A spin loop with a no-op body. `while :; do sleep 5; …; done` (polling, the documented way to
  # wait on a condition) is NOT matched: the body has to be `:` or `true` for this to fire.
  if printf '%s' "$CODE" | grep -Eq 'while[[:space:]]+(:|true)[[:space:]]*;[[:space:]]*do[[:space:]]*(:|true)[[:space:]]*;[[:space:]]*done'; then
    deny "That is a busy loop (\`while :; do :; done\`), which pins a core for as long as it runs and survives the agent that started it — sixteen of them ran through every merge gate for 2 h 18 m on 2026-09-22. To WAIT on something, poll with a sleep in the body (\`while :; do sleep 5; check; done\`) or use the Monitor tool. To load the box on purpose, HK_ALLOW_LOAD=1."
  fi
  if printf '%s' "$CODE" | grep -Eq '(^|[;&|(]|[[:space:]])yes[[:space:]]*(\||>)' ; then
    deny "\`yes\` burns a whole core producing output nobody reads. If you need a stream of input, use a file or \`head -c\`. HK_ALLOW_LOAD=1 to override."
  fi
  if printf '%s' "$CODE" | grep -Eq '(^|[;&|(]|[[:space:]])(stress|stress-ng)([[:space:]]|$)'; then
    deny "\`stress\`/\`stress-ng\` loads the box deliberately, and this box runs a merge gate with a 14-core reserve plus up to four bounded workers. If you are reproducing a contention bug, say so with HK_ALLOW_LOAD=1 — and tell the coordinator, so a gate is not blamed for it."
  fi
  # More than two backgrounded loops in one command: the shape that produced the sixteen.
  if printf '%s' "$CODE" | grep -Eq '(while|for|until)[[:space:]]'; then
    NBG=$(printf '%s' "$CODE" | grep -o '&' | wc -l | tr -d ' ')
    NAND=$(printf '%s' "$CODE" | grep -o '&&' | wc -l | tr -d ' ')
    NRED=$(printf '%s' "$CODE" | grep -oE '[0-9]?>&[0-9]' | wc -l | tr -d ' ')
    BG=$(( NBG - 2 * NAND - NRED ))
    if [ "$BG" -gt 2 ]; then
      deny "$BG backgrounded loops in one command. Each one outlives this tool call and, if this agent exits, is reparented to launchd where nothing bounds it — that is exactly the 2026-09-22 failure. Run them one at a time, or with HK_ALLOW_LOAD=1 if you truly need them concurrent."
    fi
  fi
  # A browser-spec run or a test server beside the gate's own browser tier: they share ports and
  # the cores the gate reserved, and on 2026-09-22 they turned three green specs red in two
  # different gates (ops/merge-runner.sh counts them as workers for the same reason).
  if printf '%s' "$CODE" | grep -Eq 'npm run e2e|node e2e/run\.mjs|(^|[;&|(]|[[:space:]])hk serve|target/[a-z]*/hk serve'; then
    OPS="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
    WHY=""
    [ -e "$OPS/bulk-in-progress" ] && WHY="a bulk merge is in progress ($OPS/bulk-in-progress)"
    # Anchored: an agent's own `pgrep -f "just gate"` wait loop carries the string in its argv and
    # would count as a running gate forever (2026-09-23, T-846). The real gate's argv starts with it.
    if [ -z "$WHY" ] && pgrep -f '^just gate' >/dev/null 2>&1; then WHY="a merge gate is running right now"; fi
    # A run on its OWN ports may go beside the gate (user lead, 2026-09-23): the 2026-09-22 reds
    # were PORT sharing, and ports are per-run now (HK_E2E_PORT lanes; every spec derives its port
    # since task-e2e-lane-ports). The gate's lanes sit at 8791 / 8951 / 8983 with a 24-port sweep
    # each (9 lanes still end below 9216), so a base >= 9216 cannot reach them - cmd_code.py --ports
    # decides, over EVERY port the command names; CPU is already bounded (the worker's cpulimit, the
    # gate's reserve). The work runner gives each worker such a base (ops/work-runner.py
    # e2e_port_for). Measured cost of the old rule: 46 wait-for-gate calls, 250 worker-minutes on
    # 2026-09-23, one worker (T-845) parked 2 h+ because back-to-back gates never left a gap.
    if [ -n "$WHY" ] && printf '%s' "$CMD" | python3 "$(dirname "$0")/cmd_code.py" --ports 2>/dev/null; then WHY=""; fi
    [ -n "$WHY" ] && deny "Not while $WHY. A spec run or an \`hk serve\` on the gate's ports (below 9216) shares its browser tier's ports and turns green specs red - three of them in two gates on 2026-09-22. Run on your own ports instead: HK_E2E_PORT=<9216 or above> HK_E2E_JOURNEY_PORT=<9216 or above> npm run e2e ... (a work-runner worker already has both in its environment), or \`hk serve --bind 127.0.0.1:<9216+>\`. Or wait with \`just wait-for-gate\`, or HK_ALLOW_LOAD=1 if you accept both results being untrustworthy."
  fi
fi

# Is this a subagent? agent_id present OR cwd inside a worktree. Coordinator/main = neither.
IS_SUB=0
[ -n "$AGENT_ID" ] && IS_SUB=1
case "$CWD" in */.claude/worktrees/*) IS_SUB=1 ;; esac
[ "$IS_SUB" = "0" ] && exit 0                       # coordinator/main -> allow everything
[ "${HK_ALLOW_FULL:-0}" = "1" ] && exit 0           # explicit override for debug/repro agents

# Blocked full-suite patterns (match anywhere, so 'cd wt && just gate' / 'bash -c' are caught).
# 'just test' matches only the full recipe, NOT test-crate / test-one.
if printf '%s' "$CODE" | grep -Eq 'just gate([[:space:]]|-|$)|just acceptance|just test([[:space:]]|$)|cargo (nextest run|test)[^|]*--workspace'; then
  deny "Full-suite gate/acceptance/workspace-test is blocked in worker subagents. Run targeted tests only — 'just test-crate <crate>' or 'just test-one <name>' — then HAND BACK; the coordinator runs the gate at merge. Override with HK_ALLOW_FULL=1 only for a genuine debug/repro agent."
fi

# docs/tasks.yaml is edited through `just task ...` only (user, 2026-09-22): a redirect or
# `sed -i` in a Bash command is the same forbidden direct edit as an Edit/Write tool call
# (blocked separately by block-board-edits.sh), just spelled as a shell command instead.
if printf '%s' "$CODE" | grep -Eq '(>|>>|sed -i[^|]*|tee )[^|]*docs/tasks\.yaml'; then
  deny "docs/tasks.yaml is edited through the task CLI only: \`just task show|list|set|result|note|new|validate\` (py/hkpy/tasks.py). Direct edits broke the board's YAML twice on 2026-09-22."
fi
exit 0
