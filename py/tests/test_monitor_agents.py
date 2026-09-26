"""The dashboard's agents panel (ops/monitor.py:agents) names roles and decides liveness honestly
(user ask 2026-09-25 09:45: six sessions were labelled 'supervisor', and sessions killed minutes
earlier still showed as running). A role comes ONLY from $HACKRIFF_OPS/role-session/<role> (written
by ops/launch.sh); a session is live only with a claude process for its id. Hermetic: PROJ,
SCRATCH and ~/.claude/sessions are temp dirs and `ps` is a stub."""

import importlib.util
import json
import os
import subprocess
import sys

import pytest

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
MONITOR_PATH = os.path.join(REPO, "ops", "monitor.py")

SUP = "f3bbe26c-3320-4bd0-b691-0ea6983829f6"
PM = "446ef7c1-2cbb-4fab-a8b1-046b2c5f3bc7"
OTHER = "11111111-2222-4333-8444-555555555555"


@pytest.fixture()
def mon(tmp_path, monkeypatch):
    before = set(sys.modules)
    spec = importlib.util.spec_from_file_location("hk_ops_monitor_agents_under_test", MONITOR_PATH)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    proj, scratch, reg = tmp_path / "proj", tmp_path / "ops", tmp_path / "sessions"
    for d in (proj, scratch, reg):
        d.mkdir()
    monkeypatch.setattr(m, "PROJ", str(proj))
    monkeypatch.setattr(m, "SCRATCH", str(scratch))
    monkeypatch.setattr(m, "CLAUDE_SESSIONS", str(reg))
    monkeypatch.setattr(m, "COORD", "00000000-0000-4000-8000-000000000000")
    monkeypatch.setattr(m, "load_tasks_yaml", lambda: [])
    m.ps_out = ""
    monkeypatch.setattr(m, "_ps", lambda: m.ps_out)
    m.t = {"proj": proj, "scratch": scratch, "reg": reg}
    yield m
    for k in set(sys.modules) - before:
        if k == "hk_ops_monitor_agents_under_test":
            sys.modules.pop(k, None)


def _session(m, sid, age_s, subagents=False):
    p = m.t["proj"] / f"{sid}.jsonl"
    p.write_text(json.dumps({"type": "user", "timestamp": "2026-09-25T09:00:00Z",
                             "message": {"content": f"hello from {sid[:8]}"}}) + "\n")
    t = m.time.time() - age_s
    os.utime(p, (t, t))
    if subagents:
        (m.t["proj"] / sid / "subagents").mkdir(parents=True)


def _role(m, role, sid):
    d = m.t["scratch"] / "role-session"
    d.mkdir(exist_ok=True)
    (d / role).write_text(sid + "\n")


def _rows(m):
    return {a["session"]: a for a in m.agents({}) if a.get("session")}


def test_roles_come_only_from_role_session_files(mon):
    _role(mon, "supervisor", SUP)
    _role(mon, "pipeline-manager", PM)
    for sid in (SUP, PM, OTHER):
        _session(mon, sid, 10, subagents=True)
    mon.ps_out = f"  100 claude --resume {SUP} --x\n  200 claude --session-id {PM}\n  300 claude --session-id {OTHER}\n"
    rows = _rows(mon)
    assert rows[SUP[:8]]["name"] == "supervisor"
    assert rows[PM[:8]]["name"] == "pipeline-manager"
    # a subagents/ dir no longer makes a session the supervisor
    assert rows[OTHER[:8]]["name"] == "session"
    assert all(r["running"] for r in rows.values())


def test_without_role_files_nothing_is_the_supervisor(mon):
    for sid in (SUP, PM, OTHER):
        _session(mon, sid, 10, subagents=True)
    mon.ps_out = "".join(f"  {i} claude --session-id {s}\n" for i, s in enumerate((SUP, PM, OTHER), 1))
    assert {r["name"] for r in _rows(mon).values()} == {"session"}


def test_the_coordinator_file_replaces_the_stale_pointer(mon):
    (mon.t["scratch"] / "coordinator-session").write_text(OTHER)
    _session(mon, OTHER, 10)
    _session(mon, PM, 10)
    mon.ps_out = f"  1 claude --session-id {OTHER}\n  2 claude --session-id {PM}\n"
    assert _rows(mon)[OTHER[:8]]["name"] == "coordinator"       # the pointer stands in until...
    _role(mon, "coordinator", PM)                                # ...launch.sh writes the role file
    rows = _rows(mon)
    assert rows[PM[:8]]["name"] == "coordinator" and rows[OTHER[:8]]["name"] == "session"


def test_the_claude_session_registry_is_liveness_evidence(mon):
    _role(mon, "pipeline-manager", PM)
    _session(mon, PM, 600)                                       # quiet for 10 min, but live
    (mon.t["reg"] / "38811.json").write_text(json.dumps({"pid": 38811, "sessionId": PM}))
    mon.ps_out = "38811 claude --model opus --append-system-prompt-file /r/.claude/roles/pipeline-manager.md\n"
    row = _rows(mon)[PM[:8]]
    assert row["running"] and "ended" not in row
    mon.ps_out = "38811 /bin/zsh\n"                              # the pid was reused by something else
    assert PM[:8] not in _rows(mon)


def test_a_dead_role_session_is_ended_then_hidden(mon):
    _role(mon, "supervisor", SUP)
    _session(mon, SUP, 60)                                       # fresh transcript, no process
    row = _rows(mon)[SUP[:8]]
    assert row["running"] is False
    assert row["ended"] == mon.time.strftime("%H:%M", mon.time.localtime(mon.time.time() - 60))
    _session(mon, SUP, 400)                                      # ended more than 5 min ago
    assert SUP[:8] not in _rows(mon)


def test_other_sessions_fall_back_to_transcript_age(mon):
    _session(mon, OTHER, 60)
    assert _rows(mon)[OTHER[:8]]["running"] is True
    _session(mon, OTHER, 240)
    row = _rows(mon)[OTHER[:8]]
    assert row["running"] is False and row["ended"]
    _session(mon, OTHER, 400)
    assert OTHER[:8] not in _rows(mon)


def _launch(tmp_path, *args):
    """ops/launch.sh against a stub tmux (logs its argv) and a temp HACKRIFF_OPS; no real session."""
    stub = tmp_path / "bin"
    stub.mkdir(exist_ok=True)
    (stub / "tmux").write_text('#!/bin/sh\necho "$*" >> "$(dirname "$0")/tmux.log"\n[ "$1" = has-session ] && exit 1\nexit 0\n')
    (stub / "cpulimit").write_text("#!/bin/sh\nexit 0\n")        # no include-children: unbounded, no wait
    for f in ("tmux", "cpulimit"):
        (stub / f).chmod(0o755)
    ops = tmp_path / "ops"
    ops.mkdir(exist_ok=True)
    env = dict(os.environ, HACKRIFF_OPS=str(ops), PATH=f"{stub}:{os.environ['PATH']}")
    r = subprocess.run(["bash", os.path.join(REPO, "ops", "launch.sh"), *args], env=env, capture_output=True, text=True, timeout=30)
    typed = [line for line in (stub / "tmux.log").read_text().splitlines() if line.startswith("send-keys") and "exec env" in line]
    (stub / "tmux.log").unlink()
    return r, typed[0], ops


@pytest.mark.skipif(not os.path.exists("/Users/daniellewis/hackriff/.claude/roles/coordinator.md"), reason="launch.sh reads the role file from the repo")
def test_launch_sh_passes_and_records_the_session_id(tmp_path):
    r, typed, ops = _launch(tmp_path, "coordinator")
    assert r.returncode == 0, r.stderr
    sid = (ops / "role-session" / "coordinator").read_text().strip()
    assert len(sid) == 36 and sid == sid.lower() and f"--session-id {sid}" in typed
    assert (ops / "coordinator-session").read_text().strip() == sid      # the older pointer, for worklog.py
    # a resume keeps its id: no --session-id added, the resumed id recorded
    r, typed, ops = _launch(tmp_path, "supervisor", "--resume", SUP)
    assert r.returncode == 0 and "--session-id" not in typed and f"--resume {SUP}" in typed
    assert (ops / "role-session" / "supervisor").read_text().strip() == SUP
    # --continue resumes an id launch.sh cannot know: the file is left alone, and it says so
    r, typed, ops = _launch(tmp_path, "supervisor", "--continue")
    assert r.returncode == 0 and "--session-id" not in typed and "left as it was" in r.stderr
    assert (ops / "role-session" / "supervisor").read_text().strip() == SUP
