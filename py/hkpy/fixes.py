"""Why tickets get handed back for a fix run (user, 2026-09-24) - read-only over the ops files.

The work runner resumes a ticket's own worker (`claude -p --resume`) when its branch fails review,
fails its merge gate, conflicts with main or is left uncommitted. Until now the REASON lived only
in the claim's fail_line (overwritten when the ticket merges) and in work/<T>/fix<n>.json. Now:

  * the runner's `FIX T-nnn attempt n [CLASS] <reason>` log line and the fix run's work-done.jsonl
    record carry `reason_class` + `reason` (ops/work-runner.py, via `classify` / `reason_for` here);
  * `rows()` reads every fix run - finished (work-done.jsonl, kind fix) and running
    (work-claims.json) - and BACKFILLS the reason of records written before this change from what
    triggered them (`backfill`: the work runner's log, else the merge runner's attention line);
  * `tally()` counts them per day and class - the /worklog "Fix runs" table and the digest line.

A GATE_FAIL's reason is what the merge runner's own triage said stopped that gate: the red tests
or browser specs, or "lint/build/ui-unit" when no test line was red.
"""

from __future__ import annotations

import json
import os
import re
from collections import Counter, defaultdict
from datetime import datetime

CLASSES = ("REVIEW_FAIL", "GATE_FAIL", "CONFLICT", "UNCOMMITTED", "OTHER")
_LINE_TS = re.compile(r"^\[(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\] (.*)$")


def classify(fail_line: str) -> str:
    f = (fail_line or "").strip()
    if f.startswith("REVIEW_FAIL"):
        return "REVIEW_FAIL"
    if f.startswith("UNCOMMITTED"):
        return "UNCOMMITTED"
    parts = f.split()
    if len(parts) > 4 and parts[4].startswith("CONFLICT") or f.startswith("CONFLICT"):
        return "CONFLICT"
    if "GATE_FAIL" in f:
        return "GATE_FAIL"
    return "OTHER"


def gate_fail_detail(log_text: str, branch: str) -> str:
    """What the merge runner's triage said stopped `branch`'s last failed gate."""
    lines = log_text.splitlines()
    for i in range(len(lines) - 1, -1, -1):
        m = _LINE_TS.match(lines[i].strip())
        if not (m and m.group(6).startswith(f"GATE FAILED {branch} ")):
            continue
        for j in range(i - 1, max(-1, i - 4000), -1):
            mj = _LINE_TS.match(lines[j].strip())
            if not mj:
                continue
            rest = mj.group(6)
            hit = re.match(r"TRIAGE: (?:re-running the failing tests alone: |browser specs red: )(.*?)(?: - re-running them alone)?\s*$", rest)
            if hit:
                how = next((_LINE_TS.match(lines[k].strip()).group(6) for k in range(j + 1, i)
                            if _LINE_TS.match(lines[k].strip()) and "FAILS alone" in lines[k]), "")
                return hit.group(1).strip() + (" (fails alone)" if how else "")
            if rest.startswith("TRIAGE: no FAIL lines found"):
                return "lint/build/ui-unit (no test line red)"
            if rest.startswith(("MERGE start", "BULK attempt")):
                break
        return "(no triage line)"
    return ""


def reason_for(fail_line: str, log_text: str = "", branch: str = "") -> tuple[str, str]:
    cls = classify(fail_line)
    f = " ".join((fail_line or "").split())
    if cls == "REVIEW_FAIL":
        f = re.sub(r"^REVIEW_FAIL\s+(VERDICT:\s*FAIL\s*)?", "", f)
    elif cls == "UNCOMMITTED":
        f = re.sub(r"^UNCOMMITTED\s+", "", f)
    elif cls == "GATE_FAIL":
        f = gate_fail_detail(log_text, branch) or "merge gate red"
    elif cls == "CONFLICT":
        f = "no longer merges cleanly into main"
    return cls, f[:200]


def _jsonl(path: str) -> list[dict]:
    out = []
    try:
        for ln in open(path, encoding="utf-8"):
            try:
                out.append(json.loads(ln))
            except ValueError:
                continue
    except OSError:
        pass
    return out


def _attention(ops: str) -> list[tuple[float, str, str, str]]:
    """[(ts, ticket, kind, rest)] from both attention files: the GATE_FAIL / CONFLICT lines the
    merge runner writes (the work runner resumes a worker from exactly these)."""
    out = []
    year = datetime.now().year
    for name in ("work-needs-attention.txt", "merge-needs-attention.txt"):
        try:
            text = open(os.path.join(ops, name), encoding="utf-8").read()
        except OSError:
            continue
        for ln in text.splitlines():
            m = re.match(r"^(\d\d-\d\d \d\d:\d\d)\s+\S+\s+(T-\d+)\s+(REVIEW_FAIL|UNCOMMITTED|GATE_FAIL|CONFLICT)\S*\s*(.*)$", ln)
            if m:
                try:
                    ts = datetime.strptime(f"{year}-{m.group(1)}", "%Y-%m-%d %H:%M").timestamp()
                except ValueError:
                    continue
                out.append((ts, m.group(2), m.group(3), m.group(4)))
    return out


def _log_ts(line: str, year: int) -> float | None:
    m = _LINE_TS.match(line.strip())
    if not m:
        return None
    mo, d, h, mi, se = (int(g) for g in m.groups()[:5])
    try:
        return datetime(year, mo, d, h, mi, se).timestamp()
    except ValueError:
        return None


def _summary(path: str) -> str:
    """The fix run's own first line (fix<n>.json `result`) - what it says it fixed."""
    try:
        text = str(json.load(open(path)).get("result", ""))
    except Exception:
        return ""
    return next((ln.strip(" #*") for ln in text.splitlines() if ln.strip(" #*")), "")


def backfill(ops: str, rec: dict, wlog: list[str], att, mlog: str = "") -> tuple[str, str, int | None]:
    """(class, reason, attempt) for a fix record written before the runner recorded them. Source, in
    order: the work runner's own line before that FIX (a relaunched held fix carries its fail
    line; an escalation's ATTENTION line carries it too; a bare REVIEW line means the review
    that ran just before it failed - its verdict text is overwritten by the next review, so the
    reason is the fix run's own summary); else the merge runner's GATE_FAIL/CONFLICT attention."""
    tid, started = rec.get("ticket"), float(rec.get("started") or 0)
    year = datetime.fromtimestamp(started).year
    for i, ln in enumerate(wlog):
        if f"] FIX {tid} attempt " not in ln:
            continue
        ts = _log_ts(ln, year)
        if ts is None or abs(ts - started) > 120:
            continue
        am = re.search(r" attempt (\d+)", ln)
        rec = dict(rec, attempt=rec.get("attempt") or (int(am.group(1)) if am else None))
        for j in range(i - 1, max(-1, i - 600), -1):
            prev = wlog[j]
            if tid not in prev or "] tick:" in prev or f"] FIX {tid} attempt " in prev:
                continue
            m = re.search(r"relaunching the held fix \((.*)\)\s*$", prev)
            if m:
                return (*reason_for(m.group(1), mlog, rec.get("branch", "")), rec["attempt"])
            m = re.search(rf"\] ATTENTION {tid} ((?:REVIEW_FAIL|UNCOMMITTED) .*)$", prev)
            if m:
                return (*reason_for(m.group(1), mlog, rec.get("branch", "")), rec["attempt"])
            if re.search(rf"\] REVIEW {tid} ", prev):
                n = rec.get("attempt") or 1
                why = _summary(os.path.join(ops, "work", tid, f"fix{n}.json"))
                return ("REVIEW_FAIL", ("fixed: " + why)[:200] if why else "review verdict FAIL (text overwritten by the next review)",
                        rec["attempt"])
            break
        break
    prior = [a for a in att if a[1] == tid and a[0] <= started + 120]
    if prior:
        _, _, kind, rest = max(prior)
        return (*reason_for(f"{kind} {rest}", mlog, rec.get("branch", "")), rec.get("attempt"))
    return "OTHER", "(reason not recorded)", rec.get("attempt")


def rows(ops: str, since: float, mlog: str = "") -> list[dict]:
    """Every fix run since `since`, newest first: finished ones from work-done.jsonl and the
    running ones from work-claims.json."""
    att = _attention(ops)
    try:
        wlog = open(os.path.join(ops, "work-runner.log"), encoding="utf-8", errors="replace").read().splitlines()
    except OSError:
        wlog = []
    out = []
    for o in _jsonl(os.path.join(ops, "work-done.jsonl")):
        if o.get("kind") != "fix" or float(o.get("started") or 0) < since:
            continue
        cls, why, n, back = o.get("reason_class"), o.get("reason"), o.get("attempt"), False
        if not cls:
            cls, why, n = backfill(ops, o, wlog, att, mlog)
            back = True
        out.append({"ts": float(o["started"]), "ticket": o.get("ticket"), "attempt": n,
                    "reason_class": cls, "reason": why, "backfilled": back,
                    "outcome": o.get("outcome"), "minutes": o.get("minutes")})
    try:
        claims = json.load(open(os.path.join(ops, "work-claims.json")))
    except Exception:
        claims = {}
    for c in claims.values():
        if c.get("state") == "running" and c.get("kind") == "fix" and float(c.get("started") or 0) >= since:
            out.append({"ts": float(c.get("started") or 0), "ticket": c.get("ticket"), "attempt": c.get("fix_attempts"),
                        "reason_class": c.get("fix_reason_class") or "OTHER", "reason": c.get("fix_reason") or "",
                        "backfilled": False, "outcome": "running", "minutes": None})
    out.sort(key=lambda r: -r["ts"])
    return out


def tally(rs: list[dict]) -> dict[str, dict[str, int]]:
    by: dict[str, Counter] = defaultdict(Counter)
    for r in rs:
        by[datetime.fromtimestamp(r["ts"]).strftime("%Y-%m-%d")][r["reason_class"]] += 1
    return {d: dict(sorted(c.items(), key=lambda kv: -kv[1])) for d, c in sorted(by.items(), reverse=True)}


def tally_line(rs: list[dict]) -> str:
    c = Counter(r["reason_class"] for r in rs)
    return "fix runs 24h: " + (" · ".join(f"{k} {v}" for k, v in c.most_common()) or "none")


def summary(ops: str, days: int = 7, now: float | None = None) -> dict:
    """What /fixes.json serves (ops/monitor.py): the last `days` of fix runs, newest first, the
    per-day tally by class and the digest's 24 h line. Reads the last 4 MB of merge-runner.log for
    a GATE_FAIL's triage detail."""
    import time as _time
    now = now or _time.time()
    mlog = ""
    path = os.path.join(ops, "merge-runner.log")
    try:
        with open(path, "rb") as f:
            f.seek(max(0, os.path.getsize(path) - 4_000_000))
            mlog = f.read().decode("utf-8", "replace")
    except OSError:
        pass
    rs = rows(ops, now - days * 86400, mlog)
    return {"rows": rs, "tally": tally(rs), "classes": list(CLASSES),
            "line": tally_line([r for r in rs if r["ts"] >= now - 86400])}
