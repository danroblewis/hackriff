#!/bin/bash
# PreToolUse(Edit|Write|MultiEdit) hook: block direct edits to docs/tasks.yaml in worktrees.
# Rationale: user directive 2026-09-22 ("they should actually be banned from editing tasks.yaml
# directly") — direct hand-edits broke the board's strict YAML twice that day (T-640's unquoted
# ": " turned a title into a nested mapping; a lost `- id:` line merged two tickets). The task CLI
# (py/hkpy/tasks.py, `just task show|list|set|result|note|new|validate`) edits one ticket's block
# textually and re-validates every write, restoring the original bytes on any failure.
# FAIL-OPEN: any error/unexpected input -> allow (exit 0), same style as block-full-gate.sh.
INPUT=$(cat 2>/dev/null) || exit 0
AGENT_ID=$(printf '%s' "$INPUT" | jq -r '.agent_id // empty' 2>/dev/null) || exit 0
CWD=$(printf '%s' "$INPUT" | jq -r '.cwd // empty' 2>/dev/null)
FILE=$(printf '%s' "$INPUT" | jq -r '.tool_input.file_path // empty' 2>/dev/null)

# Is this a subagent? agent_id present OR cwd inside a worktree. Coordinator/main = neither.
IS_SUB=0
[ -n "$AGENT_ID" ] && IS_SUB=1
case "$CWD" in */.claude/worktrees/*) IS_SUB=1 ;; esac
[ "$IS_SUB" = "0" ] && exit 0                       # coordinator/main -> allow everything
[ "${HK_ALLOW_FULL:-0}" = "1" ] && exit 0           # explicit override for debug/repro agents

case "$FILE" in
  */docs/tasks.yaml|docs/tasks.yaml)
    R="docs/tasks.yaml is edited through the task CLI only: \`just task show|list|set|result|note|new|validate\` (py/hkpy/tasks.py). Direct edits broke the board's YAML twice on 2026-09-22."
    printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":%s}}\n' "$(printf '%s' "$R" | jq -Rs .)"
    exit 0
    ;;
esac
exit 0
