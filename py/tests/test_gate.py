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
