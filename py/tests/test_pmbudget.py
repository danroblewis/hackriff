"""The pipeline manager's scope check: a `task-pm-*` branch merges only with a stated, warranted
reason and only inside pipeline paths (user, 2026-09-23: "it should stick to its directive")."""

import subprocess

from hkpy import pmbudget


def _repo(tmp_path):
    r = str(tmp_path / "r")
    subprocess.run(["git", "init", "-q", "-b", "main", r], check=True)
    subprocess.run(["git", "-C", r, "config", "user.email", "t@t"], check=True)
    subprocess.run(["git", "-C", r, "config", "user.name", "t"], check=True)
    (tmp_path / "r" / "ops").mkdir()
    (tmp_path / "r" / "ops" / "a.sh").write_text("base\n")
    subprocess.run(["git", "-C", r, "add", "."], check=True)
    subprocess.run(["git", "-C", r, "commit", "-q", "-m", "base"], check=True)
    return r


def _branch(r, name, message, files=None, lines=10):
    files = files or (f"ops/{name}.sh",)          # a file of its own, so each branch's lines are additions
    subprocess.run(["git", "-C", r, "checkout", "-q", "-b", name, "main"], check=True)
    for f in files:
        p = f"{r}/{f}"
        subprocess.run(["mkdir", "-p", p.rsplit("/", 1)[0]], check=True)
        with open(p, "w") as fh:
            fh.write("x\n" * lines)
    subprocess.run(["git", "-C", r, "add", "."], check=True)
    subprocess.run(["git", "-C", r, "commit", "-q", "-m", message], check=True)
    subprocess.run(["git", "-C", r, "checkout", "-q", "main"], check=True)


def _merge(r, name):
    subprocess.run(["git", "-C", r, "merge", "-q", "--no-ff", "-m", f"Merge {name} ({name}): gate passed", name], check=True)


def test_non_pm_branches_are_never_held(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-t123", "a worker's ticket, no Serves line", files=("crates/hk-x/src/lib.rs",), lines=5000)
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-t123")
    assert ok and "not a pipeline-manager" in why


def test_a_pm_branch_without_a_reason_is_held(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-pm-thing", "pm: a nice improvement")
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-thing")
    assert not ok and "no `Serves:`" in why


def test_each_warranted_reason_passes_and_a_cost_needs_a_number(tmp_path):
    r = _repo(tmp_path)
    for name, msg in (("task-pm-e", "x\n\nServes: E-003"), ("task-pm-i", "x\n\nServes: incident runner wedged"),
                      ("task-pm-u", "x\n\nServes: user the flow panel"), ("task-pm-c", "x\n\nServes: cost just test is 1800 s of a 45-min gate")):
        _branch(r, name, msg)
        ok, why = pmbudget.check(r, str(tmp_path), "main", name)
        assert ok, (name, why)
    _branch(r, "task-pm-vague", "x\n\nServes: cost the gate feels slow")
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-vague")
    assert not ok and "no number" in why


def test_volume_is_not_a_reason_to_hold(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-pm-big", "x\n\nServes: E-001", files=("ops/big.py", "py/hkpy/big.py"), lines=5000)
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-big")
    assert ok and "10000 lines" in why


def test_a_pm_branch_touching_product_code_or_the_board_is_held(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-pm-prod", "x\n\nServes: E-001", files=("ops/ok.sh", "crates/hk-api/src/tiles.rs"))
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-prod")
    assert not ok and "crates/hk-api/src/tiles.rs" in why and "outside the pipeline directive" in why
    _branch(r, "task-pm-board", "x\n\nServes: incident y", files=("docs/tasks.yaml",))
    assert not pmbudget.check(r, str(tmp_path), "main", "task-pm-board")[0]
    _branch(r, "task-pm-spec", "x\n\nServes: incident z", files=("ui/e2e/fog-of-war.e2e.mjs",))
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-spec")
    assert not ok and "fog-of-war.e2e.mjs" in why          # a product spec's assertions: deflaker territory
    _branch(r, "task-pm-harness", "x\n\nServes: incident z", files=("ui/e2e/harness.mjs", "ui/e2e/run.mjs"))
    assert pmbudget.check(r, str(tmp_path), "main", "task-pm-harness")[0]


def test_landed_today_is_reported_not_capped(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-pm-a", "a\n\nServes: E-001", lines=250)
    _merge(r, "task-pm-a")
    _branch(r, "task-pm-b", "b\n\nServes: user big panel", lines=900)
    _merge(r, "task-pm-b")
    _branch(r, "task-t9", "worker", files=("crates/x/lib.rs",), lines=700)
    _merge(r, "task-t9")
    count, lines = pmbudget.landed_today(r)
    assert count == 2 and lines == 1150
    _branch(r, "task-pm-c", "c\n\nServes: E-001", lines=3000)
    assert pmbudget.check(r, str(tmp_path), "main", "task-pm-c")[0]


def test_a_persons_release_marker_lets_a_held_branch_through(tmp_path):
    r = _repo(tmp_path)
    _branch(r, "task-pm-held", "no reason")
    assert not pmbudget.check(r, str(tmp_path), "main", "task-pm-held")[0]
    assert pmbudget.main(["--repo", r, "--ops", str(tmp_path), "release", "task-pm-held"]) == 0
    ok, why = pmbudget.check(r, str(tmp_path), "main", "task-pm-held")
    assert ok and "released" in why
