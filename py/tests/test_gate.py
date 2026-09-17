"""The `just gate` classifier (T-396).

A gate gets tested like a gate: every assertion below is on the **suites chosen**, not on
an exit status, because a classifier that picks the wrong suites and exits 0 is exactly the
failure this command exists to prevent.

The three cases a naive implementation gets wrong have their own tests — `fixtures/`,
`justfile` and `.github/` are each full, not cheap — alongside the fail-closed case that an
invented path nobody has classified is full.
"""

from __future__ import annotations

import pytest

from hkpy.gate import (
    DOCS,
    FULL,
    PHASE_ACCEPTANCE,
    PHASE_CHECK,
    UI,
    Decision,
    classify,
    classify_path,
    render,
    Source,
)


def suites(*paths, phase="all"):
    """The suites `just gate` would run for this changed-file list, as strings."""
    return [" ".join(c) for c in classify(paths).commands(phase)]


# --------------------------------------------------------------------------- classes


def test_ui_only_runs_the_ui_suite_alone():
    d = classify(["ui/src/app/time_nav.ts", "ui/test/run.mjs", "ui/package.json"])
    assert d.label == UI
    assert [" ".join(c) for c in d.commands()] == ["just test-ui"]
    # And nothing in the acceptance phase, so CI's acceptance job is a fast no-op.
    assert d.commands(PHASE_ACCEPTANCE) == []


def test_crates_run_the_full_gate():
    d = classify(["crates/hk-detect/src/lib.rs"])
    assert d.label == FULL
    assert [" ".join(c) for c in d.commands(PHASE_CHECK)] == ["just lint", "just test"]
    assert [" ".join(c) for c in d.commands(PHASE_ACCEPTANCE)] == ["just acceptance-ci"]


def test_api_contract_doc_is_full_not_docs():
    # docs/api.md is the client/server contract, not prose.
    assert classify(["docs/api.md"]).label == FULL
    assert suites("docs/api.md") == [
        "just lint",
        "just test",
        "just acceptance-ci",
    ]


def test_docs_only_has_no_suite_and_says_so():
    d = classify(["docs/10-test-strategy.md", "docs/planning-log.md", "docs/tasks.yaml"])
    assert d.label == DOCS
    assert d.commands() == []
    text = "\n".join(render(d, Source("test", []), "all"))
    assert "no link checker" in text


def test_python_only_runs_the_python_suites():
    assert suites("py/hkpy/synth/fsk.py", "py/tests/test_synth.py") == [
        "just lint-py",
        "just test-py",
    ]


# ------------------------------------------------------------------- fails closed


@pytest.mark.parametrize(
    "path",
    [
        "newdir/x.rs",
        "newdir/nested/thing.ts",
        "Makefile",
        "README.md",
        "CLAUDE.md",
        ".gitattributes",
        "spikes/s9-whatever/main.rs",
        "prompts/model-selection.md",
        "tools/sweep_plot.py",
    ],
)
def test_unclassified_paths_fail_closed_to_full(path):
    """Nothing said is never permissive.

    A top-level directory nobody thought to classify must not inherit the cheapest gate.
    """
    klass, reason = classify_path(path)
    assert klass == FULL, f"{path} should fail closed to the full gate"
    assert classify([path]).label == FULL


def test_fixtures_are_full_because_the_suites_read_them():
    # The case a naive implementation gets wrong #1: fixtures look like data, but they are
    # acceptance *input* (T-317, T-373, T-382 all changed fixture metadata).
    d = classify(["fixtures/hackrf/2026-09-13/fm_100p8M.sigmf-meta"])
    assert d.label == FULL
    assert "acceptance input" in dict((p, r) for p, _, r in d.files)[
        "fixtures/hackrf/2026-09-13/fm_100p8M.sigmf-meta"
    ]


def test_justfile_is_full_because_it_is_the_gate():
    # #2: the gate must not be able to certify its own weakening.
    d = classify(["justfile"])
    assert d.label == FULL
    assert "gate itself" in d.files[0][2]


def test_github_workflows_are_full_because_they_are_the_gate():
    # #3: same reason, other half of the gate.
    assert classify([".github/workflows/ci.yml"]).label == FULL
    assert classify([".github/dependabot.yml"]).label == FULL


def test_the_classifier_itself_is_full_not_python_only():
    """This file and gate.py are the gate, so they escape the cheap `py` class."""
    assert classify(["py/hkpy/gate.py"]).label == FULL
    assert classify(["py/tests/test_gate.py"]).label == FULL


@pytest.mark.parametrize(
    "path",
    [
        ".config/nextest.toml",
        "tests/e2e/tests/acceptance_m0.rs",
        "plugins/readsb/manifest.toml",
        "recipes/wfm.yaml",
        "Cargo.toml",
        "Cargo.lock",
    ],
)
def test_supporting_infrastructure_is_full(path):
    assert classify([path]).label == FULL


# ------------------------------------------------------------------------- mixtures


def test_ui_plus_crates_is_full():
    d = classify(["ui/src/app/main.ts", "crates/hk-api/src/lib.rs"])
    assert d.label == FULL
    assert suites("ui/src/app/main.ts", "crates/hk-api/src/lib.rs") == [
        "just lint",
        "just test",
        "just acceptance-ci",
    ]


def test_ui_plus_docs_runs_the_union_not_the_full_gate():
    # Neither class can move the Rust path, so the union is the honest answer.
    d = classify(["ui/src/app/main.ts", "docs/14-ui-rewrite.md"])
    assert d.label == "ui+docs"
    assert [" ".join(c) for c in d.commands()] == ["just test-ui"]


def test_ui_plus_py_runs_both_cheap_suites():
    d = classify(["ui/src/app/main.ts", "py/hkpy/synth/tone.py"])
    assert d.label == "ui+py"
    assert [" ".join(c) for c in d.commands()] == [
        "just lint-py",
        "just test-ui",
        "just test-py",
    ]


def test_one_unclassified_file_drags_a_ui_change_to_full():
    d = classify(["ui/src/app/main.ts", "newdir/x.rs"])
    assert d.label == FULL
    # Only the file that forced it is reported as deciding — the point of printing them.
    assert [p for p, _, _ in d.deciding()] == ["newdir/x.rs"]


# ---------------------------------------------------------------------- empty + shape


def test_empty_diff_is_a_clear_no_op():
    d = classify([])
    assert d.is_empty
    assert d.label == "empty"
    assert d.commands() == []
    assert d.commands(PHASE_ACCEPTANCE) == []
    assert "nothing changed" in "\n".join(render(d, Source("test", []), "all"))


def test_blank_and_duplicate_paths_are_ignored():
    assert classify(["", "  ", "./ui/src/a.ts", "ui/src/a.ts"]).label == UI
    assert len(classify(["./ui/src/a.ts", "ui/src/a.ts"]).files) == 1


def test_renames_and_deletions_count_as_touching_both_paths():
    # The caller passes both sides (git diff --no-renames / status R entries); the
    # classifier must treat each as a touched path.
    assert classify(["crates/hk-dsp/src/old.rs", "crates/hk-dsp/src/new.rs"]).label == FULL
    assert classify(["ui/src/old.ts", "fixtures/gone.sigmf-data"]).label == FULL


def test_forced_full_prints_its_reason():
    d = Decision(label=FULL, classes=(FULL,), files=(), forced_reason="no base ref")
    text = "\n".join(render(d, Source("CI push build", None), "all"))
    assert "no base ref" in text
    assert [" ".join(c) for c in d.commands()] == [
        "just lint",
        "just test",
        "just acceptance-ci",
    ]


def test_every_class_declares_both_phases():
    from hkpy.gate import CLASS_ORDER, SUITES

    for klass in CLASS_ORDER:
        assert set(SUITES[klass]) == {PHASE_CHECK, PHASE_ACCEPTANCE}


def test_the_decision_is_always_printed_before_anything_runs():
    d = classify(["ui/src/app/main.ts"])
    text = "\n".join(render(d, Source("merge base with main + uncommitted", []), "all"))
    assert "class    = ui" in text
    assert "ui/src/app/main.ts" in text
    assert "just test-ui" in text
    assert "merge base with main" in text
