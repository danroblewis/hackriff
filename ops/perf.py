#!/usr/bin/env python3
"""ops/perf.py -- mine Claude Code agent session transcripts for per-ticket time/token spend.

Standalone, pure-stdlib. Feeds a dashboard page (see ops/monitor.py). Do not import
anything outside the stdlib. The public contract the dashboard calls is:

    scan(cache=True) -> dict
    aggregate(frm=None, to=None, scope="all", agent=None) -> dict

Run `python3 ops/perf.py` for an eyeball summary.

--------------------------------------------------------------------------------
JSONL schema discovered on disk (Claude Code v2.1.x transcripts)
--------------------------------------------------------------------------------
Transcripts live under $CLAUDE_PROJECT_DIR (see PROJECT_DIR below):
  <session>.jsonl                     top-level sessions (coordinator, supervisor, ...)
  <session>/subagents/agent-*.jsonl   one file per spawned subagent (one ticket each)
  <session>/subagents/agent-*.meta.json  sidecar: {agentType, description, model, ...}
  <session>/tool-results/*.txt        NOT transcripts (large tool dumps) -- ignored
  <session>/workflows/*.json          NOT transcripts -- ignored

Each .jsonl line is one JSON object. Many `type`s exist (mode, system, attachment,
queue-operation, ...); we only use `user` and `assistant`. Relevant fields:
  o["type"]            "user" | "assistant" (+ many ignored)
  o["timestamp"]       ISO-8601 e.g. "2026-09-17T13:53:03.519Z"
  o["message"]["content"]  str (a plain user prompt) OR list of blocks:
       assistant blocks: {type:"tool_use", id, name, input:{command,...}}
                         {type:"text", text}  {type:"thinking", thinking}
       user blocks:      {type:"tool_result", tool_use_id, content}
  o["message"]["usage"]    only on assistant turns:
       {input_tokens, output_tokens, cache_read_input_tokens,
        cache_creation_input_tokens, ...}
  o["toolUseResult"]   on user/tool_result turns; for Bash: {stdout,stderr,interrupted,...}
  o["agentId"]         present on subagent lines
  o["sessionId"], o["slug"]  session identity / human slug

Bash duration = ts(tool_result) - ts(tool_use), matched by tool_use_id WITHIN a file.
Thinking time per assistant turn ~= ts(assistant) - ts(previous user/tool_result),
capped at THINK_CAP_S (longer gaps are the agent idle/waiting, not generating).
"""

import json
import os
import glob
import re
import statistics
import time
from datetime import datetime, timezone

PROJECT_DIR = os.environ.get(
    "HACKRIFF_TRANSCRIPTS",
    os.path.expanduser("~/.claude/projects/-Users-daniellewis-hackriff"),
)

# Longer than this between the previous turn and an assistant reply is idle/waiting,
# not model generation -- clamp so a coordinator parked overnight doesn't score hours
# of "thinking". Documented, tunable.
THINK_CAP_S = 600.0
# Guard against clock skew / a tool_result that crosses a very long backgrounded gap.
MAX_CMD_S = 3600.0
# A cargo/just build-or-test invocation over this wall time is treated as a COLD
# (full-compile) run; under it, incremental. Also cold if we captured no stdout at all.
COLD_THRESHOLD_S = 90.0

# --------------------------------------------------------------------------------
# Command STEM normalizer -- a curated regex -> family map (ordered; first match wins).
# Each entry is (compiled_regex, family) where family is a str or a callable(match)->str.
# Extend by adding rows. Applied to the command with leading `cd ...&&` / `ENV=..` /
# `sudo` noise stripped (see _stem()).
# --------------------------------------------------------------------------------
FAMILY_RULES = [
    (re.compile(r"\bjust\s+(gate|gate-merge|acceptance-ci)\b"), "just gate"),
    (re.compile(r"\bjust\s+(test-crate|test-one)\b"), "just test-crate"),
    (re.compile(r"\bjust\s+(acceptance\S*)\b"), "just acceptance"),
    (re.compile(r"\bjust\s+([a-zA-Z0-9_-]+)"), lambda m: "just " + m.group(1)),
    (re.compile(r"\bcargo\s+(?:\+\S+\s+)?(build|check)\b"), "cargo build"),
    (re.compile(r"\bcargo\s+(?:\+\S+\s+)?(nextest|test)\b"), "cargo test"),
    (re.compile(r"\bcargo\s+(?:\+\S+\s+)?clippy\b"), "cargo clippy"),
    (re.compile(r"\bcargo\s+(?:\+\S+\s+)?([a-zA-Z0-9_-]+)"), lambda m: "cargo " + m.group(1)),
    (re.compile(r"\bgit\s+(-C\s+\S+\s+)?([a-zA-Z0-9_-]+)"), lambda m: "git " + m.group(2)),
    (re.compile(r"\b(grep|rg|ag|find|ls|cat|sed|awk|head|tail|wc|less|nl)\b"), "search/inspect"),
    (re.compile(r"\btmux\b"), "tmux"),
    (re.compile(r"\b(curl|wget|http|nc)\b"), "probe"),
    (re.compile(r"\b(npm|npx|node|pnpm|yarn|vite|tsc)\b"), "npm/node"),
    (re.compile(r"\b(python3?|uv|pip3?|pytest)\b"), "python"),
    (re.compile(r"\b(hackrf_\w+|hk)\b"), "hackrf/hk"),
    (re.compile(r"\b(df|du|free|top|ps|kill|pkill|sleep|mkdir|rm|cp|mv|chmod|echo)\b"), "shell util"),
]

# Families whose duration counts as build/test work for cold/incremental split.
BUILD_FAMILIES = {"cargo build", "cargo test", "just gate", "just test-crate", "just acceptance"}

_TICKET_RE = re.compile(r"\b(?:T-(\d+)|task-t(\d+))\b", re.IGNORECASE)


# --------------------------------------------------------------------------------
# helpers
# --------------------------------------------------------------------------------
def _parse_ts(s):
    """ISO-8601 (Z or offset) -> epoch seconds (float), or None."""
    if not s or not isinstance(s, str):
        return None
    try:
        if s.endswith("Z"):
            s = s[:-1] + "+00:00"
        dt = datetime.fromisoformat(s)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.timestamp()
    except Exception:
        return None


def _strip_prefix(cmd):
    """Remove leading `cd X &&`, env-var assignments and sudo so the stem sees the real verb."""
    c = cmd.strip()
    # peel repeated leading `cd ... &&`
    c = re.sub(r"^\s*cd\s+[^&|;]+&&\s*", "", c)
    # peel leading ENV=val assignments
    c = re.sub(r"^\s*(?:[A-Z_][A-Z0-9_]*=(?:\"[^\"]*\"|'[^']*'|\S+)\s+)+", "", c)
    c = re.sub(r"^\s*sudo\s+", "", c)
    return c.strip()


def _stem(cmd):
    if not cmd:
        return "(empty)"
    probe = _strip_prefix(cmd)
    for rx, fam in FAMILY_RULES:
        m = rx.search(probe)
        if m:
            return fam(m) if callable(fam) else fam
    # fallback: first bare token of the stripped command
    tok = re.split(r"[\s|;&<>()]+", probe.strip())
    return tok[0] if tok and tok[0] else "(other)"


def _find_ticket(text):
    if not text:
        return None
    m = _TICKET_RE.search(text)
    if not m:
        return None
    num = m.group(1) or m.group(2)
    return "T-" + num


def _text_of(content):
    """Flatten a message.content (str or block list) to plain text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        out = []
        for b in content:
            if isinstance(b, dict):
                if b.get("type") == "text" and isinstance(b.get("text"), str):
                    out.append(b["text"])
                elif b.get("type") == "thinking" and isinstance(b.get("thinking"), str):
                    out.append(b["thinking"])
                elif isinstance(b.get("content"), str):
                    out.append(b["content"])
        return "\n".join(out)
    return ""


def _pct(vals, q):
    """Percentile (q in [0,1]) via linear interpolation on a sorted copy.

    Never crashes on n=1 (returns that single value, so p90==p50==p95==it).
    """
    if not vals:
        return 0.0
    s = sorted(vals)
    if len(s) == 1:
        return float(s[0])
    idx = q * (len(s) - 1)
    lo = int(idx)
    hi = min(lo + 1, len(s) - 1)
    frac = idx - lo
    return float(s[lo] + (s[hi] - s[lo]) * frac)


def _stat(vals, nd=2):
    """A distribution summary {sum,avg,p50,p90,p95} over a list of numbers.

    Empty -> all zeros. n=1 -> every stat is that value (see _pct). Percentiles
    use _pct (linear interpolation); avg is the arithmetic mean.
    """
    if not vals:
        return {"sum": 0.0, "avg": 0.0, "p50": 0.0, "p90": 0.0, "p95": 0.0}
    n = len(vals)
    tot = sum(vals)
    return {
        "sum": round(tot, nd),
        "avg": round(tot / n, nd),
        "p50": round(_pct(vals, 0.5), nd),
        "p90": round(_pct(vals, 0.9), nd),
        "p95": round(_pct(vals, 0.95), nd),
    }


# --------------------------------------------------------------------------------
# per-file parse
# --------------------------------------------------------------------------------
def _load_meta(path):
    """Sidecar agent-*.meta.json for a subagent transcript, if present."""
    meta_path = path[:-6] + ".meta.json" if path.endswith(".jsonl") else None
    if meta_path and os.path.exists(meta_path):
        try:
            with open(meta_path) as fh:
                return json.load(fh)
        except Exception:
            return {}
    return {}


def _parse_file(path):
    """Parse one transcript file into a session record. Never raises on content."""
    meta = _load_meta(path)
    is_subagent = os.path.basename(os.path.dirname(path)) == "subagents"

    pending = {}          # tool_use_id -> (ts, command)  (only Bash)
    commands = []         # {cmd,stem,dur_s,cold,ts}
    turns = []            # {ts, thinking_s, tok_in, tok_out, tok_cr, tok_cc}
    first_prompt = None
    slug = None
    session_id = None
    agent_id = None
    model = meta.get("model")

    bad_lines = 0
    prev_ts = None        # ts of the most recent user / tool_result entry
    first_ts = None
    last_ts = None

    try:
        fh = open(path, "r", errors="replace")
    except Exception:
        return None
    with fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                o = json.loads(line)
            except Exception:
                bad_lines += 1
                continue
            t = o.get("type")
            if t not in ("user", "assistant"):
                continue
            ts = _parse_ts(o.get("timestamp"))
            if ts is not None:
                if first_ts is None:
                    first_ts = ts
                last_ts = ts
            slug = slug or o.get("slug")
            session_id = session_id or o.get("sessionId")
            agent_id = agent_id or o.get("agentId")
            msg = o.get("message") or {}
            content = msg.get("content")

            if t == "user":
                if first_prompt is None:
                    txt = _text_of(content)
                    if txt and txt.strip():
                        first_prompt = txt.strip()
                # resolve any tool_results in this turn
                if isinstance(content, list):
                    for b in content:
                        if isinstance(b, dict) and b.get("type") == "tool_result":
                            tuid = b.get("tool_use_id")
                            if tuid in pending and ts is not None:
                                tu_ts, cmd = pending.pop(tuid)
                                dur = ts - tu_ts
                                if dur is not None and 0 <= dur <= MAX_CMD_S:
                                    stem = _stem(cmd)
                                    stdout = ""
                                    tur = o.get("toolUseResult")
                                    if isinstance(tur, dict):
                                        stdout = tur.get("stdout") or ""
                                    cold = None
                                    if stem in BUILD_FAMILIES:
                                        cold = dur > COLD_THRESHOLD_S or not stdout.strip()
                                    commands.append({
                                        "cmd": cmd, "stem": stem, "dur_s": dur,
                                        "cold": cold, "ts": tu_ts,
                                    })
                if ts is not None:
                    prev_ts = ts

            elif t == "assistant":
                # thinking/generation gap for this turn
                think = 0.0
                if ts is not None and prev_ts is not None:
                    gap = ts - prev_ts
                    if gap > 0:
                        think = min(gap, THINK_CAP_S)
                u = msg.get("usage") or {}
                turns.append({
                    "ts": ts if ts is not None else (prev_ts or first_ts or 0.0),
                    "thinking_s": think,
                    "tok_in": int(u.get("input_tokens") or 0),
                    "tok_out": int(u.get("output_tokens") or 0),
                    "tok_cr": int(u.get("cache_read_input_tokens") or 0),
                    "tok_cc": int(u.get("cache_creation_input_tokens") or 0),
                })
                # register Bash tool_use calls for later duration matching
                if isinstance(content, list):
                    for b in content:
                        if isinstance(b, dict) and b.get("type") == "tool_use" and b.get("name") == "Bash":
                            cmd = (b.get("input") or {}).get("command")
                            if cmd and ts is not None:
                                pending[b.get("id")] = (ts, cmd)
                # an assistant turn advances the clock too (so back-to-back
                # assistant turns don't each re-charge the same idle gap)
                if ts is not None:
                    prev_ts = ts

    # ----- identity, label, ticket -----
    ticket = None
    desc = meta.get("description") or ""
    ticket = _find_ticket(desc) or _find_ticket(first_prompt)
    if is_subagent:
        label = (desc.strip() or (first_prompt or "")[:60].strip() or "subagent")
    else:
        label = slug or (first_prompt or "")[:60].strip() or os.path.basename(path)

    sid = os.path.basename(path)
    if sid.endswith(".jsonl"):
        sid = sid[:-6]

    return {
        "id": sid,
        "file": path,
        "is_subagent": is_subagent,
        "label": label,
        "ticket": ticket,
        "model": model,
        "agent_type": meta.get("agentType"),
        "slug": slug,
        "session_id": session_id,
        "agent_id": agent_id,
        "start": first_ts,
        "end": last_ts,
        "commands": commands,
        "turns": turns,
        "bad_lines": bad_lines,
    }


# --------------------------------------------------------------------------------
# scan (memoized by file mtime+size)
# --------------------------------------------------------------------------------
_CACHE = {}          # path -> (mtime, size, session_record)
_SCAN_RESULT = None  # last full scan dict


def _discover():
    """All transcript .jsonl files (top-level + subagents), excluding sidecars/dumps."""
    files = set()
    files.update(glob.glob(os.path.join(PROJECT_DIR, "*.jsonl")))
    files.update(glob.glob(os.path.join(PROJECT_DIR, "*", "subagents", "*.jsonl")))
    # any other nested .jsonl transcripts, robustly
    for root, _dirs, names in os.walk(PROJECT_DIR):
        if os.path.basename(root) in ("tool-results", "memory", "workflows", "scripts"):
            continue
        for n in names:
            if n.endswith(".jsonl"):
                files.add(os.path.join(root, n))
    return sorted(files)


def scan(cache=True, subagents_only=False):
    """Parse all transcripts, memoizing per file by (mtime,size). Returns raw session data.

    Shape:
      {"sessions": {id: <session record>}, "stats": {...}}

    subagents_only=True returns only the per-ticket subagent transcripts, excluding
    the giant top-level firehose sessions (coordinator/supervisor). The full scan is
    still memoized in _SCAN_RESULT; only the returned view is narrowed.
    """
    global _SCAN_RESULT
    sessions = {}
    files_parsed = files_skipped = bad_lines = total_cmds = 0
    for path in _discover():
        try:
            st = os.stat(path)
        except OSError:
            files_skipped += 1
            continue
        key = (path, st.st_mtime, st.st_size)
        rec = None
        if cache and path in _CACHE and _CACHE[path][0] == st.st_mtime and _CACHE[path][1] == st.st_size:
            rec = _CACHE[path][2]
        if rec is None:
            try:
                rec = _parse_file(path)
            except Exception:
                rec = None
            if rec is None:
                files_skipped += 1
                continue
            _CACHE[path] = (st.st_mtime, st.st_size, rec)
        files_parsed += 1
        bad_lines += rec.get("bad_lines", 0)
        total_cmds += len(rec["commands"])
        # de-dup id collisions (shouldn't happen; basenames are unique)
        sid = rec["id"]
        if sid in sessions:
            sid = sid + "-" + str(len(sessions))
        sessions[sid] = rec

    _SCAN_RESULT = {
        "sessions": sessions,
        "stats": {
            "files_parsed": files_parsed,
            "files_skipped": files_skipped,
            "bad_lines": bad_lines,
            "total_commands": total_cmds,
        },
    }
    if subagents_only:
        subs = {k: v for k, v in _SCAN_RESULT["sessions"].items() if v.get("is_subagent")}
        return {"sessions": subs, "stats": _SCAN_RESULT["stats"]}
    return _SCAN_RESULT


# --------------------------------------------------------------------------------
# aggregate
# --------------------------------------------------------------------------------
def _ticket_key(rec):
    return rec["ticket"] if rec["ticket"] else ("session:" + rec["label"][:48])


def aggregate(frm=None, to=None, scope="all", agent=None, subagents_only=True):
    """Filter events to [frm,to] epoch window and return the dashboard contract shape.

    subagents_only defaults True: the two giant top-level firehose sessions (the
    coordinator pinned to T-001 and the supervisor) dominate every total and drown
    the clean per-ticket data, so they are excluded unless subagents_only=False.
    """
    data = _SCAN_RESULT or scan()
    sessions = data["sessions"]
    if subagents_only:
        sessions = {k: v for k, v in sessions.items() if v.get("is_subagent")}

    def in_win(ts):
        if ts is None:
            return False
        if frm is not None and ts < frm:
            return False
        if to is not None and ts > to:
            return False
        return True

    # full data extent (independent of frm/to) -- for the slider bounds
    all_ts = []
    for rec in sessions.values():
        if rec["start"] is not None:
            all_ts.append(rec["start"])
        if rec["end"] is not None:
            all_ts.append(rec["end"])
    win = {"min_ts": min(all_ts) if all_ts else None,
           "max_ts": max(all_ts) if all_ts else None}

    # scope selection
    if scope == "agent" and agent:
        selected = {k: v for k, v in sessions.items() if k == agent or v["agent_id"] == agent}
    else:
        selected = sessions

    agents_out = []
    fam = {}            # stem -> {durs:[], cold_s, incr_s, by_agent:{sid:[durs]}}
    per_agent_acc = {}  # sid -> {label, ticket, fams:{stem:[durs]}} (command runners only)
    tickets = {}        # key -> segment accumulators
    slowest = []        # (dur, cmd, stem, ticket, ts)
    tok = {"in": 0, "out": 0, "cache_read": 0, "cache_creation": 0}
    agg_model_s = 0.0
    agg_tool_s = 0.0

    for sid, rec in selected.items():
        cmds = [c for c in rec["commands"] if in_win(c["ts"])]
        turns = [t for t in rec["turns"] if in_win(t["ts"])]

        s_think = sum(t["thinking_s"] for t in turns)
        s_tool = sum(c["dur_s"] for c in cmds)
        s_tok_out = sum(t["tok_out"] for t in turns)

        agg_model_s += s_think
        agg_tool_s += s_tool
        for t in turns:
            tok["in"] += t["tok_in"]
            tok["out"] += t["tok_out"]
            tok["cache_read"] += t["tok_cr"]
            tok["cache_creation"] += t["tok_cc"]

        agents_out.append({
            "id": sid,
            "label": rec["label"],
            "ticket": rec["ticket"],
            "start": rec["start"],
            "end": rec["end"],
            "n_cmds": len(cmds),
            "tokens_out": s_tok_out,
            "thinking_s": round(s_think, 2),
            "tool_s": round(s_tool, 2),
            "total_s": round(s_think + s_tool, 2),
        })

        # ticket segments
        tk = _ticket_key(rec)
        seg = tickets.setdefault(tk, {
            "ticket": rec["ticket"], "label": rec["label"],
            "build_s": 0.0, "test_s": 0.0, "gate_s": 0.0,
            "thinking_s": 0.0, "other_s": 0.0, "tokens_out": 0,
        })
        seg["thinking_s"] += s_think
        seg["tokens_out"] += s_tok_out

        for c in cmds:
            stem = c["stem"]
            d = c["dur_s"]
            f = fam.setdefault(stem, {"durs": [], "cold_s": 0.0, "incr_s": 0.0, "by_agent": {}})
            f["durs"].append(d)
            f["by_agent"].setdefault(sid, []).append(d)
            if c["cold"] is True:
                f["cold_s"] += d
            elif c["cold"] is False:
                f["incr_s"] += d

            pa = per_agent_acc.setdefault(sid, {"label": rec["label"], "ticket": rec["ticket"], "fams": {}})
            pa["fams"].setdefault(stem, []).append(d)

            if stem == "just gate":
                seg["gate_s"] += d
            elif stem == "cargo build":
                seg["build_s"] += d
            elif stem in ("cargo test", "just test-crate", "just acceptance"):
                seg["test_s"] += d
            else:
                seg["other_s"] += d

            slowest.append((d, c["cmd"], stem, rec["ticket"], c["ts"]))

    # distinct agents that ran ANY command in the window -- the denominator for
    # each family's coverage (pct_agents). A session with turns but no commands
    # does not count, since families are command-based.
    n_agents_total = len(per_agent_acc)

    families_out = []
    for stem, f in fam.items():
        durs = f["durs"]
        agent_tots = [sum(v) for v in f["by_agent"].values()]      # per-agent TOTAL duration
        agent_cnts = [float(len(v)) for v in f["by_agent"].values()]  # per-agent invocation COUNT
        dur_inv = _stat(durs)
        dur_agent = _stat(agent_tots)
        cnt_agent = _stat(agent_cnts)
        n_inv = len(durs)
        n_ag = len(f["by_agent"])
        pct = round(100.0 * n_ag / n_agents_total, 1) if n_agents_total else 0.0
        # "how much slower than average"; guard div0 (all-zero durations -> equal -> 1.0)
        p90oa = round(dur_inv["p90"] / dur_inv["avg"], 3) if dur_inv["avg"] > 1e-9 else 1.0
        families_out.append({
            "stem": stem,
            # legacy flat fields (kept: CLI summary + any old caller)
            "count": n_inv,
            "total_s": dur_inv["sum"],
            "p50_s": dur_inv["p50"],
            "p90_s": dur_inv["p90"],
            "cold_s": round(f["cold_s"], 2),
            "incr_s": round(f["incr_s"], 2),
            # distribution block
            "n_invocations": n_inv,
            "n_agents": n_ag,
            "pct_agents": pct,
            "cold_sum": round(f["cold_s"], 2),
            "incr_sum": round(f["incr_s"], 2),
            "dur_inv": dur_inv,
            "dur_agent": dur_agent,
            "cnt_inv": {"sum": n_inv},   # per-invocation count is trivially 1 each
            "cnt_agent": cnt_agent,
            "p90_over_avg": p90oa,
        })
    families_out.sort(key=lambda x: x["total_s"], reverse=True)

    # P90-slowest command TYPES (families ranked by their p90 invocation duration).
    p90_slowest = sorted(families_out, key=lambda x: x["dur_inv"]["p90"], reverse=True)
    p90_slowest_out = [{
        "stem": r["stem"],
        "p90_s": r["dur_inv"]["p90"],
        "avg_s": r["dur_inv"]["avg"],
        "p90_over_avg": r["p90_over_avg"],
        "n_agents": r["n_agents"],
        "pct_agents": r["pct_agents"],
        "n_invocations": r["n_invocations"],
    } for r in p90_slowest[:25]]

    # Per-agent view: one entry per command-running agent, its top families.
    # Capped at PER_AGENT_FAM_CAP families each (by total time); n_families surfaces the cap.
    PER_AGENT_FAM_CAP = 10
    per_agent_out = []
    for sid, pa in per_agent_acc.items():
        fams = []
        for stem, dl in pa["fams"].items():
            fams.append({
                "stem": stem,
                "total_s": round(sum(dl), 2),
                "count": len(dl),
                "p50_s": round(_pct(dl, 0.5), 2),
                "p90_s": round(_pct(dl, 0.9), 2),
            })
        fams.sort(key=lambda x: x["total_s"], reverse=True)
        per_agent_out.append({
            "agent_id": sid,
            "label": pa["label"],
            "ticket": pa["ticket"],
            "n_families": len(fams),
            "families": fams[:PER_AGENT_FAM_CAP],
        })
    per_agent_out.sort(key=lambda a: sum(f["total_s"] for f in a["families"]), reverse=True)

    tickets_out = []
    for tk, seg in tickets.items():
        total = seg["build_s"] + seg["test_s"] + seg["gate_s"] + seg["thinking_s"] + seg["other_s"]
        tickets_out.append({
            "ticket": seg["ticket"] or tk,
            "label": seg["label"],
            "build_s": round(seg["build_s"], 2),
            "test_s": round(seg["test_s"], 2),
            "gate_s": round(seg["gate_s"], 2),
            "thinking_s": round(seg["thinking_s"], 2),
            "other_s": round(seg["other_s"], 2),
            "total_s": round(total, 2),
            "tokens_out": seg["tokens_out"],
        })
    tickets_out.sort(key=lambda x: x["total_s"], reverse=True)

    slowest.sort(key=lambda x: x[0], reverse=True)
    slowest_out = [{
        "cmd": (c[1][:200]), "stem": c[2], "dur_s": round(c[0], 2),
        "ticket": c[3], "ts": c[4],
    } for c in slowest[:25]]

    agents_out.sort(key=lambda a: a["total_s"], reverse=True)

    return {
        "window": win,
        "agents": agents_out,
        "families": families_out,
        "n_agents_total": n_agents_total,
        "per_agent": per_agent_out,
        "per_agent_family_cap": PER_AGENT_FAM_CAP,
        "tickets": tickets_out,
        "thinking": {"model_s": round(agg_model_s, 2), "tool_s": round(agg_tool_s, 2)},
        "slowest": slowest_out,
        "p90_slowest": p90_slowest_out,
        "tokens": tok,
    }


# --------------------------------------------------------------------------------
# self-test + eyeball summary
# --------------------------------------------------------------------------------
def _selftest():
    """Lightweight invariants -- raises AssertionError on contract drift."""
    assert _stem("cargo build --workspace") == "cargo build"
    assert _stem("CARGO_BUILD_JOBS=6 cargo nextest run -p hk-dsp") == "cargo test"
    assert _stem("cd /x && just gate") == "just gate"
    assert _stem("just gate-merge") == "just gate"
    assert _stem("just test-crate hk-core") == "just test-crate"
    assert _stem("just acceptance-ci") == "just gate"  # gate-family regex owns acceptance-ci
    assert _stem("just deploy-jetson") == "just deploy-jetson"
    assert _stem("git commit -m x") == "git commit"
    assert _stem("git -C /w log --oneline") == "git log"
    assert _stem("rg -n foo crates/") == "search/inspect"
    assert _stem("cargo clippy --all") == "cargo clippy"
    assert _stem("python3 ops/perf.py") == "python"
    assert _find_ticket("Working task **T-425** in ...") == "T-425"
    assert _find_ticket("branch task-t542 lands") == "T-542"
    assert _find_ticket("no ticket here") is None
    assert abs(_pct([1, 2, 3, 4], 0.5) - 2.5) < 1e-9
    assert _stat([]) == {"sum": 0.0, "avg": 0.0, "p50": 0.0, "p90": 0.0, "p95": 0.0}
    assert _stat([5]) == {"sum": 5.0, "avg": 5.0, "p50": 5.0, "p90": 5.0, "p95": 5.0}
    r = aggregate()
    for k in ("window", "agents", "families", "tickets", "thinking", "slowest",
              "tokens", "p90_slowest", "per_agent", "n_agents_total"):
        assert k in r, "missing key " + k
    assert set(r["thinking"]) == {"model_s", "tool_s"}
    assert set(r["tokens"]) == {"in", "out", "cache_read", "cache_creation"}
    if r["families"]:
        f0 = r["families"][0]
        for k in ("stem", "n_invocations", "n_agents", "pct_agents", "cold_sum",
                  "incr_sum", "dur_inv", "dur_agent", "cnt_inv", "cnt_agent", "p90_over_avg"):
            assert k in f0, "family missing " + k
        for blk in ("dur_inv", "dur_agent", "cnt_agent"):
            assert set(f0[blk]) == {"sum", "avg", "p50", "p90", "p95"}, "bad stat block " + blk
        assert f0["cnt_inv"]["sum"] == f0["n_invocations"]
        assert abs(f0["dur_agent"]["sum"] - f0["dur_inv"]["sum"]) < 0.05  # same total, two groupings
    if r["p90_slowest"]:
        assert set(r["p90_slowest"][0]) == {
            "stem", "p90_s", "avg_s", "p90_over_avg", "n_agents", "pct_agents", "n_invocations"}
    if r["per_agent"]:
        assert set(r["per_agent"][0]) >= {"agent_id", "label", "ticket", "families", "n_families"}
    # window sub-filtering must not exceed full-run counts
    full_cmds = sum(a["n_cmds"] for a in r["agents"])
    mid = None
    if r["window"]["min_ts"] and r["window"]["max_ts"]:
        mid = (r["window"]["min_ts"] + r["window"]["max_ts"]) / 2
        half = aggregate(frm=mid)
        assert sum(a["n_cmds"] for a in half["agents"]) <= full_cmds
    print("selftest: OK")


if __name__ == "__main__":
    t0 = time.time()
    data = scan()
    dt = time.time() - t0
    st = data["stats"]
    agg = aggregate()
    n_tickets = sum(1 for r in data["sessions"].values() if r["ticket"])
    print(f"scan: {dt:.1f}s  files_parsed={st['files_parsed']} skipped={st['files_skipped']} "
          f"bad_lines={st['bad_lines']} total_commands={st['total_commands']}")
    print(f"sessions={len(data['sessions'])}  with_ticket={n_tickets}  "
          f"agents(non-empty in window)={len([a for a in agg['agents'] if a['n_cmds']])}")
    tk = agg["tokens"]
    print(f"tokens: in={tk['in']:,} out={tk['out']:,} cache_read={tk['cache_read']:,} "
          f"cache_creation={tk['cache_creation']:,}")
    print(f"thinking model_s={agg['thinking']['model_s']:,.0f}  tool_s={agg['thinking']['tool_s']:,.0f}")
    print("\nTop 10 command families by total time:")
    print(f"  {'stem':<18}{'count':>7}{'total_s':>11}{'p50_s':>9}{'p90_s':>9}{'cold_s':>11}{'incr_s':>11}")
    for f in agg["families"][:10]:
        print(f"  {f['stem']:<18}{f['count']:>7}{f['total_s']:>11.0f}{f['p50_s']:>9.1f}"
              f"{f['p90_s']:>9.1f}{f['cold_s']:>11.0f}{f['incr_s']:>11.0f}")
    print("\nTop 5 slowest single invocations:")
    for s in agg["slowest"][:5]:
        print(f"  {s['dur_s']:>8.0f}s  {s['stem']:<14} {s['ticket'] or '-':<7} {s['cmd'][:60]}")
    _selftest()
