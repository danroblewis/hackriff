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
