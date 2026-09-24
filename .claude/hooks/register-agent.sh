#!/bin/bash
# PreToolUse(Agent) hook: record every subagent spawn in $HACKRIFF_OPS/agent-registry.jsonl.
#
# The orchestrator knows which ticket an agent is for at the moment it is launched (user,
# 2026-09-23: "there is probably a more straightforward way to know which task is associated
# with an agent"); the dashboard used to re-derive it from transcript text and failed for
# agents whose prompt did not lead with a ticket id. This writes {ts, session, cwd, ticket,
# type, description, prompt_head} at spawn time — the record the dashboard, the work runner's
# board sync and the watchdog read. Always allows the call (exit 0); FAIL-OPEN on any error.
INPUT=$(cat 2>/dev/null) || exit 0
command -v jq >/dev/null 2>&1 || exit 0
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
printf '%s' "$INPUT" | jq -c --arg s "$S" '
  (.tool_input.prompt // "") as $p
  | {ts: (now|floor),
     session: (.session_id // ""),
     cwd: (.cwd // ""),
     type: (.tool_input.subagent_type // "general-purpose"),
     description: (.tool_input.description // ""),
     ticket: (($p | capture("(?<t>T-[0-9]{2,4})") | .t) // (.cwd // "" | capture("t(?<n>[0-9]{2,4})(/|$)") | "T-" + .n) // null),
     prompt_head: ($p | .[0:240])}' >> "$S/agent-registry.jsonl" 2>/dev/null
exit 0
