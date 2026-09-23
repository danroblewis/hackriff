#!/bin/bash
# Stop / SubagentStop hook: kill the busy shells an agent leaves behind when it exits.
#
# 2026-09-22: a deflaker agent finished and left SIXTEEN
#   /bin/zsh -c source /Users/daniellewis/.claude/shell-snapshots/snapshot-zsh-….sh …
# loops behind, reparented to launchd (ppid 1), each pinned at 100 % CPU, running from 14:29 to
# 16:47 — through every merge gate in those two and a quarter hours. Nothing noticed: the agent
# was gone, and no runner tracks a process it did not start.
#
# ops/watchdog.py catches this from outside, but only after ten minutes and only if it is
# running. This is the cheap half: the moment an agent stops, sweep its own leftovers. The
# agent that made the mess is the one process that is certainly around to clean it up.
#
# WHAT IT KILLS, and why each condition is needed:
#   * `shell-snapshots/snapshot-zsh` in the command line — the signature of an agent's Bash-tool
#     shell, and nothing else on this box. Never a general "high CPU" sweep.
#   * ppid 1, or a parent that is no longer alive — the shell has been ORPHANED. A live agent's
#     shell (including every other agent running right now, and this session's own) has a live
#     parent and is never touched. This is the condition that makes the kill safe.
#   * >50 % CPU — an orphan sitting idle harms nobody and will be reaped by the OS; a spinning
#     one is what ran through two hours of gates.
#
# FAIL-OPEN, always: a hook that can fail an agent's turn is worse than the leftovers. Every
# error path exits 0 with no output. Exit 0 + a `systemMessage` is informational — it records
# the reap in the transcript, so the next person to read it knows what happened and why.
set -u
# `${HOME:-/tmp}`, not `$HOME`: with `set -u` an unset HOME makes this line itself exit 1, which
# is a FAILING hook - the one outcome this script must never have. (Caught by test_hooks.py.)
OPS="${HACKRIFF_OPS:-${HOME:-/tmp}/.hackriff-ops}"

{
  cat >/dev/null 2>&1        # drain the hook's JSON on stdin; nothing here needs it

  ORPHANS=""
  while read -r pid ppid cpu; do
    [ -z "${pid:-}" ] && continue
    # Orphaned? ppid 1 is launchd having adopted it; otherwise ask whether the parent still exists.
    if [ "$ppid" != "1" ]; then
      ps -p "$ppid" >/dev/null 2>&1 && continue
    fi
    ORPHANS="$ORPHANS $pid"
  done <<EOF
$(ps -axo pid=,ppid=,pcpu=,command= 2>/dev/null |
  awk 'index($0, "shell-snapshots/snapshot-zsh") > 0 && ($3 + 0) > 50 { print $1, $2, $3 }')
EOF

  ORPHANS="${ORPHANS# }"
  [ -z "$ORPHANS" ] && exit 0

  KILLED=""
  for pid in $ORPHANS; do
    # Re-check the signature against this exact pid immediately before signalling: pids are
    # recycled, and the gap between the scan and the kill is where that bites.
    ps -p "$pid" -o command= 2>/dev/null | grep -q 'shell-snapshots/snapshot-zsh' || continue
    kill -9 "$pid" 2>/dev/null && KILLED="$KILLED $pid"
  done
  KILLED="${KILLED# }"
  [ -z "$KILLED" ] && exit 0

  N=$(printf '%s\n' $KILLED | wc -l | tr -d ' ')
  mkdir -p "$OPS" 2>/dev/null
  printf '%s REAP (%s hook) killed %s orphaned shell loop(s): %s\n' \
    "$(date '+%Y-%m-%d %H:%M:%S')" "${CLAUDE_HOOK_EVENT:-stop}" "$N" "$KILLED" >> "$OPS/watchdog.log" 2>/dev/null

  MSG="reaped $N orphaned shell loops: $KILLED"
  printf '{"systemMessage":%s}\n' "$(printf '%s' "$MSG" | sed 's/\\/\\\\/g; s/"/\\"/g; s/^/"/; s/$/"/')"
} 2>/dev/null
exit 0
