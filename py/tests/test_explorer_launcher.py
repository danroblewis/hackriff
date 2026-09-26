"""The explorer window (T-923): `ops/launch.sh explorer` validates its arguments and refuses a second
instance; `ops/explorer-window.sh` takes the radio lock before the agent starts and releases it on
every way out (normal exit, agent crash, window end, a tmux kill). Driven against a temp
HACKRIFF_OPS with a stub `just radio` and a stub claude - no tmux window, no HackRF."""

import os
import pathlib
import platform
import shutil
import signal
import socket
import subprocess
import tempfile
import time

import pytest

OPS = pathlib.Path(__file__).resolve().parents[2] / "ops"
WINDOW = OPS / "explorer-window.sh"
LAUNCH = OPS / "launch.sh"

pytestmark = pytest.mark.skipif(platform.system() != "Darwin", reason="the explorer is Mac Studio only")

# A hang detector, not a latency budget: nothing here asserts how fast a window runs (that would be the
# `timing` tier). On a loaded box one fork+exec was measured taking ~15 s (2026-09-25, load 25-37), so a
# budget of a few seconds failed healthy windows. Every stub agent that should be stopped sleeps far
# longer than this, so a window that is really stuck still fails here instead of passing late.
HANG_S = 120
LONG_AGENT = "exec sleep 600"  # exec: the agent becomes one plain process, not a bash that forks


@pytest.fixture(autouse=True)
def _own_port(monkeypatch):
    """Every test's windows get a port no other process on the box can hold, reserved by a listening
    socket for the test's whole life (the stub hk never binds it). A window's exit pattern-kills `hk
    serve` on its port, so the fixed port 65531, shared by concurrent runs of this file - the gate's, a
    worker's and a deflaker's on one box, 2026-09-25 - let one run's exit stop another run's kept
    server and, before T-1029 anchored that pattern, its agent."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        s.listen(1)
        monkeypatch.setenv("EXPLORER_PORT", str(s.getsockname()[1]))
        yield


def _stubs(tmp_path, claude_body="exit 0", take_rc=0, staging="replay (radio-lock: explorer until 05:00)",
           ring_kb=8):
    """A stub `just radio`, `claude` and `hk` (T-983: the window now starts its own `hk serve` and
    reaps its ring, so the stub `hk serve` behaves like a real one just enough to exercise that -
    it logs its args, drops `ring_kb` KiB under `<data-dir>/iqbuffer`, and runs until TERMed."""
    ops = tmp_path / "ops"
    ops.mkdir()
    radio_log = tmp_path / "radio.log"
    radio = tmp_path / "radio"
    radio.write_text(
        "#!/bin/sh\n"
        f'echo "$*" >> "{radio_log}"\n'
        f'[ "$1" = take ] && exit {take_rc}\n'
        f'[ "$1" = status ] && {{ echo "radio: explorer since 02:00 until 05:00"; echo "staging: {staging}"; }}\n'
        "exit 0\n"
    )
    claude = tmp_path / "claude"
    claude.write_text(
        "#!/bin/bash\n"
        f'echo "$EXPLORER_DEADLINE $*" > "{tmp_path}/claude.args"\n'
        f'echo $$ > "{tmp_path}/agent.pid"\n'
        f'env | grep ^EXPLORER_SERVER_ > "{tmp_path}/claude.server-env" || true\n'
        f"{claude_body}\n"
    )
    hk_log = tmp_path / "hk.log"
    hk = tmp_path / "hk"
    hk.write_text(
        "#!/bin/bash\n"
        'if [ "$1" = serve ]; then\n'
        "  shift\n"
        f'  echo "$*" >> "{hk_log}"\n'
        '  datadir=""; prev=""\n'
        '  for a in "$@"; do [ "$prev" = --data-dir ] && datadir="$a"; prev="$a"; done\n'
        '  if [ -n "$datadir" ]; then\n'
        '    mkdir -p "$datadir/iqbuffer"\n'
        f'    dd if=/dev/zero "of=$datadir/iqbuffer/ring.bin" bs=1024 count={ring_kb} status=none\n'
        "  fi\n"
        '  trap "exit 0" TERM\n'
        "  while :; do sleep 1; done\n"
        "fi\n"
    )
    for f in (radio, claude, hk):
        f.chmod(0o755)
    env = dict(
        os.environ,
        HACKRIFF_OPS=str(ops),
        EXPLORER_RADIO=str(radio),
        EXPLORER_CLAUDE=str(claude),
        EXPLORER_HK=str(hk),
        EXPLORER_REPO=str(tmp_path),
        EXPLORER_KILL_GRACE="2",
        EXPLORER_WRAPUP_S="1",
        EXPLORER_POLL="1",
        EXPLORER_STAGING_WAIT="3",
        EXPLORER_WATCH_POLL="1",
        EXPLORER_SERVER_SETTLE="0",
        EXPLORER_TOKEN="test-token",
    )
    return env, ops, radio_log


def _clean_signal_mask():
    signal.pthread_sigmask(signal.SIG_SETMASK, [])
    # An IGNORED signal survives exec too, and bash cannot trap a signal ignored on entry: a pytest
    # started as a background job of a non-interactive shell (`cmd &`, as a runner or a soak loop
    # does) has SIGINT ignored, and the SIGINT pane-kill then hung for HANG_S (T-1029, 4 of 4 runs).
    for sig in (signal.SIGINT, signal.SIGQUIT, signal.SIGHUP, signal.SIGTERM):
        signal.signal(sig, signal.SIG_DFL)


def _start(args, env):
    """The window as tmux starts a pane: its own session (a leftover can be found and killed as its
    process group), an empty signal mask and default signal dispositions. A mask is inherited across fork and exec, and this test
    process is sometimes launched with SIGINT blocked (seen from the agent shells' zsh loops,
    2026-09-25: pytest itself started with mask {SIGINT}); the window, its trap and its agent then
    never see the SIGINT the pane-kill test sends, and that case hung for the whole run."""
    return subprocess.Popen(args, env=env, start_new_session=True, text=True, preexec_fn=_clean_signal_mask,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)


def _finish(p):
    """Wait for the window AND every process holding its stdout/stderr - a timer or agent it leaked
    keeps the pipes open, so a leak fails here. On a hang the group is killed, never left running
    (its id cannot have been reused: the leader or a leftover member still holds it)."""
    try:
        return p.communicate(timeout=HANG_S)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.communicate()
        raise


def _run(args, env):
    p = _start(args, env)
    out, err = _finish(p)
    return subprocess.CompletedProcess(args, p.returncode, out, err)


def _agent_running(tmp_path, p):
    """Wait until the stub agent has exec'd its sleep. Only then is it one plain process with default
    signal dispositions; before that it is a bash (or a forked copy of the window's shell) on its way
    to exec, and a signal landing there can be swallowed - under load that window is seconds wide."""
    pidf = tmp_path / "agent.pid"
    deadline = time.monotonic() + HANG_S
    while time.monotonic() < deadline:
        if p.poll() is not None:
            pytest.fail(f"the window exited ({p.returncode}) before its agent was running")
        pid = pidf.read_text().strip() if pidf.exists() else ""
        if pid:
            comm = subprocess.run(["ps", "-o", "comm=", "-p", pid], capture_output=True, text=True).stdout
            if comm.strip().endswith("sleep"):
                return
        time.sleep(0.05)
    os.killpg(p.pid, signal.SIGKILL)
    p.communicate()
    pytest.fail(f"the agent was not running after {HANG_S}s")


def _radio_calls(log):
    """take/release in order; the staging `status` polls are left out (checked where they matter)."""
    calls = [line.split()[0] for line in log.read_text().splitlines()] if log.exists() else []
    return [c for c in calls if c != "status"]


@pytest.mark.parametrize("window", ["3x", "9h", "0", "h", "-1m", "", "481"])
def test_a_bad_window_is_refused_before_anything_runs(tmp_path, window):
    env, _, radio_log = _stubs(tmp_path)
    r = subprocess.run([str(WINDOW), "--window", window], env=env, capture_output=True, text=True)
    assert r.returncode == 2, r.stderr
    assert _radio_calls(radio_log) == []


def test_an_unknown_argument_is_refused(tmp_path):
    env, _, _ = _stubs(tmp_path)
    r = subprocess.run([str(WINDOW), "--windw", "3h"], env=env, capture_output=True, text=True)
    assert r.returncode == 2 and "unknown argument" in r.stderr


def test_dry_run_validates_and_touches_nothing(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    r = subprocess.run([str(WINDOW), "--window", "2h30m", "--dry-run"], env=env, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    assert "9000s" in r.stdout and "take explorer 2h30m" in r.stdout and "release explorer" in r.stdout
    assert not radio_log.exists() and not (ops / "explorer" / "window.pid").exists()
    assert not (tmp_path / "claude.args").exists()


def test_the_session_id_from_launch_sh_reaches_the_agent(tmp_path):
    """ops/launch.sh picks the id and records it in role-session/explorer; the agent must run as it."""
    env, _, _ = _stubs(tmp_path)
    sid = "0f0e0d0c-0b0a-4908-8706-050403020100"
    r = _run([str(WINDOW), "--window", "1m", "--session-id", sid], env)
    assert r.returncode == 0, r.stderr
    assert f"--session-id {sid}" in (tmp_path / "claude.args").read_text()


def test_a_bare_number_is_minutes_like_just_radio_take(tmp_path):
    env, _, _ = _stubs(tmp_path)
    r = subprocess.run([str(WINDOW), "--window", "90", "--dry-run"], env=env, capture_output=True, text=True)
    assert r.returncode == 0 and "5400s" in r.stdout


def test_a_second_window_is_refused_while_one_runs(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    (ops / "explorer").mkdir()
    (ops / "explorer" / "window.pid").write_text(str(os.getpid()))  # a live pid: this test
    r = subprocess.run([str(WINDOW), "--window", "1m"], env=env, capture_output=True, text=True)
    assert r.returncode == 3 and "one instance" in r.stderr
    assert _radio_calls(radio_log) == []


def test_a_stale_pidfile_does_not_block(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    (ops / "explorer").mkdir()
    dead = subprocess.Popen(["true"])
    dead.wait()
    (ops / "explorer" / "window.pid").write_text(str(dead.pid))
    r = _run([str(WINDOW), "--window", "1m"], env)
    assert r.returncode == 0, r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]


def test_normal_exit_takes_then_releases_and_passes_the_deadline(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    t0 = int(time.time())
    r = _run([str(WINDOW), "--window", "1h"], env)
    assert r.returncode == 0, r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]
    take = radio_log.read_text().splitlines()[0]
    assert take.startswith("take explorer 1h ")
    deadline, *args = (tmp_path / "claude.args").read_text().split()
    assert abs(int(deadline) - (t0 + 3600)) <= 5
    assert args[:2] == ["--agent", "explorer"]
    assert not (ops / "explorer" / "window.pid").exists()


def test_normal_exit_starts_one_server_exports_it_and_reaps_its_ring(tmp_path):
    """T-983: the launcher, not the agent, starts the window's one hk serve; the agent gets its
    URL/token/data-dir over the environment; on the way out the server is stopped and its IQ ring
    (dropped by the stub hk) is reaped, with the bytes reclaimed logged."""
    env, ops, radio_log = _stubs(tmp_path, ring_kb=16)
    port = env["EXPLORER_PORT"]
    r = _run([str(WINDOW), "--window", "1h"], env)
    assert r.returncode == 0, r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]

    d = ops / "explorer"
    assert (d / "server.url").read_text().strip() == f"http://127.0.0.1:{port}"
    assert (d / "server.token").read_text().strip() == "test-token"
    datadir = pathlib.Path((d / "server.datadir").read_text().strip())
    assert datadir.name == "data"

    server_env = (tmp_path / "claude.server-env").read_text()
    assert f"EXPLORER_SERVER_URL=http://127.0.0.1:{port}" in server_env
    assert "EXPLORER_SERVER_TOKEN=test-token" in server_env
    assert f"EXPLORER_SERVER_DATADIR={datadir}" in server_env

    hk_args = (tmp_path / "hk.log").read_text()
    assert "--data-dir" in hk_args and f"--bind 127.0.0.1:{port}" in hk_args

    log = (d / "window.log").read_text()
    assert "started the window's one hk serve" in log
    assert "reaped IQ ring(s): 16384 bytes freed" in log
    assert not (datadir / "iqbuffer").exists()  # reaped, not just stopped


def test_dry_run_reports_what_it_would_reap_without_deleting_it(tmp_path):
    """Item 3: --dry-run prints the existing plan (a leftover ring from a prior, uncleanly-ended
    window) and touches nothing - no lock taken, nothing deleted."""
    env, ops, radio_log = _stubs(tmp_path)
    leftover = ops / "explorer" / "data-old" / "iqbuffer"
    leftover.mkdir(parents=True)
    (leftover / "ring.bin").write_bytes(b"\0" * 4096)
    r = subprocess.run([str(WINDOW), "--window", "2h30m", "--dry-run"], env=env, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    assert "would reap now" in r.stdout
    assert "iqbuffer" in r.stdout and "4096" in r.stdout
    assert not radio_log.exists()
    assert leftover.is_dir() and (leftover / "ring.bin").exists()  # dry-run: untouched


def test_an_agent_crash_still_releases_and_keeps_its_status(tmp_path):
    env, _, radio_log = _stubs(tmp_path, claude_body="exit 7")
    r = _run([str(WINDOW), "--window", "1h"], env)
    assert r.returncode == 7
    assert _radio_calls(radio_log) == ["take", "release"]


def test_a_refused_lock_never_starts_the_agent_and_releases_nothing(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, take_rc=1)
    r = _run([str(WINDOW), "--window", "1h"], env)
    assert r.returncode == 4
    assert _radio_calls(radio_log) == ["take"]
    assert not (tmp_path / "claude.args").exists()
    assert not (ops / "explorer" / "window.pid").exists()


def test_the_agent_waits_for_staging_to_let_go_and_a_stuck_staging_releases(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, staging="live")
    r = _run([str(WINDOW), "--window", "1h"], env)
    assert r.returncode == 5
    calls = [line.split()[0] for line in radio_log.read_text().splitlines()]
    assert calls[0] == "take" and calls[-1] == "release" and calls.count("status") >= 3
    assert not (tmp_path / "claude.args").exists()
    assert "did not switch to replay" in (ops / "explorer" / "window.log").read_text()


def test_window_end_stops_the_agent_and_releases(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, claude_body=LONG_AGENT)  # would run 600 s if not stopped
    r = _run([str(WINDOW), "--window", "3s"], env)
    # Stopped by the window end - TERM, or KILL after the grace if the TERM met it mid-exec - and
    # never by finishing on its own (that would be 0, and 600 s > HANG_S).
    assert r.returncode in (128 + signal.SIGTERM, 128 + signal.SIGKILL), r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]
    log = (ops / "explorer" / "window.log").read_text()
    wrap, end, released = (log.index(m) for m in ("wrap-up", "window end", "radio lock released"))
    assert wrap < end < released, log
    assert not (ops / "explorer" / "wrap-up").exists()


def _wait_for(p, what, done):
    """A hang detector around a state the window reaches on its own: fails at once if the window
    exits first, and only after HANG_S if it never gets there (then its group is killed, not left)."""
    deadline = time.monotonic() + HANG_S
    while time.monotonic() < deadline:
        if done():
            return
        if p.poll() is not None:
            pytest.fail(f"the window exited ({p.returncode}) before: {what}")
        time.sleep(0.05)
    os.killpg(p.pid, signal.SIGKILL)
    p.communicate()
    pytest.fail(f"not after {HANG_S}s: {what}")


def _log_has(ops, line):
    log = ops / "explorer" / "window.log"
    return lambda: log.exists() and line in log.read_text()


def _stop(p):
    """End a still-running window as tmux kill-session does (TERM to its whole process group) and wait
    for it and for everything holding its pipes."""
    if p.returncode is None:
        os.killpg(p.pid, signal.SIGTERM)
        _finish(p)


def _args(pid):
    return subprocess.run(["ps", "-o", "args=", "-p", str(pid)], capture_output=True, text=True).stdout.strip()


def _exited_uncollected(proc):
    """The process has exited but is still this test's zombie. It is collected only after the window
    is gone: the fake process table keeps listing its pid, and the watcher re-signals every pid it
    lists each poll, so a collected (freed, reusable) pid must never be left in its sights."""
    return subprocess.run(["ps", "-o", "stat=", "-p", str(proc.pid)], capture_output=True, text=True).stdout.startswith("Z")


def _watched_window(tmp_path, ring_kb=8, extra_env=None):
    """A window whose rogue-server watcher reads a fake process table (EXPLORER_PS_OUTPUT), so the
    test decides exactly which `hk serve` lines it sees. The window is 1 h and the agent sleeps 600 s:
    nothing ends it but `_stop`. The T-983 version ran 20 s windows against its own 10 s waits, and a
    window that ended (or was ended - see test_one_windows_exit_never_stops_another_process_naming_its_server)
    between the ALERT and the reap stopped the watcher before its reap line (T-1029, load 40)."""
    env, ops, radio_log = _stubs(tmp_path, claude_body=LONG_AGENT, ring_kb=ring_kb)
    ps_file = tmp_path / "ps.txt"
    ps_file.write_text("")
    env["EXPLORER_PS_OUTPUT"] = str(ps_file)
    if extra_env:
        env.update(extra_env)
    p = _start([str(WINDOW), "--window", "1h"], env)
    d = ops / "explorer"
    # server.datadir is the last of the server.* records the window writes
    _wait_for(p, "the window's own hk serve started", lambda: (d / "server.datadir").exists())
    kept_pid = int((d / "server.pid").read_text())
    kept_datadir = pathlib.Path((d / "server.datadir").read_text().strip())
    return p, env, ops, radio_log, ps_file, kept_pid, kept_datadir


def test_a_second_server_under_the_window_is_stopped_and_reaped_untouched_first(tmp_path):
    """The 2026-09-25 06:43 amendment: even after being told at launch, the agent started a second
    hk serve per target. The window's watcher finds it, stops it, reaps its ring, and never touches
    the window's own (kept) server."""
    p, env, ops, radio_log, ps_file, kept_pid, kept_datadir = _watched_window(tmp_path)
    port = env["EXPLORER_PORT"]
    rogue = None
    try:
        rogue = subprocess.Popen(["sleep", "600"])
        rogue_datadir = ops / "explorer" / "data-rogue"
        (rogue_datadir / "iqbuffer").mkdir(parents=True)
        (rogue_datadir / "iqbuffer" / "ring.bin").write_bytes(b"\0" * 2048)
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:{port}\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {rogue_datadir} --bind 127.0.0.1:{port}\n"
        )
        # The reap line is the last step of the watcher's stop-then-reap sequence for one rogue.
        _wait_for(p, "the watcher reaped the rogue's ring", _log_has(ops, "rogue server's ring reaped: 2048 bytes"))
        log = (ops / "explorer" / "window.log").read_text()
        assert f"ALERT: rogue hk serve detected (pid {rogue.pid}, data-dir {rogue_datadir})" in log
        # The watcher TERMs (then KILLs) the rogue before it writes the reap line.
        assert _exited_uncollected(rogue), "the rogue was never stopped"
        assert not (rogue_datadir / "iqbuffer").exists(), "the rogue's ring was never reaped"
        assert (kept_datadir / "iqbuffer" / "ring.bin").exists(), "the kept server's ring was reaped with the rogue's"
        os.kill(kept_pid, 0)  # the window's own server is untouched (raises if it is gone)
    finally:
        _stop(p)
        if rogue is not None:
            rogue.kill()  # a no-op on the zombie it should be
            rogue.wait()
    assert rogue.returncode in (-signal.SIGTERM, -signal.SIGKILL)
    assert p.returncode == 128 + signal.SIGTERM
    assert _radio_calls(radio_log) == ["take", "release"]


def test_a_second_listener_sharing_the_kept_data_dir_never_reaps_the_kept_ring(tmp_path):
    """Review round 2: a rogue hk serve that reports the SAME --data-dir as the kept server (a
    second listener on it - exactly the shape explorer.md hands the agent, since the agent gets
    that literal EXPLORER_SERVER_DATADIR path) must be stopped, but its ring must never be
    reaped, because that directory IS the kept server's own still-live ring."""
    p, env, ops, radio_log, ps_file, kept_pid, kept_datadir = _watched_window(tmp_path, ring_kb=32)
    port = int(env["EXPLORER_PORT"])
    kept_ring = kept_datadir / "iqbuffer" / "ring.bin"
    rogue = None
    try:
        # the stub hk drops its ring as it starts; wait for all of it to land
        _wait_for(p, "the kept server's ring landed", lambda: kept_ring.exists() and kept_ring.stat().st_size == 32 * 1024)
        rogue = subprocess.Popen(["sleep", "600"])  # a second listener, the SAME --data-dir as kept
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:{port}\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:{port + 1}\n"
        )
        _wait_for(p, "the watcher finished with the rogue listener", _log_has(ops, "rogue server's ring reaped"))
        log = (ops / "explorer" / "window.log").read_text()
        assert f"ALERT: rogue hk serve detected (pid {rogue.pid}, data-dir {kept_datadir})" in log
        assert "rogue server's ring reaped: 0 bytes" in log
        assert _exited_uncollected(rogue), "the rogue listener was never stopped"
        # The point of this test: the kept server's OWN ring must survive, byte for byte.
        assert kept_ring.exists() and kept_ring.stat().st_size == 32 * 1024
        os.kill(kept_pid, 0)  # the kept server itself is untouched too (raises if it is gone)
    finally:
        _stop(p)
        if rogue is not None:
            rogue.kill()  # a no-op on the zombie it should be
            rogue.wait()
    assert rogue.returncode in (-signal.SIGTERM, -signal.SIGKILL)
    assert p.returncode == 128 + signal.SIGTERM
    assert _radio_calls(radio_log) == ["take", "release"]


def _identity_stub_ps(tmp_path):
    """A stub `ps` for EXPLORER_PS_BIN, the seam `ps_stamp()` uses to verify a pid's identity right
    before the final -KILL. It alternates its answer for a given pid on every call (1st call "A", 2nd
    "B", 3rd "A", ...), so the baseline captured before a kill sequence and the check right before its
    final -KILL - always a consecutive pair for the same pid - never match. That is exactly what a pid
    reused in that window looks like to the script, without needing the OS to actually reuse a pid
    inside a test's wall-clock budget (T-1033)."""
    counts = tmp_path / "ps-counts"
    counts.mkdir()
    ps = tmp_path / "ps-stub"
    ps.write_text(
        "#!/bin/bash\n"
        'pid="${@: -1}"\n'
        f'f="{counts}/$pid"\n'
        'n=0; [ -f "$f" ] && n=$(cat "$f")\n'
        'n=$((n+1)); echo "$n" > "$f"\n'
        'if [ $((n % 2)) = 1 ]; then echo "stamp-A-$pid"; else echo "stamp-B-$pid"; fi\n'
    )
    ps.chmod(0o755)
    return ps


def test_a_pid_whose_identity_changed_before_the_final_kill_is_not_killed(tmp_path):
    """Red on the old script: once the grace period elapsed it sent `kill -KILL $pid` unconditionally,
    trusting that a pid still (or again) answering `kill -0` was still the process it TERMed - a
    narrow but real window for an unrelated process, given a reused pid, to take that -KILL instead.
    The stub `ps` above makes the identity check see a DIFFERENT process at the final-kill instant
    than it did when the kill sequence started. The fix must withhold the -KILL; the rogue - which
    ignores TERM, so only a delivered -KILL would end it - must still be alive once the window is
    done handling it."""
    ps_stub = _identity_stub_ps(tmp_path)
    p, env, ops, radio_log, ps_file, kept_pid, kept_datadir = _watched_window(
        tmp_path, extra_env={"EXPLORER_PS_BIN": str(ps_stub)})
    port = env["EXPLORER_PORT"]
    rogue = subprocess.Popen(["bash", "-c", "trap '' TERM; exec sleep 600"])
    try:
        rogue_datadir = ops / "explorer" / "data-rogue"
        (rogue_datadir / "iqbuffer").mkdir(parents=True)
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:{port}\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {rogue_datadir} --bind 127.0.0.1:{port}\n"
        )
        _wait_for(p, "the watcher finished with the rogue (ring reaped)",
                  _log_has(ops, "rogue server's ring reaped"))
        log = (ops / "explorer" / "window.log").read_text()
        assert f"ALERT: rogue hk serve detected (pid {rogue.pid}, data-dir {rogue_datadir})" in log
        os.kill(rogue.pid, 0)  # still alive: the -KILL was correctly withheld, not delivered
    finally:
        _stop(p)
        rogue.kill()
        rogue.wait()


def test_a_rogue_detected_just_before_the_window_ends_is_still_stopped(tmp_path):
    """T-1033: the watcher only checks every EXPLORER_WATCH_POLL seconds, and the old cleanup() just
    killed it outright on the way out - so a rogue that appeared (or was mid-way through being
    stopped) right as the window itself ended used to survive whenever the watcher's next poll never
    came round. Here the poll interval is far longer than the test, so the watcher on its own will
    never see this rogue; the window's own exit must still stop it, by finishing (or re-running) the
    watcher's job itself rather than merely killing it."""
    env, ops, radio_log = _stubs(tmp_path, claude_body=LONG_AGENT)
    ps_file = tmp_path / "ps.txt"
    ps_file.write_text("")
    env["EXPLORER_PS_OUTPUT"] = str(ps_file)
    env["EXPLORER_WATCH_POLL"] = "3600"  # the background watcher will not poll again in this test
    p = _start([str(WINDOW), "--window", "1h"], env)
    d = ops / "explorer"
    _wait_for(p, "the window's own hk serve started", lambda: (d / "server.datadir").exists())
    kept_pid = int((d / "server.pid").read_text())
    kept_datadir = pathlib.Path((d / "server.datadir").read_text().strip())
    port = env["EXPLORER_PORT"]
    rogue = None
    try:
        rogue = subprocess.Popen(["sleep", "600"])
        rogue_datadir = ops / "explorer" / "data-rogue"
        (rogue_datadir / "iqbuffer").mkdir(parents=True)
        (rogue_datadir / "iqbuffer" / "ring.bin").write_bytes(b"\0" * 4096)
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:{port}\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {rogue_datadir} --bind 127.0.0.1:{port}\n"
        )
        _stop(p)  # ends the window right away, long before the watcher's next (3600s-away) poll
        log = (ops / "explorer" / "window.log").read_text()
        assert f"ALERT: rogue hk serve detected (pid {rogue.pid}, data-dir {rogue_datadir})" in log
        assert "rogue server's ring reaped: 4096 bytes" in log
        assert _exited_uncollected(rogue), "the window's own exit never stopped the rogue"
        assert not (rogue_datadir / "iqbuffer").exists(), "the rogue's ring was never reaped"
    finally:
        if rogue is not None:
            rogue.kill()
            rogue.wait()
    assert p.returncode == 128 + signal.SIGTERM
    assert _radio_calls(radio_log) == ["take", "release"]


def test_one_windows_exit_never_stops_another_process_naming_its_server(tmp_path):
    """T-1029. On the way out a window also stops "a leftover hk serve on :PORT" by pattern. That
    pattern (`hk serve.*127.0.0.1:PORT`) matched the AGENT too: the real agent's command line is the
    prompt, which says "... your one hk serve at http://127.0.0.1:PORT ...". So any window's exit
    TERMed any other process naming that server - here, a second window's agent on the same port
    (exit 143); on 2026-09-25 the gate's own run of this file, whose windows all shared port 65531
    with a worker's and a deflaker's concurrent runs, saw test_an_agent_crash_still_releases_and_keeps_its_status
    exit 143 instead of 7. Window A's agent is a bash whose argv keeps that prompt (no exec, like the
    real one); window B runs start to finish on A's port; A's agent must come through untouched."""
    (tmp_path / "a").mkdir()
    (tmp_path / "b").mkdir()
    env_a, ops_a, radio_a = _stubs(tmp_path / "a", claude_body="sleep 600")
    env_b, _, radio_b = _stubs(tmp_path / "b")
    port = env_a["EXPLORER_PORT"]
    assert env_b["EXPLORER_PORT"] == port  # on purpose: the two windows share one port
    a = _start([str(WINDOW), "--window", "1h"], env_a)
    try:
        agent_pidf = tmp_path / "a" / "agent.pid"
        _wait_for(a, "window A's agent started", lambda: agent_pidf.exists() and agent_pidf.read_text().strip())
        agent_pid = int(agent_pidf.read_text())
        agent_args = _args(agent_pid)
        assert f"hk serve at http://127.0.0.1:{port}" in agent_args  # what the old pattern matched

        b = _run([str(WINDOW), "--window", "1h"], env_b)
        assert b.returncode == 0, b.stderr
        assert _radio_calls(radio_b) == ["take", "release"]
        # B's exit pattern-kill waits (pgrep) until what it TERMed no longer matches, so had it hit
        # A's agent, the agent would be dead by now: gone, a zombie, or its pid reused - args differ.
        assert _args(agent_pid) == agent_args, "window B's exit stopped window A's agent"
        assert a.poll() is None, f"window B's exit ended window A ({a.returncode})"
    finally:
        _stop(a)
    assert a.returncode == 128 + signal.SIGTERM  # ended by this test, not by window B
    assert _radio_calls(radio_a) == ["take", "release"]


@pytest.mark.parametrize("sig", [signal.SIGHUP, signal.SIGTERM, signal.SIGINT])
def test_a_kill_of_the_pane_releases(tmp_path, sig):
    """tmux kill-session HUPs the pane's whole process group; the trap still releases."""
    env, ops, radio_log = _stubs(tmp_path, claude_body=LONG_AGENT)
    p = _start([str(WINDOW), "--window", "1h"], env)
    _agent_running(tmp_path, p)
    os.killpg(p.pid, sig)
    _finish(p)
    assert p.returncode == 128 + sig  # the window's own trap for this signal ended it
    assert _radio_calls(radio_log) == ["take", "release"]
    assert not (ops / "explorer" / "window.pid").exists()


def _tmux_env(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    env.pop("TMUX", None)
    env["TMUX_TMPDIR"] = tempfile.mkdtemp(prefix="hkx", dir="/tmp")  # a unix socket path is <= 104 bytes
    return env, radio_log


def test_launch_dry_run_validates_without_a_session(tmp_path):
    if not shutil.which("tmux"):
        pytest.skip("no tmux")
    env, radio_log = _tmux_env(tmp_path)
    r = subprocess.run([str(LAUNCH), "explorer", "--window", "3h", "--dry-run"], env=env,
                       capture_output=True, text=True, timeout=HANG_S)
    assert r.returncode == 0, r.stdout + r.stderr
    assert "take explorer 3h" in r.stdout
    assert _radio_calls(radio_log) == []
    bad = subprocess.run([str(LAUNCH), "explorer", "--window", "12h", "--dry-run"], env=env,
                         capture_output=True, text=True, timeout=HANG_S)
    assert bad.returncode != 0
    assert "take explorer" not in bad.stdout
    odd = subprocess.run([str(LAUNCH), "explorer", "--resume", "x"], env=env,
                         capture_output=True, text=True, timeout=HANG_S)
    assert odd.returncode != 0 and "unknown argument" in odd.stdout


def test_launch_refuses_a_second_explorer_session(tmp_path):
    if not shutil.which("tmux"):
        pytest.skip("no tmux")
    env, radio_log = _tmux_env(tmp_path)
    subprocess.run(["tmux", "new-session", "-d", "-s", "explore"], env=env, check=True)
    try:
        r = subprocess.run([str(LAUNCH), "explorer", "--window", "3h", "--dry-run"], env=env,
                           capture_output=True, text=True, timeout=HANG_S)
        assert r.returncode == 1 and "one explorer at a time" in r.stdout
        assert _radio_calls(radio_log) == []
    finally:
        subprocess.run(["tmux", "kill-server"], env=env)
