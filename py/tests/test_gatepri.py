"""Cheap branches first, alone (hkpy.gatepri + ops/merge-runner.sh; user, 2026-09-24), and the
dashboard preview (ops/preview-dashboard.sh, monitor.py MONITOR_PREVIEW)."""
from __future__ import annotations

import importlib.util
import os
import pathlib
import subprocess

from hkpy import codemetrics, gate, gatepri

ROOT = pathlib.Path(__file__).resolve().parents[2]


def _git(repo, *a):
    subprocess.run(["git", "-C", str(repo), "-c", "user.email=t@t", "-c", "user.name=t", *a], check=True, capture_output=True)


def test_a_branch_is_classified_by_the_gates_own_rule(tmp_path):
    r = tmp_path / "r"
    (r / "ops").mkdir(parents=True)
    (r / "ops" / "x.py").write_text("a = 1\n")
    _git(r, "init", "-q", "-b", "main")
    _git(r, "add", "-A")
    _git(r, "commit", "-qm", "base")
    for b, path in (("dash", "ops/monitor.py"), ("prod", "crates/hk-x/src/lib.rs")):
        _git(r, "checkout", "-q", "-b", b, "main")
        (r / path).parent.mkdir(parents=True, exist_ok=True)
        (r / path).write_text("x\n")
        _git(r, "add", "-A")
        _git(r, "commit", "-qm", b)
    assert gatepri.branch_class(str(r), "main", "dash") == gate.classify(["ops/monitor.py"]).label != gate.FULL
    assert gatepri.branch_class(str(r), "main", "prod") == gate.FULL
    assert gatepri.branch_class(str(r), "main", "no-such-branch") == gate.FULL      # cannot say -> full
    _git(r, "checkout", "-q", "-b", "mv", "prod")
    _git(r, "mv", "crates/hk-x/src/lib.rs", "ops/lib.rs")
    _git(r, "commit", "-qm", "move")
    assert gatepri.branch_class(str(r), "prod", "mv") == gate.FULL          # a move out of crates/ is full (--no-renames)


def test_partition_keeps_queue_order_and_unknowns_are_full():
    classes = {"t1": "full", "pm-a": "py+ops", "t2": "full", "ui-b": "ui"}
    assert gatepri.partition(classes, ["t1", "pm-a", "t2", "ui-b", "t9"]) == (["pm-a", "ui-b"], ["t1", "t2", "t9"])


def test_the_runner_takes_the_cheap_ones_alone_before_forming_the_batch():
    sh = (ROOT / "ops" / "merge-runner.sh").read_text()
    block = sh.index("CHEAP FIRST (user, 2026-09-24")
    # before the SUITE_BROKEN hold (review): the hold must compare the batch that will actually gate,
    # or a red cheap batch never matches the mixed queue's signature and re-gates forever
    assert block < sh.index("# After a SUITE_BROKEN rewind") < sh.index("set -- $ready", block)
    assert sh.index("set -- $ready", block) < sh.index('if [ "$#" -gt "$BULK_MAX" ]; then')
    seg = sh[block:sh.index("# After a SUITE_BROKEN rewind")]
    assert 'for b in $rest; do echo "$b" >> "$QUEUE"; done' in seg and 'ready="$cheap"' in seg
    assert '[ -n "$cheap" ] && [ -n "$rest" ]' in seg          # nothing changes unless both kinds are queued


def test_a_preview_runs_its_own_code_and_writes_no_trend(tmp_path, monkeypatch):
    spec = importlib.util.spec_from_file_location("hk_mon_preview", ROOT / "ops" / "monitor.py")
    monkeypatch.setenv("MONITOR_PREVIEW", "1")
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    assert m.CODE_ROOT == str(ROOT) and m.METRICS_CACHE.startswith(str(ROOT))   # the copy's own root
    assert m.STATE == str(ROOT) and m.USAGE_FILE.startswith(str(ROOT))           # writes nothing the real one reads
    src = (ROOT / "ops" / "monitor.py").read_text()
    assert "if not PREVIEW:" in src[src.index('if __name__ == "__main__":'):src.index("target=_usage_poller")]
    monkeypatch.setenv("HK_METRICS_NO_SAMPLE", "1")
    codemetrics.sample(str(tmp_path), {"sha": "x", "lines": {"total": {}, "by_lang": []}, "churn": {"totals": {"24h": {}}}}, 0)
    assert not (tmp_path / "metrics.jsonl").exists()


def test_the_preview_script_never_takes_the_real_dashboards_port():
    env = dict(os.environ, PREVIEW_PORT="8901", PREVIEW_DIR="/nonexistent-preview")
    r = subprocess.run(["bash", str(ROOT / "ops" / "preview-dashboard.sh"), "main"], capture_output=True, text=True, env=env)
    assert r.returncode == 2 and "8901" in r.stdout
    r = subprocess.run(["bash", str(ROOT / "ops" / "preview-dashboard.sh"), "main"], capture_output=True, text=True,
                       env=dict(env, PREVIEW_PORT="08901"))                  # a string compare let this through
    assert r.returncode == 2 and "8901" in r.stdout
    r = subprocess.run(["bash", str(ROOT / "ops" / "preview-dashboard.sh"), "no-such-branch-xyz"], capture_output=True, text=True,
                       env=dict(os.environ, PREVIEW_DIR="/nonexistent-preview"))
    assert r.returncode == 2 and "no such branch" in r.stdout
