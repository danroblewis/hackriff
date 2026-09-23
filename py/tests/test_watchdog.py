"""ops/watchdog.py: attribution and the five contention rules, against a synthetic `ps` table.

The incident this exists for cannot be reproduced on demand: a deflaker agent exited and left
sixteen `/bin/zsh -c source …/shell-snapshots/…` loops reparented to launchd, each at 100 %
CPU, running from 14:29 to 16:47 through every merge gate; a killed merge runner left an
orphan `just gate` beside its replacement's; `ops/monitor.py` sat at 440 %. Nobody is going to
recreate that to test a watchdog, and a watchdog whose rules have never been exercised is a
watchdog that fires wrongly, or not at all, the first time it matters.

So the process table is the test input. `parse_ps` is a pure function over `ps` output and
`attribute`/`evaluate` are pure functions over its rows plus a carried `since` dict, which
makes the whole incident expressible as text - including the parts that only exist over time,
because `since` is passed in and the clock is an argument.

TWO PROPERTIES ARE WORTH MORE THAN THE REST:

  * **Ancestry answers before any command-line guess.** A worker's `rustc`, its `git`, its
    `/bin/zsh` are the worker's, however they are spelled; only a process whose ancestors are
    all gone can be UNOWNED. If that inverted, a live agent's shell would look orphaned and
    rule (b) would kill working agents.
  * **The kill is signature-locked and owner-locked.** Exactly one rule kills, only for
    `shell-snapshots/snapshot-zsh`, only when unowned, only above 50 % for ten minutes. Every
    other rule alerts. `test_kill_*` pins each of those four conditions separately, because a
    watchdog that kills on a guess is worse than the contention it watches for.
"""
from __future__ import annotations

import importlib.util
import pathlib
import sys

import pytest

_WD = pathlib.Path(__file__).resolve().parents[2] / "ops" / "watchdog.py"


def _load():
    """Import ops/watchdog.py by path: `ops/` is not a package and must not become one (its
    scripts are launched as files, per ops/README.md)."""
    spec = importlib.util.spec_from_file_location("hk_watchdog", _WD)
    mod = importlib.util.module_from_spec(spec)
    sys.modules["hk_watchdog"] = mod
    spec.loader.exec_module(mod)
    return mod


W = _load()

SNAP = "/bin/zsh -c source /Users/daniellewis/.claude/shell-snapshots/snapshot-zsh-1758547-abc.sh && cargo test"


def row(pid, ppid, cmd, cpu=0.0, pgid=None, rss_kb=10240, etime="01:00"):
    return f"{pid} {ppid} {pgid or pid} {cpu} {rss_kb} {etime} {cmd}"


def table(*lines):
    return W.parse_ps("  PID  PPID  PGID  %CPU    RSS     ELAPSED COMMAND\n" + "\n".join(lines))


# --------------------------------------------------------------------- parsing
def test_parse_ps_reads_the_real_column_layout():
    rows = table(row(4242, 1, "/opt/homebrew/bin/sccache --start-server", cpu=12.5,
                     rss_kb=2048, etime="03:21"))
    assert rows == [{"pid": 4242, "ppid": 1, "pgid": 4242, "cpu": 12.5, "rss_mb": 2.0,
                     "etime": 201, "cmd": "/opt/homebrew/bin/sccache --start-server"}]


def test_parse_ps_keeps_spaces_in_the_command():
    (r,) = table(row(9, 1, "/bin/zsh -c source /x/y.sh && cargo test --all"))
    assert r["cmd"] == "/bin/zsh -c source /x/y.sh && cargo test --all"


@pytest.mark.parametrize("text,secs", [
    ("00:42", 42), ("07:30", 450), ("1:02:03", 3723), ("04-11:51:55", 388315), ("garbage", 0),
])
def test_etime_seconds(text, secs):
    assert W.etime_seconds(text) == secs


def test_parse_ps_skips_truncated_lines_rather_than_dying():
    """`ps` under load is exactly when this runs, and exactly when it truncates."""
    rows = table("123 1 123 0.0", row(9, 1, "ok"))
    assert [r["pid"] for r in rows] == [9]


# --------------------------------------------------------------------- attribution
CLAIMS = {"T-513": {"state": "running", "pid": 800, "ticket": "T-513"},
          "T-607": {"state": "queued", "pid": 900, "ticket": "T-607"}}


def test_worker_owns_its_whole_process_tree():
    rows = table(
        row(800, 1, "cpulimit -l 300 -i -- taskpolicy -c background claude -p"),
        row(810, 800, "claude -p --model sonnet"),
        row(820, 810, SNAP, cpu=4.0),                       # a LIVE agent's shell
        row(830, 820, "cargo nextest run -p hk-dsp", cpu=180.0),
        row(840, 830, "/usr/bin/git rev-parse HEAD"),       # system path, but the worker's
    )
    agg, unowned = W.owners(rows, CLAIMS)
    assert set(agg) == {"worker:T-513"}
    assert agg["worker:T-513"]["cpu"] == 184.0
    assert unowned == []


def test_a_finished_claim_owns_nothing():
    """`state` decides, not the pid: a reaped claim's pid may have been reused."""
    rows = table(row(900, 1, "some leftover", cpu=99.0))
    _, unowned = W.owners(rows, CLAIMS)
    assert [r["pid"] for r in unowned] == [900]


def test_role_session_named_from_its_role_file():
    rows = table(
        row(500, 1, "cpulimit -l 800 -i -- claude --model opus "
                    "--append-system-prompt-file /Users/d/hackriff/.claude/roles/coordinator.md"),
        row(510, 500, "claude --model sonnet", cpu=30.0),   # a subagent it spawned
    )
    agg, _ = W.owners(rows, {})
    assert set(agg) == {"role:coordinator"}
    assert agg["role:coordinator"]["cpu"] == 30.0


def test_hackriff_role_env_names_the_session_when_present():
    assert W.role_name("claude", "HACKRIFF_ROLE=supervisor PATH=/usr/bin") == "supervisor"
    assert W.role_name("bash ops/launch.sh supervisor") == "supervisor"
    assert W.role_name("claude --append-system-prompt-file /x/roles/coordinator.md") == "coordinator"
    assert W.role_name("claude") == "session"


def test_merge_runner_and_its_gate_are_separate_owners():
    """The 14-core reserve belongs to the GATE, so it gets its own line even under the runner
    that started it - otherwise the reserve and an orphan gate hide in the same number."""
    rows = table(
        row(100, 1, "bash ops/merge-runner.sh"),
        row(110, 100, "just gate --base abc123", cpu=5.0),
        row(120, 110, "uv run --project py python -m hkpy.gate --base abc123", cpu=20.0),
        row(130, 120, "cargo-nextest nextest run --workspace", cpu=900.0),
    )
    agg, unowned = W.owners(rows, {})
    assert agg["gate"]["cpu"] == 925.0
    assert agg["merge-runner"]["cpu"] == 0.0
    assert unowned == []


def test_one_gate_is_one_root_not_one_process_or_one_group():
    rows = table(
        row(110, 100, "just gate --base abc", pgid=110),
        row(120, 110, "uv run python -m hkpy.gate --base abc", pgid=110),
        row(121, 120, "just test", pgid=999),               # a `set -m` subshell, same gate
    )
    assert [g["pid"] for g in W.gate_roots(rows)] == [110]


def test_sccache_rustc_is_not_unowned():
    """Measured 2026-09-23: sccache reparents rustc to its own server, so a 429 % compile had
    ppid 1 by way of the daemon. Uncorrected, rule (a) ambers on every build."""
    rows = table(
        row(93140, 1, "/opt/homebrew/bin/sccache"),
        row(62637, 93140, "/opt/homebrew/bin/rustc --crate-name hk_dsp", cpu=429.6),
    )
    agg, unowned = W.owners(rows, {})
    assert unowned == []
    assert agg["sccache"]["cpu"] == 429.6


def test_macos_daemons_and_apps_are_named_not_unowned():
    rows = table(
        row(67258, 1, "/System/Library/PrivateFrameworks/MediaAnalysis.framework/…/mediaanalysisd", cpu=95.0),
        row(993, 778, "/Applications/Google Chrome.app/Contents/…/Helper", cpu=40.0),
    )
    agg, unowned = W.owners(rows, {})
    assert unowned == []
    assert set(agg) == {"system", "apps"}


def test_ancestry_beats_the_command_line_fallback():
    """A worker's own `claude` subprocess must not become `claude-other`, and its rustc must not
    become `sccache`: the fallback runs only after ancestry has failed."""
    rows = table(
        row(800, 1, "cpulimit -- claude -p"),
        row(801, 800, "claude --model haiku", cpu=10.0),
        row(802, 800, "/opt/homebrew/bin/rustc --crate-name x", cpu=10.0),
    )
    agg, _ = W.owners(rows, CLAIMS)
    assert set(agg) == {"worker:T-513"}


def test_an_orphaned_shell_loop_is_unowned():
    """The incident, in one row: parent gone, so nothing on this box accounts for it."""
    rows = table(row(31000, 1, SNAP, cpu=100.0, etime="02:18:00"))
    agg, unowned = W.owners(rows, CLAIMS)
    assert agg == {}
    assert [r["pid"] for r in unowned] == [31000]


# --------------------------------------------------------------------- budget
def test_budget_sums_the_owners_present():
    agg = {"gate": {"cpu": 0}, "worker:T-1": {"cpu": 0}, "worker:T-2": {"cpu": 0},
           "role:coordinator": {"cpu": 0}, "dashboard": {"cpu": 0}}
    assert W.budget(agg, cores=64) == 14 + 3 + 3 + 8 + 1 + W.HEADROOM


def test_budget_excludes_macos_and_the_users_desktop():
    assert W.budget({"dashboard": {}, "system": {}, "apps": {}, "tunnel": {}}, cores=64) == 1 + W.HEADROOM


def test_budget_is_capped_at_the_core_count():
    """ops/README.md's own plan (34 at peak) exceeds the 28-core box. Uncapped, the plan would
    always exceed the load and rule (e) could never fire."""
    agg = {f"worker:T-{i}": {} for i in range(20)}
    assert W.budget(agg, cores=28) == 28 + W.HEADROOM


# --------------------------------------------------------------------- rules
def fire(rows, since, now, claims=None, load=0.0):
    agg, unowned = W.owners(rows, claims or {})
    alarms, kills = W.evaluate(rows, agg, unowned, load, since, now, claims or {})
    return {a["rule"] for a in alarms}, kills, alarms


def test_rule_a_unowned_cpu_needs_to_be_sustained():
    rows = table(row(31000, 1, "/Users/d/scratch/spin", cpu=99.0))
    since = {}
    assert fire(rows, since, 1000.0)[0] == set()                     # first sighting: no alarm
    assert fire(rows, since, 1000.0 + W.UNOWNED_FOR - 1)[0] == set()
    assert "unowned-cpu" in fire(rows, since, 1000.0 + W.UNOWNED_FOR)[0]


def test_rule_a_clock_restarts_when_the_spike_passes():
    """A 99 % second inside a 2-minute window is not contention; a continuous 2 minutes is."""
    hot = table(row(31000, 1, "/Users/d/scratch/spin", cpu=99.0))
    cool = table(row(31000, 1, "/Users/d/scratch/spin", cpu=3.0))
    since = {}
    fire(hot, since, 0.0)
    fire(cool, since, 60.0)
    assert fire(hot, since, 119.0)[0] == set()
    assert fire(hot, since, 119.0 + W.UNOWNED_FOR)[0] == {"unowned-cpu"}


def test_rule_a_does_not_fire_on_an_owned_process():
    rows = table(row(800, 1, "cpulimit -- claude -p"), row(830, 800, "cargo test", cpu=290.0))
    assert fire(rows, {}, 10_000.0, CLAIMS)[0] == set()


def test_rule_b_kills_the_sixteen_orphaned_shells():
    """2026-09-22, as a table. Sixteen at 100 %, parent gone, two hours old."""
    rows = table(*[row(31000 + i, 1, SNAP, cpu=100.0, etime="02:18:00") for i in range(16)])
    since = {}
    fire(rows, since, 0.0)
    rules, kills, alarms = fire(rows, since, W.ZOMBIE_FOR)
    assert len(kills) == 16
    assert "zombie-shell" in rules
    red = next(a for a in alarms if a["rule"] == "zombie-shell")
    assert red["level"] == "red" and len(red["pids"]) == 16


def test_kill_requires_the_snapshot_signature():
    """Only the one measured signature. A busy `python -c 'while 1: pass'` gets an alert."""
    rows = table(row(31000, 1, "python3 -c while True: pass", cpu=100.0, etime="02:18:00"))
    since = {}
    fire(rows, since, 0.0)
    rules, kills, _ = fire(rows, since, W.ZOMBIE_FOR)
    assert kills == [] and rules == {"unowned-cpu"}


def test_kill_never_touches_a_live_agents_shell():
    """Same command line, same CPU, same age - but its session is alive, so it is owned."""
    rows = table(
        row(800, 1, "cpulimit -- claude -p"),
        row(820, 800, SNAP, cpu=100.0, etime="02:18:00"),
    )
    since = {}
    fire(rows, since, 0.0, CLAIMS)
    rules, kills, _ = fire(rows, since, W.ZOMBIE_FOR, CLAIMS)
    assert kills == [] and rules == set()


def test_kill_needs_ten_minutes_not_two():
    rows = table(row(31000, 1, SNAP, cpu=100.0))
    since = {}
    fire(rows, since, 0.0)
    assert fire(rows, since, W.UNOWNED_FOR)[1] == []          # ambers at 2 min, does not kill
    assert len(fire(rows, since, W.ZOMBIE_FOR)[1]) == 1


def test_kill_now_refuses_anything_off_signature(monkeypatch):
    """Belt and braces at the edge: even handed a row, kill_now checks the signature itself."""
    sent = []
    monkeypatch.setattr(W.os, "kill", lambda pid, sig: sent.append(pid))
    W.kill_now(table(row(1234, 1, "/bin/zsh -c echo hi", cpu=100.0)))
    assert sent == []
    W.kill_now(table(row(1234, 1, SNAP, cpu=100.0)))
    assert sent == [1234]


def test_rule_c_two_gates_is_red_immediately():
    """No sustain window: a second gate is wrong the instant it exists, and every minute it
    runs costs the other gate its reserved cores and corrupts both results."""
    rows = table(
        row(100, 1, "bash ops/merge-runner.sh"),
        row(110, 100, "just gate --base abc", cpu=5.0),
        row(210, 1, "just gate --base def", cpu=5.0),        # the killed runner's orphan
    )
    rules, _, alarms = fire(rows, {}, 0.0)
    assert "double-gate" in rules
    assert next(a for a in alarms if a["rule"] == "double-gate")["level"] == "red"


def test_rule_c_quiet_for_one_gate():
    rows = table(row(110, 1, "just gate --base abc", cpu=5.0),
                 row(120, 110, "uv run python -m hkpy.gate", cpu=900.0))
    assert fire(rows, {}, 0.0)[0] == set()


def test_rule_d_dashboard_ceiling_on_cpu_and_on_memory():
    """440 % CPU and 2.1 GB, 2026-09-22 - it stopped answering and nothing said why."""
    for cmd_cpu, rss in ((440.0, 10240), (5.0, 2_100_000)):
        rows = table(row(22685, 1, "python3 ops/monitor.py", cpu=cmd_cpu, rss_kb=rss))
        since = {}
        fire(rows, since, 0.0)
        assert "dashboard" in fire(rows, since, W.DASH_FOR)[0]


def test_rule_d_quiet_for_a_healthy_dashboard():
    rows = table(row(22685, 1, "python3 ops/monitor.py", cpu=38.0, rss_kb=420_000))
    since = {}
    fire(rows, since, 0.0)
    assert fire(rows, since, W.DASH_FOR)[0] == set()


def test_rule_e_over_budget_names_the_top_consumers():
    rows = table(
        row(22685, 1, "python3 ops/monitor.py", cpu=40.0),
        row(31000, 1, SNAP, cpu=100.0),
    )
    since = {}
    fire(rows, since, 0.0, load=40.0)
    rules, _, alarms = fire(rows, since, W.LOAD_FOR, load=40.0)
    assert "over-budget" in rules
    body = next(a for a in alarms if a["rule"] == "over-budget")["body"]
    assert "31000" in body and "UNOWNED" in body


def test_rule_e_quiet_when_the_load_is_within_the_plan():
    rows = table(row(110, 1, "just gate --base abc", cpu=900.0))
    since = {}
    fire(rows, since, 0.0, load=16.0)
    assert fire(rows, since, W.LOAD_FOR, load=16.0)[0] == set()


def test_alarm_keys_are_per_condition_so_the_dedupe_is_per_condition():
    """ops/alert.py drops a repeat of the same key for 30 min. Two distinct unowned processes
    must therefore not share a key, or the second one is never reported."""
    rows = table(row(31000, 1, "/x/spin", cpu=99.0), row(31001, 1, "/x/spin", cpu=99.0))
    since = {}
    fire(rows, since, 0.0)
    keys = {a["key"] for a in fire(rows, since, W.UNOWNED_FOR)[2]}
    assert keys == {"watchdog:unowned:31000", "watchdog:unowned:31001"}


def test_state_for_a_vanished_process_is_forgotten():
    """`since` is carried across every tick for the life of the process. Without the sweep it
    grows by one entry per short-lived compile, for ever."""
    since = {}
    fire(table(row(31000, 1, "/x/spin", cpu=99.0)), since, 0.0)
    assert any(k.startswith("unowned:") for k in since)
    fire(table(row(1, 0, "/sbin/launchd")), since, 20.0)
    assert not any(k.startswith("unowned:") for k in since)


# --------------------------------------------------------------------- snapshot
def test_tick_writes_the_snapshot_the_dashboard_reads(tmp_path, monkeypatch):
    monkeypatch.setattr(W, "S", str(tmp_path))
    monkeypatch.setattr(W, "STATE", str(tmp_path / "watchdog.json"))
    monkeypatch.setattr(W, "LOG", str(tmp_path / "watchdog.log"))
    monkeypatch.setattr(W, "CLAIMS", str(tmp_path / "work-claims.json"))
    snap = W.tick({}, dry=True)
    assert set(snap) >= {"ts", "load", "owners", "unowned", "alarms", "budget"}
    import json
    assert json.load(open(tmp_path / "watchdog.json"))["owners"] == snap["owners"]


def test_hackriff_role_env_wins_over_the_command_line():
    """ops/launch.sh exports it; where a kernel lets it be read, the session's own statement
    beats parsing its arguments."""
    rows = table(row(500, 1, "claude --model opus --append-system-prompt-file /x/roles/coordinator.md"))
    rows[0]["env"] = "HACKRIFF_ROLE=supervisor"
    agg, _ = W.owners(rows, {})
    assert set(agg) == {"role:supervisor"}
