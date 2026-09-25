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
        EXPLORER_PORT="65531",  # never a real explorer's server: cleanup pkills hk serve on this port
    )
    return env, ops, radio_log


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
    r = subprocess.run([str(WINDOW), "--window", "1m", "--session-id", sid], env=env, capture_output=True, text=True, timeout=30)
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
    r = subprocess.run([str(WINDOW), "--window", "1m"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]


def test_normal_exit_takes_then_releases_and_passes_the_deadline(tmp_path):
    env, ops, radio_log = _stubs(tmp_path)
    t0 = int(time.time())
    r = subprocess.run([str(WINDOW), "--window", "1h"], env=env, capture_output=True, text=True, timeout=30)
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
    r = subprocess.run([str(WINDOW), "--window", "1h"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, r.stderr
    assert _radio_calls(radio_log) == ["take", "release"]

    d = ops / "explorer"
    assert (d / "server.url").read_text().strip() == "http://127.0.0.1:65531"
    assert (d / "server.token").read_text().strip() == "test-token"
    datadir = pathlib.Path((d / "server.datadir").read_text().strip())
    assert datadir.name == "data"

    server_env = (tmp_path / "claude.server-env").read_text()
    assert "EXPLORER_SERVER_URL=http://127.0.0.1:65531" in server_env
    assert "EXPLORER_SERVER_TOKEN=test-token" in server_env
    assert f"EXPLORER_SERVER_DATADIR={datadir}" in server_env

    hk_args = (tmp_path / "hk.log").read_text()
    assert "--data-dir" in hk_args and "--bind 127.0.0.1:65531" in hk_args

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
    r = subprocess.run([str(WINDOW), "--window", "1h"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 7
    assert _radio_calls(radio_log) == ["take", "release"]


def test_a_refused_lock_never_starts_the_agent_and_releases_nothing(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, take_rc=1)
    r = subprocess.run([str(WINDOW), "--window", "1h"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 4
    assert _radio_calls(radio_log) == ["take"]
    assert not (tmp_path / "claude.args").exists()
    assert not (ops / "explorer" / "window.pid").exists()


def test_the_agent_waits_for_staging_to_let_go_and_a_stuck_staging_releases(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, staging="live")
    r = subprocess.run([str(WINDOW), "--window", "1h"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 5
    calls = [line.split()[0] for line in radio_log.read_text().splitlines()]
    assert calls[0] == "take" and calls[-1] == "release" and calls.count("status") >= 3
    assert not (tmp_path / "claude.args").exists()
    assert "did not switch to replay" in (ops / "explorer" / "window.log").read_text()


def test_window_end_stops_the_agent_and_releases(tmp_path):
    env, ops, radio_log = _stubs(tmp_path, claude_body="sleep 60")
    t0 = time.monotonic()
    r = subprocess.run([str(WINDOW), "--window", "3s"], env=env, capture_output=True, text=True, timeout=30)
    assert time.monotonic() - t0 < 15
    assert r.returncode != 0  # the agent was TERMed
    assert _radio_calls(radio_log) == ["take", "release"]
    log = (ops / "explorer" / "window.log").read_text()
    assert "wrap-up" in log and "window end" in log and "radio lock released" in log
    assert not (ops / "explorer" / "wrap-up").exists()


def test_a_second_server_under_the_window_is_stopped_and_reaped_untouched_first(tmp_path):
    """The 2026-09-25 06:43 amendment: even after being told at launch, the agent started a second
    hk serve per target. The window's watcher (fed a fake process table via EXPLORER_PS_OUTPUT, so
    this doesn't depend on the real `ps` seeing a real second `hk serve`) finds it, stops it, reaps
    its ring, and never touches the window's own (kept) server."""
    env, ops, radio_log = _stubs(tmp_path, claude_body="sleep 30")
    ps_file = tmp_path / "ps.txt"
    ps_file.write_text("")
    env["EXPLORER_PS_OUTPUT"] = str(ps_file)
    # The launcher's own foreground child is the 30 s claude stub, and bash does not run a TERM trap
    # until that foreground command returns - so at teardown this signals the whole process GROUP
    # (like test_a_kill_of_the_pane_releases below), not just the launcher's own pid, or the wait
    # below would hang for the stub's remaining sleep every time.
    p = subprocess.Popen([str(WINDOW), "--window", "20s"], env=env, start_new_session=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    rogue = None
    try:
        server_pidf = ops / "explorer" / "server.pid"
        for _ in range(100):
            if server_pidf.exists():
                break
            time.sleep(0.1)
        assert server_pidf.exists(), "the window's own hk serve never started"
        kept_pid = server_pidf.read_text().strip()
        kept_datadir = (ops / "explorer" / "server.datadir").read_text().strip()

        rogue = subprocess.Popen(["sleep", "100"])
        rogue_datadir = ops / "explorer" / "data-rogue"
        (rogue_datadir / "iqbuffer").mkdir(parents=True)
        (rogue_datadir / "iqbuffer" / "ring.bin").write_bytes(b"\0" * 2048)
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:65531\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {rogue_datadir} --bind 127.0.0.1:65531\n"
        )

        log_path = ops / "explorer" / "window.log"
        for _ in range(100):
            if log_path.exists() and "ALERT: rogue hk serve detected" in log_path.read_text():
                break
            time.sleep(0.1)
        assert "ALERT: rogue hk serve detected" in log_path.read_text()

        # The watcher's own kill-then-reap sequence (ops/explorer-window.sh: kill -TERM, a bounded
        # kill -0 wait loop, then the reap) takes a moment after the ALERT line - poll for its
        # completion (the "ring reaped" log line, which the script writes right after deleting the
        # directory) rather than asserting the instant the rogue process itself dies, which races it.
        for _ in range(100):
            if log_path.exists() and "rogue server's ring reaped" in log_path.read_text():
                break
            time.sleep(0.1)
        assert "rogue server's ring reaped" in log_path.read_text()
        assert rogue.poll() is not None, "the rogue server was never stopped"
        assert not (rogue_datadir / "iqbuffer").exists(), "the rogue's ring was never reaped"
        assert os.kill(int(kept_pid), 0) is None  # the window's own server is untouched
    finally:
        os.killpg(p.pid, signal.SIGTERM)
        p.wait(timeout=15)
        if rogue is not None and rogue.poll() is None:
            rogue.kill()
    assert _radio_calls(radio_log) == ["take", "release"]


def test_a_second_listener_sharing_the_kept_data_dir_never_reaps_the_kept_ring(tmp_path):
    """Review round 2: a rogue hk serve that reports the SAME --data-dir as the kept server (a
    second listener on it - exactly the shape explorer.md hands the agent, since the agent gets
    that literal EXPLORER_SERVER_DATADIR path) must be stopped, but its ring must never be
    reaped, because that directory IS the kept server's own still-live ring."""
    env, ops, radio_log = _stubs(tmp_path, claude_body="sleep 30", ring_kb=32)
    ps_file = tmp_path / "ps.txt"
    ps_file.write_text("")
    env["EXPLORER_PS_OUTPUT"] = str(ps_file)
    p = subprocess.Popen([str(WINDOW), "--window", "20s"], env=env, start_new_session=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    rogue = None
    try:
        server_pidf = ops / "explorer" / "server.pid"
        for _ in range(100):
            if server_pidf.exists():
                break
            time.sleep(0.1)
        assert server_pidf.exists(), "the window's own hk serve never started"
        kept_pid = server_pidf.read_text().strip()
        kept_datadir = pathlib.Path((ops / "explorer" / "server.datadir").read_text().strip())
        kept_ring = kept_datadir / "iqbuffer" / "ring.bin"
        for _ in range(100):  # the stub hk drops its ring right at start; wait for it to land
            if kept_ring.exists():
                break
            time.sleep(0.1)
        assert kept_ring.exists()
        kept_ring_bytes = kept_ring.stat().st_size

        rogue = subprocess.Popen(["sleep", "100"])  # a second listener, the SAME --data-dir as kept
        ps_file.write_text(
            f"{kept_pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:65531\n"
            f"{rogue.pid} hk serve --hackrf --data-dir {kept_datadir} --bind 127.0.0.1:65532\n"
        )

        log_path = ops / "explorer" / "window.log"
        for _ in range(100):
            if log_path.exists() and "rogue server's ring reaped" in log_path.read_text():
                break
            time.sleep(0.1)
        assert "rogue server's ring reaped" in log_path.read_text()
        assert rogue.poll() is not None, "the rogue listener was never stopped"
        # The point of this test: the kept server's OWN ring must survive, byte for byte.
        assert kept_ring.exists() and kept_ring.stat().st_size == kept_ring_bytes
        assert os.kill(int(kept_pid), 0) is None  # the kept server itself is untouched too
    finally:
        os.killpg(p.pid, signal.SIGTERM)
        p.wait(timeout=15)
        if rogue is not None and rogue.poll() is None:
            rogue.kill()
    assert _radio_calls(radio_log) == ["take", "release"]


@pytest.mark.parametrize("sig", [signal.SIGHUP, signal.SIGTERM, signal.SIGINT])
def test_a_kill_of_the_pane_releases(tmp_path, sig):
    """tmux kill-session HUPs the pane's whole process group; the trap still releases."""
    env, ops, radio_log = _stubs(tmp_path, claude_body="sleep 60")
    p = subprocess.Popen([str(WINDOW), "--window", "1h"], env=env, start_new_session=True,
                         stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    for _ in range(100):
        if (tmp_path / "claude.args").exists():
            break
        time.sleep(0.1)
    assert (tmp_path / "claude.args").exists()
    os.killpg(p.pid, sig)
    p.wait(timeout=15)
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
