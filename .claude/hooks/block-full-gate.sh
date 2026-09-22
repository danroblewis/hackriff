#!/bin/bash
# PreToolUse(Bash) hook: block full gate/acceptance/workspace-test in SUBAGENTS.
# Rationale: CLAUDE.md — worker agents run targeted tests and HAND BACK; only the
# coordinator gates. /perf metrics showed ~23h of agent time on suites they must not run.
# FAIL-OPEN: any error/unexpected input -> allow (exit 0), so a hook bug never blocks work.
INPUT=$(cat 2>/dev/null) || exit 0
AGENT_ID=$(printf '%s' "$INPUT" | jq -r '.agent_id // empty' 2>/dev/null) || exit 0
CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
CMD=$(printf '%s' "$INPUT" | jq -r '.tool_input.command // empty' 2>/dev/null)

# Is this a subagent? agent_id present OR cwd inside a worktree. Coordinator/main = neither.
IS_SUB=0
[ -n "$AGENT_ID" ] && IS_SUB=1
case "$CWD" in */.claude/worktrees/*) IS_SUB=1 ;; esac
[ "$IS_SUB" = "0" ] && exit 0                       # coordinator/main -> allow everything
[ "${HK_ALLOW_FULL:-0}" = "1" ] && exit 0           # explicit override for debug/repro agents

# Blocked full-suite patterns (match anywhere, so 'cd wt && just gate' / 'bash -c' are caught).
# 'just test' matches only the full recipe, NOT test-crate / test-one.
if printf '%s' "$CMD" | grep -Eq 'just gate([[:space:]]|-|$)|just acceptance|just test([[:space:]]|$)|cargo (nextest run|test)[^|]*--workspace'; then
  R="Full-suite gate/acceptance/workspace-test is blocked in worker subagents. Run targeted tests only — 'just test-crate <crate>' or 'just test-one <name>' — then HAND BACK; the coordinator runs the gate at merge. Override with HK_ALLOW_FULL=1 only for a genuine debug/repro agent."
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$R" | jq -Rs .)"
  exit 0
fi

# docs/tasks.yaml is edited through `just task ...` only (user, 2026-09-22): a redirect or
# `sed -i` in a Bash command is the same forbidden direct edit as an Edit/Write tool call
# (blocked separately by block-board-edits.sh), just spelled as a shell command instead.
if printf '%s' "$CMD" | grep -Eq '(>|>>|sed -i[^|]*|tee )[^|]*docs/tasks\.yaml'; then
  R="docs/tasks.yaml is edited through the task CLI only: \`just task show|list|set|result|note|new|validate\` (py/hkpy/tasks.py). Direct edits broke the board's YAML twice on 2026-09-22."
  printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$R" | jq -Rs .)"
  exit 0
fi
exit 0
