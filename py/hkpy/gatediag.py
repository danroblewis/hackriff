"""The gate diagnoses its OWN contention, per suite, from its own JUnit records.

WHY THIS EXISTS. `docs/test-speed-review-2026-09-22.md` §1 measured the one thing a wall-clock
number can never show: at the 09-21 04:55 gate, **crates the diff never touched ran 18-79x
dearer** - hk-recipe 79x, hk-dsp 20x, hk-core 18x - while the 8 tests the change actually added
cost **0 s**. The gate got 5 minutes slower and every available reading said "the change did it".
It did not. The box was loaded.

Those two stories are indistinguishable from a total, and identical in shape to a real
regression, so a person reading `gate: just test took 756s` guesses - and on 2026-09-22 guessed
wrong twice, in both directions (T-763). They are trivially distinguishable *per test*:

  * a **regression** is concentrated in the crates the diff touched, and usually in a few tests;
  * **contention** is spread across crates nobody touched, and lands hardest on the SMALL unit
    crates whose tests are pure CPU with no sleeps to hide behind (hk-recipe's 43 tests total
    0.54 s when the box is quiet - so a 4x is 1.4 s and is pure scheduler starvation).

So the rule this module implements: **compare each test to its own history, aggregate by crate,
and split the crates by whether this diff touched them.** Untouched crates over threshold are
contention and say nothing about the change; touched crates over threshold are the change's own
cost, which is exactly how T-589's `carrier_line` would have announced itself instead of being
found by hand a day later.

WHAT COUNTS AS A BASELINE. The per-test median `time` over the last N (default 5) **green** runs
of the SAME suite, matched by `classname` + `name`. Green means that suite's own JUnit reported
no failure and no error: a suite that died half way measured a prefix, not a suite (T-763's
"an aborted run is not a measurement", applied one level down). Only tests present in BOTH the
baseline and the current run are compared - which is not a limitation but the point: a test
added by this diff has no history, contributes to neither side, and therefore cannot be blamed
for a slowdown it did not cause. That is precisely the 09-21 case.

WHAT IT MUST NEVER DO. Fail a gate, or cry wolf. Every entry point is total - a malformed XML,
an empty history, a missing ops directory all degrade to "no baseline yet", never to an alarm -
and `gate.py` calls the whole thing inside `try/except` besides. Under three green runs of a
suite there is no median worth the name and this module says so rather than guessing, because a
contention alarm that fires on noise is one nobody reads by the third day.
"""

from __future__ import annotations

import os
import statistics
import subprocess
import sys
import xml.etree.ElementTree as ET
from dataclasses import dataclass, field
from typing import Any, Iterable

try:  # `python -m hkpy.gatediag`
    from . import gatelog
except ImportError:  # pragma: no cover - direct execution
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from hkpy import gatelog  # type: ignore[no-redef]

#: How many green runs of a suite make a baseline, and the floor below which there is none.
#: Three is the smallest set where a median is not just "the other one"; five is what a stable
#: suite gives inside a day of merging, and older than that the tree has usually moved.
BASELINE_RUNS = 5
MIN_BASELINE_RUNS = 3

#: A crate is reported when its matched tests cost more than this multiple of their own history.
#: 3x is deliberately far above the 1.1-1.8x that ordinary scheduling jitter produces on this
#: box (measured across 14 consecutive green runs), and far below the 18-79x of the 09-21 gate.
RATIO_THRESHOLD = 3.0

#: Two guards against a ratio that is arithmetically large and physically meaningless: a crate
#: whose whole baseline is a rounding error, or one where too few tests matched to average out.
#: They are LOW on purpose - hk-recipe's entire baseline is 0.54 s and it is the single loudest
#: contention witness this repo has, so a floor that silenced it would defeat the module.
MIN_BASELINE_SECONDS = 0.25
MIN_MATCHED_TESTS = 5

#: Report at most this many crates on one line; the rest are counted.
MAX_REPORTED = 6


# ---------------------------------------------------------------------------
# Reading what the gate already wrote
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class SuiteReport:
    """One nextest JUnit file: `$HACKRIFF_OPS/junit/<run>/<n>-<suite>-<profile>.xml`."""

    path: str
    run: str
    #: The suite identity across runs — the filename with its ordinal prefix and extension
    #: stripped, so `02-test-default.xml` and `01-test-default.xml` are the same suite. The
    #: ordinal is a position within one gate (it shifts when a class skips `lint`), never an
    #: identity.
    suite: str
    mtime: float
    #: `(classname, name) -> seconds`.
    cases: dict[tuple[str, str], float]
    failures: int
    total_seconds: float

    @property
    def green(self) -> bool:
        return self.failures == 0


def suite_key(filename: str) -> str:
    """`02-test-default.xml` -> `test-default`. See `SuiteReport.suite`."""
    base = os.path.basename(filename)
    if base.endswith(".xml"):
        base = base[:-4]
    head, sep, rest = base.partition("-")
    if sep and head.isdigit():
        return rest
    return base


def read_junit(path: str) -> SuiteReport | None:
    """Parse one JUnit file. `None` if it cannot be read — never raises."""
    try:
        root = ET.parse(path).getroot()
    except Exception:
        return None
    cases: dict[tuple[str, str], float] = {}
    failures = 0
    total = 0.0
    try:
        for tc in root.iter("testcase"):
            key = (tc.get("classname") or "", tc.get("name") or "")
            try:
                secs = float(tc.get("time") or 0.0)
            except ValueError:
                secs = 0.0
            cases[key] = secs
            total += secs
            if tc.find("failure") is not None or tc.find("error") is not None:
                failures += 1
    except Exception:
        return None
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        mtime = 0.0
    return SuiteReport(
        path=path,
        run=os.path.basename(os.path.dirname(path)),
        suite=suite_key(path),
        mtime=mtime,
        cases=cases,
        failures=failures,
        total_seconds=round(total, 2),
    )


def history(junit_root: str | None = None, *, suite: str | None = None) -> list[SuiteReport]:
    """Every readable JUnit report under `$HACKRIFF_OPS/junit/`, oldest first.

    Oldest-first by file mtime rather than by run id: run ids are random (so concurrent gates
    in different worktrees cannot collide — `gatelog.new_run_id`), which makes them useless for
    ordering. The file's own mtime is when the gate copied it, which is what "the last five
    runs" means.
    """
    root = junit_root or os.path.join(gatelog.ops_dir(), "junit")
    out: list[SuiteReport] = []
    try:
        entries = sorted(os.listdir(root))
    except OSError:
        return []
    for run in entries:
        run_dir = os.path.join(root, run)
        if not os.path.isdir(run_dir):
            continue
        try:
            names = sorted(os.listdir(run_dir))
        except OSError:
            continue
        for name in names:
            if not name.endswith(".xml"):
                continue
            if suite is not None and suite_key(name) != suite:
                continue
            rep = read_junit(os.path.join(run_dir, name))
            if rep is not None:
                out.append(rep)
    out.sort(key=lambda r: r.mtime)
    return out


# ---------------------------------------------------------------------------
# The baseline
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class Baseline:
    """Per-test median seconds over the green runs it was built from."""

    suite: str
    #: `(classname, name) -> median seconds`.
    medians: dict[tuple[str, str], float]
    runs: int
    #: Why there is no baseline, when `runs < MIN_BASELINE_RUNS`.
    reason: str = ""

    @property
    def usable(self) -> bool:
        return self.runs >= MIN_BASELINE_RUNS and bool(self.medians)


def baseline(
    reports: Iterable[SuiteReport],
    suite: str,
    *,
    exclude_run: str | None = None,
    n: int = BASELINE_RUNS,
) -> Baseline:
    """The per-test median over the last `n` GREEN reports of `suite`.

    `exclude_run` drops the run being diagnosed, so a suite never sits in its own baseline —
    without it a contended run would drag its own median up and under-report itself.

    A test is only given a median once at least `MIN_BASELINE_RUNS` of the chosen reports
    contain it: a test that appears in one of five runs has a sample, not a median, and
    comparing against it is how a flaky-by-absence test would look like a 5x regression.
    """
    green = [
        r
        for r in reports
        if r.suite == suite and r.green and (exclude_run is None or r.run != exclude_run)
    ]
    chosen = green[-n:]
    if len(chosen) < MIN_BASELINE_RUNS:
        return Baseline(
            suite,
            {},
            len(chosen),
            reason=(
                f"only {len(chosen)} green run(s) of {suite} on record "
                f"(need {MIN_BASELINE_RUNS})"
            ),
        )
    samples: dict[tuple[str, str], list[float]] = {}
    for rep in chosen:
        for key, secs in rep.cases.items():
            samples.setdefault(key, []).append(secs)
    medians = {
        k: statistics.median(v) for k, v in samples.items() if len(v) >= MIN_BASELINE_RUNS
    }
    return Baseline(suite, medians, len(chosen))


def suite_seconds_baseline(records: list[dict[str, Any]], cmd: str) -> float | None:
    """Median wall seconds for one suite command over the `suite` records that PASSED.

    The per-test view is the diagnosis; this is the sanity check beside it — the suite's own
    wall clock, from `gate-timings.jsonl`. `rc != 0` records are excluded for T-763's reason:
    a suite that stopped at the first failure timed a prefix.
    """
    secs = [
        float(r.get("seconds") or 0.0)
        for r in records
        if r.get("kind") == "suite" and r.get("cmd") == cmd and r.get("rc") == 0
    ]
    secs = [s for s in secs if s > 0]
    return round(statistics.median(secs), 1) if secs else None


# ---------------------------------------------------------------------------
# Touched vs untouched
# ---------------------------------------------------------------------------


def crate_of(classname: str) -> str:
    """The crate a JUnit `classname` belongs to — the part before any `::`.

    nextest writes the package name as the classname (`hk-api`), but a suffixed form is
    accepted too so this never depends on that staying true.
    """
    return (classname or "").split("::")[0].strip()


def touched_crates(paths: Iterable[str] | None) -> set[str] | None:
    """The crates this diff changed: every `crates/<name>/...` path in it.

    `None` means the changed-path list is unknown (a forced-full gate), in which case nothing
    can be called untouched and the module reports no contention at all. Fail quiet, not loud:
    the cost of a missed alarm here is a person reading a total, which is where we already are;
    the cost of a false one is that the next real one is ignored.
    """
    if paths is None:
        return None
    out: set[str] = set()
    for raw in paths:
        p = str(raw).strip().replace("\\", "/")
        while p.startswith("./"):
            p = p[2:]
        if not p.startswith("crates/"):
            continue
        rest = p[len("crates/"):]
        name = rest.split("/", 1)[0]
        if name:
            out.add(name)
    return out


# ---------------------------------------------------------------------------
# The diagnosis
# ---------------------------------------------------------------------------


@dataclass(frozen=True)
class CrateRatio:
    crate: str
    ratio: float
    seconds: float
    baseline_seconds: float
    tests: int
    touched: bool

    def label(self) -> str:
        return f"{self.crate} {self.ratio:.0f}x" if self.ratio >= 10 else f"{self.crate} {self.ratio:.1f}x"


@dataclass(frozen=True)
class Diagnosis:
    suite: str
    run: str
    #: False when there is no baseline — then every other field is empty and the line says so.
    have_baseline: bool
    contended: bool
    #: The untouched crates over threshold, dearest first.
    contended_crates: tuple[CrateRatio, ...] = ()
    #: The touched crates over threshold: the change's own cost, not contention.
    dearer: tuple[CrateRatio, ...] = ()
    max_untouched_ratio: float | None = None
    loadavg: float | None = None
    baseline_runs: int = 0
    #: Wall seconds this suite took vs the median of its passing history, when known.
    suite_seconds: float | None = None
    suite_baseline_seconds: float | None = None
    reason: str = ""
    #: Every crate compared, dearest first — for a `--explain` dump, not for the one line.
    all_crates: tuple[CrateRatio, ...] = field(default=(), repr=False)

    def line(self) -> str:
        """The single line the gate prints. Its three shapes are the whole vocabulary."""
        if not self.have_baseline:
            return f"gate: timing   = no baseline yet — {self.reason}"
        load = f"; load {self.loadavg:.1f}" if self.loadavg is not None else ""
        if self.contended:
            named = ", ".join(c.label() for c in self.contended_crates[:MAX_REPORTED])
            more = len(self.contended_crates) - MAX_REPORTED
            if more > 0:
                named += f", +{more} more"
            return f"gate: CONTENDED — {named} (untouched){load}"
        top = f"{self.max_untouched_ratio:.1f}x" if self.max_untouched_ratio is not None else "n/a"
        return f"gate: timing ok — max untouched crate {top}{load}"

    def dearer_line(self) -> str | None:
        """The touched-crate line, printed beside `line()` when the change itself cost time."""
        if not self.dearer:
            return None
        named = ", ".join(c.label() for c in self.dearer[:MAX_REPORTED])
        return f"gate: DEARER — {named} (touched: the change cost this)"

    def record_fields(self) -> dict[str, Any]:
        """The fields folded into `gatelog.end_record`. Added, never renamed — old readers
        of `gate_end` keep working, which is the same compatibility rule `gatelog` states."""
        return {
            "contended": self.contended,
            "max_untouched_ratio": self.max_untouched_ratio,
            "contended_crates": [[c.crate, round(c.ratio, 2)] for c in self.contended_crates],
            "dearer": [[c.crate, round(c.ratio, 2)] for c in self.dearer],
            "timing_suite": self.suite,
            "timing_baseline_runs": self.baseline_runs,
        }


def diagnose(
    current: SuiteReport,
    base: Baseline,
    *,
    touched: set[str] | None,
    loadavg: float | None = None,
    suite_seconds: float | None = None,
    suite_baseline_seconds: float | None = None,
) -> Diagnosis:
    """Compare one suite run against its baseline and split the crates by touched/untouched.

    Aggregating per crate rather than reporting per test is deliberate. One test at 8x is a
    story about that test (and often a real one); twenty tests in one untouched crate all at
    2-4x is a story about the machine, and only the aggregate tells them apart.
    """
    if not base.usable:
        return Diagnosis(
            suite=current.suite,
            run=current.run,
            have_baseline=False,
            contended=False,
            reason=base.reason or "no green runs of this suite on record",
            loadavg=loadavg,
            baseline_runs=base.runs,
            suite_seconds=suite_seconds,
            suite_baseline_seconds=suite_baseline_seconds,
        )

    agg: dict[str, list[float]] = {}
    for key, secs in current.cases.items():
        med = base.medians.get(key)
        if med is None:
            # A test this diff added has no history. It contributes to NEITHER side, so it can
            # never be blamed for, nor hide, a slowdown — the 09-21 case, where the 8 added
            # tests cost 0 s and the untouched crates cost everything.
            continue
        crate = crate_of(key[0])
        if not crate:
            continue
        slot = agg.setdefault(crate, [0.0, 0.0, 0.0])
        slot[0] += secs
        slot[1] += med
        slot[2] += 1

    ratios: list[CrateRatio] = []
    for crate, (secs, base_secs, n) in agg.items():
        if base_secs < MIN_BASELINE_SECONDS or n < MIN_MATCHED_TESTS:
            continue
        ratios.append(
            CrateRatio(
                crate=crate,
                ratio=secs / base_secs,
                seconds=round(secs, 2),
                baseline_seconds=round(base_secs, 2),
                tests=int(n),
                touched=bool(touched and crate in touched),
            )
        )
    ratios.sort(key=lambda c: -c.ratio)

    # `touched is None` means we do not know the diff: nothing may be called untouched.
    untouched = [c for c in ratios if not c.touched] if touched is not None else []
    over = tuple(c for c in untouched if c.ratio > RATIO_THRESHOLD)
    dear = tuple(c for c in ratios if c.touched and c.ratio > RATIO_THRESHOLD)
    return Diagnosis(
        suite=current.suite,
        run=current.run,
        have_baseline=True,
        contended=bool(over),
        contended_crates=over,
        dearer=dear,
        max_untouched_ratio=round(untouched[0].ratio, 2) if untouched else None,
        loadavg=loadavg,
        baseline_runs=base.runs,
        suite_seconds=suite_seconds,
        suite_baseline_seconds=suite_baseline_seconds,
        reason=(
            "the changed-path list is unknown, so no crate can be called untouched"
            if touched is None
            else ""
        ),
        all_crates=tuple(ratios),
    )


def merge(diagnoses: Iterable[Diagnosis]) -> Diagnosis | None:
    """One verdict for a whole gate: the worst suite wins.

    A gate is several suites and the record carries one answer, so the rule has to be stated:
    contention anywhere contends, and among contended suites the one with the dearest untouched
    crate is the one worth naming.
    """
    items = [d for d in diagnoses if d.have_baseline]
    if not items:
        return next(iter(diagnoses), None)
    contended = [d for d in items if d.contended]
    if contended:
        return max(contended, key=lambda d: d.contended_crates[0].ratio)
    return max(items, key=lambda d: d.max_untouched_ratio or 0.0)


# ---------------------------------------------------------------------------
# Alerts
# ---------------------------------------------------------------------------


def alert(root: str, level: str, title: str, body: str, key: str) -> bool:
    """Post one Discord alert through `ops/alert.py`, best effort.

    Shelled out rather than imported: `ops/` is not a package on this interpreter's path, and
    a diagnostic must not acquire an import of the alerting layer that could fail at gate time.
    Never raises, and returns False when anything at all goes wrong.
    """
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


def announce(diag: Diagnosis, *, root: str, run_id: str) -> None:
    """The alerts for one diagnosis: ONE amber for contention, one info for a touched crate.

    One per gate run, keyed by run id, because the interesting fact is "this gate ran on a
    loaded box", not "this suite did" — and `ops/alert.py` dedupes by key for 30 minutes on
    top of that.
    """
    if not diag.have_baseline:
        return
    if diag.contended:
        named = ", ".join(
            f"{c.crate} {c.ratio:.1f}x ({c.baseline_seconds:.1f}s -> {c.seconds:.1f}s, {c.tests} tests)"
            for c in diag.contended_crates[:MAX_REPORTED]
        )
        load = f"load {diag.loadavg:.1f}" if diag.loadavg is not None else "load unknown"
        alert(
            root,
            "amber",
            f"gate CONTENDED ({diag.suite})",
            f"Crates this diff never touched ran dearer than their own history: {named}. "
            f"{load}. The suite's time is the box, not the change.",
            f"gate:contended:{run_id}",
        )
    if diag.dearer:
        named = ", ".join(
            f"{c.crate} {c.ratio:.1f}x ({c.baseline_seconds:.1f}s -> {c.seconds:.1f}s)"
            for c in diag.dearer[:MAX_REPORTED]
        )
        alert(
            root,
            "info",
            f"gate DEARER ({diag.suite})",
            f"Crates THIS DIFF TOUCHED cost more than their history: {named}. "
            "That is the change's own cost, not contention.",
            f"gate:dearer:{run_id}",
        )


# ---------------------------------------------------------------------------
# What gate.py calls
# ---------------------------------------------------------------------------


def diagnose_suite(
    junit_path: str,
    *,
    changed_paths: Iterable[str] | None,
    junit_root: str | None = None,
    records: list[dict[str, Any]] | None = None,
    cmd: str | None = None,
    loadavg: float | None = None,
) -> Diagnosis | None:
    """Diagnose one JUnit file the gate has just kept. The whole entry point gate.py needs.

    Returns `None` only when the file itself is unreadable — everything else, including an
    empty history, comes back as a Diagnosis that says "no baseline yet".
    """
    current = read_junit(junit_path)
    if current is None:
        return None
    hist = history(junit_root, suite=current.suite)
    base = baseline(hist, current.suite, exclude_run=current.run)
    if loadavg is None:
        load = gatelog.loadavg()
        loadavg = load[0] if load else None
    suite_base = (
        suite_seconds_baseline(records, cmd) if records is not None and cmd else None
    )
    return diagnose(
        current,
        base,
        touched=touched_crates(changed_paths),
        loadavg=loadavg,
        suite_seconds=current.total_seconds,
        suite_baseline_seconds=suite_base,
    )


def main(argv: list[str] | None = None) -> int:
    """`python -m hkpy.gatediag [--run ID] [--files a b c]` — diagnose recorded runs by hand.

    With no arguments it walks every recorded run in order and prints the verdict each one
    would have had, which is how the module was calibrated against real history.
    """
    import argparse

    parser = argparse.ArgumentParser(prog="hkpy.gatediag", description=__doc__)
    parser.add_argument("--run", help="diagnose one run id from $HACKRIFF_OPS/junit/")
    parser.add_argument("--junit-root", default=None)
    parser.add_argument(
        "--files", nargs="*", default=None, help="the changed paths of that gate's diff"
    )
    parser.add_argument("--explain", action="store_true", help="print every crate compared")
    args = parser.parse_args(argv)

    reports = history(args.junit_root)
    if not reports:
        print("gate: timing   = no JUnit records yet")
        return 0
    seen: dict[str, list[SuiteReport]] = {}
    for rep in reports:
        if args.run and rep.run != args.run:
            seen.setdefault(rep.suite, []).append(rep)
            continue
        base = baseline(seen.get(rep.suite, []), rep.suite, exclude_run=rep.run)
        diag = diagnose(rep, base, touched=touched_crates(args.files))
        print(f"{rep.run} {rep.suite:24s} {diag.line()}")
        dl = diag.dearer_line()
        if dl:
            print(f"{'':13s} {'':24s} {dl}")
        if args.explain:
            for c in diag.all_crates[:12]:
                print(
                    f"{'':13s}   {c.crate:14s} {c.ratio:6.2f}x  {c.baseline_seconds:8.2f}s"
                    f" -> {c.seconds:8.2f}s  ({c.tests} tests"
                    f"{', touched' if c.touched else ''})"
                )
        seen.setdefault(rep.suite, []).append(rep)
    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
