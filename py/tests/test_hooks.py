"""The `.claude/hooks/` PreToolUse guards, run as the harness runs them: JSON in, JSON out.

These scripts are the only thing standing between an agent and a mistake that costs hours -
a worker running the full gate, a direct edit that breaks the board's YAML, a busy loop that
outlives its agent. Nothing else tests them: they are bash, they are invoked by the harness,
and `just ops-check` only proves they parse. A hook that silently stopped matching would be
discovered the way the 2026-09-22 failures were discovered, by a person running `ps`.

So each case here feeds the real script the real payload shape and asserts on the decision.
Two properties matter more than the individual patterns:

  * **FAIL-OPEN.** Garbage in, missing `jq`, an empty payload - the hook must exit 0 and allow.
    A hook that can fail an agent's turn is worse than the thing it guards against, and this is
    asserted separately from every allow case so a refactor cannot quietly lose it.
  * **The load guard binds every session, the full-gate guard binds only subagents.** They are
    different rules with different reasons: contention is contention whoever causes it, but the
    coordinator is *supposed* to run the gate.
"""
from __future__ import annotations

import json
import pathlib
import shutil
import subprocess

import pytest

HOOKS = pathlib.Path(__file__).resolve().parents[2] / ".claude" / "hooks"
BLOCK = HOOKS / "block-full-gate.sh"
BOARD = HOOKS / "block-board-edits.sh"
REAP = HOOKS / "reap-agent-processes.sh"

pytestmark = pytest.mark.skipif(shutil.which("jq") is None, reason="the hooks need jq")

WORKTREE = "/Users/daniellewis/hackriff/.claude/worktrees/t999"


def run(script, payload, env=None, ops=None):
    e = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin", "HOME": "/tmp"}
    if ops:
        e["HACKRIFF_OPS"] = str(ops)
    e.update(env or {})
    p = subprocess.run([str(script)], input=json.dumps(payload), text=True,
                       capture_output=True, env=e, timeout=30)
    assert p.returncode == 0, f"a hook must always exit 0; got {p.returncode}: {p.stderr}"
    if not p.stdout.strip():
        return None
    return json.loads(p.stdout)


def decision(out):
    return (out or {}).get("hookSpecificOutput", {}).get("permissionDecision")


def reason(out):
    return (out or {}).get("hookSpecificOutput", {}).get("permissionDecisionReason", "")


def bash(cmd, *, sub=True, **kw):
    payload = {"tool_input": {"command": cmd}, "cwd": WORKTREE if sub else "/Users/daniellewis/hackriff"}
    if sub:
        payload["agent_id"] = "agent_x"
    return run(BLOCK, payload, **kw)


# ------------------------------------------------------------------ the load guard
@pytest.mark.parametrize("cmd", [
    "while :; do :; done",
    "while true; do true; done &",
    "/bin/zsh -c 'source /x/snapshot-zsh-1.sh; while :; do :; done'",
])
def test_busy_loops_are_denied_for_everyone(cmd):
    """The 2026-09-22 shape. Denied in a subagent AND in the coordinator: a spinning core is a
    spinning core whoever started it, and the shell outlives the session either way."""
    assert decision(bash(cmd)) == "deny"
    assert decision(bash(cmd, sub=False)) == "deny"


def test_the_busy_loop_reason_names_the_override():
    assert "HK_ALLOW_LOAD=1" in reason(bash("while :; do :; done"))


@pytest.mark.parametrize("cmd", [
    "while :; do sleep 5; curl -s localhost:8901/x && break; done",   # the documented way to wait
    "until curl -sf localhost:8901; do sleep 2; done",
    "for f in ops/*.sh; do bash -n $f; done",
    "while read -r l; do echo $l; done < file.txt",
])
def test_polling_and_ordinary_loops_are_allowed(cmd):
    """The guard must not make waiting impossible - that would just move the damage."""
    assert bash(cmd) is None


@pytest.mark.parametrize("cmd", ["yes > /dev/null", "yes | head -5", "yes >/dev/null &"])
def test_yes_is_denied(cmd):
    assert decision(bash(cmd)) == "deny"


def test_the_word_yes_inside_another_command_is_allowed():
    assert bash("git log --grep=yes | head") is None
    assert bash("echo yes") is None


@pytest.mark.parametrize("cmd", ["stress -c 8", "stress-ng --cpu 4", "cd /tmp && stress -c 2"])
def test_stress_is_denied(cmd):
    assert decision(bash(cmd)) == "deny"


def test_more_than_two_backgrounded_loops_in_one_command():
    three = " ".join(["(while :; do sleep 1; done) &"] * 3)
    assert decision(bash(three)) == "deny"
    two = " ".join(["(while :; do sleep 1; done) &"] * 2)
    assert bash(two) is None


def test_redirections_and_and_lists_are_not_counted_as_backgrounding():
    """`2>&1` and `&&` are ampersands that background nothing. Counting them would deny most
    ordinary commands, and a guard that fires on everything gets switched off."""
    cmd = "for f in a b; do cargo build 2>&1 && echo ok && echo done; done"
    assert bash(cmd) is None


# ------------------------------------------------------------------ e2e / hk serve beside a gate
def test_spec_run_allowed_when_no_gate_is_running(tmp_path):
    """The one rule here that reads LIVE machine state, deliberately: "is a gate running right
    now" cannot be answered from the payload. So this case is skipped when a gate really is
    running, rather than asserted and flaky — it failed exactly that way on first run, while the
    merge runner happened to be gating. The deny side below is file-controlled and always runs.
    """
    if subprocess.run(["pgrep", "-f", "just gate"], capture_output=True).returncode == 0:
        pytest.skip("a merge gate is running on this box; the hook is correct to deny")
    assert bash("npm run e2e", ops=tmp_path) is None


def test_spec_run_denied_during_a_bulk_merge(tmp_path):
    (tmp_path / "bulk-in-progress").write_text("base=abc123\n")
    for cmd in ("npm run e2e", "node e2e/run.mjs", "./target/debug/hk serve --bind 127.0.0.1:8770"):
        out = bash(cmd, ops=tmp_path)
        assert decision(out) == "deny", cmd
        assert "bulk-in-progress" in reason(out)


def test_unrelated_commands_are_untouched_during_a_bulk_merge(tmp_path):
    (tmp_path / "bulk-in-progress").write_text("base=abc123\n")
    assert bash("cargo nextest run -p hk-dsp", ops=tmp_path) is None


def test_hk_allow_load_overrides_every_load_rule(tmp_path):
    (tmp_path / "bulk-in-progress").write_text("base=abc\n")
    env = {"HK_ALLOW_LOAD": "1"}
    for cmd in ("while :; do :; done", "stress -c 8", "yes >/dev/null", "npm run e2e"):
        assert bash(cmd, env=env, ops=tmp_path) is None, cmd


# ------------------------------------------------------------------ the pre-existing guards
def test_full_gate_still_blocked_in_a_subagent():
    out = bash("just gate")
    assert decision(out) == "deny" and "HK_ALLOW_FULL=1" in reason(out)


def test_full_gate_still_allowed_for_the_coordinator():
    assert bash("just gate", sub=False) is None


def test_targeted_tests_are_still_allowed_in_a_subagent():
    assert bash("just test-crate hk-dsp") is None
    assert bash("cargo nextest run -p hk-dsp -E 'binary(unit)'") is None


def test_board_writes_by_shell_are_still_blocked():
    assert decision(bash("sed -i '' s/x/y/ docs/tasks.yaml")) == "deny"
    assert decision(bash("echo x >> docs/tasks.yaml")) == "deny"


def test_board_edit_tool_guard_still_blocks_worktree_edits():
    out = run(BOARD, {"agent_id": "a", "cwd": WORKTREE,
                      "tool_input": {"file_path": f"{WORKTREE}/docs/tasks.yaml"}})
    assert decision(out) == "deny"


# ------------------------------------------------------------------ fail-open
@pytest.mark.parametrize("script", [BLOCK, BOARD, REAP])
@pytest.mark.parametrize("payload", ["", "not json at all", "{}", '{"tool_input":{}}'])
def test_every_hook_fails_open(script, payload):
    """A hook that can fail a turn is worse than what it guards against."""
    p = subprocess.run([str(script)], input=payload, text=True, capture_output=True,
                       env={"PATH": "/usr/bin:/bin:/opt/homebrew/bin", "HOME": "/tmp"}, timeout=30)
    assert p.returncode == 0
    assert decision(json.loads(p.stdout) if p.stdout.strip() else None) != "deny"


def test_hooks_fail_open_without_jq_or_ps():
    for script in (BLOCK, BOARD, REAP):
        p = subprocess.run([str(script)], input='{"tool_input":{"command":"while :; do :; done"}}',
                           text=True, capture_output=True, env={"PATH": "/nonexistent"}, timeout=30)
        assert p.returncode == 0, script


# ------------------------------------------------------------------ the reaper
def test_reaper_is_silent_when_there_is_nothing_to_reap(tmp_path):
    """It runs after every single agent turn on this box, so "nothing happened" must cost
    nothing and say nothing."""
    assert run(REAP, {"hook_event_name": "SubagentStop"}, ops=tmp_path) is None


def test_reaper_kill_conditions_are_all_three(tmp_path):
    """Read back from the script itself: the signature, the orphan test and the CPU floor are
    the three conditions that make the kill safe, and losing any one of them turns a cleanup
    into killing live agents' shells."""
    src = REAP.read_text()
    assert "shell-snapshots/snapshot-zsh" in src
    assert 'ps -p "$ppid"' in src          # the parent-alive test
    assert "> 50" in src                   # the CPU floor
    assert "kill -9" in src


def test_settings_wires_both_stop_events_to_the_reaper():
    s = json.loads((HOOKS.parent / "settings.json").read_text())
    for event in ("Stop", "SubagentStop"):
        cmds = [h["command"] for g in s["hooks"][event] for h in g["hooks"]]
        assert any("reap-agent-processes.sh" in c for c in cmds), event


def test_settings_bounds_every_session_build(tmp_path):
    """T-144's bound, applied where it actually binds: a Claude session's own shells. The work
    runner's cpulimit wraps a worker's tree, but an agent's `cargo build -j 28` inside that tree
    still oversubscribes the box's scheduler."""
    env = json.loads((HOOKS.parent / "settings.json").read_text())["env"]
    assert env["CARGO_BUILD_JOBS"] == "3"
    assert env["NEXTEST_TEST_THREADS"] == "2"
    assert env["CARGO_INCREMENTAL"] == "0"
