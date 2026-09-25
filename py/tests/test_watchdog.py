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
    The one other kill, rule (h) (2026-09-25's worktree orphans), is locked the same way: a
    build/test process, unowned, in a worktree no live claim and no owned process protects, for
    ten minutes - `test_h_*` pins each protection separately.
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


@pytest.fixture(autouse=True)
def _isolate_ops_state(tmp_path, monkeypatch):
    """No test may write to the real `$HACKRIFF_OPS`.

    This is T-763's lesson applied before it can happen again: `gate-timings.jsonl` ended up
    with 31 of its 33 records written by the test suite itself, which made the one history that
    could answer "did the gate get slower" useless. `watchdog.log` is the record of every kill
    this thing has ever made — the first run of these tests appended a `KILL pid=1234` line to
    the live one, from a test whose `os.kill` was mocked and which killed nothing. A log that
    contains kills that did not happen is worse than no log.
    """
    monkeypatch.setattr(W, "S", str(tmp_path))
    monkeypatch.setattr(W, "STATE", str(tmp_path / "watchdog.json"))
    monkeypatch.setattr(W, "LOG", str(tmp_path / "watchdog.log"))
    monkeypatch.setattr(W, "CLAIMS", str(tmp_path / "work-claims.json"))
    monkeypatch.setenv("HK_ALERT_OFF", "1")        # and no test ever posts to Discord


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


def test_a_role_sessions_attached_limiter_is_named_not_unowned():
    """ops/launch.sh execs claude into the pane and attaches `cpulimit -p` from outside it (a
    wrapped child is not the pane's foreground group and stops on SIGTTIN)."""
    rows = table(
        row(500, 1, "claude --model opus --append-system-prompt-file /r/.claude/roles/pipeline-manager.md"),
        row(600, 1, "/Users/d/.hackriff-ops/bin/cpulimit -l 800 -i -p 500", cpu=1.5),
    )
    agg, unowned = W.owners(rows, {})
    assert set(agg) == {"role:pipeline-manager", "limiter"}
    assert unowned == []
    # an orphaned WRAPPER is not a limiter, whatever `-p <digits>` its wrapped command carries
    orphan = table(row(700, 1, "/x/bin/cpulimit -l 300 -i -- taskpolicy -c background claude -p 12", cpu=95.0))
    _, unowned = W.owners(orphan, {})
    assert [r["pid"] for r in unowned] == [700]


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


def test_a_shell_that_quotes_just_gate_is_not_a_second_gate():
    """2026-09-23: 567 double-gate alarms, every window opened by an agent's wait loop. The Bash
    tool's shell carries the command text in its argv; the gate's own processes START with it."""
    rows = table(
        row(100, 1, "bash ops/merge-runner.sh"),
        row(110, 100, "just gate --base abc", cpu=5.0),
        row(111, 110, "uv run --locked --project py python -m hkpy.gate --base abc"),
        row(112, 111, "/Users/d/hackriff/py/.venv/bin/python3 -m hkpy.gate --base abc"),
        row(300, 1, "claude -p --agent worker --model opus"),
        # verbatim shape of the 13:51:31 loop that held the alarm to 15:26:29
        row(310, 300, "/bin/zsh -c source /Users/d/.claude/shell-snapshots/snapshot-zsh-1.sh && "
                      "eval 'cd /x/t846fix && just wait-for-gate 2>&1 | tail -5; pgrep -fl '\\''just gate'\\'' ; date'"),
        row(320, 300, "/bin/zsh -c eval 'until ! pgrep -f \"just gate\"; do sleep 30; done; "
                      "uv run python -m hkpy.gate --dry-run'"),
    )
    assert [g["pid"] for g in W.gate_roots(rows)] == [110]
    rules, _, _ = fire(rows, {}, 0.0)
    assert "double-gate" not in rules
    agg, _ = W.owners(rows, {})
    assert 310 not in agg["gate"]["pids"] and 320 not in agg["gate"]["pids"]
    # and `just gate-merge` (the coordinator's merge-index gate) is still a gate
    assert [g["pid"] for g in W.gate_roots(table(row(400, 1, "just gate-merge")))] == [400]


def test_an_alarm_logs_which_processes_tripped_it(tmp_path, monkeypatch):
    """Discord dedupes by key, so the log line is the only lasting record of the pids."""
    rows = table(
        row(100, 1, "bash ops/merge-runner.sh"),
        row(110, 100, "just gate --base abc", cpu=5.0),
        row(210, 1, "just gate --base def", cpu=5.0),
    )
    monkeypatch.setattr(W, "read_ps", lambda: rows)
    W.tick({}, dry=True)
    line = next(ln for ln in open(tmp_path / "watchdog.log") if "double-gate" in ln)
    assert "pid 110" in line and "pid 210" in line and "\n" not in line.rstrip("\n")


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
def test_tick_writes_the_snapshot_the_dashboard_reads(tmp_path):
    snap = W.tick({}, dry=True)
    assert set(snap) >= {"ts", "load", "owners", "unowned", "alarms", "budget"}
    import json
    assert json.load(open(tmp_path / "watchdog.json"))["owners"] == snap["owners"]
    # `system` alone is 600+ processes here and the dashboard re-reads this file every 5 s, so
    # the snapshot carries the COUNT plus a sample - not every pid.
    for name, o in snap["owners"].items():
        assert len(o["pids"]) <= 12 and o["n"] >= len(o["pids"]), name


def test_hackriff_role_env_wins_over_the_command_line():
    """ops/launch.sh exports it; where a kernel lets it be read, the session's own statement
    beats parsing its arguments."""
    rows = table(row(500, 1, "claude --model opus --append-system-prompt-file /x/roles/coordinator.md"))
    rows[0]["env"] = "HACKRIFF_ROLE=supervisor"
    agg, _ = W.owners(rows, {})
    assert set(agg) == {"role:supervisor"}


# --------------------------------------------------------------------- contention (merge gate)
def test_contention_reports_unowned_cpu_and_over_budget():
    """What `ops/merge-runner.sh` waits for before starting a gate. Its own `workers_running`
    counts only processes this orchestration started, which is why the sixteen orphaned shells
    ran through every gate on 2026-09-22 with the drain check reporting zero."""
    now = 1_000_000.0
    snap = {"ts": now, "load": 44.0, "budget": 32.0,
            "unowned": [{"pid": 31000, "cpu": 99.5, "cmd": SNAP},
                        {"pid": 31001, "cpu": 12.0, "cmd": "quiet"}]}
    c = W.contention(snap, now)
    assert "pid 31000" in c and "100%" in c
    assert "31001" not in c
    assert "load 44.0 over budget 32.0" in c


def test_contention_is_empty_on_a_clear_box():
    assert W.contention({"ts": 1.0, "load": 8.0, "budget": 32.0, "unowned": []}, 1.0) == ""


def test_a_stale_or_missing_tick_says_nothing_rather_than_clear_or_contended():
    """A dead watchdog must not silently license a contended gate - and must not block every
    merge either. The runner's 45-minute cap is what makes erring either way survivable."""
    snap = {"ts": 0.0, "load": 44.0, "budget": 32.0,
            "unowned": [{"pid": 31000, "cpu": 99.5, "cmd": SNAP}]}
    assert W.contention(snap, W.CONTENTION_STALE_S + 1) == ""
    assert W.contention({}, 1.0) == ""
    assert W.contention(None, 1.0) == ""


# --------------------------------------------------------------------- (f) role-session liveness
# Incident 2026-09-24 04:07: a `pkill` took the coordinator (`dev`) and the pipeline manager
# (`flow`) down and nothing noticed for 5.5 h. tmux and ops/launch.sh are faked at
# `subprocess.run`, the one door both go through; the ps table is synthetic as above.
COORD = "claude --model opus --effort high --append-system-prompt-file /r/.claude/roles/coordinator.md"
PM = "claude --model opus --effort high --append-system-prompt-file /r/.claude/roles/pipeline-manager.md"


class FakeTmux:
    """`sessions` maps a session name to its pane pid, or to (pid, "1") for a `remain-on-exit`
    corpse; every argv run is recorded."""

    def __init__(self, sessions):
        self.sessions, self.calls = dict(sessions), []

    def __call__(self, argv, **kw):
        self.calls.append(list(argv))
        out, rc = "", 0
        if argv[0] == "tmux":
            name = argv[argv.index("-t") + 1].lstrip("=")
            if argv[1] == "kill-session":
                self.sessions.pop(name, None)
            elif name not in self.sessions:
                rc = 1
            elif argv[1] == "list-panes":
                pid, dead = (self.sessions[name] if isinstance(self.sessions[name], tuple)
                             else (self.sessions[name], "0"))
                out = f"{pid} {dead}\n"
            elif argv[1] == "capture-pane":
                out = "error: claude: command not found\n"
        else:                                         # ops/launch.sh <role>
            out = f"launched '{argv[1]}'\n"
        return W.subprocess.CompletedProcess(argv, rc, out, "")

    def launches(self):
        return [c[1] for c in self.calls if c[0].endswith("launch.sh")]

    def kills(self):
        return [c[-1] for c in self.calls if c[:2] == ["tmux", "kill-session"]]


def _live(monkeypatch, sessions, ps=None):
    """`ps` is what a fresh `read_ps()` returns - the re-read before any kill-session."""
    fake = FakeTmux(sessions)
    monkeypatch.setattr(W.subprocess, "run", fake)
    monkeypatch.setattr(W, "read_ps", lambda: ps if ps is not None else [])
    return fake


ALIVE = table(row(100, 1, COORD), row(200, 1, PM), row(300, 1, "/sbin/launchd"))


def test_liveness_alive_sessions_raise_nothing(monkeypatch):
    fake = _live(monkeypatch, {"dev": 100, "flow": 200})
    since: dict = {}
    for i in range(5):
        assert W.liveness(ALIVE, since, 1000.0 + 60 * i) == []
    assert fake.launches() == [] and fake.kills() == []


def test_liveness_claude_below_the_pane_pid_counts(monkeypatch):
    """launch.sh execs claude into the pane, but a claude one shell down is alive too."""
    fake = _live(monkeypatch, {"dev": 50, "flow": 60})
    rows = table(row(50, 1, "-zsh"), row(100, 50, COORD), row(60, 1, "-zsh"), row(61, 60, "sh -c x"),
                 row(200, 61, PM))
    assert W.liveness(rows, {}, 1000.0) == [] and fake.launches() == []


def test_liveness_missing_session_alerts_then_relaunches_on_the_second_miss(monkeypatch):
    fake = _live(monkeypatch, {"flow": 200})          # `dev` is gone
    since: dict = {}
    a1 = W.liveness(ALIVE, since, 1000.0)
    assert [(a["level"], a["key"]) for a in a1] == [("red", "watchdog:liveness:coordinator")]
    assert "'dev'" in a1[0]["title"] and "no tmux session" in a1[0]["title"]
    assert fake.launches() == []                      # one miss is not enough
    a2 = W.liveness(ALIVE, since, 1060.0)
    assert fake.launches() == ["coordinator"]
    assert fake.kills() == []                         # nothing to kill: the session was gone
    assert [a["key"] for a in a2] == ["watchdog:liveness:coordinator", "watchdog:relaunch:coordinator"]
    assert "exit 0" in a2[1]["body"] and "launched 'coordinator'" in a2[1]["body"]
    assert "liveness:miss:coordinator" not in since   # the count starts again after a relaunch


def test_liveness_a_miss_between_alive_checks_does_not_add_up(monkeypatch):
    fake = _live(monkeypatch, {"flow": 200})
    since: dict = {}
    W.liveness(ALIVE, since, 1000.0)                  # miss 1
    fake.sessions["dev"] = 100
    W.liveness(ALIVE, since, 1060.0)                  # alive: resets
    del fake.sessions["dev"]
    W.liveness(ALIVE, since, 1120.0)                  # miss 1 again, not 2
    assert fake.launches() == []


def test_liveness_session_without_claude_is_killed_then_relaunched(monkeypatch, tmp_path):
    """A `remain-on-exit` pane whose claude died: the session exists, so launch.sh would refuse."""
    rows = table(row(100, 1, "-zsh"), row(200, 1, PM))
    fake = _live(monkeypatch, {"dev": 100, "flow": 200}, ps=rows)
    since: dict = {}
    a1 = W.liveness(rows, since, 1000.0)
    assert "no claude" in a1[0]["title"] and fake.kills() == []
    a2 = W.liveness(rows, since, 1060.0)
    assert fake.kills() == ["=dev"] and fake.launches() == ["coordinator"]
    kill_at = next(i for i, c in enumerate(fake.calls) if c[:2] == ["tmux", "kill-session"])
    launch_at = next(i for i, c in enumerate(fake.calls) if c[0].endswith("launch.sh"))
    assert kill_at < launch_at
    assert "killed the claude-less session" in a2[-1]["title"]
    log = (tmp_path / "watchdog.log").read_text()
    assert "PANE dev before kill-session" in log and "command not found" in log
    capture_at = next(i for i, c in enumerate(fake.calls) if c[:2] == ["tmux", "capture-pane"])
    assert capture_at < kill_at


def test_liveness_relaunches_a_role_at_most_once_per_ten_minutes(monkeypatch):
    fake = _live(monkeypatch, {"flow": 200})
    since: dict = {}
    t = 1000.0
    for _ in range(2):                                # relaunch #1 at t=1060
        W.liveness(ALIVE, since, t)
        t += 60
    assert fake.launches() == ["coordinator"]
    fake.sessions.pop("dev", None)                    # it died again at once
    for _ in range(6):                                # two more misses, well inside ten minutes
        a = W.liveness(ALIVE, since, t)
        t += 60
        assert a and a[0]["key"] == "watchdog:liveness:coordinator"   # still alerting
    assert fake.launches() == ["coordinator"]
    assert "Not relaunching" in a[0]["body"]
    t = 1060.0 + W.RELAUNCH_GAP
    W.liveness(ALIVE, since, t)
    assert fake.launches() == ["coordinator", "coordinator"]


def test_liveness_dry_run_never_kills_or_launches(monkeypatch, tmp_path):
    rows = table(row(100, 1, "-zsh"))
    fake = _live(monkeypatch, {"dev": 100}, ps=rows)  # dev without claude, flow gone
    since: dict = {}
    W.liveness(rows, since, 1000.0, dry=True)
    alarms = W.liveness(rows, since, 1060.0, dry=True)
    assert fake.kills() == [] and fake.launches() == []
    assert {a["key"] for a in alarms} >= {"watchdog:relaunch:coordinator",
                                          "watchdog:relaunch:pipeline-manager"}
    assert "DRY-RUN would relaunch coordinator" in (tmp_path / "watchdog.log").read_text()


def test_liveness_checks_at_most_once_a_minute(monkeypatch):
    fake = _live(monkeypatch, {"flow": 200})
    since: dict = {}
    for t in (1000.0, 1020.0, 1040.0):                # three 20 s ticks = one check
        W.liveness(ALIVE, since, t)
    assert since["liveness:miss:coordinator"] == 1 and fake.launches() == []
    assert sum(c[:2] == ["tmux", "has-session"] for c in fake.calls) == 2   # dev + flow, once
    W.liveness(ALIVE, since, 1060.0)
    assert fake.launches() == ["coordinator"]


def test_liveness_an_empty_ps_table_is_unknown_not_dead(monkeypatch):
    fake = _live(monkeypatch, {"dev": 100, "flow": 200})
    since: dict = {}
    for t in (1000.0, 1060.0, 1120.0):
        assert W.liveness([], since, t) == []
    assert fake.calls == []


def test_liveness_a_remain_on_exit_corpse_is_dead(monkeypatch):
    """`#{pane_dead}` = 1: claude exited and tmux kept the pane; its old pid means nothing."""
    fake = _live(monkeypatch, {"dev": (100, "1"), "flow": 200}, ps=ALIVE)
    since: dict = {}
    a = W.liveness(ALIVE, since, 1000.0)             # pid 100 is even still `claude` in ps
    assert [x["key"] for x in a] == ["watchdog:liveness:coordinator"] and "pane dead" in a[0]["title"]
    W.liveness(ALIVE, since, 1060.0)
    assert fake.kills() == ["=dev"] and fake.launches() == ["coordinator"]


def test_liveness_a_pane_pid_missing_from_ps_is_unknown_not_dead(monkeypatch):
    """The tick's ps table predates the pane (a fresh launch): skip, never count a miss."""
    fake = _live(monkeypatch, {"dev": 999, "flow": 200})
    since: dict = {}
    for t in (1000.0, 1060.0, 1120.0):
        assert W.liveness(ALIVE, since, t) == []
    assert fake.kills() == [] and fake.launches() == []


def test_liveness_rereads_ps_before_a_kill_and_spares_a_live_claude(monkeypatch):
    stale = table(row(100, 1, "-zsh"), row(200, 1, PM))
    fresh = table(row(100, 1, "-zsh"), row(101, 100, COORD), row(200, 1, PM))
    fake = _live(monkeypatch, {"dev": 100, "flow": 200}, ps=fresh)
    since: dict = {}
    W.liveness(stale, since, 1000.0)
    a = W.liveness(stale, since, 1060.0)
    assert fake.kills() == [] and fake.launches() == [] and a == []
    assert not any(c[:2] == ["tmux", "capture-pane"] for c in fake.calls)


@pytest.mark.parametrize("marker", ["roles-stopped", "dispatch-paused"])
def test_liveness_a_deliberate_stop_alerts_but_never_relaunches(monkeypatch, tmp_path, marker):
    rows = table(row(100, 1, "-zsh"))                 # dev claude-less, flow gone
    fake = _live(monkeypatch, {"dev": 100}, ps=rows)
    (tmp_path / marker).write_text("")
    since: dict = {}
    for t in (1000.0, 1060.0, 1120.0, 1180.0):
        a = W.liveness(rows, since, t)
        assert {x["key"] for x in a} == {"watchdog:liveness:coordinator",
                                         "watchdog:liveness:pipeline-manager"}
    assert fake.kills() == [] and fake.launches() == []
    assert f"{marker} exists" in a[0]["body"]
    (tmp_path / marker).unlink()                      # /dev-env start removed it: back to normal
    W.liveness(rows, since, 1240.0)
    assert fake.launches() == ["coordinator", "pipeline-manager"]


def test_liveness_gives_up_after_three_relaunches_that_came_back_dead(monkeypatch):
    fake = _live(monkeypatch, {"flow": 200})          # dev never comes up
    since: dict = {}
    t = 1000.0
    while t < 1000.0 + 6 * W.RELAUNCH_GAP:
        a = W.liveness(ALIVE, since, t)
        t += 60
    assert fake.launches() == ["coordinator"] * 3
    assert a and a[0]["key"] == "watchdog:liveness:coordinator"      # still alerting
    assert "giving up relaunching coordinator after 3 attempts - see watchdog.log" in a[0]["body"]
    fake.sessions["dev"] = 100                        # a person brought it back: the count resets
    W.liveness(ALIVE, since, t)
    assert "liveness:failed:coordinator" not in since


def test_a_process_running_from_a_live_claims_worktree_is_that_workers():
    """2026-09-24 22:4x: T-577's own targeted test (a Bash tool shell reparented to launchd) alarmed as
    'unowned at 93% CPU - kill it'; T-882's and T-901's did the same earlier that day."""
    claims = {"T-577": {"state": "running", "pid": 800, "ticket": "T-577", "wt": "/r/.claude/worktrees/t577"},
              "T-607": {"state": "queued", "pid": 900, "ticket": "T-607", "wt": "/r/.claude/worktrees/t607"}}
    rows = table(
        row(800, 1, "cpulimit -l 300 -i -- taskpolicy -c background claude -p"),
        row(44791, 1, "/bin/zsh -c source /Users/d/.claude/shell-snapshots/snapshot-zsh-1.sh"),
        row(44793, 44791, "/opt/homebrew/bin/cargo-nextest nextest run -p hk-estimate -E binary(degenerate_null)"),
        row(44877, 44793, "/r/.claude/worktrees/t577/target/debug/deps/degenerate_null-3765 --exact x", cpu=93.0),
        row(44900, 1, "/r/.claude/worktrees/t607/target/debug/hk serve --bind 127.0.0.1:9930", cpu=95.0),
        row(44901, 1, "/r/.claude/worktrees/t5770/target/debug/hk serve", cpu=91.0),   # not t577's: a prefix is a dir
    )
    agg, unowned = W.owners(rows, claims)
    assert 44877 in agg["worker:T-577"]["pids"]
    assert {r["pid"] for r in unowned} >= {44900, 44901}      # a queued claim owns nothing; t5770 is not t577


def test_an_over_budget_episode_ends_in_one_recovered_line():
    """Supervisor 2026-09-25 01:52 (the user sleeps): one alarm per episode (its key's dedupe), and ONE all-clear when
    the load has held under plan as long as the alarm needed - its own key, outside the prefixes that page anyone."""
    rows = table(row(31000, 1, SNAP, cpu=100.0))
    since = {}
    fire(rows, since, 0.0, load=40.0)
    rules, _, _ = fire(rows, since, W.LOAD_FOR, load=45.0)
    assert "over-budget" in rules
    fire(rows, since, W.LOAD_FOR + 10, load=1.0)                       # under plan: the clock starts
    rules, _, alarms = fire(rows, since, 2 * W.LOAD_FOR + 10, load=1.0)
    rec = [a for a in alarms if a["rule"] == "recovered"]
    assert len(rec) == 1 and rec[0]["level"] == "green" and "peak 45.0" in rec[0]["body"]
    assert not rec[0]["key"].startswith(__import__("alert").WAKE_PREFIXES)
    rules, _, _ = fire(rows, since, 3 * W.LOAD_FOR + 10, load=1.0)
    assert "recovered" not in rules                                    # once per episode


def test_the_explorer_windows_server_and_agent_are_the_explorers():
    """2026-09-25 04:0x: the explorer's hk serve on the HackRF (ppid 1, started by the agent) alarmed as unowned."""
    rows = table(
        row(63953, 1, "/Users/d/.hackriff-ops/target-serve/release/hk serve --hackrf --bind 127.0.0.1:8897 "
                      "--data-dir /Users/d/.hackriff-ops/explorer/data --center-hz 98000000", cpu=387.0),
        row(62901, 62445, "claude --agent explorer --model opus --effort high", cpu=20.0),
        row(62445, 1, "bash /Users/d/hackriff/ops/explorer-window.sh --window 3h"),
    )
    agg, unowned = W.owners(rows, {})
    assert set(agg["explorer"]["pids"]) == {63953, 62901, 62445} and unowned == []


# --------------------------------------------------------------------- (h) worktree orphans
# 2026-09-25: a `zsh -c 'cargo build -p hk-cli --bin hk | grep | head'` in worktrees/t901 ran 15 h with
# ppid 1 and no claim; the nextest runs of the sessions killed at 09:34 kept going in t926/t940/t950-red/
# t953. None was over 90 % CPU. lsof (the cwd) and os.kill are faked; ps and the claims file are synthetic.
WT = f"{W.REPO}/.claude/worktrees/t0-wdtest"   # this repo's worktrees (WT_RE is anchored); never exists
NEXTEST = "/opt/homebrew/bin/cargo-nextest nextest run -p hk-pipeline -E binary(listen_waits_for_carrier)"
WRAPPER = "/bin/zsh -c cargo build -p hk-cli --bin hk 2>&1 | grep -E error | head -20"
ROLE = "claude --model opus --append-system-prompt-file /Users/d/hackriff/.claude/roles/coordinator.md"


class FakeLsof:
    """`cwds` maps pid -> cwd; every argv is recorded, so a test can count the lsof calls."""

    def __init__(self, cwds, fail=False):
        self.cwds, self.fail, self.calls = dict(cwds), fail, []

    def __call__(self, argv, **kw):
        self.calls.append(list(argv))
        if self.fail:
            raise OSError("lsof: not found")
        pids = [int(p) for p in argv[argv.index("-p") + 1].split(",")]
        out = "".join(f"p{p}\nfcwd\nn{self.cwds[p]}\n" for p in pids if p in self.cwds)
        return W.subprocess.CompletedProcess(argv, 0, out, "")


def _orphan_env(monkeypatch, tmp_path, rows, claims=None, cwds=None, fail=False):
    """Returns (lsof, signals sent). The claims file is what kill_orphans re-reads before a signal."""
    import json
    (tmp_path / "work-claims.json").write_text(json.dumps(claims or {}))
    lsof = FakeLsof(cwds or {}, fail)
    monkeypatch.setattr(W.subprocess, "run", lsof)
    monkeypatch.setattr(W, "read_ps", lambda: rows)
    sent = []
    monkeypatch.setattr(W.os, "kill", lambda pid, sig: sent.append((pid, sig)))
    return lsof, sent


def _ticks(rows, claims, since, times, dry=False):
    out = None
    for t in times:
        out = W.wt_orphans(rows, claims or {}, since, t, dry)
    return out


def test_h_stops_an_orphan_nextest_after_ten_minutes_not_before(monkeypatch, tmp_path):
    rows = table(row(35689, 1, NEXTEST, cpu=0.8))
    lsof, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT + "/crates"})
    since: dict = {}
    assert _ticks(rows, {}, since, [0.0, W.ORPHAN_FOR - 1]) == ([], []) and sent == []
    alarms, pids = W.wt_orphans(rows, {}, since, W.ORPHAN_FOR)
    assert sent == [(35689, W.signal.SIGTERM)] and pids == [35689]
    (a,) = alarms
    assert a["level"] == "red" and a["key"] == "watchdog:wt-orphan" and "t0-wdtest" in a["title"]
    assert "pid 35689" in a["body"] and WT in a["body"] and "no running/fix-held/limited claim on any host" in a["body"].lower()
    # ONE lsof per tick, over the candidate only; cwd found the worktree though the command names none
    assert len(lsof.calls) == 3 and all(c[-1] == "35689" for c in lsof.calls)
    log = (tmp_path / "watchdog.log").read_text()
    assert "KILL-ORPHAN SIGTERM pid=35689" in log and NEXTEST in log


@pytest.mark.parametrize("state", ["running", "fix-held", "limited"])
@pytest.mark.parametrize("host", [None, "node2"])
def test_h_never_with_a_live_claim_on_that_worktree_on_any_host(monkeypatch, tmp_path, state, host):
    claims = {"T-926": {"state": state, "ticket": "T-926", "wt": WT, "pid": 4242, "host": host}}
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, claims, cwds={35689: WT})
    assert _ticks(rows, claims, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []


def test_h_a_queued_claim_does_not_protect(monkeypatch, tmp_path):
    """A queued claim is a handed-back branch: nothing of its agent should still be running."""
    claims = {"T-926": {"state": "queued", "ticket": "T-926", "wt": WT}}
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, claims, cwds={35689: WT})
    _ticks(rows, claims, {}, [0.0, W.ORPHAN_FOR])
    assert sent == [(35689, W.signal.SIGTERM)]


@pytest.mark.parametrize("how", ["command line", "cwd"])
def test_h_never_while_an_owned_process_runs_in_that_worktree(monkeypatch, tmp_path, how):
    """A role session's subagent working there: its shell is owned by ancestry."""
    shell = "/bin/zsh -c source /Users/d/.claude/shell-snapshots/snapshot-zsh-1.sh && eval " + (
        f"'cd {WT} && git status'" if how == "command line" else "'git status'")
    rows = table(row(500, 1, ROLE), row(510, 500, shell), row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT, 510: WT, 500: "/Users/d/hackriff"})
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []


def test_h_never_a_rustc_under_sccache_but_a_bare_rustc_is_a_candidate(monkeypatch, tmp_path):
    rustc = f"/opt/homebrew/bin/rustc --crate-name hk_cli --out-dir {WT}/target/debug/deps"
    rows = table(row(93140, 1, "/opt/homebrew/bin/sccache"), row(62637, 93140, rustc, cpu=429.0))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows)
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []
    bare = table(row(62637, 1, rustc, cpu=100.0))
    _, sent = _orphan_env(monkeypatch, tmp_path, bare)
    _ticks(bare, {}, {}, [0.0, W.ORPHAN_FOR])
    assert sent == [(62637, W.signal.SIGTERM)]


@pytest.mark.parametrize("cwd,fail", [("/Users/d/hackriff", False), ("/Users/d/hackriff/target", False),
                                      (None, True)])
def test_h_never_outside_claude_worktrees_or_without_a_cwd(monkeypatch, tmp_path, cwd, fail):
    """The main checkout and the gate's target are not worktrees; a failed lsof is no cwd, not a guess."""
    rows = table(row(35689, 1, NEXTEST), row(35700, 1, "/Users/d/hackriff/target/debug/hk serve --bind 127.0.0.1:9"))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: cwd, 35700: cwd} if cwd else {}, fail=fail)
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []


def test_h_a_shell_wrapper_that_survives_sigterm_gets_sigkill_next_tick(monkeypatch, tmp_path):
    """The t901 wrapper ignored SIGTERM."""
    rows = table(row(7001, 1, WRAPPER), row(7002, 7001, "/opt/homebrew/bin/cargo build -p hk-cli --bin hk"),
                 row(7003, 7001, "grep -E error"))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={7001: WT, 7002: WT, 7003: WT})
    since: dict = {}
    _ticks(rows, {}, since, [0.0, W.ORPHAN_FOR])
    assert sorted(sent) == [(7001, W.signal.SIGTERM), (7002, W.signal.SIGTERM)]    # grep is not a build
    sent.clear()
    W.wt_orphans(rows, {}, since, W.ORPHAN_FOR + 20)
    assert sorted(sent) == [(7001, W.signal.SIGKILL), (7002, W.signal.SIGKILL)]
    assert "KILL-ORPHAN SIGKILL pid=7001" in (tmp_path / "watchdog.log").read_text()
    W.wt_orphans(table(row(1, 0, "/sbin/launchd")), {}, since, W.ORPHAN_FOR + 40)   # gone: clocks forgotten
    assert not any(k.startswith("orphan") for k in since)


def test_h_dry_run_kills_nothing(monkeypatch, tmp_path):
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT})
    since: dict = {}
    _ticks(rows, {}, since, [0.0], dry=True)
    alarms, pids = W.wt_orphans(rows, {}, since, W.ORPHAN_FOR, dry=True)
    assert sent == [] and pids == [] and "would stop 1" in alarms[0]["title"]
    W.wt_orphans(rows, {}, since, W.ORPHAN_FOR + 20, dry=True)
    assert sent == []


def test_h_rechecks_the_claims_file_before_each_signal(monkeypatch, tmp_path):
    """Belt and braces: a claim that appeared since this tick's read wins."""
    rows = table(row(35689, 1, NEXTEST))
    live = {"T-926": {"state": "running", "ticket": "T-926", "wt": WT}}
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, claims=live, cwds={35689: WT})
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR]) == ([], []) and sent == []   # stale {} in, fresh claim on disk
    assert "KILL-ORPHAN-REFUSED pid=35689" in (tmp_path / "watchdog.log").read_text()


def test_h_a_quiet_box_runs_no_lsof(monkeypatch, tmp_path):
    rows = table(row(500, 1, ROLE), row(510, 500, "/opt/homebrew/bin/cargo build"))
    lsof, _ = _orphan_env(monkeypatch, tmp_path, rows)
    W.wt_orphans(rows, {}, {}, 0.0)
    assert lsof.calls == []


def _admin(tmp_path, monkeypatch, mtime=None, which="index", gitfile=True):
    """A worktree under a tmp repo whose `.git` file points at an admin dir with a DIFFERENT name,
    so the test proves the gitdir: line is read rather than the name assumed."""
    import os
    repo = tmp_path / "repo"
    wt = repo / ".claude" / "worktrees" / "t9"
    admin = repo / ".git" / "worktrees" / "t9-renamed"
    (admin / "logs").mkdir(parents=True)
    wt.mkdir(parents=True)
    if gitfile:
        (wt / ".git").write_text(f"gitdir: {admin}\n")
    for f in ("index", "HEAD", "logs/HEAD"):
        (admin / f).write_text("x")
        os.utime(admin / f, (1.0, 1.0))
    if mtime is not None:
        os.utime(admin / which, (mtime, mtime))
    monkeypatch.setattr(W, "WT_RE", W._wt_re(str(repo)))
    return str(wt)


NOW = 1_000_000.0


@pytest.mark.parametrize("dry", [False, True])       # dry: the tick's own check, with no kill-time re-check behind it
@pytest.mark.parametrize("which", ["index", "HEAD", "logs/HEAD"])
def test_h_git_activity_in_the_worktree_protects_it(monkeypatch, tmp_path, which, dry):
    """A live claim-less agent's detached `cmd &` looks like t901 from ps; its git status/diff does not."""
    wt = _admin(tmp_path, monkeypatch, mtime=NOW + W.ORPHAN_FOR - 60, which=which)
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: wt})
    assert _ticks(rows, {}, {}, [NOW, NOW + W.ORPHAN_FOR], dry=dry) == ([], []) and sent == []


@pytest.mark.parametrize("mtime,gitfile", [(NOW - W.ORPHAN_FOR - 1, True), (None, True), (NOW + 10, False)])
def test_h_stale_missing_or_unreadable_git_activity_is_no_evidence(monkeypatch, tmp_path, mtime, gitfile):
    wt = _admin(tmp_path, monkeypatch, mtime=mtime, gitfile=gitfile)
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: wt})
    _ticks(rows, {}, {}, [NOW, NOW + W.ORPHAN_FOR])
    assert sent == [(35689, W.signal.SIGTERM)]


def test_h_another_repos_worktrees_are_never_candidates(monkeypatch, tmp_path):
    other = "/Users/daniellewis/other/.claude/worktrees/t1"
    nested = f"{W.REPO}/hackriff-2/.claude/worktrees/t1"
    assert W.worktrees_in(f"cd {other} && cd {nested}") == []
    assert W.worktrees_in(f"cd {WT}/crates && x") == [WT]
    rows = table(row(35689, 1, NEXTEST), row(35690, 1, f"{nested}/target/debug/hk serve --bind 127.0.0.1:9"),
                 row(35691, 1, f"/bin/zsh -c cd {other} && cargo test"))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: other, 35690: nested, 35691: other})
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []


@pytest.mark.parametrize("cmd_names_the_claimed_one", [True, False])
def test_h_protected_if_either_the_command_lines_or_the_cwds_worktree_is(monkeypatch, tmp_path,
                                                                          cmd_names_the_claimed_one):
    other = f"{W.REPO}/.claude/worktrees/t0-other"
    claimed, free = (WT, other) if cmd_names_the_claimed_one else (other, WT)
    claims = {"T-1": {"state": "running", "ticket": "T-1", "wt": claimed}}
    rows = table(row(35689, 1, f"/bin/zsh -c cd {free if not cmd_names_the_claimed_one else claimed} && cargo test"))
    cwd = free if cmd_names_the_claimed_one else claimed
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, claims, cwds={35689: cwd})
    assert _ticks(rows, claims, {}, [0.0, W.ORPHAN_FOR]) == ([], []) and sent == []


def test_h_recheck_refuses_a_reused_pid(monkeypatch, tmp_path):
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT})
    monkeypatch.setattr(W, "read_ps", lambda: table(row(35689, 1, "/opt/homebrew/bin/cargo build -p other")))
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR]) == ([], []) and sent == []
    assert "KILL-ORPHAN-REFUSED pid=35689" in (tmp_path / "watchdog.log").read_text()


def test_h_recheck_refuses_a_row_that_is_now_owned(monkeypatch, tmp_path):
    rows = table(row(35689, 1, NEXTEST))
    _, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT})
    monkeypatch.setattr(W, "read_ps", lambda: table(row(500, 1, ROLE), row(35689, 500, NEXTEST)))
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR]) == ([], []) and sent == []


@pytest.mark.parametrize("cmd", ["/bin/zsh -c sleep 1000", f"/bin/zsh -c cd {WT} && sleep 1000"])
def test_h_a_shell_that_wraps_no_build_is_not_a_candidate(monkeypatch, tmp_path, cmd):
    rows = table(row(35689, 1, cmd))
    lsof, sent = _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT})
    assert _ticks(rows, {}, {}, [0.0, W.ORPHAN_FOR, 2 * W.ORPHAN_FOR]) == ([], []) and sent == []
    assert lsof.calls == []


def test_h_nothing_due_runs_no_second_ps(monkeypatch, tmp_path):
    rows = table(row(35689, 1, NEXTEST))
    _orphan_env(monkeypatch, tmp_path, rows, cwds={35689: WT})
    reads = []
    monkeypatch.setattr(W, "read_ps", lambda: reads.append(1) or rows)
    W.wt_orphans(rows, {}, {}, 0.0)
    assert reads == []
