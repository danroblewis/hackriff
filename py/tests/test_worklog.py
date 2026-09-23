"""ops/worklog.py - the role work log the user reads on the dashboard (2026-09-23)."""

import importlib.util
import json
import os

_spec = importlib.util.spec_from_file_location(
    "worklog", os.path.join(os.path.dirname(__file__), "..", "..", "ops", "worklog.py"))
W = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(W)


def user(text, ts, **kw):
    return json.dumps({"type": "user", "timestamp": ts, "message": {"role": "user", "content": text}, **kw})


def asst(blocks, ts, **kw):
    return json.dumps({"type": "assistant", "timestamp": ts, "message": {"role": "assistant", "content": blocks}, **kw})


def text(t):
    return {"type": "text", "text": t}


def tool(name="Bash"):
    return {"type": "tool_use", "name": name, "input": {}}


def tool_result(ts):
    return json.dumps({"type": "user", "timestamp": ts,
                       "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}})


TRANSCRIPT = [
    json.dumps({"type": "mode", "mode": "normal"}),
    user("start the tick", "2026-09-23T22:00:00Z"),
    asst([text("Checking the gate first.")], "2026-09-23T22:00:05Z"),     # narration, not the result
    asst([tool()], "2026-09-23T22:00:06Z"),
    tool_result("2026-09-23T22:00:07Z"),
    asst([text("## Tick\nAll quiet.")], "2026-09-23T22:00:10Z"),
    asst([text("flow: 1.2/h (6h) · reds 0/3 · touchpoints 0 · E-001 gate 1/8 · holding: none")], "2026-09-23T22:00:11Z"),
    asst([text("sub-agent chatter")], "2026-09-23T22:00:12Z", isSidechain=True),
    user("meta", "2026-09-23T22:00:13Z", isMeta=True),
    user([{"type": "text", "text": "second request\nwith detail"}], "2026-09-23T22:30:00Z"),
    asst([text("Working on it.")], "2026-09-23T22:30:05Z"),
]


def test_a_turns_result_is_its_last_run_of_text_not_its_narration():
    st = {"turns": [], "cur": None}
    W.feed(st, TRANSCRIPT)
    done = st["turns"]
    assert len(done) == 1
    assert done[0]["prompt"] == "start the tick"
    assert done[0]["text"].startswith("## Tick") and "flow: 1.2/h" in done[0]["text"]
    assert "Checking the gate" not in done[0]["text"] and "sub-agent" not in done[0]["text"]
    assert st["cur"]["prompt"] == "second request" and st["cur"]["parts"] == ["Working on it."]


def test_turns_of_is_incremental_and_marks_the_open_turn(tmp_path):
    p = tmp_path / "s.jsonl"
    p.write_text("\n".join(TRANSCRIPT[:7]) + "\n")
    first = W.turns_of(str(p))
    assert [t.get("open", False) for t in first] == [True]         # no next prompt yet: still open
    with open(p, "a") as fh:
        fh.write("\n".join(TRANSCRIPT[7:]) + "\n" + '{"type":"assist')   # a half-written line waits
    later = W.turns_of(str(p))
    assert [t.get("open", False) for t in later] == [False, True]
    assert later[0]["text"].startswith("## Tick") and later[1]["text"] == "Working on it."


def test_discover_names_roles_from_argv_tmux_pointer_and_registry(tmp_path):
    sess, ops = tmp_path / "sessions", tmp_path / "ops"
    sess.mkdir(); ops.mkdir()
    (sess / "1.json").write_text(json.dumps({"pid": 1, "sessionId": "pm-1", "tmux": "flow:@1.%1"}))
    (sess / "2.json").write_text(json.dumps({"pid": 2, "sessionId": "co-2", "tmux": None}))
    (sess / "3.json").write_text(json.dumps({"pid": 3, "sessionId": "user-3", "tmux": None}))
    (sess / "4.json").write_text(json.dumps({"pid": 4, "sessionId": "sup-4", "tmux": None}))
    (ops / "coordinator-session").write_text("co-old\n")
    (ops / "role-sessions.json").write_text(json.dumps({"sup-4": {"role": "supervisor", "source": "manual"}}))
    argv = {1: "claude --model opus", 2: "claude --append-system-prompt-file /r/.claude/roles/coordinator.md",
            3: "claude", 4: "claude"}.get
    reg = W.discover(str(sess), str(ops), now=100.0, argv=argv)
    assert reg["pm-1"]["role"] == "pipeline-manager" and reg["pm-1"]["source"] == "tmux"
    assert reg["co-2"]["role"] == "coordinator" and reg["co-2"]["source"] == "argv"
    assert reg["co-old"]["role"] == "coordinator" and not reg["co-old"].get("live")
    assert reg["sup-4"]["role"] == "supervisor" and reg["sup-4"]["live"]      # manual entry kept
    assert "user-3" not in reg                                                  # not a role session
    saved = json.load(open(ops / "role-sessions.json"))
    assert set(saved) == {"pm-1", "co-2", "co-old", "sup-4"} and "live" not in saved["pm-1"]


def test_build_orders_roles_and_turns_and_extracts_flow_lines(tmp_path):
    proj = tmp_path / "proj"
    proj.mkdir()
    (proj / "pm-1.jsonl").write_text("\n".join(TRANSCRIPT) + "\n")
    (proj / "co-2.jsonl").write_text("\n".join(TRANSCRIPT[:3]) + "\n")
    reg = {"co-2": {"role": "coordinator", "last_seen": 5}, "pm-1": {"role": "pipeline-manager", "last_seen": 5},
           "gone": {"role": "supervisor"}}                                     # no transcript: skipped
    d = W.build(proj=str(proj), reg=reg)
    assert [r["role"] for r in d["roles"]] == ["pipeline-manager", "coordinator", "supervisor"]
    pm = d["roles"][0]["turns"]
    assert pm[0]["text"] == "Working on it." and pm[0]["open"]              # newest first
    assert pm[1]["flow"] == ["flow: 1.2/h (6h) · reds 0/3 · touchpoints 0 · E-001 gate 1/8 · holding: none"]
    assert d["roles"][2]["turns"] == [] and d["roles"][2]["sessions"] == []
