"""A malformed board cannot reach `main` by ANY route (T-764).

The 2026-09-22 break — T-640's title carrying an unquoted ``sources[iq-ring].available: true``,
which makes ``yaml.safe_load`` reject the whole document — arrived through a DIRECT BOARD COMMIT,
and direct board commits run no gate. So this file proves the refusal down each path in turn,
using that exact break as the payload:

* a direct board commit           -> `.githooks/pre-commit` refuses  (the path that actually broke)
* a driver-resolved merge         -> `hkpy.boardmerge._validate` refuses (no human in the loop)
* a merge commit (the runner)     -> the same hook refuses, MERGE_HEAD and all
* a gated merge                   -> `py/tests/test_task_board.py` already fails (T-762)

Every assertion here is on a REFUSAL of a known-bad board, not on a good board passing. A guard
asserted only by its silence can rot into a no-op and nobody finds out — which is the whole shape
of this defect, and of T-631's nextest override and T-761's unread marker.
"""

from __future__ import annotations

import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import pytest

REPO = Path(__file__).resolve().parents[2]
BOARD = REPO / "docs" / "tasks.yaml"
HOOK = REPO / ".githooks" / "pre-commit"
PYDIR = REPO / "py"

# The exact construct, verbatim from the ticket that broke it.
T640_TITLE = "the live view reports sources[iq-ring].available: true while showing nothing"


@pytest.fixture(scope="module")
def good_board() -> str:
    return BOARD.read_text()


def _break_it(text: str) -> str:
    """The 2026-09-22 break, reintroduced into a real board: one unquoted title with ': '."""
    out = re.sub(r"^    title: .*$", f"    title: {T640_TITLE}", text, count=1, flags=re.M)
    assert out != text, "could not find a title to break — the board's shape changed"
    return out


def _git(repo: Path, *args: str, env: dict[str, str] | None = None) -> subprocess.CompletedProcess:
    e = {**os.environ, "HACKRIFF_OPS": _EMPTY_OPS, **(env or {})}
    return subprocess.run(
        ["git", *args], cwd=repo, env=e, capture_output=True, text=True, check=False
    )


# --------------------------------------------------------------------------- the checker itself


def test_the_checker_refuses_the_2026_09_22_break(good_board) -> None:
    from hkpy.boardcheck import problems

    bad = problems(_break_it(good_board))
    assert bad, "the checker accepted the exact board that blanked the dashboard"
    assert any("': '" in p for p in bad), bad
    # And it names the field, rather than only echoing the loader's line number.
    assert any("title" in p for p in bad), bad


def test_the_checker_refuses_a_board_that_only_strict_yaml_can_see_is_broken(good_board) -> None:
    """A tab indent passes every textual guard in the tree and is not YAML."""
    from hkpy.boardcheck import problems

    bad = problems(good_board.replace("\n  - id: ", "\n\t- id: ", 1))
    assert bad, "a tab-indented board was accepted"


def test_the_checker_accepts_the_real_board(good_board) -> None:
    """The control. If this fails, `main` is carrying a board no consumer can load."""
    from hkpy.boardcheck import problems

    assert problems(good_board) == []


def test_the_checker_refuses_a_board_it_cannot_read(tmp_path) -> None:
    """Fail closed: "we could not look" must never read as "it is fine"."""
    from hkpy.boardcheck import main

    assert main(["boardcheck", str(tmp_path / "nope.yaml")]) == 1


def test_the_checker_announces_success_with_a_token_the_hook_can_see(good_board) -> None:
    """Exit 0 alone is not evidence: `true` exits 0. The hook requires this line."""
    from hkpy.boardcheck import check_text

    code, msg = check_text(good_board)
    assert code == 0 and msg.startswith("board-ok:"), msg


def test_the_real_checker_command_works_as_the_hook_invokes_it(good_board) -> None:
    """The hook's DEFAULT command, run for real — not a stand-in the tests wired up.

    `uv run --locked --project <repo>/py` is what supplies the strict loader on both writer
    paths; if that invocation ever stops working, the hook fails closed and every board commit
    stops, so it is worth asserting directly.
    """
    if shutil.which("uv") is None:  # pragma: no cover - uv is required by every other py recipe
        pytest.skip("uv not installed")
    cmd = ["uv", "run", "--locked", "--project", str(PYDIR), "python", "-m", "hkpy.boardcheck", "-"]
    bad = subprocess.run(cmd, input=_break_it(good_board), capture_output=True, text=True)
    assert bad.returncode == 1, bad.stderr
    assert "REFUSING" in bad.stderr
    ok = subprocess.run(cmd, input=good_board, capture_output=True, text=True)
    assert ok.returncode == 0 and "board-ok:" in ok.stdout, ok.stderr


# ------------------------------------------------------------------- path 1: a direct commit


@pytest.fixture()
def hooked_repo(tmp_path, good_board) -> Path:
    """A repo wired exactly as `just setup-git` wires one, holding the real board."""
    repo = tmp_path / "repo"
    (repo / "docs").mkdir(parents=True)
    (repo / ".githooks").mkdir()
    shutil.copy2(HOOK, repo / ".githooks" / "pre-commit")
    (repo / "docs" / "tasks.yaml").write_text(good_board)
    _git(repo, "init", "-q", "-b", "main")
    _git(repo, "config", "user.email", "t@t")
    _git(repo, "config", "user.name", "t")
    _git(repo, "config", "core.hooksPath", ".githooks")
    _git(repo, "add", "-A")
    c = _git(repo, "commit", "-qm", "board", env=_checker_env())
    assert c.returncode == 0, c.stderr  # the good board commits, or the test proves nothing
    return repo


_EMPTY_OPS = tempfile.mkdtemp(prefix="hk-boardcheck-ops-")


def _checker_env() -> dict[str, str]:
    """Point the hook at THIS interpreter's hkpy — the temp repo has no `py/` of its own."""
    return {
        # Never the live ops dir: the main merge guard reads its bulk-in-progress, and the merge gate runs
        # these tests while a batch is in progress (2026-09-24: the fixture's first commit was refused).
        "HACKRIFF_OPS": _EMPTY_OPS,
        "HK_BOARDCHECK": f"{sys.executable} -m hkpy.boardcheck",
        "PYTHONPATH": str(PYDIR) + os.pathsep + os.environ.get("PYTHONPATH", ""),
    }


def test_a_direct_board_commit_is_refused(hooked_repo, good_board) -> None:
    """THE PATH THAT ACTUALLY BROKE IT: "File T-700", committed straight to the board."""
    (hooked_repo / "docs" / "tasks.yaml").write_text(_break_it(good_board))
    _git(hooked_repo, "add", "-A")
    r = _git(hooked_repo, "commit", "-qm", "Board: file a ticket", env=_checker_env())
    assert r.returncode != 0, "a board no YAML loader accepts was committed"
    assert "REFUSING" in r.stderr, r.stderr
    assert _git(hooked_repo, "rev-list", "--count", "HEAD").stdout.strip() == "1"


def test_the_hook_reads_the_INDEX_not_the_working_tree(hooked_repo, good_board) -> None:
    """What gets committed is the index. A tidy working tree must not vouch for a bad stage."""
    (hooked_repo / "docs" / "tasks.yaml").write_text(_break_it(good_board))
    _git(hooked_repo, "add", "-A")
    (hooked_repo / "docs" / "tasks.yaml").write_text(good_board)  # working tree now innocent
    r = _git(hooked_repo, "commit", "-qm", "Board: staged bad, tree good", env=_checker_env())
    assert r.returncode != 0 and "REFUSING" in r.stderr, r.stderr


def test_the_hook_fails_closed_when_the_checker_cannot_run(hooked_repo, good_board) -> None:
    """Missing interpreter, missing module, broken env — REFUSE, never allow.

    A validator that silently no-ops is how this class of defect recurs; the point of the hook is
    that it is never the reason a bad board got through.
    """
    (hooked_repo / "docs" / "tasks.yaml").write_text(good_board + "\n")
    _git(hooked_repo, "add", "-A")
    r = _git(hooked_repo, "commit", "-qm", "board", env={"HK_BOARDCHECK": "/nonexistent/checker"})
    assert r.returncode != 0, "a commit went through with no working checker"
    assert "REFUSING" in r.stderr, r.stderr


def test_the_override_cannot_become_a_bypass(hooked_repo, good_board) -> None:
    """`HK_BOARDCHECK=true` exits 0 and validates nothing; the token check catches it."""
    (hooked_repo / "docs" / "tasks.yaml").write_text(_break_it(good_board))
    _git(hooked_repo, "add", "-A")
    r = _git(hooked_repo, "commit", "-qm", "board", env={"HK_BOARDCHECK": "true"})
    assert r.returncode != 0 and "REFUSING" in r.stderr, r.stderr


def test_a_commit_that_does_not_touch_the_board_is_untouched(hooked_repo) -> None:
    """The cost of the hook falls only on board commits; a worker committing Rust pays nothing.

    Deliberately run with NO checker configured at all: if this commit needed one, every
    unrelated commit in every worktree would depend on a working uv environment.
    """
    (hooked_repo / "src.rs").write_text("fn main() {}\n")
    _git(hooked_repo, "add", "-A")
    r = _git(hooked_repo, "commit", "-qm", "code", env={"HK_BOARDCHECK": "/nonexistent/checker"})
    assert r.returncode == 0, r.stderr


# ------------------------------------------------------- path 2: a driver-resolved merge


def _mini(*blocks: str) -> str:
    return "version: 1\ntasks:\n" + "\n".join(blocks) + "\nnotes:\n  - a note\n"


T1 = "  - id: T-001\n    title: one\n    status: todo\n"
T2 = "  - id: T-002\n    title: two\n    status: todo\n"
BAD = f"  - id: T-003\n    title: {T640_TITLE}\n    status: todo\n"


def test_the_merge_driver_refuses_a_resolution_that_is_not_strict_yaml() -> None:
    """`hkpy.boardmerge` resolves the board with NO HUMAN IN THE LOOP, so it must vouch for this.

    Both sides only append, which is precisely the case the driver automates — it would write
    this union happily on its structural checks alone. It is the strict parse that stops it.
    """
    from hkpy.boardmerge import _validate, merge, split

    base, ours, theirs = _mini(T1), _mini(T1, T2), _mini(T1, BAD)
    out = merge(base, ours, theirs)
    assert out is not None, "the driver refused for an unrelated reason; the test proves nothing"
    need = set(split(ours)[1]) | set(split(theirs)[1])
    why = _validate(out, need)
    assert why and "': '" in why, why


def test_the_merge_driver_still_writes_a_clean_append() -> None:
    """The control for the above: the same shape, correctly quoted, is still automated."""
    from hkpy.boardmerge import _validate, merge, split

    ok = f'  - id: T-003\n    title: "{T640_TITLE}"\n    status: todo\n'
    base, ours, theirs = _mini(T1), _mini(T1, T2), _mini(T1, ok)
    out = merge(base, ours, theirs)
    assert out is not None
    assert _validate(out, set(split(ours)[1]) | set(split(theirs)[1])) is None


def test_the_driver_cli_refuses_and_leaves_ours_untouched(tmp_path) -> None:
    """End to end as git invokes it: non-zero exit, and `%A` is not overwritten."""
    from hkpy.boardmerge import main

    paths = {}
    for name, text in (("base", _mini(T1)), ("ours", _mini(T1, T2)), ("theirs", _mini(T1, BAD))):
        p = tmp_path / name
        p.write_text(text)
        paths[name] = p
    rc = main(["boardmerge", str(paths["base"]), str(paths["ours"]), str(paths["theirs"])])
    assert rc == 1
    assert paths["ours"].read_text() == _mini(T1, T2), "a refused merge still wrote the result"


# --------------------------------------------- path 3: the runner's merge commit (MERGE_HEAD)


def test_a_merge_commit_carrying_a_broken_board_is_refused(hooked_repo, good_board) -> None:
    """`ops/merge-runner.sh` does `git merge --no-ff --no-commit` then `git commit` — this one.

    The branch's board is the 2026-09-22 break. The merge itself succeeds (it is a clean
    fast-forwardable change); the COMMIT is where it must stop, with `main` left untouched.
    """
    _git(hooked_repo, "checkout", "-q", "-b", "task-tbad")
    (hooked_repo / "docs" / "tasks.yaml").write_text(_break_it(good_board))
    _git(hooked_repo, "add", "-A")
    # The branch commit is refused by the same hook, so make it the one way a bad board can
    # legitimately exist on a branch: committed before the hook existed / with --no-verify.
    c = _git(hooked_repo, "commit", "-q", "--no-verify", "-m", "T-x: board")
    assert c.returncode == 0, c.stderr
    _git(hooked_repo, "checkout", "-q", "main")
    m = _git(hooked_repo, "merge", "--no-ff", "--no-commit", "task-tbad")
    assert m.returncode == 0, m.stderr + m.stdout
    r = _git(hooked_repo, "commit", "-qm", "Merge T-x (task-tbad): gate passed", env={**_checker_env(), "HK_MERGE_RUNNER": "1"})
    assert r.returncode != 0, "a merge commit put a board no loader accepts on main"
    assert "REFUSING" in r.stderr, r.stderr
    assert _git(hooked_repo, "rev-list", "--count", "main").stdout.strip() == "1"


# --------------------------------------------------------------------- path 4: the gated merge


def test_the_board_test_rejects_the_same_break(good_board) -> None:
    """The gate layer, asserted by its refusal rather than by its silence (T-762's guard)."""
    import test_task_board as board_test

    broken = _break_it(good_board)
    recs = board_test._records(broken)
    bad = [
        (i, k, v)
        for i, f in recs
        for k, v in f.items()
        if isinstance(v, str) and v[:1] not in ("'", '"', "|", ">", "[") and ": " in v
    ]
    assert bad, "the gate-layer textual guard no longer sees the 2026-09-22 break"


# --------------------------------------------------------------------------- the main merge guard

def test_no_commit_in_main_while_the_runner_has_a_merge_staged(hooked_repo, tmp_path) -> None:
    """2026-09-24 15:05:04 (292de09b): a board note committed in main while task-t899's merge was staged
    became a merge commit that landed T-899 UNGATED; 09-22 ea91c27c was the first. Only the merge
    runner's own commits (HK_MERGE_RUNNER=1) go through while a merge or a batch is in flight."""
    ops = tmp_path / "ops"
    ops.mkdir()
    env = {**_checker_env(), "HACKRIFF_OPS": str(ops)}
    _git(hooked_repo, "checkout", "-qb", "task-x")
    (hooked_repo / "x.txt").write_text("x\n")
    _git(hooked_repo, "add", "x.txt")
    assert _git(hooked_repo, "commit", "-qm", "work", env=env).returncode == 0
    _git(hooked_repo, "checkout", "-q", "main")
    assert _git(hooked_repo, "merge", "--no-ff", "--no-commit", "task-x", env=env).returncode == 0
    r = _git(hooked_repo, "commit", "-qm", "Board: a note", env=env)
    assert r.returncode != 0 and "main-guard: REFUSING" in r.stderr and "MERGE_HEAD" in r.stderr
    r = _git(hooked_repo, "commit", "-qm", "Merge task-x: gate passed", env={**env, "HK_MERGE_RUNNER": "1"})
    assert r.returncode == 0, r.stderr
    (ops / "bulk-in-progress").write_text("base=abc\n")
    (hooked_repo / "y.txt").write_text("y\n")
    _git(hooked_repo, "add", "y.txt")
    r = _git(hooked_repo, "commit", "-qm", "Board: another", env=env)
    assert r.returncode != 0 and "bulk-in-progress" in r.stderr
    (ops / "bulk-in-progress").unlink()
    assert _git(hooked_repo, "commit", "-qm", "Board: after the gate", env=env).returncode == 0


def test_a_worktree_merging_main_into_itself_is_never_refused(hooked_repo, tmp_path) -> None:
    ops = tmp_path / "ops"
    ops.mkdir()
    (ops / "bulk-in-progress").write_text("base=abc\n")
    env = {**_checker_env(), "HACKRIFF_OPS": str(ops)}
    wt = tmp_path / "wt"
    _git(hooked_repo, "worktree", "add", "-q", "-b", "task-w", str(wt))
    (wt / "w.txt").write_text("w\n")
    _git(wt, "add", "w.txt")
    r = _git(wt, "commit", "-qm", "T-1: work", env=env)
    assert r.returncode == 0, r.stderr
