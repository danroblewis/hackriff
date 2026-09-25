"""The explorer window (T-923): `ops/launch.sh explorer` validates its arguments and refuses a second
instance; `ops/explorer-window.sh` takes the radio lock before the agent starts and releases it on
every way out (normal exit, agent crash, window end, a tmux kill). Driven against a temp
HACKRIFF_OPS with a stub `just radio` and a stub claude - no tmux window, no HackRF."""

import os
import pathlib
import platform
import shutil
import signal
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


def _stubs(tmp_path, claude_body="exit 0", take_rc=0, staging="replay (radio-lock: explorer until 05:00)"):
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
        f"{claude_body}\n"
    )
    for f in (radio, claude):
        f.chmod(0o755)
    env = dict(
        os.environ,
        HACKRIFF_OPS=str(ops),
        EXPLORER_RADIO=str(radio),
        EXPLORER_CLAUDE=str(claude),
        EXPLORER_REPO=str(tmp_path),
        EXPLORER_KILL_GRACE="2",
        EXPLORER_WRAPUP_S="1",
        EXPLORER_POLL="1",
        EXPLORER_STAGING_WAIT="3",
        EXPLORER_PORT="65531",  # never a real explorer's server: cleanup pkills hk serve on this port
    )
    return env, ops, radio_log


def _clean_signal_mask():
    signal.pthread_sigmask(signal.SIG_SETMASK, [])


def _start(args, env):
    """The window as tmux starts a pane: its own session (a leftover can be found and killed as its
    process group) and an empty signal mask. A mask is inherited across fork and exec, and this test
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
                       capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, r.stdout + r.stderr
    assert "take explorer 3h" in r.stdout
    assert _radio_calls(radio_log) == []
    bad = subprocess.run([str(LAUNCH), "explorer", "--window", "12h", "--dry-run"], env=env,
                         capture_output=True, text=True, timeout=30)
    assert bad.returncode != 0
    assert "take explorer" not in bad.stdout
    odd = subprocess.run([str(LAUNCH), "explorer", "--resume", "x"], env=env,
                         capture_output=True, text=True, timeout=30)
    assert odd.returncode != 0 and "unknown argument" in odd.stdout


def test_launch_refuses_a_second_explorer_session(tmp_path):
    if not shutil.which("tmux"):
        pytest.skip("no tmux")
    env, radio_log = _tmux_env(tmp_path)
    subprocess.run(["tmux", "new-session", "-d", "-s", "explore"], env=env, check=True)
    try:
        r = subprocess.run([str(LAUNCH), "explorer", "--window", "3h", "--dry-run"], env=env,
                           capture_output=True, text=True, timeout=30)
        assert r.returncode == 1 and "one explorer at a time" in r.stdout
        assert _radio_calls(radio_log) == []
    finally:
        subprocess.run(["tmux", "kill-server"], env=env)
