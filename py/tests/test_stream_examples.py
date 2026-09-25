"""T-060 stream client examples (py/examples): the stdlib parser against a captured bits stream
(bytes read over TCP from the mock-SDR e2e test, tests/e2e/tests/stream_external.rs), refusal
frames, the handshake line and the WAV writer."""

import io
import json
import struct
import sys
import wave
from pathlib import Path

import pytest

EXAMPLES = Path(__file__).resolve().parents[1] / "examples"
sys.path.insert(0, str(EXAMPLES))

import hk_audio_wav  # noqa: E402
import hk_bits  # noqa: E402
import hkstream  # noqa: E402

CAPTURE = Path(__file__).resolve().parent / "data" / "t060_fsk_bits.hkstream"
SENSOR_ID = bytes.fromhex("5a3c")  # fsk_burst_train default sensor id (first payload bytes)
SYNC = bytes.fromhex("2dd4")


def _bursts(data: bytes):
    reader = hkstream.StreamReader(io.BytesIO(data))
    header = reader.read_header()
    out = []
    try:
        for b in hk_bits.bursts(reader):
            out.append(b)
    except EOFError:
        pass  # the capture ends inside a frame
    return header, out


def test_captured_bits_stream_decodes_to_the_sensor_payloads():
    header, bursts = _bursts(CAPTURE.read_bytes())
    assert header["kind"] == "bits"
    assert header["datatype"] == "ru8"
    assert header["version"].startswith("1.")
    valid = [b for b in bursts if b.status.get("crc") == "valid"]
    assert len(valid) >= 3, [b.status for b in bursts]
    for b in valid:
        assert set(b.bits) <= {0, 1}
        assert len(b.bits) == b.status["symbols"]
        assert b.payload is not None and len(b.payload) == 6
        assert b.payload[:2] == SENSOR_ID
        at = b.status["payload_bit"]
        assert hkstream.pack_bits(b.bits[at - 16:at]) == SYNC
    line = hk_bits.describe(valid[0])
    assert "crc=valid" in line and "payload=5a3c" in line


def test_bits_cli_reads_a_dumped_stream(capsys):
    assert hk_bits.main(["--file", str(CAPTURE), "--count", "2"]) == 0
    lines = capsys.readouterr().out.strip().splitlines()
    assert len(lines) == 2
    assert all(line.startswith("burst ") for line in lines)


def test_a_refusal_frame_raises_refused():
    body = json.dumps({"type": "refused", "status": 401, "code": "unauthorized",
                       "reason": "missing or invalid token", "content_class": None}).encode()
    reader = hkstream.StreamReader(io.BytesIO(struct.pack("<I", len(body)) + body))
    with pytest.raises(hkstream.Refused) as e:
        reader.read_header()
    assert e.value.status == 401


def test_oversize_frames_are_fatal():
    reader = hkstream.StreamReader(io.BytesIO(struct.pack("<I", 1 << 20)))
    with pytest.raises(ValueError):
        reader.read_header()


def test_handshake_line_encodes_the_query():
    line = hkstream.handshake_line("open/bits", "a+b/c", f_lo=433.9e6, emitter=None)
    assert line == b"open/bits?token=a%2Bb%2Fc&f_lo=433900000.0\n"


def _frame(payload: bytes) -> bytes:
    return struct.pack("<I", len(payload)) + payload


def _record(rtype: int, flags: int, seq: int, index: int, payload: bytes) -> bytes:
    head = hkstream.RECORD_HEADER.pack(rtype, flags, 0, len(payload), seq, 1_000, index)
    return _frame(head + payload)


def test_audio_is_written_to_wav_with_gaps_filled():
    header = {
        "schema": "hackriff.stream", "version": "1.1", "stream_id": "listen/1", "kind": "audio",
        "content_class": "unrestricted", "source": "test", "datatype": "ri16_le",
        "sample_rate_hz": 48000, "audio": {"channels": 1, "mode": "wfm"},
        "max_frame_len": 1952, "record_header_len": 32, "t_start": 0, "hackriff_version": "test",
    }
    tone = struct.pack("<960h", *([1000, -1000] * 480))
    stream = (
        _frame(json.dumps(header).encode())
        + _record(1, 0, 0, 0, tone)
        + _record(3, 0, 1, 960, b'{"level_dbfs":-20.0,"squelch_open":true}')
        + _record(2, 2, 2, 960, struct.pack("<Q", 1))
        + _record(1, 2, 3, 1920, tone)  # 960 samples missing before it
    )
    out = io.BytesIO()
    n = hk_audio_wav.write_wav(hkstream.StreamReader(io.BytesIO(stream)), out, 10.0,
                               log=io.StringIO())
    assert n == 3 * 960
    out.seek(0)
    with wave.open(out, "rb") as w:
        assert (w.getnchannels(), w.getsampwidth(), w.getframerate()) == (1, 2, 48000)
        pcm = w.readframes(w.getnframes())
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm)
    assert samples[:2] == (1000, -1000)
    assert set(samples[960:1920]) == {0}
    assert samples[1920:1922] == (1000, -1000)


def test_stereo_audio_is_written_as_two_channel_wav_and_asked_for_only_on_request():
    """T-874: a two-channel stream (interleaved L, R) becomes a two-channel WAV, a gap is filled
    per frame (both channels), and ``channels=2`` is sent only with ``--stereo``."""
    header = {
        "schema": "hackriff.stream", "version": "1.5", "stream_id": "listen/2", "kind": "audio",
        "content_class": "unrestricted", "source": "test", "datatype": "ri16_le",
        "sample_rate_hz": 48000, "audio": {"channels": 2, "frame_samples": 960, "mode": "wfm"},
        "max_frame_len": 32 + 8 * 960, "record_header_len": 32, "t_start": 0,
        "hackriff_version": "test",
    }
    lr = struct.pack("<1920h", *([1000, -2000] * 960))
    stream = (
        _frame(json.dumps(header).encode())
        + _record(1, 0, 0, 0, lr)
        + _record(3, 0, 1, 960, b'{"level_dbfs":-20.0,"squelch_open":true,"stereo":true}')
        + _record(1, 2, 2, 1920, lr)  # one frame of 960 missing before it
    )
    out = io.BytesIO()
    n = hk_audio_wav.write_wav(hkstream.StreamReader(io.BytesIO(stream)), out, 10.0,
                               log=io.StringIO())
    assert n == 3 * 960, "sample frames, not interleaved values"
    out.seek(0)
    with wave.open(out, "rb") as w:
        assert (w.getnchannels(), w.getsampwidth(), w.getframerate()) == (2, 2, 48000)
        assert w.getnframes() == 3 * 960
        pcm = w.readframes(w.getnframes())
    samples = struct.unpack(f"<{len(pcm) // 2}h", pcm)
    assert samples[:4] == (1000, -2000, 1000, -2000)
    assert set(samples[2 * 960:2 * 1920]) == {0}
    assert samples[2 * 1920:2 * 1920 + 2] == (1000, -2000)

    mono = hkstream.handshake_line("open/listen", "t", f_lo=1.0, f_hi=2.0, channels=None)
    assert b"channels" not in mono, "a client that does not ask gets mono"
    stereo = hkstream.handshake_line("open/listen", "t", f_lo=1.0, f_hi=2.0, channels=2)
    assert stereo.endswith(b"&channels=2\n")
