"""The HackRF radio lock (T-922): `just radio`, the stage daemon's respect for it, the watchdog's
stale release. Every test runs against a temp `$HACKRIFF_OPS`; none touches the real radio or lock."""

import importlib.util
import pathlib
import re
import subprocess
import sys
import time

import pytest

from hkpy import radio as R

ROOT = pathlib.Path(__file__).resolve().parents[2]
STAGE = ROOT / "ops" / "stage.sh"
NOW = 1_790_000_000


@pytest.fixture(autouse=True)
def ops(tmp_path, monkeypatch):
    monkeypatch.setenv("HACKRIFF_OPS", str(tmp_path))
    monkeypatch.setenv("HK_ALERT_OFF", "1")
    return tmp_path


# ---------------------------------------------------------------- the module / CLI


@pytest.mark.parametrize("text,secs", [("3h", 10800), ("90m", 5400), ("45s", 45), ("2h30m", 9000), ("20", 1200)])
def test_durations(text, secs):
    assert R.parse_duration(text) == secs


@pytest.mark.parametrize("bad", ["", "0h", "soon", "3x", "3h tomorrow"])
def test_bad_durations_refused(bad):
    with pytest.raises(ValueError):
        R.parse_duration(bad)


def test_take_writes_the_key_value_file(ops):
    ok, msg = R.take("explorer", 3 * 3600, "window 1: FM/RDS", now=NOW)
    assert ok, msg
    text = (ops / "radio-lock").read_text()
    assert text == f"owner=explorer\nsince={NOW}\nuntil={NOW + 10800}\nwhy=window 1: FM/RDS\n"
    assert R.holder(now=NOW + 60)["owner"] == "explorer"


def test_take_refuses_while_held_even_by_the_same_owner():
    assert R.take("explorer", 3600, "a", now=NOW)[0]
    for who in ("capture-agent", "explorer"):
        ok, msg = R.take(who, 600, "b", now=NOW + 60)
        assert not ok and "held by explorer" in msg
    assert R.read()["owner"] == "explorer" and R.read()["until"] == NOW + 3600


def test_a_stale_lock_is_taken_over_and_the_takeover_says_whose_it_was():
    R.take("explorer", 60, "a", now=NOW)
    ok, msg = R.take("capture-agent", 600, "b", now=NOW + 61)
    assert ok and "replaced a stale lock: explorer" in msg
    assert R.read()["owner"] == "capture-agent"


def test_take_needs_a_one_word_owner_and_a_reason():
    assert not R.take("two words", 60, "x", now=NOW)[0]
    assert not R.take("explorer", 60, "  ", now=NOW)[0]


def test_release_only_by_the_owner(ops):
    R.take("explorer", 3600, "a", now=NOW)
    ok, msg = R.release("capture-agent")
    assert not ok and "held by explorer" in msg and (ops / "radio-lock").exists()
    assert R.release("explorer")[0] and not (ops / "radio-lock").exists()
    assert R.release("explorer") == (True, "not held - nothing to release")


def test_holder_ignores_stale_and_unparseable_locks(ops):
    R.take("explorer", 60, "a", now=NOW)
    assert R.holder(now=NOW + 60) and R.holder(now=NOW + 61) is None
    (ops / "radio-lock").write_text("garbage\n")
    lock = R.read()
    assert lock["owner"] == "?" and R.is_stale(lock, NOW) and R.holder(now=NOW) is None


def test_release_stale_removes_only_a_stale_lock(ops):
    R.take("explorer", 60, "a", now=NOW)
    assert R.release_stale(now=NOW + 30) is None and (ops / "radio-lock").exists()
    gone = R.release_stale(now=NOW + 61)
    assert gone["owner"] == "explorer" and not (ops / "radio-lock").exists()


def test_status_prints_holder_until_and_staging_mode(ops):
    assert R.status(now=NOW) == "radio: free\nstaging: unknown (no hk-serve-source)"
    R.take("explorer", 3600, "window 1", now=NOW)
    (ops / "hk-serve-source").write_text("source: replay (radio-lock: explorer until 06:55)\n")
    out = R.status(now=NOW + 60)
    assert out.startswith("radio: explorer since ") and "59 min left" in out and "window 1" in out
    assert out.endswith("staging: replay (radio-lock: explorer until 06:55)")


def test_the_cli_as_the_justfile_calls_it(ops, capsys):
    assert R.main(["take", "explorer", "3h", "first", "window"]) == 0
    assert "taken: explorer" in capsys.readouterr().out
    assert R.main(["take", "capture-agent", "10m", "x"]) == 1
    assert "held by explorer" in capsys.readouterr().err
    assert R.main(["take", "capture-agent", "soon", "x"]) == 2
    assert R.main(["status"]) == 0 and "radio: explorer" in capsys.readouterr().out
    assert R.main(["release", "capture-agent"]) == 1
    assert R.main(["release", "explorer"]) == 0 and not (ops / "radio-lock").exists()


def test_the_justfile_recipe_passes_arguments_through():
    text = (ROOT / "justfile").read_text()
    assert re.search(r"\[positional-arguments\]\nradio \*args:\n    uv run --locked --project py python -m hkpy\.radio \"\$@\"", text)


# ---------------------------------------------------------------- ops/stage.sh


def _stage_fn(name: str) -> str:
    m = re.search(rf"^{name}\(\)\{{.*?^\}}\n", STAGE.read_text(), re.M | re.S)
    assert m, name
    return m.group(0)


def _bash(script: str, **env) -> subprocess.CompletedProcess:
    return subprocess.run(["bash", "-c", script], capture_output=True, text=True,
                          env={"PATH": "/usr/bin:/bin", **env})


def _holder(ops) -> subprocess.CompletedProcess:
    return _bash(f'LOCK={ops}/radio-lock\n{_stage_fn("radio_holder")}radio_holder')


def test_stage_sees_a_live_lock_held_by_someone_else(ops):
    assert _holder(ops).returncode == 1                                  # no lock: free
    R.take("explorer", 3600, "w", now=time.time())
    r = _holder(ops)
    assert r.returncode == 0 and r.stdout.startswith("explorer until ")


def test_stage_ignores_its_own_lock_and_a_stale_one(ops):
    R.take("stage", 3600, "w", now=time.time())
    assert _holder(ops).returncode == 1
    R.release("stage")
    R.take("explorer", 60, "w", now=time.time() - 3600)
    assert _holder(ops).returncode == 1


@pytest.mark.parametrize("out,free", [
    ("hackrf_info version: 2024.02.1\nFound HackRF\nIndex: 0\nSerial number: 0000abcd\n"
     "Board ID Number: 2 (HackRF One)\nFirmware Version: 2024.02.1 (API:1.08)\n", True),
    # T-356's HIL: the open fails, the tool still says "Found HackRF" and exits 0.
    ("Found HackRF\nIndex: 0\nSerial number: 0000abcd\nhackrf_open() failed: Access denied (-1000)\n", False),
    ("hackrf_info version: 2024.02.1\nNo HackRF boards found.\n", False),
])
def test_stage_busy_check_reads_the_output_not_the_exit_status(tmp_path, out, free):
    fake = tmp_path / "fake_hackrf_info"
    fake.write_text(f"#!/bin/bash\ncat <<'EOF'\n{out}EOF\nexit 0\n")
    fake.chmod(0o755)
    r = _bash(f'HACKRF_INFO={fake}\n{_stage_fn("hackrf_free")}hackrf_free')
    assert (r.returncode == 0) is free


def test_stage_start_and_loop_are_lock_driven():
    text = STAGE.read_text()
    start = _stage_fn("start_server")
    assert start.index("radio_holder") < start.index("hackrf_free")   # never probe a held radio
    assert "hackrf_info >/dev/null" not in text                        # the exit-status check is gone
    assert 'start_replay "radio-lock: $held"' in start
    assert "radio-lock released -> back to LIVE" in text and "radio-lock taken by $HELD" in text


# ---------------------------------------------------------------- ops/watchdog.py


def _watchdog(ops, monkeypatch):
    spec = importlib.util.spec_from_file_location("hk_watchdog_radio", ROOT / "ops" / "watchdog.py")
    W = importlib.util.module_from_spec(spec)
    sys.modules["hk_watchdog_radio"] = W
    spec.loader.exec_module(W)
    monkeypatch.setattr(W, "S", str(ops))
    monkeypatch.setattr(W, "LOG", str(ops / "watchdog.log"))
    return W


def test_watchdog_releases_a_stale_lock_with_a_red_alert(ops, monkeypatch):
    W = _watchdog(ops, monkeypatch)
    R.take("explorer", 60, "w", now=NOW)
    assert W.radio_stale(NOW + 30) == []
    dry = W.radio_stale(NOW + 61, dry=True)
    assert dry and "would be released" in dry[0]["title"] and (ops / "radio-lock").exists()
    [a] = W.radio_stale(NOW + 61)
    assert a["level"] == "red" and a["key"] == "watchdog:radio-stale" and "explorer" in a["title"]
    assert not (ops / "radio-lock").exists()
    assert W.radio_stale(NOW + 62) == []
