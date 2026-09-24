"""ops/alert.py - Discord alerts, and pipeline alarms routed to the pipeline manager's session."""

import importlib.util
import json
import os

import pytest

_spec = importlib.util.spec_from_file_location("alert", os.path.join(os.path.dirname(__file__), "..", "..", "ops", "alert.py"))
A = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(A)


@pytest.fixture(autouse=True)
def _isolate(tmp_path, monkeypatch):
    monkeypatch.setattr(A, "LOG", str(tmp_path / "alerts.jsonl"))
    monkeypatch.setattr(A, "CFG", str(tmp_path / "discord.json"))       # unconfigured: nothing posts
    monkeypatch.delenv("HK_ALERT_OFF", raising=False)


class Tmux:
    def __init__(self, has=True):
        self.calls, self.has = [], has

    def __call__(self, args, **kw):
        self.calls.append(args)

        class R:
            returncode = 0 if (self.has or args[1] != "has-session") else 1
        return R()


def test_a_pipeline_alarm_is_typed_into_the_flow_session_once(monkeypatch):
    t = Tmux()
    monkeypatch.setattr(A, "wake_pipeline_manager", lambda *a, _w=A.wake_pipeline_manager: _w(*a, run=t))
    A.notify("red", "2 merge gates running at once", "pid 1\npid 2", key="watchdog:double-gate")
    sends = [c for c in t.calls if c[1] == "send-keys"]
    assert sends[0][:4] == ["tmux", "send-keys", "-t", "flow"] and sends[0][4] == "-l"
    assert "2 merge gates running at once - pid 1 pid 2" in sends[0][5] and "watchdog:double-gate" in sends[0][5]
    assert sends[1][-1] == "Enter"
    rec = [json.loads(ln) for ln in open(A.LOG)]
    assert rec[-1]["woke"] is True and rec[-1]["status"] == "unconfigured"
    t.calls.clear()
    A.notify("red", "2 merge gates running at once", "", key="watchdog:double-gate")   # same key: once per 30 min
    assert t.calls == []


def test_only_pipeline_keys_wake_and_a_missing_session_is_not_an_error(monkeypatch):
    t = Tmux()
    assert A.wake_pipeline_manager("green", "landed", "x", "", run=t) is False
    assert A.wake_pipeline_manager("amber", "flake", "x", "flow:digest", run=t) is False
    assert t.calls == []
    for key in ("mr:T-1 conflict", "hold:pm", "timeout:gate", "contended-gate", "flake:x.e2e.mjs"):
        assert A.wake_pipeline_manager("amber", "t", "b", key, run=Tmux()) is True
    gone = Tmux(has=False)
    assert A.wake_pipeline_manager("red", "t", "b", "watchdog:x", run=gone) is False
    assert [c[1] for c in gone.calls] == ["has-session"]


def test_hk_alert_off_wakes_nobody(monkeypatch):
    monkeypatch.setenv("HK_ALERT_OFF", "1")
    t = Tmux()
    monkeypatch.setattr(A, "wake_pipeline_manager", lambda *a: t(["tmux", "send-keys"]))
    A.notify("red", "x", "", key="watchdog:x")
    assert t.calls == []


def _records(tmp_path):
    return [json.loads(ln) for ln in (tmp_path / "alerts.jsonl").read_text().splitlines()]


def test_a_pipeline_alarm_with_no_flow_session_escalates_red_no_receiver(tmp_path, monkeypatch):
    """Incident 2026-09-24 04:07: the role sessions were dead 5.5 h; alarms were typed at nothing."""
    t = Tmux(has=False)
    monkeypatch.setattr(A, "wake_pipeline_manager", lambda *a, _w=A.wake_pipeline_manager: _w(*a, run=t))
    A.notify("amber", "merge runner needs a person", "batch red", key="mr:batch red")
    assert not [c for c in t.calls if c[1] == "send-keys"]
    nr = [r for r in _records(tmp_path) if r["key"] == "noreceiver:flow"]
    assert len(nr) == 1 and nr[0]["level"] == "red" and "pipeline-manager" in nr[0]["title"]


def test_the_shell_relays_report_a_missing_session_through_the_cli(tmp_path):
    assert A.main(["--no-receiver", "dev", "MERGE-RUNNER: batch red"]) == 0
    (r,) = _records(tmp_path)
    assert r["key"] == "noreceiver:dev" and r["level"] == "red" and "'dev' (coordinator)" in r["title"]


def test_every_tmux_relay_escalates_when_its_session_is_gone():
    """Each `has-session` guard in the runners must say so, not return quietly."""
    ops = os.path.join(os.path.dirname(__file__), "..", "..", "ops")
    mr = open(os.path.join(ops, "merge-runner.sh")).read()
    assert mr.count('tmux has-session -t dev 2>/dev/null || { no_receiver "$1"; return 0; }') == 2
    assert "has-session -t dev 2>/dev/null || return 0" not in mr
    wr = open(os.path.join(ops, "work-runner.py")).read()
    i = wr.index('["tmux", "has-session", "-t", "dev"]')
    assert '"--no-receiver", "dev"' in wr[i:i + 900]
