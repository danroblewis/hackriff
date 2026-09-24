"""Ops scripts run from the repo, never a worktree (incident, 2026-09-24).

The dashboard was started from the pm-dashmem worktree, and its heavy builds re-ran monitor.py by
the running file's path. When that branch landed the runner removed the worktree, and /flow
answered `FileNotFoundError: .../worktrees/pm-dashmem/ops/monitor.py`.
"""
from __future__ import annotations

import pathlib
import subprocess
import sys

import pytest

OPS = pathlib.Path(__file__).resolve().parents[2] / "ops"
sys.path.insert(0, str(OPS))
import launchpath  # noqa: E402


def _tree(tmp_path, inside):
    d = tmp_path / (".claude/worktrees/pm-x/ops" if inside else "repo/ops")
    d.mkdir(parents=True)
    f = d / "script"
    f.write_text("")
    return f


def test_python_scripts_log_their_path_and_refuse_a_worktree(tmp_path):
    said = []
    assert launchpath.check(str(_tree(tmp_path, False)), said.append).endswith("/repo/ops/script")
    assert said[0].startswith("PATH: ") and len(said) == 1
    said.clear()
    with pytest.raises(SystemExit) as e:
        launchpath.check(str(_tree(tmp_path, True)), said.append)
    assert e.value.code == 2 and said[1].startswith("REFUSED: started from a worktree")


@pytest.mark.parametrize("inside,code", [(False, 0), (True, 2)])
def test_the_bash_guard_does_the_same(tmp_path, inside, code):
    f = _tree(tmp_path, inside)
    r = subprocess.run(["bash", "-c", f'log(){{ echo "$*"; }}; . "{OPS}/launch-guard.sh"; launch_guard "{f}"; echo ran-on'],
                       capture_output=True, text=True)
    assert r.returncode == code and r.stdout.startswith("PATH: ")
    assert ("ran-on" in r.stdout) == (not inside) and ("REFUSED" in r.stdout) == inside


def test_every_ops_daemon_runs_the_guard_before_it_does_anything():
    for name, guard, first_effect in [
        ("monitor.py", "launchpath.check(__file__", "threading.Thread(target=_rss_guard"),
        ("work-runner.py", "launchpath.check(__file__, log)", 'log(f"VERSION:'),
        ("watchdog.py", "launchpath.check(__file__, logline)", 'logline(f"START pid='),
        ("merge-runner.sh", 'launch_guard "${BASH_SOURCE[0]}"', "just setup-git"),
        ("stage.sh", 'launch_guard "${BASH_SOURCE[0]}"', "build(){"),
    ]:
        text = (OPS / name).read_text()
        assert guard in text, name
        assert text.index(guard) < text.index(first_effect), name


def test_the_dashboards_child_builds_load_the_repos_monitor_not_the_running_file():
    text = (OPS / "monitor.py").read_text()
    assert 'os.path.join(OPSDIR, "monitor.py")' not in text
    # the child loads CODE_ROOT's monitor.py: REPO for the real instance, the preview's own copy only
    # under MONITOR_PREVIEW=1 (ops/preview-dashboard.sh) - never a worktree's
    assert 'os.path.join(CODE_ROOT, "ops", "monitor.py")' in text
    assert 'CODE_ROOT = os.path.dirname(OPSDIR) if PREVIEW else REPO' in text
    assert 'os.path.join(REPO, "ops", "alert.py")' in (OPS / "work-runner.py").read_text()
