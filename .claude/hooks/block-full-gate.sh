#!/bin/bash
# PreToolUse(Bash) hook: block full gate/acceptance/workspace-test in SUBAGENTS.
# Rationale: CLAUDE.md — worker agents run targeted tests and HAND BACK; only the
# coordinator gates. /perf metrics showed ~23h of agent time on suites they must not run.
# FAIL-OPEN: any error/unexpected input -> allow (exit 0), so a hook bug never blocks work.
INPUT=$(cat 2>/dev/null) || exit 0
AGENT_ID=$(printf '%s' "$INPUT" | jq -r '.agent_id // empty' 2>/dev/null) || exit 0
CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
CMD=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

deny(){ printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$1" | jq -Rs .)"; exit 0; }

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
  if printf '%s' "$CMD" | grep -Eq 'while[[:space:]]+(:|true)[[:space:]]*;[[:space:]]*do[[:space:]]*(:|true)[[:space:]]*;[[:space:]]*done'; then
    deny "That is a busy loop (\`while :; do :; done\`), which pins a core for as long as it runs and survives the agent that started it — sixteen of them ran through every merge gate for 2 h 18 m on 2026-09-22. To WAIT on something, poll with a sleep in the body (\`while :; do sleep 5; check; done\`) or use the Monitor tool. To load the box on purpose, HK_ALLOW_LOAD=1."
  fi
  if printf '%s' "$CMD" | grep -Eq '(^|[;&|(]|[[:space:]])yes[[:space:]]*(\||>)' ; then
    deny "\`yes\` burns a whole core producing output nobody reads. If you need a stream of input, use a file or \`head -c\`. HK_ALLOW_LOAD=1 to override."
  fi
  if printf '%s' "$CMD" | grep -Eq '(^|[;&|(]|[[:space:]])(stress|stress-ng)([[:space:]]|$)'; then
    deny "\`stress\`/\`stress-ng\` loads the box deliberately, and this box runs a merge gate with a 14-core reserve plus up to four bounded workers. If you are reproducing a contention bug, say so with HK_ALLOW_LOAD=1 — and tell the coordinator, so a gate is not blamed for it."
  fi
  # More than two backgrounded loops in one command: the shape that produced the sixteen.
  if printf '%s' "$CMD" | grep -Eq '(while|for|until)[[:space:]]'; then
    NBG=$(printf '%s' "$CMD" | grep -o '&' | wc -l | tr -d ' ')
    NAND=$(printf '%s' "$CMD" | grep -o '&&' | wc -l | tr -d ' ')
    NRED=$(printf '%s' "$CMD" | grep -oE '[0-9]?>&[0-9]' | wc -l | tr -d ' ')
    BG=$(( NBG - 2 * NAND - NRED ))
    if [ "$BG" -gt 2 ]; then
      deny "$BG backgrounded loops in one command. Each one outlives this tool call and, if this agent exits, is reparented to launchd where nothing bounds it — that is exactly the 2026-09-22 failure. Run them one at a time, or with HK_ALLOW_LOAD=1 if you truly need them concurrent."
    fi
  fi
  # A browser-spec run or a test server beside the gate's own browser tier: they share ports and
  # the cores the gate reserved, and on 2026-09-22 they turned three green specs red in two
  # different gates (ops/merge-runner.sh counts them as workers for the same reason).
  if printf '%s' "$CMD" | grep -Eq 'npm run e2e|node e2e/run\.mjs|(^|[;&|(]|[[:space:]])hk serve|target/[a-z]*/hk serve'; then
    OPS="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
    WHY=""
    [ -e "$OPS/bulk-in-progress" ] && WHY="a bulk merge is in progress ($OPS/bulk-in-progress)"
    if [ -z "$WHY" ] && pgrep -f 'just gate' >/dev/null 2>&1; then WHY="a merge gate is running right now"; fi
    [ -n "$WHY" ] && deny "Not while $WHY. A spec run or an \`hk serve\` beside the gate's browser tier shares its ports and its reserved cores, and turns green specs red — three of them in two gates on 2026-09-22. Wait for it with \`just wait-for-gate\` (blocks until the gate ends AND tells the merge runner you are idle, so it does not wait 45 min for you in turn - never a hand-rolled sleep loop), or HK_ALLOW_LOAD=1 if you accept both results being untrustworthy."
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
if printf '%s' "$CMD" | grep -Eq 'just gate([[:space:]]|-|$)|just acceptance|just test([[:space:]]|$)|cargo (nextest run|test)[^|]*--workspace'; then
  deny "Full-suite gate/acceptance/workspace-test is blocked in worker subagents. Run targeted tests only — 'just test-crate <crate>' or 'just test-one <name>' — then HAND BACK; the coordinator runs the gate at merge. Override with HK_ALLOW_FULL=1 only for a genuine debug/repro agent."
fi

# docs/tasks.yaml is edited through `just task ...` only (user, 2026-09-22): a redirect or
# `sed -i` in a Bash command is the same forbidden direct edit as an Edit/Write tool call
# (blocked separately by block-board-edits.sh), just spelled as a shell command instead.
if printf '%s' "$CMD" | grep -Eq '(>|>>|sed -i[^|]*|tee )[^|]*docs/tasks\.yaml'; then
  deny "docs/tasks.yaml is edited through the task CLI only: \`just task show|list|set|result|note|new|validate\` (py/hkpy/tasks.py). Direct edits broke the board's YAML twice on 2026-09-22."
fi
exit 0
