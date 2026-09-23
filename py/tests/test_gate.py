"""The `just gate` classifier (T-396).

A gate gets tested like a gate: every assertion below is on the **suites chosen**, not on
an exit status, because a classifier that picks the wrong suites and exits 0 is exactly the
failure this command exists to prevent.

The three cases a naive implementation gets wrong have their own tests — `fixtures/`,
`justfile` and `.github/` are each full, not cheap — alongside the fail-closed case that an
invented path nobody has classified is full.
"""

from __future__ import annotations

from pathlib import Path

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
    forced_full,
    merge_source,
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
    assert [" ".join(c) for c in d.commands()] == ["just test-ui", "just test-ui-e2e"]
    # The check phase stays the cheap one — npm + tsc + the node suites — and the browser
    # tier (T-455) lands in acceptance, where the suites that need a built binary live.
    assert [" ".join(c) for c in d.commands(PHASE_CHECK)] == ["just test-ui"]
    assert [" ".join(c) for c in d.commands(PHASE_ACCEPTANCE)] == ["just test-ui-e2e"]


def test_crates_run_the_full_gate():
    d = classify(["crates/hk-detect/src/lib.rs"])
    assert d.label == FULL
    assert [" ".join(c) for c in d.commands(PHASE_CHECK)] == ["just lint", "just test"]
    assert [" ".join(c) for c in d.commands(PHASE_ACCEPTANCE)] == [
        "just acceptance-ci",
        "just test-ui-e2e",
    ]


def test_api_contract_doc_is_full_not_docs():
    # docs/api.md is the client/server contract, not prose.
    assert classify(["docs/api.md"]).label == FULL
    assert suites("docs/api.md") == [
        "just lint",
        "just test",
        "just acceptance-ci",
        "just test-ui-e2e",
    ]


def test_docs_only_has_no_suite_and_says_so():
    # PROSE only. docs/tasks.yaml used to be in this list, which is precisely how a
    # malformed board reached main: the one test that would have caught it never ran
    # (T-561). A file the tooling parses is data, not prose - see the test below.
    d = classify(["docs/10-test-strategy.md", "docs/planning-log.md"])
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
        "just test-ui-e2e",
    ]


def test_ui_plus_docs_runs_the_union_not_the_full_gate():
    # Neither class can move the Rust path, so the union is the honest answer.
    d = classify(["ui/src/app/main.ts", "docs/14-ui-rewrite.md"])
    assert d.label == "ui+docs"
    assert [" ".join(c) for c in d.commands()] == ["just test-ui", "just test-ui-e2e"]


def test_ui_plus_py_runs_both_cheap_suites():
    d = classify(["ui/src/app/main.ts", "py/hkpy/synth/tone.py"])
    assert d.label == "ui+py"
    assert [" ".join(c) for c in d.commands()] == [
        "just lint-py",
        "just test-ui",
        "just test-py",
        "just test-ui-e2e",
    ]


def test_one_unclassified_file_drags_a_ui_change_to_full():
    d = classify(["ui/src/app/main.ts", "newdir/x.rs"])
    assert d.label == FULL
    # Only the file that forced it is reported as deciding — the point of printing them.
    assert [p for p, _, _ in d.deciding()] == ["newdir/x.rs"]


# ------------------------------------------------- the coordinator's merge gate (T-424)
#
# `--merge` is a NARROWING — it classifies fewer paths than the default, and the classifier
# is monotone (any FULL path forces FULL), so a smaller input set can only ever be cheaper
# or equal. T-396's rule is that the gate must not certify its own weakening, so the
# narrowing is tested from both ends: that it does narrow (untracked `tools/` no longer
# forces full), and that every guard which keeps it honest holds.


def _merge(staged, uncommitted, state="MERGE_HEAD"):
    """The Source `just gate-merge` would build from these git facts."""
    return merge_source(staged, uncommitted, state)


def test_merge_gate_classifies_the_index_not_the_working_tree():
    # The measured T-424 case: a docs-only merge in a tree that permanently holds untracked
    # `tools/` and an untracked diagnostic capture under `fixtures/`.
    src = _merge(
        ["docs/adr/0009-thing.md"],
        [
            "docs/adr/0009-thing.md",
            "tools/fm_rx.py",
            "fixtures/hackrf/capture-2026-09-16-101p3-diag/iq.sigmf-meta",
        ],
    )
    assert src.forced is None
    d = classify(src.paths)
    assert d.label == DOCS
    assert d.commands() == []


def test_merge_gate_prints_every_path_it_did_not_classify():
    # Silence is the failure mode that would make this a hidden ignore list. The paths it
    # narrowed away are printed WITH the class they would have had, so the choice is
    # visible and answerable — the same rule as printing the deciding files.
    src = _merge(["docs/a.md"], ["docs/a.md", "tools/fm_rx.py", "fixtures/x.sigmf-meta"])
    assert src.outside == ("fixtures/x.sigmf-meta", "tools/fm_rx.py")
    text = "\n".join(render(classify(src.paths), src, "all"))
    assert "tools/fm_rx.py" in text and "would be full" in text
    assert "fixtures/x.sigmf-meta" in text
    assert "NOT in this merge" in text


def test_merge_gate_without_a_merge_in_progress_fails_closed_to_full():
    # The misuse guard. Outside a merge nothing makes the index a merge result, so the
    # narrowing is unjustified — and an unjustified narrowing runs the expensive gate.
    src = merge_source(["docs/a.md"], ["docs/a.md"], None)
    assert src.paths is None
    assert "outside an in-progress merge" in (src.forced or "")
    d = forced_full(src.forced)
    assert [" ".join(c) for c in d.commands()] == [
        "just lint",
        "just test",
        "just acceptance-ci",
        "just test-ui-e2e",
    ]


def test_merge_gate_is_not_an_ignore_list_for_fixtures_or_tools():
    # Nothing is exempt by PATH; the only thing that changes is WHICH SET is classified.
    # A fixture staged into the merge is still full, exactly as under the default source.
    src = _merge(["fixtures/x.sigmf-meta", "docs/a.md"], ["fixtures/x.sigmf-meta", "docs/a.md"])
    assert classify(src.paths).label == FULL
    # And so is a `tools/` file, if it is ever actually committed.
    assert classify(_merge(["tools/fm_rx.py"], ["tools/fm_rx.py"]).paths).label == FULL


def test_merge_gate_reports_an_unreadable_index_as_forced_full():
    assert "cannot read" in (merge_source(None, [], "MERGE_HEAD").forced or "")
    assert "cannot read" in (merge_source([], None, "MERGE_HEAD").forced or "")


def test_merge_gate_names_the_merge_state_it_relied_on():
    text = "\n".join(render(classify(["docs/a.md"]), _merge(["docs/a.md"], []), "all"))
    assert "MERGE_HEAD" in text
    assert "puts on main" in text
    squash = _merge(["docs/a.md"], [], state="SQUASH_MSG")
    assert "SQUASH_MSG" in squash.description


def test_the_justfile_exposes_gate_merge_and_it_passes_the_flag():
    # The recipe is the interface the coordinator actually types; if it drifts from the
    # classifier the written rule in docs/10 stops describing what runs.
    justfile = Path(__file__).resolve().parents[2] / "justfile"
    text = justfile.read_text()
    assert "\ngate-merge *args:" in text
    assert "hkpy.gate --merge" in text


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
        "just test-ui-e2e",
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


# --------------------------------------------------------------- T-400 build env (in-process)
#
# `just gate` shelled out to the suites without CARGO_INCREMENTAL=0 (and the rest of T-144's
# build flags), so the coordinator's own per-merge check regenerated target/debug/incremental
# every run — precisely the state every agent brief is configured to avoid. The fix moves those
# flags into the gate's own subprocess environment, never into the justfile globally (a
# developer running `just test`/`just lint`/`just build` by hand may still want incremental).
#
# These tests must not duplicate the suite-selection tests above: they assert only that the
# CHOSEN commands are unchanged and that each one is launched with the T-144 env layered on top
# of whatever the caller already had — never that the flags change which suites run.


def test_gate_build_env_is_exactly_t144s_three_flags():
    from hkpy.gate import GATE_BUILD_ENV

    # Locks the fix to T-144's own flags, not a superset copied by reflex and not a subset
    # quietly dropped. nextest's thread cap is deliberately absent — T-436 already pins it in
    # .config/nextest.toml for every nextest invocation, gate included, so repeating it here
    # would be a second, driftable copy of a rule that already lives in the runner.
    assert GATE_BUILD_ENV == {
        "CARGO_INCREMENTAL": "0",
        "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
        "CARGO_BUILD_JOBS": "6",
    }


def test_suite_env_adds_the_flags_without_touching_anything_else():
    from hkpy.gate import suite_env

    base = {"PATH": "/usr/bin", "HOME": "/home/x"}
    env = suite_env(base)
    assert env["PATH"] == "/usr/bin"
    assert env["HOME"] == "/home/x"
    assert env["CARGO_INCREMENTAL"] == "0"
    assert env["CARGO_PROFILE_DEV_DEBUG"] == "line-tables-only"
    assert env["CARGO_BUILD_JOBS"] == "6"
    # Pure: the caller's mapping is never mutated.
    assert base == {"PATH": "/usr/bin", "HOME": "/home/x"}


def test_suite_env_overrides_a_conflicting_value_from_the_caller():
    from hkpy.gate import suite_env

    # If something upstream already exported CARGO_INCREMENTAL=1, the gate's own suites still
    # get 0 — that is the entire point of the ticket, so the gate's flags must win.
    assert suite_env({"CARGO_INCREMENTAL": "1"})["CARGO_INCREMENTAL"] == "0"


def test_main_runs_the_same_suites_with_the_build_env_layered_on(monkeypatch, tmp_path):
    # Full round-trip through `main()`, but with subprocess.run faked out so this stays a
    # targeted, in-process test rather than an actual build. Asserts two independent things
    # that the fix must hold at once: the SUITES CHOSEN are exactly what classify() says (the
    # flags must not change that), and every one of them is launched with T-144's env plus
    # whatever the caller's environment already had.
    from hkpy import gate as gate_mod

    calls: list[tuple[list[str], dict[str, str]]] = []

    class FakeCompleted:
        returncode = 0

    def fake_run(cmd, cwd=None, env=None, check=False):
        calls.append((cmd, env))
        return FakeCompleted()

    monkeypatch.setattr(gate_mod.subprocess, "run", fake_run)
    monkeypatch.setattr(gate_mod.shutil, "which", lambda name: "/usr/bin/just")
    monkeypatch.setenv("SOME_UNRELATED_VAR", "kept")
    # T-763: main() records the run through gatelog, which writes to $HACKRIFF_OPS. Without
    # this the SUITE'S OWN fake gates land in the production history that `just cycle-time`
    # and the budget guard read: on 2026-09-22, 31 of the 33 runs in the real
    # `gate-timings.jsonl` were records written from here (their `root` is a pytest tmpdir),
    # a 0.0-second `py` run each time. A measurement tool whose own tests pollute the
    # measurement is worse than one nobody runs.
    monkeypatch.setenv("HACKRIFF_OPS", str(tmp_path / "ops"))

    rc = gate_mod.main(["--files", "py/hkpy/synth.py", "--root", str(tmp_path)])

    assert rc == 0
    assert [" ".join(cmd) for cmd, _ in calls] == [
        " ".join(c) for c in classify(["py/hkpy/synth.py"]).commands()
    ]
    for _, env in calls:
        assert env["CARGO_INCREMENTAL"] == "0"
        assert env["CARGO_PROFILE_DEV_DEBUG"] == "line-tables-only"
        assert env["CARGO_BUILD_JOBS"] == "6"
        assert env["SOME_UNRELATED_VAR"] == "kept"


# ---------------------------------------------------------------------------------------
# T-543: which CRATES, once the class is `full`.
#
# `py/hkpy/crates.py` narrows the Rust suite inside the `full` class, so these tests are the
# same shape as the class tests above: assert the answer case by case, and assert the
# fail-closed cases hardest, because a wrong narrowing is silent. "Fail closed" here means
# `crates is None` — run the whole workspace.
# ---------------------------------------------------------------------------------------


def _ws(packages):
    """Build a Workspace from a compact spec: {name: (dir, lib_deps, dev_deps)}."""
    from hkpy.crates import Workspace

    return Workspace(
        dirs={n: spec[0] for n, spec in packages.items()},
        lib_deps={n: set(spec[1]) for n, spec in packages.items()},
        dev_deps={n: set(spec[2]) for n, spec in packages.items()},
    )


#: A miniature of the real workspace's decisive shape: a leaf model everything uses, a
#: mid-level crate, a top-level binary, and a test-harness crate that is a DEV-dependency of
#: a leaf while itself DEV-depending on the binary. That last edge is the one that makes the
#: real graph subtle, so the fixture has to carry it.
_MINI = _ws(
    {
        "hk-model": ("crates/hk-model", [], []),
        "hk-dsp": ("crates/hk-dsp", ["hk-model"], ["hk-e2e"]),
        "hk-pipeline": ("crates/hk-pipeline", ["hk-model", "hk-dsp"], []),
        "hk-cli": ("crates/hk-cli", ["hk-pipeline"], []),
        "hk-e2e": ("tests/e2e", ["hk-model"], ["hk-cli", "hk-pipeline", "hk-sim"]),
        "hk-sim": ("crates/hk-sim", ["hk-model"], []),
    }
)


def test_a_leaf_binary_change_narrows_to_itself_and_the_harness_that_drives_it():
    from hkpy.crates import select

    sel = select(["crates/hk-cli/src/main.rs"], _MINI)
    # hk-e2e DEV-depends on hk-cli, so its test targets link the change and must run.
    # Nothing else does: hk-cli is a leaf in the lib graph.
    assert sel.crates == ("hk-cli", "hk-e2e")


def test_a_dev_dependency_edge_is_followed_or_tests_that_link_the_change_would_be_skipped():
    from hkpy.crates import select

    # hk-pipeline -> (lib) hk-cli, and (dev) hk-e2e. hk-dsp DEV-depends on hk-e2e, but
    # hk-e2e's LIB does not depend on hk-pipeline, so hk-dsp's test binary does NOT link
    # hk-pipeline and correctly stays out. Getting this edge right in both directions is
    # the whole difficulty of the rule.
    sel = select(["crates/hk-pipeline/src/lib.rs"], _MINI)
    assert sel.crates == ("hk-cli", "hk-e2e", "hk-pipeline")


def test_a_change_under_the_harness_reaches_every_crate_that_dev_depends_on_it():
    from hkpy.crates import select

    sel = select(["tests/e2e/src/lib.rs"], _MINI)
    assert sel.crates == ("hk-dsp", "hk-e2e")


def test_the_foundation_crate_is_the_whole_workspace_and_says_so():
    from hkpy.crates import select

    # Everything depends on the model, so there is nothing to narrow to. The honest answer
    # is the expensive one, and it must be reported as such rather than as a 6-crate list
    # that happens to be all of them.
    sel = select(["crates/hk-model/src/lib.rs"], _MINI)
    assert sel.is_workspace
    assert "whole workspace" in sel.reason


def test_the_acceptance_harness_is_in_every_crates_selection():
    from hkpy.crates import select

    # The rule this ticket may not break: the suite that caught T-484's dark demo runs for
    # every `crates/` change. It is not enforced by a special case — the graph says so —
    # but it is asserted here, because a future graph change that quietly dropped it would
    # be exactly the coverage loss the ticket forbids.
    for name, (directory, _, _) in (
        ("hk-cli", ("crates/hk-cli", [], [])),
        ("hk-dsp", ("crates/hk-dsp", [], [])),
        ("hk-sim", ("crates/hk-sim", [], [])),
        ("hk-pipeline", ("crates/hk-pipeline", [], [])),
    ):
        sel = select([f"{directory}/src/lib.rs"], _MINI)
        assert sel.is_workspace or "hk-e2e" in (sel.crates or ()), name


def test_cargo_lock_forces_the_whole_workspace_even_beside_a_one_crate_edit():
    from hkpy.crates import select

    sel = select(["crates/hk-cli/src/main.rs", "Cargo.lock"], _MINI)
    assert sel.is_workspace


def test_a_dot_directory_still_matches_its_rule():
    from hkpy.crates import select

    # Regression: `lstrip("./")` strips a CHARACTER SET, turning ".config/nextest.toml"
    # into "config/nextest.toml" so the `.config/` rule never fires. `.config/` changes the
    # nextest thread cap and the serial groups — how every test in the workspace is
    # scheduled — so a selection made while ignoring it is made on the wrong information.
    for path in (".config/nextest.toml", ".github/workflows/ci.yml"):
        sel = select(["crates/hk-cli/src/main.rs", path], _MINI)
        assert sel.is_workspace, path


def test_a_path_in_the_rust_tree_belonging_to_no_package_fails_closed():
    from hkpy.crates import select

    # A brand-new crate nobody has added to the workspace yet. The graph does not describe
    # it, so no answer derived from the graph is trustworthy.
    sel = select(["crates/hk-brand-new/src/lib.rs"], _MINI)
    assert sel.is_workspace
    assert "no workspace package" in sel.reason


def test_no_workspace_graph_means_the_whole_workspace():
    from hkpy.crates import select

    assert select(["crates/hk-cli/src/main.rs"], None).is_workspace


def test_paths_outside_the_rust_tree_never_narrow_on_their_own():
    from hkpy.crates import select

    # `ui/`, `docs/` and `py/` belong to classes that do not run the Rust suite at all. They
    # must not produce a narrowed Rust selection by themselves.
    sel = select(["ui/src/app.ts", "docs/api.md", "py/hkpy/synth.py"], _MINI)
    assert sel.is_workspace


def test_selection_env_sets_the_variable_only_for_a_real_narrowing():
    from hkpy.crates import Selection
    from hkpy.gate import CRATES_ENV, selection_env

    narrowed = selection_env({}, Selection(("hk-cli", "hk-e2e"), "why"))
    assert narrowed[CRATES_ENV] == "hk-cli hk-e2e"

    # And an inherited value from an outer shell must never survive a workspace decision:
    # that would silently narrow a gate that decided not to narrow.
    widened = selection_env({CRATES_ENV: "hk-cli"}, Selection(None, "no narrowing"))
    assert CRATES_ENV not in widened


def test_a_forced_full_gate_never_narrows_crates():
    from hkpy.gate import Source, forced_full, resolve_selection

    decision = forced_full("CI push build: no pull-request base, so the full gate runs")
    source = Source("CI push build", None, forced="no base")
    sel = resolve_selection(decision, source, ".")
    assert sel.is_workspace


def test_a_non_full_class_has_no_rust_suite_to_narrow():
    from hkpy.gate import Source, resolve_selection

    decision = classify(["ui/src/app.ts"])
    source = Source("explicit --files", ["ui/src/app.ts"])
    assert resolve_selection(decision, source, ".").is_workspace


# ---------------------------------------------------------------------------------------
# T-543, CORRECTED BY THE USER (2026-09-20): affected-crate selection is for an agent's own
# local iteration ONLY. The merge gate (`just gate-merge`) and the CI gate (`just gate`
# inside GitHub Actions) must NEVER narrow, unconditionally — not "narrowing wasn't
# requested this time" but structurally refused even if `--select-crates` is passed. These
# pin that on `resolve_selection` itself, the one function every path (CLI, gate-merge, CI)
# funnels through, rather than on trusting that no caller ever wires the flag in wrong.
# ---------------------------------------------------------------------------------------


def test_crate_narrowing_is_opt_in_and_off_by_default():
    # A plain full-class diff, not a merge, not CI, `--select-crates` not passed: still the
    # whole workspace. This is the corrected default — T-543 originally had this narrow
    # automatically, which is exactly the mistake the user's ruling reverses.
    from hkpy.gate import Source, resolve_selection

    decision = classify(["crates/hk-cli/src/lib.rs"])
    source = Source("explicit --files", ["crates/hk-cli/src/lib.rs"])
    sel = resolve_selection(decision, source, ".")
    assert sel.is_workspace
    assert "opt-in" in sel.reason


def test_select_crates_narrows_a_plain_local_run(monkeypatch):
    # The opt-in DOES work, for the one path it is meant for: an agent's own local
    # iteration, not a merge and not CI. Pin it with a deterministic fake workspace
    # (`_MINI`, the same fixture `crates.py`'s own tests use) rather than the real
    # `cargo metadata`, so the assertion doesn't depend on what's installed.
    from hkpy import crates as crate_select
    from hkpy.gate import Source, resolve_selection

    monkeypatch.setattr(crate_select, "load_workspace", lambda root: _MINI)

    decision = classify(["crates/hk-cli/src/main.rs"])
    source = Source("explicit --files", ["crates/hk-cli/src/main.rs"])
    sel = resolve_selection(decision, source, ".", select_crates=True)
    assert not sel.is_workspace
    assert sel.crates == ("hk-cli", "hk-e2e")

    # And the same diff, still opted in, is refused for merge/CI regardless.
    assert resolve_selection(
        decision, source, ".", select_crates=True, merge=True
    ).is_workspace
    assert resolve_selection(
        decision, source, ".", select_crates=True, ci=True
    ).is_workspace


def test_merge_gate_never_narrows_crates_even_if_select_crates_is_passed():
    from hkpy.gate import Source, resolve_selection

    decision = classify(["crates/hk-cli/src/lib.rs"])
    source = Source(
        "merge index [MERGE_HEAD]", ["crates/hk-cli/src/lib.rs"]
    )
    sel = resolve_selection(decision, source, ".", select_crates=True, merge=True)
    assert sel.is_workspace
    assert "merge gate" in sel.reason


def test_ci_gate_never_narrows_crates_even_if_select_crates_is_passed():
    from hkpy.gate import Source, resolve_selection

    decision = classify(["crates/hk-model/src/lib.rs"])
    source = Source(
        "CI pull request, merge base with origin/main", ["crates/hk-model/src/lib.rs"]
    )
    sel = resolve_selection(decision, source, ".", select_crates=True, ci=True)
    assert sel.is_workspace
    assert "CI gate" in sel.reason


def test_merge_and_ci_are_checked_before_the_opt_in_flag():
    # Even a caller that forgot to pass select_crates at all still gets the merge/CI
    # short-circuit reason, not the generic "opt-in, not requested" one — the two must not
    # collapse into one message that could plausibly be satisfied by only one of the guards.
    from hkpy.gate import Source, resolve_selection

    decision = classify(["crates/hk-cli/src/lib.rs"])
    source = Source("merge index [MERGE_HEAD]", ["crates/hk-cli/src/lib.rs"])
    sel = resolve_selection(decision, source, ".", merge=True)
    assert sel.is_workspace
    assert "merge gate" in sel.reason


def test_main_never_narrows_under_merge_even_with_select_crates(monkeypatch, tmp_path, capsys):
    # Full round-trip through main(): `--merge --select-crates` together must still run the
    # whole workspace, because it is the wiring in main() (ci = GITHUB_ACTIONS, merge =
    # args.merge, both passed into resolve_selection) that has to get this right, not just
    # the pure function in isolation.
    from hkpy import gate as gate_mod

    monkeypatch.setattr(gate_mod, "merge_state", lambda root: "MERGE_HEAD")
    monkeypatch.setattr(
        gate_mod, "staged_changes", lambda root: ["crates/hk-cli/src/lib.rs"]
    )
    monkeypatch.setattr(
        gate_mod, "worktree_changes", lambda root: ["crates/hk-cli/src/lib.rs"]
    )
    monkeypatch.delenv("GITHUB_ACTIONS", raising=False)

    rc = gate_mod.main(
        ["--merge", "--select-crates", "--dry-run", "--root", str(tmp_path)]
    )
    assert rc == 0
    out = capsys.readouterr().out
    assert "WHOLE WORKSPACE" in out
    assert "merge gate" in out


def test_main_never_narrows_in_ci_even_with_select_crates(monkeypatch, tmp_path, capsys):
    from hkpy import gate as gate_mod

    monkeypatch.setenv("GITHUB_ACTIONS", "true")
    monkeypatch.setenv("GITHUB_BASE_REF", "main")
    monkeypatch.setattr(
        gate_mod,
        "committed_changes",
        lambda root, ref: ["crates/hk-model/src/lib.rs"],
    )

    rc = gate_mod.main(["--select-crates", "--dry-run", "--root", str(tmp_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "WHOLE WORKSPACE" in out
    assert "CI gate" in out


def test_main_default_ci_run_never_narrows_even_without_the_flag(
    monkeypatch, tmp_path, capsys
):
    # The realistic case: CI's actual invocation (`just gate --phase check`, no
    # `--select-crates` at all) over a diff that touches one crate.
    from hkpy import gate as gate_mod

    monkeypatch.setenv("GITHUB_ACTIONS", "true")
    monkeypatch.setenv("GITHUB_BASE_REF", "main")
    monkeypatch.setattr(
        gate_mod,
        "committed_changes",
        lambda root, ref: ["crates/hk-cli/src/lib.rs"],
    )

    rc = gate_mod.main(["--phase", "check", "--dry-run", "--root", str(tmp_path)])
    assert rc == 0
    out = capsys.readouterr().out
    assert "WHOLE WORKSPACE" in out


# ---------------------------------------------------------------------------------------
# T-543: the gate records its own duration.
# ---------------------------------------------------------------------------------------


def test_the_gate_writes_a_start_and_an_end_record_for_every_run(monkeypatch, tmp_path):
    from hkpy import gate as gate_mod
    from hkpy import gatelog

    class FakeCompleted:
        returncode = 0

    monkeypatch.setattr(
        gate_mod.subprocess, "run", lambda *a, **k: FakeCompleted()
    )
    monkeypatch.setattr(gate_mod.shutil, "which", lambda name: "/usr/bin/just")
    monkeypatch.setenv("HACKRIFF_OPS", str(tmp_path))

    assert gate_mod.main(["--files", "py/hkpy/synth.py", "--root", str(tmp_path)]) == 0

    runs = gatelog.runs(gatelog.read())
    assert len(runs) == 1
    assert runs[0]["finished"] is True
    assert runs[0]["class"] == "py"
    assert runs[0]["result"] == "pass"
    # One `suite` line per command launched — the per-phase resolution the ledger exists for.
    assert [s["cmd"] for s in runs[0]["suites"]] == [
        " ".join(c) for c in classify(["py/hkpy/synth.py"]).commands()
    ]


def test_a_run_that_never_finished_is_still_visible():
    from hkpy import gatelog

    # The 62-minute starved gate of 2026-09-20 would have left NO trace under an
    # on-completion-only design. An unterminated run is data.
    records = [gatelog.start_record("abc", klass="full", phase="all", source="s", n_files=1)]
    runs = gatelog.runs(records)
    assert len(runs) == 1
    assert runs[0]["finished"] is False
    assert runs[0]["seconds"] is None


def test_the_ledger_never_raises_when_it_cannot_write(tmp_path):
    from hkpy import gatelog

    # Instrumentation that can fail the thing it measures is worse than none.
    unwritable = tmp_path / "file" / "nested" / "log.jsonl"
    (tmp_path / "file").write_text("not a directory")
    assert gatelog.append({"kind": "gate_start"}, path=str(unwritable)) is False


def test_a_half_written_line_does_not_destroy_the_history(tmp_path):
    from hkpy import gatelog

    path = tmp_path / "gate-timings.jsonl"
    gatelog.append(gatelog.start_record("r1", klass="ui", phase="all", source="s", n_files=1), path=str(path))
    with open(path, "a", encoding="utf-8") as fh:
        fh.write('{"kind": "gate_en')  # killed mid-write
    gatelog.append(gatelog.end_record("r1", klass="ui", phase="all", seconds=3.0, rc=0), path=str(path))

    runs = gatelog.runs(gatelog.read(str(path)))
    assert len(runs) == 1 and runs[0]["finished"] is True


# ---------------------------------------------------------------------------------------
# T-543: reading the merge runner's log back as cycle time.
# ---------------------------------------------------------------------------------------


def test_the_merge_log_parses_into_branch_lives_and_deduplicates_its_doubled_lines():
    from hkpy.cycletime import lives_from_events, parse_runner_log

    # `ops/merge-runner.sh`'s `log()` tees while its stdout is redirected to the same file,
    # so every line genuinely appears twice. Counting a doubled QUEUED as two events would
    # not change the times, but a doubled GATE FAILED would double the failure count.
    text = "\n".join(
        [
            "[09-20 10:00:00] QUEUED task-t100",
            "[09-20 10:00:00] QUEUED task-t100",
            "[09-20 11:00:00] MERGE start task-t100 (T-100, 1 commits ahead)",
            "[09-20 11:00:00] GATE task-t100 (just gate-merge; may take 15-25 min)…",
            "[09-20 11:20:00] GATE FAILED task-t100 (attempt 1/2, tip abc) -> abort",
            "[09-20 11:20:00] GATE FAILED task-t100 (attempt 1/2, tip abc) -> abort",
            "[09-20 12:00:00] MERGE start task-t100 (T-100, 1 commits ahead)",
            "[09-20 12:22:00] MERGED task-t100 ✓",
        ]
    )
    lives = lives_from_events(parse_runner_log(text, 2026))
    life = lives["task-t100"]
    assert life.gate_failures == 1
    assert life.gate_seconds == 22 * 60
    assert life.wait_seconds is None  # no commit times without a repo
    assert life.queued is not None and life.queued.hour == 10


# ---------------------------------------------------------------------------------------
# T-543: a slow-down must trip a TEST, not wait for somebody to notice.
#
# The check is a ROLLING MEDIAN, not a per-run bound, and that is deliberate. This box runs
# up to four building agents plus an `hk serve` by policy, and one contended run says nothing
# — `cargo build -p hk-plugins --bins`, documented in the justfile as ~0.05 s on a warm
# target, was measured at 500 s at load 211 on the day this was written. A single red from
# that would be noise, and a noisy guard gets muted. A median that moves is signal.
# ---------------------------------------------------------------------------------------


def _finished(klass, seconds, n):
    from hkpy import gatelog

    out = []
    for _ in range(n):
        rid = gatelog.new_run_id()
        out.append(
            gatelog.start_record(rid, klass=klass, phase="all", source="s", n_files=1)
        )
        out.append(
            gatelog.end_record(rid, klass=klass, phase="all", seconds=seconds, rc=0)
        )
    return out


def test_a_class_over_its_rolling_budget_is_reported():
    from hkpy import gatelog
    from hkpy.cycletime import budget_breaches

    runs = gatelog.runs(_finished("full", 45 * 60, 6))
    breaches = budget_breaches(runs)
    assert len(breaches) == 1 and breaches[0].startswith("full:")


def test_a_class_inside_its_budget_reports_nothing():
    from hkpy import gatelog
    from hkpy.cycletime import budget_breaches

    assert budget_breaches(gatelog.runs(_finished("full", 20 * 60, 6))) == []


def test_too_few_runs_is_silence_not_a_failure():
    from hkpy import gatelog
    from hkpy.cycletime import budget_breaches

    # A fresh machine, or a class that has run twice. Failing here would make the guard fire
    # on an absence of evidence, which is the fastest way to get a guard switched off.
    assert budget_breaches(gatelog.runs(_finished("full", 99 * 60, 2))) == []


def test_one_slow_run_among_fast_ones_does_not_trip_it():
    from hkpy import gatelog
    from hkpy.cycletime import budget_breaches

    runs = gatelog.runs(_finished("full", 15 * 60, 6) + _finished("full", 62 * 60, 1))
    assert budget_breaches(runs) == []


def test_unfinished_runs_are_never_counted_as_fast():
    from hkpy import gatelog
    from hkpy.cycletime import rolling_medians

    # A killed gate has no duration. Treating it as a zero would make starvation look like
    # speed — the exact wrong conclusion.
    records = _finished("full", 20 * 60, 5)
    records.append(
        gatelog.start_record(
            gatelog.new_run_id(), klass="full", phase="all", source="s", n_files=1
        )
    )
    median, n = rolling_medians(gatelog.runs(records))["full"]
    assert n == 5 and median == 20 * 60


def test_the_recorded_gate_history_is_reportable_without_blocking_the_merge_gate():
    """The budget guard REPORTS here; it does not fail the merge gate. T-762.

    It used to assert, and on 2026-09-22 that deadlocked the whole pipeline: the recorded median
    reached 35.8 min against a 35 min budget, `test-py` failed, `just test` failed, and every
    branch's gate failed with it — including the branches that would have made the gate faster.
    Worse, it is SELF-REINFORCING: a failed gate is itself another slow run appended to
    `gate-timings.jsonl`, so each attempt pushed the median further over.

    The distinction that matters is WHAT A TEST IS ALLOWED TO BE A FUNCTION OF. Every other guard
    in this file is a pure function of the diff: the same diff gets the same verdict from anyone,
    anywhere. This one is a function of THIS MACHINE'S ACCUMULATED HISTORY, so no diff can clear
    it and a fresh checkout and a week-old one disagree about identical code. That is a monitor,
    not a gate, and putting a monitor on the merge path stops the work it is monitoring.

    The signal is NOT discarded — the user named iteration speed a priority, and the guard was
    RIGHT: `CLAUDE.md` records a 21.4 min median on 2026-09-20 and it is now ~34. That regression
    is T-763, filed rather than silenced. `just cycle-time` still asserts it (a human asking the
    question, on purpose), and this test keeps the breach VISIBLE in the suite output so it cannot
    rot unnoticed — it simply refuses to hold merges hostage to it.
    """
    import sys

    from hkpy import gatelog
    from hkpy.cycletime import budget_breaches

    breaches = budget_breaches(gatelog.runs(gatelog.read()))
    if breaches:
        print(
            "\nGATE DURATION OVER BUDGET (reported, not fatal — see T-762/T-763):\n  "
            + "\n  ".join(breaches),
            file=sys.stderr,
        )
    # What IS asserted: the reporting path itself works, so this test cannot quietly become a
    # no-op that reports nothing however far the gate regresses.
    assert isinstance(breaches, list)
    assert all(isinstance(b, str) and b for b in breaches)


def test_the_ui_suite_is_skipped_only_for_a_full_diff_with_no_ui_path():
    from hkpy.gate import Source, forced_full, skip_ui

    def decide(paths):
        return classify(paths), Source("explicit --files", list(paths))

    assert skip_ui(*decide(["crates/hk-cli/src/lib.rs"])) is True
    # A ui/ path anywhere in the diff, and the UI suite runs.
    assert skip_ui(*decide(["crates/hk-cli/src/lib.rs", "ui/src/app.ts"])) is False
    # A forced full gate does not know what changed, so it cannot claim the UI is untouched.
    assert (
        skip_ui(
            forced_full("CI push build"), Source("CI push build", None, forced="no base")
        )
        is False
    )


def test_t561_the_board_file_runs_the_python_suite_not_nothing():
    """docs/tasks.yaml is DATA, and `docs/` runs nothing.

    A malformed `blocked_on:` - an unquoted value containing ": " - reached main
    through a docs-class gate, so `py/tests/test_task_board.py`, which exists to
    catch exactly that, never ran. It broke the board test, the dashboard's task
    map, and every later gate that parses the file. Prose stays `docs`.
    """
    from hkpy.gate import classify, classify_path

    assert classify_path("docs/tasks.yaml")[0] == "py"
    assert classify_path("docs/use-cases.yaml")[0] == "py"
    assert classify_path("docs/README.md")[0] == "docs"
    assert classify(["docs/tasks.yaml"]).classes == ("py",)
    # a board edit alongside prose still runs the Python suite
    assert "py" in classify(["docs/tasks.yaml", "docs/README.md"]).classes


def test_t562_the_coordinator_only_guard_is_wired_to_the_expensive_recipes():
    """The gate, acceptance and the full workspace test refuse from an agent worktree.

    /perf measured ~23 h of agent time on suites the brief forbids: 42 of 47 agents
    that ran `just gate` were implementation agents self-verifying, 111 of 122 on
    `just acceptance`, 93 of 108 on full `just test`. Prose did not stop it, so the
    rule lives in the runner - the same move as T-396 (which-suites) and T-477 (the
    board check). HK_ALLOW_FULL=1 is the documented escape for a genuine repro agent.
    """
    from pathlib import Path

    jf = Path(__file__).resolve().parents[2] / "justfile"
    text = jf.read_text()

    assert "_coordinator-only recipe:" in text
    for recipe in ("gate", "gate-merge", "test", "acceptance-ci"):
        assert f'(_coordinator-only "{recipe}")' in text, recipe
    # the escape hatch exists and is named in the refusal
    assert "HK_ALLOW_FULL" in text
    # the refusal points at the cheap forms, and away from the workspace-wide one
    assert "just test-crate <crate>" in text
    assert "cargo nextest run -p <crate> -E 'binary(<name>)'" in text
    assert "NOT 'just test-one'" in text


def test_ops_paths_run_the_ops_suites_not_the_full_gate():
    """ops/, .claude/ and prompts/ are orchestration: no crate links them, no suite reads them. Four
    docs/ops-only branches lost 50-minute full gates to load-sensitive Rust tests on 2026-09-22."""
    from hkpy.gate import classify, OPS
    d = classify(["ops/work-runner.py", ".claude/roles/coordinator.md", "prompts/model-selection.md"])
    assert d.classes == (OPS,)
    cmds = d.commands()
    assert ["just", "ops-check"] in cmds and ["just", "test-py"] in cmds and ["just", "lint-py"] in cmds
    assert ["just", "test"] not in cmds and ["just", "acceptance-ci"] not in cmds


def test_ops_plus_crates_is_still_full():
    from hkpy.gate import classify
    assert classify(["ops/stage.sh", "crates/hk-core/src/lib.rs"]).is_full


def test_the_justfile_stays_full_even_though_it_lives_beside_ops():
    from hkpy.gate import classify
    assert classify(["justfile", "ops/stage.sh"]).is_full


def test_a_contended_gate_records_what_it_was_gating_beside():
    """`ops/merge-runner.sh` waits for the box to clear, but the wait is capped at 45 min so a
    stuck worker or a leaked process cannot hold every merge. Past the cap it gates anyway and
    exports `HK_GATE_CONTENDED`.

    That run is still a real gate — the code is still tested — but it is NOT a measurement of
    the code's cost, and the timing log is the only place that can still say so afterwards. On
    2026-09-22 sixteen unowned busy loops ran through every gate for 2 h 18 m, and the gates
    they slowed were read as a regression in the suites.
    """
    from hkpy import gatelog

    r = gatelog.start_record("abc", klass="full", phase="all", source="s", n_files=1,
                             contended="load 44.0 over budget 32.0")
    assert r["contended"] == "load 44.0 over budget 32.0"
    assert gatelog.start_record("abc", klass="full", phase="all", source="s", n_files=1)["contended"] is None


def test_the_contended_env_name_is_the_one_the_merge_runner_exports():
    """The two halves live in different languages and different files; nothing but this pins
    them together."""
    import pathlib

    from hkpy.gate import CONTENDED_ENV

    runner = (pathlib.Path(__file__).resolve().parents[2] / "ops" / "merge-runner.sh").read_text()
    assert CONTENDED_ENV == "HK_GATE_CONTENDED"
    assert f"export {CONTENDED_ENV}" in runner or f"{CONTENDED_ENV}=" in runner
