#!/usr/bin/env python3
"""Pipeline alerts to Discord (user, 2026-09-23: "alert me somehow when there are issues").

    python3 ops/alert.py <level> "<title>" ["<body>"] [--key K]
    from ops.alert import notify; notify("red", "gate failed", "...", key="gate:task-x")

Levels: red (a person should look), amber (degraded, self-healing), green (landed / recovered),
info. Configuration lives OUTSIDE the repo in $HACKRIFF_OPS/discord.json (mode 600):
{"token": "...", "channel_id": "...", "guild_id": "..."} - the token never appears in a commit.
`HK_ALERT_CHANNEL` overrides the channel; `HK_ALERT_OFF=1` silences everything (tests).

Dedupe: an alert with the same `key` inside DEDUPE_S is dropped, so a runner looping on one
condition posts once, not every tick. Every attempt - sent, deduped, failed - is appended to
$HACKRIFF_OPS/alerts.jsonl, which is the record the dashboard reads; a failed post never raises
into the caller (an alert path must not be able to take the pipeline down).
"""
from __future__ import annotations

import json
import os
import sys
import time
import urllib.request

S = os.environ.get("HACKRIFF_OPS") or os.path.expanduser("~/.hackriff-ops")
CFG = os.path.join(S, "discord.json")
LOG = os.path.join(S, "alerts.jsonl")
DEDUPE_S = 30 * 60
ICON = {"red": "🔴", "amber": "🟠", "green": "🟢", "info": "🔵"}


def _config() -> dict:
    try:
        return json.load(open(CFG))
    except Exception:
        return {}


def _recent(key: str) -> bool:
    if not key:
        return False
    try:
        cut = time.time() - DEDUPE_S
        with open(LOG) as f:
            for line in f.readlines()[-500:]:
                o = json.loads(line)
                if o.get("key") == key and o.get("ts", 0) > cut and o.get("status") == "sent":
                    return True
    except Exception:
        pass
    return False


def _record(rec: dict) -> None:
    try:
        os.makedirs(S, exist_ok=True)
        with open(LOG, "a") as f:
            f.write(json.dumps(rec) + "\n")
    except Exception:
        pass


def notify(level: str, title: str, body: str = "", key: str | None = None) -> bool:
    """Post one message; return True when Discord accepted it. Never raises."""
    rec = {"ts": time.time(), "level": level, "title": title[:200], "key": key, "status": ""}
    if os.environ.get("HK_ALERT_OFF") == "1":
        rec["status"] = "off"; _record(rec); return False
    if _recent(key or ""):
        rec["status"] = "deduped"; _record(rec); return False
    cfg = _config()
    tok, chan = cfg.get("token"), os.environ.get("HK_ALERT_CHANNEL") or cfg.get("channel_id")
    if not tok or not chan:
        rec["status"] = "unconfigured"; _record(rec); return False
    text = f"{ICON.get(level, '•')} **hackriff · {title}**"
    if body:
        text += "\n" + body[:1700]
    req = urllib.request.Request(
        f"https://discord.com/api/v10/channels/{chan}/messages",
        data=json.dumps({"content": text}).encode(),
        headers={"Authorization": "Bot " + tok, "Content-Type": "application/json", "User-Agent": "hackriff-ops/1"},
        method="POST",
    )
    try:
        urllib.request.urlopen(req, timeout=15).read()
        rec["status"] = "sent"; _record(rec); return True
    except Exception as e:  # rate limit, network, revoked token - all non-fatal
        rec["status"] = f"failed: {str(e)[:120]}"; _record(rec); return False


def main(argv: list[str]) -> int:
    key = None
    if "--key" in argv:
        i = argv.index("--key"); key = argv[i + 1]; argv = argv[:i] + argv[i + 2:]
    if len(argv) < 2:
        print("usage: alert.py <red|amber|green|info> <title> [body] [--key K]", file=sys.stderr); return 2
    level, title = argv[0], argv[1]
    body = argv[2] if len(argv) > 2 else ""
    return 0 if notify(level, title, body, key=key) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
