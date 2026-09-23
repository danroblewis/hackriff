"""The gate's self-diagnosis — `py/hkpy/gatediag.py`.

The cases that matter are the three the module exists to tell apart, and they are written here
as the shapes they had in real life:

  * **contention** — the 09-21 04:55 signature from `docs/test-speed-review-2026-09-22.md` §1:
    crates the diff never touched running ~20x dearer, while the tests the diff ADDED cost 0 s.
    A total cannot distinguish that from a regression; this must.
  * **a real cost** — only the touched crate is dearer. That is T-589's `carrier_line`, which
    took a day to find by hand, and it must read as DEARER, never as contention.
  * **not enough history** — a false contention alarm is worse than none, because it is the
    reason nobody reads the third one. Under three green runs the answer is "no baseline yet".
"""

from __future__ import annotations

import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from hkpy import gatediag, gatelog  # noqa: E402


# ---------------------------------------------------------------------------
# Synthetic JUnit
# ---------------------------------------------------------------------------


def write_junit(path: str, cases: dict[str, list[tuple[str, float]]], *, failed: str = "") -> str:
    """Write a nextest-shaped JUnit file. `cases` is `{crate: [(test name, seconds), ...]}`."""
    os.makedirs(os.path.dirname(path), exist_ok=True)
    total = sum(s for tests in cases.values() for _, s in tests)
    out = ['<?xml version="1.0" encoding="UTF-8"?>']
    n = sum(len(v) for v in cases.values())
    out.append(f'<testsuites name="nextest-run" tests="{n}" failures="{1 if failed else 0}" time="{total}">')
    for crate, tests in cases.items():
        out.append(f'<testsuite name="{crate}" tests="{len(tests)}">')
        for name, secs in tests:
            if name == failed:
                out.append(
                    f'<testcase name="{name}" classname="{crate}" time="{secs}">'
                    '<failure message="assertion failed">boom</failure></testcase>'
                )
            else:
                out.append(f'<testcase name="{name}" classname="{crate}" time="{secs}"/>')
        out.append("</testsuite>")
    out.append("</testsuites>")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("\n".join(out))
    return path


def quiet_run(scale: float = 1.0, extra: dict | None = None) -> dict[str, list[tuple[str, float]]]:
    """A green run of the workspace suite, at `scale` times its usual cost.

    Proportions taken from the real records: hk-recipe's 43 tests total ~0.54 s, hk-dsp's 165
    total ~16 s, hk-core's 174 total ~5.5 s, hk-estimate's 99 total ~460 s. The small crates
    matter most — they are the ones contention shows up in first, having no sleeps to hide in.
    """
    cases = {
        "hk-recipe": [(f"recipe_{i}", round(0.0125 * scale, 4)) for i in range(43)],
        "hk-dsp": [(f"dsp_{i}", round(0.097 * scale, 4)) for i in range(165)],
        "hk-core": [(f"core_{i}", round(0.0316 * scale, 4)) for i in range(174)],
        "hk-estimate": [(f"estimate_{i}", round(4.65 * scale, 4)) for i in range(99)],
    }
    if extra:
        cases.update(extra)
    return cases


def seed(root: str, n: int, *, suite: str = "test-default", scale: float = 1.0) -> None:
    """`n` green runs of `suite`, each in its own run directory, in mtime order."""
    for i in range(n):
        path = os.path.join(root, f"run{i:03d}", f"02-{suite}.xml")
        write_junit(path, quiet_run(scale))
        os.utime(path, (1_790_000_000 + i * 600, 1_790_000_000 + i * 600))


# ---------------------------------------------------------------------------
# The three verdicts
# ---------------------------------------------------------------------------


def test_the_09_21_signature_reads_as_contended(tmp_path):
    """Untouched crates ~20x dearer, the diff's own new tests costing 0 s -> CONTENDED.

    This is the exact reading of the 09-21 04:55 gate: the change added tests that cost nothing
    and the suite still got 5 minutes dearer, because hk-recipe, hk-dsp and hk-core — none of
    them in the diff — each ran many times their own history.
    """
    root = str(tmp_path / "junit")
    seed(root, 5)
    # The gate under diagnosis: everything 20x, plus eight brand-new tests in the crate the
    # diff DID touch, costing 0 s. Nothing in the diff can explain the cost.
    slow = quiet_run(20.0)
    slow["hk-classify"] = [(f"new_sweep_{i}", 0.0) for i in range(8)]
    current = write_junit(str(tmp_path / "junit" / "now" / "02-test-default.xml"), slow)
    os.utime(current, (1_790_009_999, 1_790_009_999))

    diag = gatediag.diagnose_suite(
        current, changed_paths=["crates/hk-classify/src/sweep.rs"], junit_root=root
    )
    assert diag is not None and diag.have_baseline
    assert diag.contended
    named = [c.crate for c in diag.contended_crates]
    assert {"hk-recipe", "hk-dsp", "hk-core"} <= set(named)
    # The crate the diff DID touch is not in the contention list, and its new tests are not
    # blamed: they have no baseline, so they weigh nothing on either side.
    assert "hk-classify" not in named
    assert not diag.dearer
    assert diag.max_untouched_ratio is not None and diag.max_untouched_ratio > 15
    assert diag.line().startswith("gate: CONTENDED — ")
    assert "(untouched)" in diag.line()


def test_only_the_touched_crate_is_dearer_so_it_is_not_contention(tmp_path):
    """T-589's shape: the change cost the time. DEARER, and explicitly not CONTENDED."""
    root = str(tmp_path / "junit")
    seed(root, 5)
    slow = quiet_run(1.0)
    slow["hk-estimate"] = [(f"estimate_{i}", 4.65 * 3.2) for i in range(99)]
    current = write_junit(str(tmp_path / "junit" / "now" / "02-test-default.xml"), slow)
    os.utime(current, (1_790_009_999, 1_790_009_999))

    diag = gatediag.diagnose_suite(
        current, changed_paths=["crates/hk-estimate/src/receiver.rs"], junit_root=root
    )
    assert diag is not None and diag.have_baseline
    assert not diag.contended
    assert [c.crate for c in diag.dearer] == ["hk-estimate"]
    assert diag.dearer[0].ratio > 3
    assert diag.line().startswith("gate: timing ok — max untouched crate")
    line = diag.dearer_line()
    assert line is not None
    assert "hk-estimate" in line and "touched: the change cost this" in line


def test_too_few_green_runs_is_no_baseline_never_an_alarm(tmp_path):
    """Two green runs is a sample, not a median. It must never produce a verdict."""
    root = str(tmp_path / "junit")
    seed(root, 2)
    current = write_junit(
        str(tmp_path / "junit" / "now" / "02-test-default.xml"), quiet_run(50.0)
    )
    os.utime(current, (1_790_009_999, 1_790_009_999))

    diag = gatediag.diagnose_suite(current, changed_paths=[], junit_root=root)
    assert diag is not None
    assert not diag.have_baseline
    assert not diag.contended
    assert diag.max_untouched_ratio is None
    assert diag.line().startswith("gate: timing   = no baseline yet")
    assert diag.dearer_line() is None


def test_an_empty_history_is_no_baseline_not_a_crash(tmp_path):
    root = str(tmp_path / "junit")
    os.makedirs(root, exist_ok=True)
    current = write_junit(
        str(tmp_path / "junit" / "now" / "02-test-default.xml"), quiet_run()
    )
    diag = gatediag.diagnose_suite(current, changed_paths=[], junit_root=root)
    assert diag is not None and not diag.have_baseline


# ---------------------------------------------------------------------------
# The rules the verdicts rest on
# ---------------------------------------------------------------------------


def test_a_red_run_is_never_a_baseline(tmp_path):
    """A suite that failed measured a prefix, not a suite (T-763) — it cannot set the median."""
    root = str(tmp_path / "junit")
    seed(root, 3)
    red = os.path.join(root, "red", "02-test-default.xml")
    write_junit(red, quiet_run(40.0), failed="recipe_0")
    os.utime(red, (1_790_005_000, 1_790_005_000))

    reports = gatediag.history(root, suite="test-default")
    assert [r.green for r in reports].count(False) == 1
    base = gatediag.baseline(reports, "test-default")
    assert base.usable and base.runs == 3
    # The 40x red run left no trace in the median.
    assert base.medians[("hk-recipe", "recipe_0")] < 0.02


def test_a_suite_is_never_in_its_own_baseline(tmp_path):
    root = str(tmp_path / "junit")
    seed(root, 5)
    reports = gatediag.history(root, suite="test-default")
    assert "run004" in {r.run for r in reports}
    assert gatediag.baseline(reports, "test-default").runs == 5
    # Without this exclusion a contended run drags its OWN median up and under-reports itself.
    assert gatediag.baseline(reports, "test-default", exclude_run="run004").runs == 4


def test_an_unknown_diff_can_call_nothing_untouched(tmp_path):
    """A forced-full gate has no path list. Fail QUIET: no crate is untouched, so no alarm."""
    root = str(tmp_path / "junit")
    seed(root, 5)
    current = write_junit(
        str(tmp_path / "junit" / "now" / "02-test-default.xml"), quiet_run(20.0)
    )
    os.utime(current, (1_790_009_999, 1_790_009_999))
    diag = gatediag.diagnose_suite(current, changed_paths=None, junit_root=root)
    assert diag is not None and diag.have_baseline
    assert not diag.contended
    assert diag.max_untouched_ratio is None
    assert "unknown" in diag.reason


def test_touched_crates_reads_the_diff():
    assert gatediag.touched_crates(
        ["crates/hk-dsp/src/fft.rs", "./crates/hk-core/Cargo.toml", "ui/src/app.ts", "docs/10.md"]
    ) == {"hk-dsp", "hk-core"}
    assert gatediag.touched_crates([]) == set()
    assert gatediag.touched_crates(None) is None


def test_suite_key_ignores_the_ordinal_prefix():
    # `01-` and `02-` are a position inside ONE gate (it shifts when a class skips `lint`),
    # never an identity — a `full` gate's acceptance suite must baseline against a `ui` gate's.
    assert gatediag.suite_key("02-test-default.xml") == "test-default"
    assert gatediag.suite_key("01-acceptance-ci-default.xml") == "acceptance-ci-default"
    assert gatediag.suite_key("/a/b/03-acceptance-ci-default.xml") == "acceptance-ci-default"


def test_a_trivial_crate_cannot_produce_a_ratio(tmp_path):
    """0.01 s -> 0.05 s is 5x and means nothing. The floors exist to keep that out."""
    root = str(tmp_path / "junit")
    for i in range(5):
        p = os.path.join(root, f"r{i}", "02-test-default.xml")
        write_junit(p, {"hk-tiny": [("t0", 0.001), ("t1", 0.001)], **quiet_run()})
        os.utime(p, (1_790_000_000 + i * 600, 1_790_000_000 + i * 600))
    current = write_junit(
        str(tmp_path / "junit" / "now" / "02-test-default.xml"),
        {"hk-tiny": [("t0", 0.5), ("t1", 0.5)], **quiet_run()},
    )
    os.utime(current, (1_790_009_999, 1_790_009_999))
    diag = gatediag.diagnose_suite(current, changed_paths=[], junit_root=root)
    assert diag is not None and diag.have_baseline
    assert "hk-tiny" not in [c.crate for c in diag.all_crates]
    assert not diag.contended


def test_a_malformed_junit_file_is_skipped_not_fatal(tmp_path):
    bad = tmp_path / "junit" / "x" / "02-test-default.xml"
    bad.parent.mkdir(parents=True)
    bad.write_text("<testsuites>not xml at all")
    assert gatediag.read_junit(str(bad)) is None
    assert gatediag.history(str(tmp_path / "junit")) == []
    assert gatediag.diagnose_suite(str(bad), changed_paths=[], junit_root=str(tmp_path)) is None


# ---------------------------------------------------------------------------
# The verdict, the record and the alert
# ---------------------------------------------------------------------------


def test_the_worst_suite_wins_the_gate_verdict(tmp_path):
    root = str(tmp_path / "junit")
    seed(root, 5)
    seed(root, 5, suite="acceptance-ci-default")
    calm = write_junit(str(tmp_path / "junit" / "n1" / "03-acceptance-ci-default.xml"), quiet_run())
    os.utime(calm, (1_790_009_999, 1_790_009_999))
    loud = write_junit(str(tmp_path / "junit" / "n2" / "02-test-default.xml"), quiet_run(20.0))
    os.utime(loud, (1_790_009_999, 1_790_009_999))

    a = gatediag.diagnose_suite(calm, changed_paths=[], junit_root=root)
    b = gatediag.diagnose_suite(loud, changed_paths=[], junit_root=root)
    verdict = gatediag.merge([a, b])
    assert verdict is not None and verdict.contended
    assert verdict.suite == "test-default"


def test_the_verdict_is_recorded_without_renaming_a_field(tmp_path, monkeypatch):
    """`contended` / `max_untouched_ratio` / `dearer` are ADDED to gate_end. Old keys survive."""
    root = str(tmp_path / "junit")
    seed(root, 5)
    current = write_junit(str(tmp_path / "junit" / "now" / "02-test-default.xml"), quiet_run(20.0))
    os.utime(current, (1_790_009_999, 1_790_009_999))
    diag = gatediag.diagnose_suite(current, changed_paths=[], junit_root=root)
    assert diag is not None

    rec = gatelog.end_record(
        "abc123", klass="full", phase="all", seconds=1234.5, rc=0, extra=diag.record_fields()
    )
    # Everything a pre-existing reader (`hkpy.cycletime`, the dashboard) relies on:
    for key in ("kind", "run", "ts", "class", "phase", "seconds", "rc", "result", "loadavg"):
        assert key in rec
    assert rec["kind"] == "gate_end" and rec["result"] == "pass" and rec["seconds"] == 1234.5
    assert rec["contended"] is True
    assert rec["max_untouched_ratio"] > 15
    assert rec["dearer"] == []
    assert [c[0] for c in rec["contended_crates"]]

    log = str(tmp_path / "gate-timings.jsonl")
    assert gatelog.append(rec, log)
    back = json.loads(open(log).read().strip())
    assert back["contended"] is True

    # And `extra` can never overwrite one of them.
    clash = gatelog.end_record(
        "abc123", klass="full", phase="all", seconds=1.0, rc=0, extra={"seconds": 99, "rc": 7}
    )
    assert clash["seconds"] == 1.0 and clash["rc"] == 0


def test_alerting_is_best_effort_and_never_raises(tmp_path, monkeypatch):
    # No ops/alert.py under this root at all — the commonest way this can go wrong.
    assert gatediag.alert(str(tmp_path), "amber", "t", "b", "k") is False

    posted = []
    monkeypatch.setattr(gatediag, "alert", lambda *a, **k: posted.append(a) or True)
    root = str(tmp_path / "junit")
    seed(root, 5)
    current = write_junit(str(tmp_path / "junit" / "now" / "02-test-default.xml"), quiet_run(20.0))
    os.utime(current, (1_790_009_999, 1_790_009_999))
    diag = gatediag.diagnose_suite(current, changed_paths=[], junit_root=root)
    assert diag is not None
    gatediag.announce(diag, root=str(tmp_path), run_id="run-1")
    assert len(posted) == 1
    assert posted[0][1] == "amber"
    assert posted[0][4] == "gate:contended:run-1"

    # No baseline -> nothing is announced at all.
    posted.clear()
    gatediag.announce(
        gatediag.Diagnosis(suite="s", run="r", have_baseline=False, contended=False),
        root=str(tmp_path),
        run_id="run-2",
    )
    assert posted == []


def test_suite_seconds_baseline_ignores_failed_runs():
    records = [
        {"kind": "suite", "cmd": "just test", "seconds": 400.0, "rc": 0},
        {"kind": "suite", "cmd": "just test", "seconds": 460.0, "rc": 0},
        {"kind": "suite", "cmd": "just test", "seconds": 5.0, "rc": 101},
        {"kind": "suite", "cmd": "just lint", "seconds": 25.0, "rc": 0},
        {"kind": "gate_end", "cmd": "just test", "seconds": 9999.0, "rc": 0},
    ]
    assert gatediag.suite_seconds_baseline(records, "just test") == 430.0
    assert gatediag.suite_seconds_baseline(records, "just nothing") is None
