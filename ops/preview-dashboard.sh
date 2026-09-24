#!/usr/bin/env bash
# Hot-deploy a PREVIEW of a branch's dashboard (user, 2026-09-24): "he does not want to wait behind a
# merge batch to SEE a dashboard change". Copies the branch's committed ops/ and py/ out of git (never
# a worktree, never main's working tree) into $PREVIEW_DIR, runs that copy's monitor.py on :8902
# beside the real :8901 (MONITOR_PREVIEW=1: its child builds run the copy's code, it keeps its own
# metrics cache and writes no daily sample), and checks each PATH returns 200 with a body. The real
# :8901 still restarts from main when the branch lands. One preview at a time: a new one replaces it.
#
#   bash ops/preview-dashboard.sh <branch> [PATH ...]     e.g. task-pm-metrics /metrics /metrics.json
set -uo pipefail
REPO=/Users/daniellewis/hackriff
S="${HACKRIFF_OPS:-$HOME/.hackriff-ops}"
DIR="${PREVIEW_DIR:-$S/preview}"
PORT="${PREVIEW_PORT:-8902}"
branch="${1:?usage: preview-dashboard.sh <branch> [PATH ...]}"; shift
[ "$PORT" = 8901 ] && { echo "refusing :8901 - that is the real dashboard"; exit 2; }
git -C "$REPO" rev-parse -q --verify "$branch^{commit}" >/dev/null || { echo "no such branch: $branch"; exit 2; }
# One preview slot: whatever dashboard holds the port is the previous preview (a tunnel may point at
# the port, so the newest preview is what it shows). Anything else holding it is refused, not killed.
for p in $(lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null); do
  cmd=$(ps -o command= -p "$p")
  case "$cmd" in
    *ops/monitor.py*) echo "replacing the preview on :$PORT (pid $p: ${cmd:0:90})"; kill "$p" ;;
    *) echo "refusing: :$PORT is held by pid $p (${cmd:0:90}), which is not a dashboard"; exit 2 ;;
  esac
done
for i in $(seq 1 20); do lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t >/dev/null 2>&1 || break; sleep 0.5; done
rm -rf "$DIR"; mkdir -p "$DIR"
git -C "$REPO" archive "$branch" ops py | tar -x -C "$DIR" || { echo "git archive failed"; exit 1; }
MONITOR_PREVIEW=1 MONITOR_PORT="$PORT" HACKRIFF_OPS="$S" nohup python3 "$DIR/ops/monitor.py" >"$DIR/monitor.log" 2>&1 &
pid=$!
echo "preview of $branch ($(git -C "$REPO" rev-parse --short "$branch")) from $DIR, pid $pid, on :$PORT"
for i in $(seq 1 30); do lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | grep -qx "$pid" && break; sleep 1; done
# Verify OUR process serves the port - a check against someone else's instance is a false pass.
lsof -nP -iTCP:"$PORT" -sTCP:LISTEN -t 2>/dev/null | grep -qx "$pid" || { echo "FAIL: pid $pid is not listening on :$PORT - $(tail -3 "$DIR/monitor.log")"; exit 1; }
rc=0
for p in "${@:-/}"; do
  out=$(curl -s -m 120 -o "$DIR/check.out" -w '%{http_code} %{size_download}' "http://127.0.0.1:$PORT$p")
  code=${out% *}; size=${out#* }
  bad=""; case "$p" in *.json*) grep -q '"error"' "$DIR/check.out" && bad=1 ;; esac   # a JSON route's own error
  if [ "$code" = 200 ] && [ "$size" -gt 0 ] && [ -z "$bad" ]; then
    echo "OK   http://127.0.0.1:$PORT$p  ($size bytes)"
  else
    echo "FAIL http://127.0.0.1:$PORT$p  (HTTP $code, $size bytes: $(head -c 160 "$DIR/check.out"))"; rc=1
  fi
done
exit $rc
