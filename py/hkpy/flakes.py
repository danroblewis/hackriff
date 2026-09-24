"""The flake ledger — `just flakes`, and what the merge runner writes into it.

WHY A LEDGER AND NOT A LOG LINE. `ops/merge-runner.sh` already triages every red gate the right
way (`flake_retry`, and the deflake-triage skill's rule): re-run the failing tests ALONE, and
split the two outcomes, because they are opposite facts.

  * **they PASS alone** -> the gate's red was CONTENTION, not the change. The runner retries,
    the branch merges, and the incident vanishes into a 12 MB log nobody re-reads.
  * **a test FAILS alone** -> a real defect in that merge. That one gets attention immediately,
    because it blocks.

The asymmetry is the problem this file fixes. The load flake is forgiven every single time, so
the same test can cost four gates in one day - `app-surface.e2e.mjs` twice on 2026-09-22, plus
`canvas-journey` and a tiles test - and nothing anywhere counts to two. Each gate rediscovers
it, forgives it and forgets it; the coordinator only learns of it if a person happens to read
the log. So: **count it**, in one durable place, in BOTH directions, and when a test has cost
two gates in a week, file it once with the evidence attached (how often, which way it went, and
the load average it went that way under) so the fix is a ticket from data rather than a hunch.

WHAT IT READS. Both of the runner's own outputs, because each holds half the story:

  * ``$HACKRIFF_OPS/flaky.jsonl`` - one line per passed-alone incident, with the load average.
    This is the only place the LOAD is recorded, and load is the evidence that distinguishes
    "needs a deterministic wait" from "genuinely racy".
  * ``$HACKRIFF_OPS/merge-runner.log`` - the TRIAGE lines, which carry BOTH outcomes. The log
    is also where a failed-alone verdict exists at all; `flaky.jsonl` never records one.

Neither is authoritative alone, so both are read and the two are reconciled by time: a
`flaky.jsonl` record and the log line that caused it share a timestamp to the second, so they
collapse into one incident rather than counting twice.

THE RULE. Two reds in seven days -> one line in ``$HACKRIFF_OPS/merge-needs-attention.txt`` and
one amber Discord alert, keyed by test so the alert layer's own dedupe applies on top. Then
silence until it has cost two MORE gates. The threshold state lives in ``flakes.json`` beside
the ledger, so re-running this tool - which the runner does after every triage - never re-files
a ticket that is already filed. A ledger that nags is a ledger that gets muted.

NOTHING HERE MAY FAIL THE RUNNER. It is invoked from `flake_retry` with `|| true`, and every
path inside is defensive besides: a missing file, an unreadable log and an unwritable ops
directory all degrade to a smaller ledger, never to an exception the runner would have to
survive.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from dataclasses import dataclass, field
from datetime import datetime, timedelta

try:  # `python -m hkpy.flakes` (how `just flakes` and the runner invoke it)
    from . import gatelog
except ImportError:  # pragma: no cover - `python3 py/hkpy/flakes.py`
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from hkpy import gatelog  # type: ignore[no-redef]

FLAKY_JSONL = "flaky.jsonl"
LEDGER_JSON = "flakes.json"
RUNNER_LOG = "merge-runner.log"
NEEDS_FILE = "merge-needs-attention.txt"

#: A test that has cost this many gates inside `WINDOW_DAYS` earns one attention entry, and
#: another every `NOTIFY_STEP` after that. Two, not one: one red is an incident, two is a
#: pattern, and the whole point is to file from evidence rather than from the latest annoyance.
RED_THRESHOLD = 2
NOTIFY_STEP = 2
WINDOW_DAYS = 7

#: The user's rule (2026-09-23): a red test that passes alone twice is accepted and the batch lands
#: - so a flake is no longer paid for in gates, and must be FIXED instead of tolerated. The 3rd
#: passed-alone incident of one test inside `WINDOW_DAYS` files a deflake request, which
#: ops/work-runner.py dispatches as a `deflaker`; another every `DEFLAKE_STEP` after that.
DEFLAKE_THRESHOLD = 3
DEFLAKE_STEP = 2
DEFLAKE_REQUESTS = "deflake-requests.jsonl"

#: `[09-22 15:35:26] TRIAGE: ...`
_LINE = re.compile(r"^\[(\d\d)-(\d\d) (\d\d):(\d\d):(\d\d)\] (.*)$")

#: The runner declares the failing set, then resolves it one way or the other. Both halves are
#: needed: the declaration is the red-in-gate, the resolution is which KIND of red it was.
_DECLARE = (
    re.compile(r"^TRIAGE: re-running the failing tests alone: (.*?)\s*$"),
    re.compile(r"^TRIAGE: browser specs red: (.*?) - re-running them alone\s*$"),
)
_PASSED = re.compile(r"^TRIAGE: they PASS alone")
#: The browser tier's own summary of each isolated run (`ui/e2e/run.mjs`, no timestamp): which of
#: the declared specs actually failed ALONE. Without it "a browser spec FAILS alone" was charged to
#: every spec in the set - 09-22 16:25 app-trace passed its isolated run (`failed: fog-of-war,
#: surface-address`) and was still counted "failed alone", which made its 09-24 FLAKY verdict
#: "both ways: real under some condition".
_E2E_SUMMARY = re.compile(r"^e2e: \d+/\d+ files passed .*; failed: (.*?)\s*$")
_FAILED = re.compile(r"^TRIAGE: (?:a test|a browser spec) FAILS alone")

#: Explicitly NOT a gate red: the runner also re-runs the same specs on a rewound `main` to ask
#: whether main itself is broken. Counting those would double every browser incident.
_ON_MAIN = re.compile(r"^TRIAGE: (?:is main itself red\?|MAIN IS RED)")
_BRANCH_INTRODUCED = re.compile(r"^TRIAGE: main is green on them")
_SINGLE_GATE_FAILED = re.compile(r"^GATE FAILED \S+")
_MAIN_RED = re.compile(r"^TRIAGE: MAIN IS RED")

PASSED_ALONE = "passed_alone"
FAILED_ALONE = "failed_alone"
UNRESOLVED = "unresolved"


# ---------------------------------------------------------------------------
# Incidents
# ---------------------------------------------------------------------------


@dataclass
class Incident:
    """One triage: a set of tests that went red in a gate, and how re-running them alone went."""

    ts: float
    tests: tuple[str, ...]
    outcome: str = UNRESOLVED
    #: 1-minute load average when the gate was red, from `flaky.jsonl`. `None` when unknown —
    #: the log does not record it, so only passed-alone incidents can have one.
    load: float | None = None
    batch: str = ""
    source: str = "log"
    #: Failed alone AND the runner then pinned it on the merge: "main is green on them -> the batch
    #: introduced it", or a single merge's `GATE FAILED <branch>`. That is the branch's own defect,
    #: not evidence the SPEC is flaky - on 2026-09-23 23:01 one branch (T-801) breaking three specs
    #: raised three FLAKY alarms. Recorded, never counted toward the FLAKY threshold.
    branch_defect: bool = False
    #: The specs the last isolated run named as failed (`_E2E_SUMMARY`); empty = not known, and a
    #: FAILED_ALONE then counts against every test in the set, as before.
    failed_alone_tests: tuple[str, ...] = ()


def _first_load(text: str) -> float | None:
    """`"30.63 31.34 29.72"` -> 30.63. The 1-minute figure: what the box was doing THEN."""
    try:
        return round(float(str(text).split()[0].strip(",")), 2)
    except (ValueError, IndexError, AttributeError):
        return None


def parse_runner_log(text: str, year: int) -> list[Incident]:
    """Every triage incident in `ops/merge-runner.log`, oldest first.

    The runner's `log()` writes through `tee` while its stdout is redirected to the same file,
    so every line appears TWICE; identical (timestamp, line) pairs collapse to one, which also
    makes this idempotent if the log is ever concatenated after a restart.
    """
    out: list[Incident] = []
    pending: Incident | None = None
    last_failed: Incident | None = None
    seen: set[tuple[str, str]] = set()
    prev: datetime | None = None
    cur_year = year
    for raw in text.splitlines():
        m = _LINE.match(raw.strip())
        if not m:
            hit = _E2E_SUMMARY.match(raw.strip()) if pending is not None else None
            if hit:
                pending.failed_alone_tests = tuple(t for t in hit.group(1).replace(",", " ").split() if t)
            continue
        mon, day, hh, mm, ss, rest = m.groups()
        if not rest.startswith("TRIAGE:") and not (last_failed is not None and _SINGLE_GATE_FAILED.match(rest)):
            continue
        try:
            when = datetime(cur_year, int(mon), int(day), int(hh), int(mm), int(ss))
        except ValueError:
            continue
        # December -> January wrap: the log is written in order, so a big jump backwards means
        # the year rolled. Same rule as `hkpy.cycletime`.
        if prev is not None and when < prev - timedelta(days=200):
            cur_year += 1
            when = when.replace(year=cur_year)
        prev = when
        key = (when.isoformat(), rest)
        if key in seen:
            continue
        seen.add(key)

        if last_failed is not None:
            if _BRANCH_INTRODUCED.match(rest) or _SINGLE_GATE_FAILED.match(rest):
                last_failed.branch_defect = True
                last_failed = None
            elif _MAIN_RED.match(rest):
                last_failed = None            # main itself is red on it: that DOES count
        if _ON_MAIN.match(rest):
            # A re-run on the rewound main, not a gate red. It also ENDS the pending incident's
            # window: whatever follows is about main, not about this merge.
            pending = None
            continue
        declared = None
        for pattern in _DECLARE:
            hit = pattern.match(rest)
            if hit:
                declared = hit.group(1)
                break
        if declared is not None:
            last_failed = None
            tests = tuple(t for t in declared.replace(",", " ").split() if t)
            if tests:
                pending = Incident(ts=when.timestamp(), tests=tests)
                out.append(pending)
            continue
        if pending is None:
            continue
        if _PASSED.match(rest):
            pending.outcome = PASSED_ALONE
            pending = None
        elif _FAILED.match(rest):
            pending.outcome = FAILED_ALONE
            last_failed = pending
            pending = None
    return out


def parse_flaky_jsonl(text: str) -> list[Incident]:
    """`$HACKRIFF_OPS/flaky.jsonl`: `{ts, tests, batch, load_before}`, one per passed-alone red.

    The runner only writes this line on the PASS-alone branch, so every record here is, by
    construction, a load flake — and the only place the load average survives.
    """
    out: list[Incident] = []
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except ValueError:
            continue
        if not isinstance(rec, dict):
            continue
        tests = tuple(t for t in str(rec.get("tests", "")).replace(",", " ").split() if t)
        if not tests:
            continue
        raw_ts = rec.get("ts")
        try:
            ts = datetime.strptime(str(raw_ts), "%Y-%m-%dT%H:%M:%S").timestamp()
        except (ValueError, TypeError):
            try:
                ts = float(raw_ts)
            except (TypeError, ValueError):
                continue
        out.append(
            Incident(
                ts=ts,
                tests=tests,
                outcome=PASSED_ALONE,
                load=_first_load(rec.get("load_before", "")),
                batch=str(rec.get("batch", "")),
                source="flaky.jsonl",
            )
        )
    return out


#: How far apart a `flaky.jsonl` record and the log line that caused it may be and still be one
#: incident. They are written by the same `printf`/`log` pair and normally share the second;
#: the window is generous only so a clock hiccup cannot double-count.
MATCH_WINDOW_S = 600


def reconcile(log_incidents: list[Incident], jsonl_incidents: list[Incident]) -> list[Incident]:
    """One incident list from the two sources, without counting the same red twice.

    The log is preferred where both have it, because only the log knows a failed-alone verdict.
    A `flaky.jsonl` record whose log line has rotated away is kept as its own incident — losing
    it would quietly shrink the counts the threshold is read from.
    """
    merged = list(log_incidents)
    for rec in jsonl_incidents:
        match = None
        for cand in merged:
            if cand.source != "log" or cand.outcome != PASSED_ALONE:
                continue
            if abs(cand.ts - rec.ts) > MATCH_WINDOW_S:
                continue
            if not set(cand.tests) & set(rec.tests):
                continue
            match = cand
            break
        if match is not None:
            if match.load is None:
                match.load = rec.load
            if not match.batch:
                match.batch = rec.batch
            continue
        merged.append(rec)
    merged.sort(key=lambda i: i.ts)
    return merged


# ---------------------------------------------------------------------------
# The ledger
# ---------------------------------------------------------------------------


@dataclass
class Entry:
    test: str
    first_seen: float
    last_seen: float
    red_in_gate: int = 0
    #: Reds that were a branch's own defect (see `Incident.branch_defect`): shown, never counted.
    branch_defects: int = 0
    passed_alone: int = 0
    failed_alone: int = 0
    loads: list[float] = field(default_factory=list)
    #: The red count at which this test was last filed, so it is filed once and then only on
    #: every further `NOTIFY_STEP`.
    notified_at: int = 0
    #: Reds inside the window, recomputed on every update — what the rule actually reads.
    recent_red: int = 0
    #: Passed-alone incidents inside the window - what the deflake rule reads.
    recent_passed: int = 0
    #: `recent_passed` when a deflake request was last filed (persisted like `notified_at`).
    deflaked_at: int = 0
    #: This test's incidents, oldest first, for the deflaker's brief.
    incidents: list = field(default_factory=list)

    def as_dict(self) -> dict:
        return {
            "test": self.test,
            "first_seen": round(self.first_seen, 1),
            "last_seen": round(self.last_seen, 1),
            "red_in_gate": self.red_in_gate,
            "branch_defects": self.branch_defects,
            "passed_alone": self.passed_alone,
            "failed_alone": self.failed_alone,
            "loads": self.loads,
            "notified_at": self.notified_at,
            "recent_red": self.recent_red,
            "recent_passed": self.recent_passed,
            "deflaked_at": self.deflaked_at,
        }

    def verdict(self) -> str:
        """What the counts say this test IS — the half a raw count does not tell you."""
        if self.failed_alone and not self.passed_alone:
            return "fails alone: a real defect, not a flake"
        if self.failed_alone and self.passed_alone:
            return "both ways: real under some condition — triage in isolation"
        return "passes alone: a load flake — needs a deterministic wait"

    def attention_line(self, days: int = WINDOW_DAYS) -> str:
        """The exact entry written to `merge-needs-attention.txt`."""
        loads = ",".join(f"{x:g}" for x in self.loads[-6:]) or "unknown"
        need = (
            "needs a deterministic wait"
            if not self.failed_alone
            else "FAILS ALONE too — triage as a real defect"
        )
        return (
            f"FLAKY {self.test} red {self.recent_red}x in {days}d "
            f"(passed alone {self.passed_alone}, failed alone {self.failed_alone}; "
            f"loads {loads}) - {need}"
        )


def build(incidents: list[Incident], *, now: float | None = None, days: int = WINDOW_DAYS) -> dict[str, Entry]:
    """The per-test ledger. `recent_red` counts only incidents inside the window."""
    now = time.time() if now is None else now
    cut = now - days * 86400
    ledger: dict[str, Entry] = {}
    for inc in incidents:
        for test in inc.tests:
            e = ledger.get(test)
            if e is None:
                e = ledger[test] = Entry(test=test, first_seen=inc.ts, last_seen=inc.ts)
            if inc.branch_defect:
                e.branch_defects += 1
                continue
            e.first_seen = min(e.first_seen, inc.ts)
            e.last_seen = max(e.last_seen, inc.ts)
            e.red_in_gate += 1
            outcome = inc.outcome
            if outcome == FAILED_ALONE and inc.failed_alone_tests and test not in inc.failed_alone_tests:
                outcome = UNRESOLVED        # it passed that isolated run; another spec in the set failed
            if outcome == PASSED_ALONE:
                e.passed_alone += 1
            elif outcome == FAILED_ALONE:
                e.failed_alone += 1
            if inc.load is not None:
                e.loads.append(inc.load)
            if inc.ts >= cut:
                e.recent_red += 1
                if inc.outcome == PASSED_ALONE:
                    e.recent_passed += 1
            e.incidents.append(inc)
    return ledger


#: The one-solo-pass rule (user, 2026-09-24 14:20; the merge runner's FLAKE_SOLO_ONE knob): a red test
#: whose ledger already shows it passing alone this often in the window, and never failing alone, is
#: accepted after ONE isolated pass instead of two. A first-time flaker still gets the twice rule.
SOLO_MIN_PASSED = 2


def solo_decision(incidents: list[Incident], tests: list[str], since: float,
                  min_passed: int = SOLO_MIN_PASSED) -> tuple[bool, int]:
    """(every test has >= min_passed passed-alone and NO failed-alone incident since `since`, the
    smallest passed-alone count). A fail-alone counts whoever it was pinned on: a branch_defect red is
    still this test failing alone (review, 2026-09-24: app-trace failed alone at 11:19/11:24/11:30 as
    branch defects and the per-test counters, which skip those, read it as never failing)."""
    passes = {t: 0 for t in tests}
    for inc in incidents:
        if inc.ts < since:
            continue
        for t in tests:
            if t not in inc.tests:
                continue
            if inc.outcome == FAILED_ALONE and (not inc.failed_alone_tests or t in inc.failed_alone_tests):
                return False, 0
            if inc.outcome == PASSED_ALONE and not inc.branch_defect:
                passes[t] += 1
    n = min(passes.values(), default=0)
    return bool(tests) and n >= min_passed, n


_LOG_TS = re.compile(r"^\[(\d\d-\d\d \d\d:\d\d:\d\d)\]", re.M)


def solo_query(ops: str, tests: list[str], *, now: float | None = None, days: int = WINDOW_DAYS) -> tuple[bool, int]:
    """solo_decision over what the runner's outputs actually cover: the window starts at the later of
    `days` ago and the oldest log line read (fail-alones live only in the log, which is read from its
    last 40 MB - about 3 days on 2026-09-24 - so passes older than that must not count either). An
    unreadable or empty log is no."""
    now = time.time() if now is None else now
    log_path = os.path.join(ops, RUNNER_LOG)
    try:
        with open(log_path, "rb") as fh:
            fh.seek(0, 2)
            fh.seek(max(0, fh.tell() - 40_000_000))
            text = fh.read().decode("utf-8", "replace")
        year = datetime.fromtimestamp(os.path.getmtime(log_path)).year
        first = _LOG_TS.search(text)
        if not first:
            return False, 0
        covered = datetime.strptime(f"{year}-{first.group(1)}", "%Y-%m-%d %H:%M:%S").timestamp()
        log_incidents = parse_runner_log(text, year)
    except Exception:
        return False, 0
    try:
        with open(os.path.join(ops, FLAKY_JSONL), encoding="utf-8") as fh:
            jsonl_incidents = parse_flaky_jsonl(fh.read())
    except Exception:
        jsonl_incidents = []
    return solo_decision(reconcile(log_incidents, jsonl_incidents), tests, max(now - days * 86400, covered))


def load_state(path: str) -> dict[str, int]:
    """`{test: notified_at}` from a previous run. A missing or corrupt file is simply empty."""
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
    except Exception:
        return {}
    tests = data.get("tests") if isinstance(data, dict) else None
    if not isinstance(tests, dict):
        return {}
    out: dict[str, int] = {}
    for name, rec in tests.items():
        if isinstance(rec, dict):
            try:
                out[str(name)] = int(rec.get("notified_at") or 0)
            except (TypeError, ValueError):
                continue
    return out


def load_deflaked(path: str) -> dict[str, int]:
    """`{test: deflaked_at}` from `flakes.json`; empty when missing or corrupt."""
    try:
        with open(path, encoding="utf-8") as fh:
            tests = json.load(fh).get("tests") or {}
        return {str(k): int(v.get("deflaked_at") or 0) for k, v in tests.items() if isinstance(v, dict)}
    except Exception:
        return {}


def deflake_due(ledger: dict[str, Entry]) -> list[Entry]:
    """Tests whose passed-alone count in the window reached DEFLAKE_THRESHOLD, then each +DEFLAKE_STEP."""
    for e in ledger.values():
        if e.recent_passed < e.deflaked_at:      # the filed incidents aged out of the window
            e.deflaked_at = 0
    out = [e for e in ledger.values()
           if e.recent_passed >= DEFLAKE_THRESHOLD and e.recent_passed >= e.deflaked_at + DEFLAKE_STEP
           and (e.deflaked_at == 0 or e.recent_passed > e.deflaked_at)]
    return sorted(out, key=lambda e: (-e.recent_passed, e.test))


def deflake_request(e: Entry, now: float, days: int = WINDOW_DAYS) -> dict:
    """The record ops/work-runner.py turns into a deflaker dispatch (its brief carries `evidence`)."""
    slug = re.sub(r"[^a-z0-9]+", "-", e.test.lower()).strip("-")[:60]
    kind = "spec" if e.test.endswith(".e2e.mjs") else "rust"
    incs = [{"ts": datetime.fromtimestamp(i.ts).strftime("%Y-%m-%d %H:%M:%S"), "outcome": i.outcome,
             "load_before": i.load, "batch": i.batch} for i in e.incidents[-12:]]
    lines = [f"{e.test}: passed alone {e.recent_passed}x in {days} days ({e.red_in_gate} reds recorded, "
             f"failed alone {e.failed_alone}); verdict so far: {e.verdict()}.",
             "Each red below is a `TRIAGE:` line in $HACKRIFF_OPS/merge-runner.log at that time "
             "(grep -n 'TRIAGE' ... | grep '<HH:MM:SS>'), with the gate output just above it; loads are 1-min averages."]
    lines += [f"  {i['ts']}  {i['outcome']}  load {i['load_before']}" for i in incs]
    return {"ts": now, "id": f"deflake-{slug}", "test": e.test, "kind": kind, "count_7d": e.recent_passed,
            "incidents": incs, "evidence": "\n".join(lines)}


def save(path: str, ledger: dict[str, Entry]) -> bool:
    """Persist the ledger. Never raises — the runner calls this."""
    try:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        payload = {
            "updated": time.time(),
            "window_days": WINDOW_DAYS,
            "tests": {name: e.as_dict() for name, e in ledger.items()},
        }
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as fh:
            json.dump(payload, fh, indent=1, sort_keys=True)
        os.replace(tmp, path)
        return True
    except Exception:
        return False


def due(ledger: dict[str, Entry]) -> list[Entry]:
    """The tests that have crossed the threshold since they were last filed.

    Fires at `RED_THRESHOLD`, then at every `+NOTIFY_STEP` — so a test that keeps costing gates
    keeps being reported, at a rate a person can actually act on, and one that has already been
    filed goes quiet until it has cost two more.
    """
    out = []
    for e in ledger.values():
        if e.recent_red < RED_THRESHOLD:
            continue
        if e.recent_red < e.notified_at + NOTIFY_STEP:
            continue
        out.append(e)
    return sorted(out, key=lambda e: (-e.recent_red, e.test))


# ---------------------------------------------------------------------------
# Filing
# ---------------------------------------------------------------------------


def append_attention(path: str, line: str) -> bool:
    """One line in `merge-needs-attention.txt`, in the runner's own `MM-DD HH:MM  …` shape."""
    try:
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        with open(path, "a", encoding="utf-8") as fh:
            fh.write(f"{time.strftime('%m-%d %H:%M')}  {line}\n")
        return True
    except Exception:
        return False


def alert(root: str, level: str, title: str, body: str, key: str) -> bool:
    """Post through `ops/alert.py`, best effort. Shelled out — see `hkpy.gatediag.alert`."""
    script = os.path.join(root, "ops", "alert.py")
    if not os.path.exists(script):
        return False
    try:
        proc = subprocess.run(
            [sys.executable, script, level, title, body, "--key", key],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        return proc.returncode == 0
    except Exception:
        return False


def repo_root() -> str:
    return os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))


# ---------------------------------------------------------------------------
# Top level
# ---------------------------------------------------------------------------


def read_sources(ops: str) -> list[Incident]:
    """Both of the runner's outputs, reconciled into one incident list."""
    log_path = os.path.join(ops, RUNNER_LOG)
    try:
        with open(log_path, "rb") as fh:
            fh.seek(0, 2)
            size = fh.tell()
            fh.seek(max(0, size - 40_000_000))
            text = fh.read().decode("utf-8", "replace")
        year = datetime.fromtimestamp(os.path.getmtime(log_path)).year
        log_incidents = parse_runner_log(text, year)
    except Exception:
        log_incidents = []
    try:
        with open(os.path.join(ops, FLAKY_JSONL), encoding="utf-8") as fh:
            jsonl_incidents = parse_flaky_jsonl(fh.read())
    except Exception:
        jsonl_incidents = []
    return reconcile(log_incidents, jsonl_incidents)


def ledger(ops: str | None = None, *, now: float | None = None, days: int = WINDOW_DAYS) -> dict[str, Entry]:
    """The ledger as it stands, with each test's `notified_at` carried over from `flakes.json`."""
    ops = ops or gatelog.ops_dir()
    entries = build(read_sources(ops), now=now, days=days)
    state = load_state(os.path.join(ops, LEDGER_JSON))
    deflaked = load_deflaked(os.path.join(ops, LEDGER_JSON))
    for name, e in entries.items():
        e.notified_at = state.get(name, 0)
        e.deflaked_at = deflaked.get(name, 0)
    return entries


def update(ops: str | None = None, *, root: str | None = None, now: float | None = None, days: int = WINDOW_DAYS) -> list[Entry]:
    """Recompute, file whatever crossed the threshold, persist. Returns what was filed.

    Called by `ops/merge-runner.sh` after every triage, and by `just flakes --update` by hand.
    """
    ops = ops or gatelog.ops_dir()
    root = root or repo_root()
    entries = ledger(ops, now=now, days=days)
    filed = due(entries)
    for e in filed:
        append_attention(os.path.join(ops, NEEDS_FILE), e.attention_line(days))
        alert(
            root,
            "amber",
            f"FLAKY {e.test}",
            (
                f"Red in {e.recent_red} gates in {days} days "
                f"(passed alone {e.passed_alone}, failed alone {e.failed_alone}; "
                f"loads {','.join(f'{x:g}' for x in e.loads[-6:]) or 'unknown'}). "
                f"{e.verdict()}."
            ),
            f"flake:{e.test}",
        )
        e.notified_at = e.recent_red
    for e in deflake_due(entries):
        req = deflake_request(e, time.time() if now is None else now, days)
        try:
            with open(os.path.join(ops, DEFLAKE_REQUESTS), "a", encoding="utf-8") as fh:
                fh.write(json.dumps(req) + "\n")
        except Exception:
            continue
        append_attention(os.path.join(ops, NEEDS_FILE),
                         f"DEFLAKE_REQUESTED {e.test} passed alone {e.recent_passed}x in {days}d - "
                         f"the work runner dispatches a deflaker ({req['id']})")
        alert(root, "amber", f"deflaker requested: {e.test}",
              f"Passed alone {e.recent_passed}x in {days} days - accepted each time under the user's rule; "
              f"now it gets fixed. Dispatch id {req['id']}.", f"deflake:{e.test}")
        e.deflaked_at = e.recent_passed
    save(os.path.join(ops, LEDGER_JSON), entries)
    return filed


def render(entries: dict[str, Entry], *, days: int = WINDOW_DAYS) -> list[str]:
    """`just flakes` — the ledger, dearest first."""
    if not entries:
        return ["flakes: nothing triaged yet (no TRIAGE lines and no flaky.jsonl records)"]
    rows = sorted(entries.values(), key=lambda e: (-e.red_in_gate, -e.recent_red, e.test))
    out = [
        f"{'red':>3} {'7d':>3} {'alone:pass':>10} {'fail':>4}  {'last':<16} test",
        "-" * 96,
    ]
    for e in rows:
        last = time.strftime("%m-%d %H:%M", time.localtime(e.last_seen))
        out.append(
            f"{e.red_in_gate:>3} {e.recent_red:>3} {e.passed_alone:>10} {e.failed_alone:>4}"
            f"  {last:<16} {e.test}"
        )
        loads = ",".join(f"{x:g}" for x in e.loads[-6:])
        out.append(f"{'':>3} {'':>3} {'':>10} {'':>4}  {'':<16} {e.verdict()}"
                   + (f"; loads {loads}" if loads else ""))
    flagged = [e for e in rows if e.recent_red >= RED_THRESHOLD]
    out.append("")
    out.append(
        f"{len(flagged)} test(s) at or over {RED_THRESHOLD} reds in {days}d"
        + (f": {', '.join(e.test for e in flagged)}" if flagged else "")
    )
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just flakes",
        description="The flake ledger: which tests have cost gates, which way, and under what load.",
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="recompute, file anything over the threshold, and persist flakes.json "
        "(what ops/merge-runner.sh calls after each triage)",
    )
    parser.add_argument("--solo-ok", nargs="+", metavar="TEST",
                        help="exit 0 and print 'solo-ok N' when every TEST qualifies for the one-solo-pass rule "
                        "(N = its smallest passed-alone count in the window), else exit 1")
    parser.add_argument("--json", action="store_true", help="the ledger as JSON")
    parser.add_argument("--days", type=int, default=WINDOW_DAYS, help=f"window (default {WINDOW_DAYS})")
    parser.add_argument("--ops", default=None, help="the ops directory (default $HACKRIFF_OPS)")
    parser.add_argument("--root", default=None, help="repo root, for ops/alert.py")
    args = parser.parse_args(argv)

    ops = args.ops or gatelog.ops_dir()
    if args.update:
        filed = update(ops, root=args.root, days=args.days)
        for e in filed:
            print(f"flakes: FILED {e.attention_line(args.days)}")
        if not filed:
            print("flakes: nothing new over the threshold")
    entries = ledger(ops, days=args.days)
    if args.solo_ok:
        ok, n = solo_query(ops, args.solo_ok, days=args.days)
        print(f"{'solo-ok' if ok else 'solo-no'} {n}")
        return 0 if ok else 1
    if args.json:
        print(json.dumps({k: v.as_dict() for k, v in entries.items()}, indent=1, sort_keys=True))
        return 0
    for line in render(entries, days=args.days):
        print(line)
    return 0


if __name__ == "__main__":
    sys.exit(main())
