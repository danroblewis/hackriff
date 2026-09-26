"""Fixture tooling in ``py/fixtures/``: sample-exact trimming, manifest verification, fetch,
annotation, the promoted RDS reference decoder, and the committed manifest itself."""

from __future__ import annotations

import json
import math
import sys
from pathlib import Path

import numpy as np
import pytest

FIXTURE_TOOLS = Path(__file__).resolve().parents[1] / "fixtures"
sys.path.insert(0, str(FIXTURE_TOOLS))

import ais_ref  # noqa: E402
import annotate  # noqa: E402
import ctcss_ref  # noqa: E402
import dmr_ref  # noqa: E402
import fetch  # noqa: E402
import flex_ref  # noqa: E402
import fxlib  # noqa: E402
import p25_ref  # noqa: E402
import rds_ref  # noqa: E402
import trim  # noqa: E402
import verify  # noqa: E402
from hkpy import sigmf  # noqa: E402

FS = 1000.0

#: Provenance exactly as the 2026-09-13 store captures wrote it (nulls, free-text enums).
STORE_PROVENANCE = {
    "device_id": "hackrf-0000000000000000d2b861dc263bc293",
    "tune": {"center_hz": 915e6, "sample_rate_hz": FS, "lna_db": 24, "vga_db": 30, "amp_on": True,
             "bandwidth_hz": None},
    "clip_count": 0, "overload": None, "temperature_c": None,
    "antenna_port": "unknown (as attached by user; not changed)",
    "clock_source": "internal TCXO/crystal", "clock_locked": None, "calibration_state_ref": None,
    "spur_mask_ref": None, "timestamp_method": "host-arrival-time (capture start, NTP host clock)",
    "timestamp_error_budget_s": 0.5,
}


def write_source(tmp_path: Path, n: int = 10_000) -> tuple[Path, np.ndarray]:
    rng = np.random.default_rng(7)
    pairs = rng.integers(-20, 21, size=(n, 2)).astype(np.int8)
    for idx in (100, 2500, 2501, 6000, 9999):  # clipped samples: one rail at full scale
        pairs[idx, idx % 2] = 127 if idx % 3 else -128
    data = tmp_path / "src.sigmf-data"
    pairs.reshape(-1).tofile(data)
    meta = {
        "global": {"core:datatype": "ci8", "core:sample_rate": FS, "core:version": "1.2.0",
                   "hackriff:provenance": STORE_PROVENANCE, "hackriff:antenna": "unknown"},
        "captures": [
            {"core:sample_start": 0, "core:frequency": 915e6, "core:datetime": "2026-09-13T11:08:46.357165+00:00"},
            {"core:sample_start": 4000, "core:frequency": 916e6, "core:datetime": "2026-09-13T11:08:50.357165Z",
             "core:global_index": 123_456},
        ],
        "annotations": [
            {"core:sample_start": 1000, "core:sample_count": 2000, "core:label": "a"},
            {"core:sample_start": 8000, "core:sample_count": 100, "core:label": "outside"},
        ],
    }
    (tmp_path / "src.sigmf-meta").write_text(json.dumps(meta))
    return tmp_path / "src.sigmf-meta", pairs


def test_trim_is_sample_exact_with_consistent_metadata(tmp_path):
    src, pairs = write_source(tmp_path)
    dst = tmp_path / "out" / "cut.sigmf-meta"
    out = trim.trim(src, dst, start_s=2.0, duration_s=3.0)  # samples [2000, 5000)

    got = np.fromfile(sigmf.data_path(dst), dtype=np.int8).reshape(-1, 2)
    assert np.array_equal(got, pairs[2000:5000])

    meta = sigmf.read_meta(dst)  # validates provenance against the schema
    caps = meta["captures"]
    assert [c["core:sample_start"] for c in caps] == [0, 2000]
    assert [c["core:global_index"] for c in caps] == [2000, 123_456]
    assert caps[0]["core:datetime"] == "2026-09-13T11:08:48.357165Z"
    assert caps[1]["core:datetime"] == "2026-09-13T11:08:50.357165Z"
    assert [c["hackriff:clip_count"] for c in caps] == [2, 0]  # 2500, 2501 in segment 0
    assert [c["core:frequency"] for c in caps] == [915e6, 916e6]

    prov = meta["global"]["hackriff:provenance"]
    assert prov["device_id"] == "hackrf:0000000000000000d2b861dc263bc293"
    assert "clip_count" not in prov
    assert prov["overload"] is True  # 2 / 3000 > 1e-4
    assert prov["clock_source"] == "internal" and prov["clock_locked"] is True
    assert prov["timestamp_method"] == "host-arrival"
    assert prov["timestamp_error_budget_ns"] == 500_000_000
    assert prov["tune"]["bandwidth_hz"] == 1.75e6
    assert isinstance(prov["quantisation_limited"], bool)
    assert not any(v is None for v in prov.values())
    assert meta["global"]["hackriff:antenna"] == "unknown"

    [ann] = meta["annotations"]
    assert (ann["core:sample_start"], ann["core:sample_count"]) == (0, 1000)
    assert out["annotations"] == meta["annotations"]


def test_trim_window_inside_second_segment_offsets_global_index(tmp_path):
    src, pairs = write_source(tmp_path)
    dst = tmp_path / "cut2.sigmf-meta"
    trim.trim(src, dst, start_sample=5000, count=5000)
    meta = sigmf.read_meta(dst)
    [cap] = meta["captures"]
    assert cap["core:sample_start"] == 0
    assert cap["core:global_index"] == 123_456 + 1000
    assert cap["core:datetime"] == "2026-09-13T11:08:51.357165Z"
    assert cap["hackriff:clip_count"] == 2  # 6000 and 9999
    assert np.array_equal(np.fromfile(sigmf.data_path(dst), dtype=np.int8).reshape(-1, 2), pairs[5000:])


def test_trim_rejects_windows_outside_the_recording(tmp_path):
    src, _ = write_source(tmp_path)
    with pytest.raises(ValueError):
        trim.trim(src, tmp_path / "bad.sigmf-meta", start_s=9.0, duration_s=2.0)


def test_hackrf_default_bandwidth_rounds_down():
    assert fxlib.hackrf_default_bandwidth(2.4e6) == 1.75e6
    assert fxlib.hackrf_default_bandwidth(2e6) == 1.75e6
    assert fxlib.hackrf_default_bandwidth(10e6) == 7e6
    assert fxlib.hackrf_default_bandwidth(20e6) == 15e6


def test_annotate_fills_times_and_rejects_unknown_roles(tmp_path):
    src, _ = write_source(tmp_path)
    dst = tmp_path / "ann.sigmf-meta"
    trim.trim(src, dst, start_s=0.0, duration_s=2.0)
    annotate.annotate(dst, {"annotations": [
        {"sample_start": 500, "sample_count": 250, "truth": {"role": "emission", "kind": "cw"}},
        {"truth": {"role": "floor", "kind": "noise-floor", "t_start_s": 1.5, "duration_s": 9.0}},
    ]})
    truths = [a for a in sigmf.read_meta(dst)["annotations"] if sigmf.TRUTH_KEY in a]
    assert [(a["core:sample_start"], a["core:sample_count"]) for a in truths] == [(500, 250), (1500, 500)]
    assert truths[0][sigmf.TRUTH_KEY]["t_start_s"] == 0.5
    assert truths[1][sigmf.TRUTH_KEY]["duration_s"] == 0.5  # clipped to the file
    with pytest.raises(annotate.TruthError):
        annotate.annotate(dst, {"annotations": [{"sample_start": 0, "sample_count": 1,
                                                 "truth": {"role": "recording", "kind": "x"}}]})


def make_manifest(root: Path, entries: list[dict]) -> Path:
    path = root / "manifest.json"
    path.write_text(json.dumps({"version": fxlib.MANIFEST_VERSION, "store": "fixtures/store", "entries": entries}))
    return path


def file_entry(root: Path, rel: str, content: bytes, status: str = "committed") -> dict:
    p = root / rel if not rel.startswith("store/") else root / "store" / rel[len("store/"):]
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(content)
    return {"name": Path(rel).stem, "status": status, "path": rel, "size_bytes": len(content),
            "sha256": fxlib.sha256_file(p)}


def test_verify_accepts_matching_files_and_reports_problems(tmp_path, monkeypatch):
    good = file_entry(tmp_path, "hackrf/x/a.sigmf-data", b"\x01\x02" * 50)
    ext_present = file_entry(tmp_path, "store/d/big.sigmf-data", b"\x00" * 64, status="external")
    ext_missing = {"name": "gone", "status": "external", "path": "store/d/gone.sigmf-data",
                   "size_bytes": 1, "sha256": "0" * 64}
    blocked = {"name": "adsb", "status": "blocked", "reason": "antenna"}
    m = make_manifest(tmp_path, [good, ext_present, ext_missing, blocked])
    problems, notes = verify.verify(m, store=tmp_path / "store", external=True)
    assert problems == [] and len(notes) == 1 and "gone" in notes[0]
    problems, _ = verify.verify(m, store=tmp_path / "store", external=True, require_external=True)
    assert len(problems) == 1

    (tmp_path / "hackrf/x/a.sigmf-data").write_bytes(b"\x01\x03" * 50)  # same size, other bytes
    lfs = file_entry(tmp_path, "hackrf/x/b.sigmf-data",
                     b"version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 9\n")
    no_reason = {"name": "t", "status": "blocked"}
    m = make_manifest(tmp_path, [good, lfs, no_reason])
    problems, _ = verify.verify(m, store=tmp_path / "store")
    assert any("sha256" in p for p in problems)
    assert any("LFS pointer" in p for p in problems)
    assert any("without reason" in p for p in problems)

    monkeypatch.setattr(fxlib, "MAX_COMMITTED_BYTES", 10)
    (tmp_path / "hackrf/x/a.sigmf-data").write_bytes(b"\x01\x02" * 50)
    problems, _ = verify.verify(make_manifest(tmp_path, [good]), store=tmp_path / "store")
    assert any("exceeds" in p for p in problems)


def test_fetch_copies_with_checksum_and_refuses_corrupt_sources(tmp_path):
    src_root, dst_root = tmp_path / "src", tmp_path / "dst"
    e = file_entry(src_root, "store/d/orig.sigmf-data", b"iq" * 1000, status="external")
    m = make_manifest(tmp_path, [e])
    assert fetch.fetch(m, src=src_root / "store", dst=dst_root) == []
    assert (dst_root / "d/orig.sigmf-data").read_bytes() == b"iq" * 1000
    assert fetch.fetch(m, src=dst_root, dst=dst_root) == []  # verify-only

    (dst_root / "d/orig.sigmf-data").unlink()
    (src_root / "store/d/orig.sigmf-data").write_bytes(b"IQ" * 1000)
    problems = fetch.fetch(m, src=src_root / "store", dst=dst_root)
    assert problems and not (dst_root / "d/orig.sigmf-data").exists()


def test_rds_reference_decoder_recovers_synthetic_pi_ps_and_stats(tmp_path):
    from hkpy.synth import generate

    out = tmp_path / "rds"
    generate("fm_broadcast_rds", 3, out, {"duration_s": 1.5}, "ci8")
    manifest = json.loads((out / "manifest.json").read_text())
    meta_path = out / manifest["recordings"][0]
    meta = sigmf.read_meta(meta_path)
    fs = meta["global"]["core:sample_rate"]
    raw = np.fromfile(sigmf.data_path(meta_path), dtype=np.int8).astype(np.float32).reshape(-1, 2)
    x = (raw[:, 0] + 1j * raw[:, 1]).astype(np.complex128)
    from scipy import signal
    x = np.convolve(x, signal.firwin(129, 140e3, fs=fs), mode="same")
    got = rds_ref.decode(x, fs)
    assert got["pi_hex"] == "C0DE"
    assert got["ps"] == "HACKRIFF" and not got["ps_dynamic"]
    assert got["pty"] == 10 and got["tp"] is True
    assert got["groups_decoded"] >= 8
    assert got["block_error_rate"] == 0.0
    assert abs(got["pilot_hz"] - 19000.0) < 0.5
    assert math.isclose(got["bitrate_bd_in_sample_clock"], got["pilot_hz"] / 16)


def test_ais_reference_decoder_recovers_synthetic_vessels_blind(tmp_path):
    """T-963 (SIGNAL-015): the ``ais_vessels`` scene's three Class A position reports, on the
    two fixed marine channels, are recovered by the independent oracle from the raw IQ alone —
    blind: `ais_ref` is never told the MMSI, message type or channel a burst is on, only the two
    channels' offsets (a receiver would tune to, same as the fixed frequencies any AIS receiver
    already knows) and the recording itself."""
    from scipy import signal

    from hkpy.synth import generate
    from hkpy.synth.ais_scene import CHANNEL_HZ

    out = tmp_path / "ais"
    generate("ais_vessels", 963, out, {}, "ci8")
    manifest = json.loads((out / "manifest.json").read_text())
    meta_path = out / manifest["recordings"][0]
    meta = sigmf.read_meta(meta_path)
    fs = meta["global"]["core:sample_rate"]
    center_hz = meta["captures"][0]["core:frequency"]
    raw = np.fromfile(sigmf.data_path(meta_path), dtype=np.int8).astype(np.float32).reshape(-1, 2)
    x = (raw[:, 0] + 1j * raw[:, 1]).astype(np.complex128)

    truth = [a["hackriff:truth"] for a in meta["annotations"] if a["hackriff:truth"].get("role") == "emission"]
    assert len(truth) == 3, f"3 vessels in the hidden truth, got {len(truth)}"

    got = []
    for ch_hz in CHANNEL_HZ:
        off = ch_hz - center_hz
        tt = np.arange(len(x)) / fs
        base = x * np.exp(-2j * math.pi * off * tt)
        bw = 40e3
        base = np.convolve(base, signal.firwin(129, bw / 2, fs=fs), mode="same")
        got += ais_ref.decode_channel(base, fs)

    got_by_mmsi = {m["mmsi"]: m for m in got}
    assert {t["mmsi"] for t in truth} == set(got_by_mmsi), (
        f"blind decode found {sorted(got_by_mmsi)}, truth has {sorted(t['mmsi'] for t in truth)}"
    )
    for t in truth:
        m = got_by_mmsi[t["mmsi"]]
        assert m["message_type"] == t["msg_type"]
        assert m["nav_status"] == t["nav_status"]
        assert math.isclose(m["sog_kt"], t["sog_kt"], abs_tol=0.05)
        assert math.isclose(m["longitude_deg"], t["longitude_deg"], abs_tol=1e-4)
        assert math.isclose(m["latitude_deg"], t["latitude_deg"], abs_tol=1e-4)
        assert math.isclose(m["cog_deg"], t["cog_deg"], abs_tol=0.05)
        assert m["heading_deg"] == t["heading_deg"]


# ---- AIS on-air conventions, validated against sources outside this repo (T-963 fix round) ----

#: A published AIS position report: the worked example of GPSD's "AIVDM/AIVDO protocol
#: decoding" (E. S. Raymond, https://gpsd.gitlab.io/gpsd/AIVDM.html, section "AIVDM/AIVDO
#: Sentence Layer"; also in gpsd's ``test/sample.aivdm`` regression data), whose documented decode
#: is: type 1, MMSI 477553000, status 5 (moored), turn 0, speed 0, accuracy false,
#: lon -122.345832, lat 47.582833, course 51.0, heading 181, second 15, maneuver 0, raim false,
#: radio 149208. The NMEA checksum (XOR between '!' and '*') is asserted too, so a mistyped
#: sentence cannot pass.
GPSD_AIVDM = "!AIVDM,1,1,,B,177KQJ5000G?tO`K>RA1wUbN0TKH,0*5C"
GPSD_FIELDS = {
    "message_type": 1, "repeat_indicator": 0, "mmsi": 477553000, "nav_status": 5, "rot": 0,
    "sog_kt": 0.0, "position_accuracy": 0, "longitude_deg": -122.345832,
    "latitude_deg": 47.582833, "cog_deg": 51.0, "heading_deg": 181, "timestamp_s": 15,
    "maneuver": 0, "raim": 0, "comm_state": 149208,
}


def _dearmour(sentence: str) -> np.ndarray:
    """The message bit string (MSB first) of a single-part !AIVDM sentence, decoded here from
    the ITU-R M.1371 / IEC 61162-1 6-bit armouring, independently of ``ais_ref``/``hkpy.synth``."""
    body = sentence[1 : sentence.index("*")]
    xor = 0
    for c in body:
        xor ^= ord(c)
    assert f"{xor:02X}" == sentence[-2:], "NMEA checksum"
    bits = []
    for c in sentence.split(",")[5]:
        v = ord(c) - 48
        v = v - 8 if v > 40 else v
        bits += [(v >> k) & 1 for k in range(5, -1, -1)]
    return np.array(bits, dtype=np.uint8)


def _rfc1662_fcs16(data: bytes) -> int:
    """RFC 1662 appendix C.2's FCS-16 (the HDLC/PPP CRC, processed as the octets go on air:
    each LSB first). Over data || FCS (complemented, least significant octet first) it leaves
    the good residue PPPGOODFCS16 = 0xF0B8."""
    fcs = 0xFFFF
    for byte in data:
        fcs ^= byte
        for _ in range(8):
            fcs = (fcs >> 1) ^ 0x8408 if fcs & 1 else fcs >> 1
    return fcs


def _serial_fcs_residue(air: np.ndarray) -> int:
    """The same FCS-16 as a bit-serial LFSR over the frame's bits in *transmission order* — no
    octet packing at all, so no bit-order convention can be smuggled in."""
    reg = 0xFFFF
    for b in air:
        fb = (reg ^ int(b)) & 1
        reg >>= 1
        if fb:
            reg ^= 0x8408
    return reg


def _air_between_flags(line: np.ndarray) -> np.ndarray:
    """Destuffs the content of a single synth frame (flag || stuffed || flag), independently of
    the oracle's destuffer."""
    stuffed = line[8:-8]
    out, ones, skip = [], 0, False
    for b in stuffed:
        b = int(b)
        if skip:
            assert b == 0, "a stuffed bit must be 0"
            skip, ones = False, 0
            continue
        out.append(b)
        ones = ones + 1 if b else 0
        if ones == 5:
            skip = True
    return np.array(out, dtype=np.uint8)


def _assert_fields(got: dict, want: dict) -> None:
    for k, v in want.items():
        if isinstance(v, float):
            # GPSD prints lon/lat truncated to 6 decimals (raw units are 1/600000 degree).
            assert math.isclose(got[k], v, abs_tol=2e-6), (k, got[k], v)
        else:
            assert got[k] == v, (k, got[k], v)


def test_ais_published_aivdm_vector_frames_lsb_first_and_decodes():
    """The GPSD example sentence, independently de-armoured, is (a) exactly the bit string
    ``hkpy.synth.ais.build_position_report`` makes from its documented fields, (b) framed by the
    synth LSB first per octet with the FCS low octet first — checked against a frame built here
    from RFC 1662's byte-wise FCS — and (c) decoded by ``ais_ref`` to every documented field,
    RAIM and communication state included."""
    from hkpy.synth import ais

    msg = _dearmour(GPSD_AIVDM)
    assert len(msg) == 168
    built = ais.build_position_report(
        mmsi=477553000, nav_status=5, rot=0, sog_kt=0.0, lon_deg=-73_407_500 / 600_000,
        lat_deg=28_549_700 / 600_000, cog_deg=51.0, heading_deg=181, timestamp_s=15, comm_state=149208,
    )
    assert np.array_equal(built, msg)

    octets = np.packbits(msg).tobytes()
    fcs = _rfc1662_fcs16(octets) ^ 0xFFFF
    on_air = octets + bytes([fcs & 0xFF, fcs >> 8])
    want_air = np.array([(o >> k) & 1 for o in on_air for k in range(8)], dtype=np.uint8)
    assert _rfc1662_fcs16(on_air) == 0xF0B8

    line = ais.frame_bits(msg)
    got_air = _air_between_flags(line)
    assert np.array_equal(got_air, want_air), "synth frame is not LSB-first / FCS low octet first"

    data_bits, ok = ais_ref.check_crc(want_air)
    assert ok, "oracle rejects a correctly ordered on-air frame"
    assert np.array_equal(data_bits, msg)
    _assert_fields(ais_ref.parse_common_block(data_bits), GPSD_FIELDS)
    got = ais_ref.decode_line_bits(np.concatenate([np.ones(16, np.uint8), line, np.ones(16, np.uint8)]))
    assert len(got) == 1
    _assert_fields(got[0], GPSD_FIELDS)


def test_ais_synth_frames_leave_the_hdlc_good_fcs_residue():
    """Every synth frame's destuffed content, run bit-serially in transmission order through the
    HDLC FCS register, ends on 0xF0B8 (RFC 1662 C.2) — the check a real HDLC receiver makes,
    independent of any octet packing."""
    from hkpy.synth import ais

    rng = np.random.default_rng(963)
    for _ in range(50):
        msg = rng.integers(0, 2, 168, dtype=np.uint8)
        assert _serial_fcs_residue(_air_between_flags(ais.frame_bits(msg))) == 0xF0B8


def _ais_burst_decode(msg: np.ndarray, fs: float = 96_000.0) -> list[dict]:
    from hkpy.synth import ais

    iq = ais.burst_iq(msg, fs, phase0=0.3)
    pad = np.zeros(int(0.04 * fs), dtype=complex)  # noise-dominated: find_bursts keys on the median
    rng = np.random.default_rng(5)
    x = np.concatenate([pad, iq, pad])
    x = x + 0.01 * (rng.standard_normal(len(x)) + 1j * rng.standard_normal(len(x)))
    return ais_ref.decode_channel(x, fs)


def test_ais_frames_whose_destuffed_content_holds_0x7e_decode():
    """The flag is unique only on the still-stuffed line: a report whose destuffed on-air bits
    contain 01111110 must decode whole (it truncated when destuffing ran before the flag
    search). Such reports are common — found here by scanning MMSIs, never hand-tuned."""
    from hkpy.synth import ais

    found = 0
    for mmsi in range(366_000_000, 366_000_400):
        msg = ais.build_position_report(mmsi, sog_kt=7.5, lon_deg=-70.25, lat_deg=41.5,
                                        cog_deg=90.0, heading_deg=90, timestamp_s=30)
        air = "".join(map(str, ais.air_bits(msg)))
        if "01111110" not in air:
            continue
        found += 1
        line = ais.frame_bits(msg)
        got = ais_ref.decode_line_bits(np.concatenate([np.ones(8, np.uint8), line]))
        assert [m["mmsi"] for m in got] == [mmsi]
        if found == 1:
            burst = _ais_burst_decode(msg)
            assert [m["mmsi"] for m in burst] == [mmsi], "IQ burst with an in-data 0x7E"
    assert found >= 5, found


def test_ais_raim_and_comm_state_round_trip():
    """RAIM is bit 148 and the communication state bits 149..167 (ITU-R M.1371-5 table 45)."""
    from hkpy.synth import ais

    msg = ais.build_position_report(244_660_000, raim=1, comm_state=0x5A5A5, timestamp_s=7)
    got = _ais_burst_decode(msg)
    assert len(got) == 1
    assert got[0]["raim"] == 1
    assert got[0]["comm_state"] == 0x5A5A5
    assert got[0]["timestamp_s"] == 7
    assert got[0]["mmsi"] == 244_660_000


def test_ais_oracle_keeps_distinct_reports_with_timestamp_not_available():
    """Two different reports from one vessel with timestamp 60 ('not available') are two
    reports, not one."""
    from hkpy.synth import ais

    a = ais.frame_bits(ais.build_position_report(211_000_001, sog_kt=1.0))
    b = ais.frame_bits(ais.build_position_report(211_000_001, sog_kt=2.0))
    got = ais_ref.decode_line_bits(np.concatenate([a, np.ones(20, np.uint8), b]))
    assert sorted(m["sog_kt"] for m in got) == [1.0, 2.0]


def test_flex_reference_oracle_finds_synthetic_sync_and_levels():
    """A synthetic FLEX preamble (alternating dotting bits) + the 32-bit frame sync
    ``0xA6C6AAAA`` + >=2 s of random 2-level data (``LEVEL_WINDOW_S``), 2-level FSK at 1600 Bd,
    +-4.8 kHz deviation -- the oracle should recover the sync at the natural bit order and report
    a 2-level channel near +-4.8 kHz when the payload is re-sliced at 1600 Bd."""
    rng = np.random.default_rng(5)
    fs = 48_000.0
    sps = fs / flex_ref.SYNC_RATE_BD  # 30, exact for a clean synthetic signal
    dev = 4800.0
    preamble = [i % 2 for i in range(64)]
    sync_bits = [(flex_ref.FLEX_SYNC >> (31 - i)) & 1 for i in range(32)]
    payload_bits = int(round(flex_ref.LEVEL_WINDOW_S * 1600.0)) + 200
    payload = rng.integers(0, 2, size=payload_bits).tolist()
    bits = np.array(preamble + sync_bits + payload, dtype=np.uint8)
    inst_freq = np.repeat(np.where(bits == 1, dev, -dev), int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)

    got = flex_ref.decode(x, fs)
    assert got["n_syncs"] >= 1
    assert got["sync_hex"] == "A6C6AAAA"
    assert any(h["order"] == "natural" and h["hamming"] == 0 for h in got["syncs"])
    # the sync should land at bit index len(preamble), give or take the timing search's rounding
    natural_hits = [h["bit"] for h in got["syncs"] if h["order"] == "natural"]
    assert min(abs(b - len(preamble)) for b in natural_hits) <= 1
    lv = got["levels"]["1600bd"]
    assert lv["n_levels"] == 2
    for c in lv["level_centres_hz"]:
        assert abs(abs(c) - dev) < 800.0


def test_flex_reference_oracle_reports_four_level_channel():
    """A synthetic 4-level FSK trace (payload-style) at +-4.9/+-1.6 kHz, sliced at its own 1600 Bd
    symbol clock, should be reported as 4 levels, not 2, by the histogram-peak heuristic."""
    rng = np.random.default_rng(3)
    fs = 48_000.0
    sps = fs / flex_ref.SYNC_RATE_BD
    n_sym = int(round(flex_ref.LEVEL_WINDOW_S * 1600.0)) + 200
    levels = np.array([-4900.0, -1600.0, 1600.0, 4900.0])
    symbols = levels[rng.integers(0, 4, size=n_sym)]
    inst_freq = np.repeat(symbols, int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)
    freq = flex_ref.fm_discriminate(x, fs)

    got = flex_ref.level_count(freq, syncs=[{"bit": 0}], off=0.0, sps_1600=sps)
    lv = got["1600bd"]
    assert lv["n_levels"] == 4
    mags = sorted(abs(c) for c in lv["level_centres_hz"])
    assert abs(mags[0] - 1600.0) < 500.0
    assert abs(mags[-1] - 4900.0) < 500.0


def test_flex_reference_oracle_finds_no_sync_in_noise():
    """A pure-noise instantaneous-frequency trace should not spuriously report a sync (guards
    against the Hamming tolerance being so loose it always fires)."""
    rng = np.random.default_rng(11)
    fs = 48_000.0
    n_bits = 2000
    sps = fs / flex_ref.SYNC_RATE_BD
    bits = rng.integers(0, 2, size=n_bits).astype(np.uint8)
    inst_freq = np.repeat(np.where(bits == 1, 4800.0, -4800.0), int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)
    got = flex_ref.decode(x, fs)
    # random bits should essentially never match a specific 32-bit pattern within Hamming 3
    assert got["n_syncs"] <= 1


def test_p25_reference_oracle_finds_synthetic_sync():
    """A synthetic C4FM frame -- preamble + the 48-bit frame sync ``0x5575F5FF77FF`` sent as a
    2-level (sign) pattern at its own outer C4FM deviation, followed by an NID-length run of
    random 2-level symbols -- should be recovered at the natural bit order with zero Hamming
    distance, at (about) the expected bit position."""
    fs = 48_000.0
    sps = fs / p25_ref.SYNC_RATE_BD  # 10, exact for a clean synthetic signal
    dev = 1800.0  # C4FM outer deviation level
    preamble = [i % 2 for i in range(48)]
    sync_bits = [(p25_ref.P25_SYNC >> (p25_ref.SYNC_BITS - 1 - i)) & 1 for i in range(p25_ref.SYNC_BITS)]
    nid_bits = int(round(0.18 * p25_ref.SYNC_RATE_BD))  # about one NID (64 bits) plus margin
    rng = np.random.default_rng(7)
    payload = rng.integers(0, 2, size=nid_bits).tolist()
    bits = np.array(preamble + sync_bits + payload, dtype=np.uint8)
    inst_freq = np.repeat(np.where(bits == 1, dev, -dev), int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)

    got = p25_ref.decode(x, fs)
    assert got["n_syncs"] >= 1
    assert got["sync_hex"] == "5575F5FF77FF"
    assert any(h["order"] == "natural" and h["hamming"] == 0 for h in got["syncs"])
    natural_hits = [h["bit"] for h in got["syncs"] if h["order"] == "natural"]
    assert min(abs(b - len(preamble)) for b in natural_hits) <= 1


def test_p25_reference_oracle_reports_four_level_payload():
    """A synthetic 4-level C4FM payload trace, sliced at the sync's own 4800 Bd symbol clock,
    should be reported as (up to) 4 levels rather than 2."""
    rng = np.random.default_rng(9)
    fs = 48_000.0
    sps = fs / p25_ref.SYNC_RATE_BD
    n_sym = int(round(p25_ref.LEVEL_WINDOW_S * p25_ref.SYNC_RATE_BD)) + 50
    levels = np.array([-1800.0, -600.0, 600.0, 1800.0])
    symbols = levels[rng.integers(0, 4, size=n_sym)]
    inst_freq = np.repeat(symbols, int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)
    freq = p25_ref.fm_discriminate(x, fs)

    got = p25_ref.level_count(freq, syncs=[{"bit": 0}], off=0.0, sps=sps)
    assert 2 <= got["n_levels"] <= 4
    mags = sorted(abs(c) for c in got["level_centres_hz"])
    assert abs(mags[-1] - 1800.0) < 500.0


def test_p25_reference_oracle_finds_no_sync_in_noise():
    """A pure-noise instantaneous-frequency trace should not spuriously report a sync (guards
    against the Hamming tolerance being so loose it always fires)."""
    rng = np.random.default_rng(13)
    fs = 48_000.0
    n_bits = 2000
    sps = fs / p25_ref.SYNC_RATE_BD
    bits = rng.integers(0, 2, size=n_bits).astype(np.uint8)
    inst_freq = np.repeat(np.where(bits == 1, 1800.0, -1800.0), int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)
    got = p25_ref.decode(x, fs)
    # random bits should essentially never match a specific 48-bit pattern within Hamming 4
    assert got["n_syncs"] <= 1


def test_dmr_reference_oracle_finds_synthetic_sync():
    """A synthetic 4-level 4FSK burst -- random preamble dibits + the 48-bit ``BS_data`` SYNC
    pattern (sent using the full 4-level alphabet, unlike P25's 2-level sync) + a random-dibit
    payload -- should be recovered at the natural bit order with zero Hamming distance."""
    fs = 48_000.0
    sps = fs / dmr_ref.SYNC_RATE_BD  # 10, exact for a clean synthetic signal
    dev_map = {-3: -dmr_ref.OUTER_DEV_HZ, -1: -dmr_ref.INNER_DEV_HZ,
              1: dmr_ref.INNER_DEV_HZ, 3: dmr_ref.OUTER_DEV_HZ}
    rng = np.random.default_rng(3)
    levels = [-3, -1, 1, 3]
    preamble = [levels[rng.integers(0, 4)] for _ in range(30)]
    payload = [levels[rng.integers(0, 4)] for _ in range(60)]
    all_syms = preamble + dmr_ref.SYNC_SYMS["BS_data"] + payload
    inst_freq = np.repeat([dev_map[s] for s in all_syms], int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)

    got = dmr_ref.decode(x, fs)
    assert got["n_syncs"] >= 1
    assert got["syncs"][0]["pattern"] == "BS_data"
    assert got["syncs"][0]["hamming"] == 0
    assert got["syncs_by_pattern"] == {"BS_data": 1}
    # the sync should land at (about) the expected symbol position
    expected_sample = len(preamble) * sps
    assert abs(got["syncs"][0]["sample"] - expected_sample) <= sps


def test_dmr_reference_oracle_finds_no_sync_in_random_symbols():
    """Pure random 4-level symbols (no embedded SYNC pattern) should essentially never corroborate
    a chance alignment across enough timing phases to be reported -- guards the timing-phase
    corroboration threshold against a noise floor that always fires."""
    fs = 48_000.0
    sps = fs / dmr_ref.SYNC_RATE_BD
    dev_map = {-3: -dmr_ref.OUTER_DEV_HZ, -1: -dmr_ref.INNER_DEV_HZ,
              1: dmr_ref.INNER_DEV_HZ, 3: dmr_ref.OUTER_DEV_HZ}
    levels = [-3, -1, 1, 3]
    worst = 0
    for seed in range(10):
        rng = np.random.default_rng(100 + seed)
        n_syms = 5 * 4800
        syms = [levels[rng.integers(0, 4)] for _ in range(n_syms)]
        inst_freq = np.repeat([dev_map[s] for s in syms], int(round(sps)))
        inst_freq = inst_freq + rng.normal(0, 300, size=inst_freq.shape)
        phase = 2 * np.pi * np.cumsum(inst_freq) / fs
        x = np.exp(1j * phase)
        got = dmr_ref.decode(x, fs)
        worst = max(worst, got["n_syncs"])
    # 4-level slicing's timing-phase corroboration is thinner-margin than P25's 2-level sign
    # slice (dmr_ref.MIN_PHASE_CORROBORATION's docstring): an occasional chance corroboration on
    # pure noise is expected, but never more than one in a 5 s clip.
    assert worst <= 1


def test_dmr_reference_oracle_reports_four_level_payload():
    rng = np.random.default_rng(9)
    fs = 48_000.0
    sps = fs / dmr_ref.SYNC_RATE_BD
    n_sym = int(round(dmr_ref.LEVEL_WINDOW_S * dmr_ref.SYNC_RATE_BD)) + 50
    levels = np.array([-dmr_ref.OUTER_DEV_HZ, -dmr_ref.INNER_DEV_HZ, dmr_ref.INNER_DEV_HZ,
                       dmr_ref.OUTER_DEV_HZ])
    symbols = levels[rng.integers(0, 4, size=n_sym)]
    inst_freq = np.repeat(symbols, int(round(sps)))
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)
    freq = dmr_ref.fm_discriminate(x, fs)

    got = dmr_ref.level_count(freq, syncs=[{"bit": 0}], off=0.0, sps=sps)
    assert 2 <= got["n_levels"] <= 4
    mags = sorted(abs(c) for c in got["level_centres_hz"])
    assert abs(mags[-1] - dmr_ref.OUTER_DEV_HZ) < 500.0


def test_ctcss_reference_oracle_measures_known_tone_to_tenth_hz():
    """A synthetic NBFM signal with a 233.6 Hz sub-audible CTCSS tone plus wideband >300 Hz
    "voice" noise: the oracle should measure the tone to within 0.1 Hz and match it to the EIA
    table entry exactly."""
    fs = 48_000.0
    t = np.arange(int(8.0 * fs)) / fs
    rng = np.random.default_rng(5)
    voice = np.convolve(rng.normal(0, 2000, size=t.shape), np.ones(50) / 50, mode="same")
    inst_freq = 500.0 * np.sin(2 * np.pi * 233.6 * t) + voice
    phase = 2 * np.pi * np.cumsum(inst_freq) / fs
    x = np.exp(1j * phase)

    got = ctcss_ref.decode(x, fs)
    assert got["tone_hz_measured"] is not None
    assert abs(got["tone_hz_measured"] - 233.6) < 0.1
    assert got["ctcss"] is not None
    assert got["ctcss"]["label"] == "233.6"
    assert got["tone_snr_db"] > ctcss_ref.MIN_SNR_DB


def test_ctcss_reference_oracle_finds_no_tone_in_noise():
    """Pure sub-audible-band noise (no tone) should not spuriously match a CTCSS table entry --
    the Welch-averaged noise guard this module's docstring explains is needed."""
    fs = 48_000.0
    t = np.arange(int(8.0 * fs)) / fs
    worst_snr = -99.0
    for seed in range(10):
        rng = np.random.default_rng(2000 + seed)
        inst_freq = rng.normal(0, 300, size=t.shape)
        phase = 2 * np.pi * np.cumsum(inst_freq) / fs
        x = np.exp(1j * phase)
        got = ctcss_ref.decode(x, fs)
        assert got["ctcss"] is None
        if got["tone_snr_db"] is not None:
            worst_snr = max(worst_snr, got["tone_snr_db"])
    assert worst_snr < ctcss_ref.MIN_SNR_DB


def test_ctcss_nearest_match_rejects_far_measurements():
    assert ctcss_ref.nearest_ctcss(233.6)["label"] == "233.6"
    assert ctcss_ref.nearest_ctcss(100.0)["delta_hz"] == 0.0
    assert ctcss_ref.nearest_ctcss(300.0) is None  # nowhere near a table entry


def committed_entries():
    manifest = fxlib.load_json(fxlib.MANIFEST)
    return manifest, [e for e in manifest["entries"] if e["status"] == "committed"]


def test_committed_manifest_verifies():
    manifest, committed = committed_entries()
    assert manifest["version"] == fxlib.MANIFEST_VERSION
    pointers = [e["path"] for e in committed if fxlib.is_lfs_pointer(fxlib.FIXTURES / e["path"])]
    if pointers:
        pytest.skip(f"Git LFS objects not pulled: {pointers[:2]}")
    problems, _ = verify.verify(fxlib.MANIFEST)
    assert problems == []


def test_committed_hackrf_fixtures_carry_provenance_and_truth():
    _, committed = committed_entries()
    metas = [fxlib.FIXTURES / e["path"] for e in committed if e.get("kind") == "sigmf-meta"]
    for path in metas:
        meta = sigmf.read_meta(path)
        g = meta["global"]
        assert g["core:license"] == "project-owned capture"
        assert any(uc in g["core:description"] for uc in ("SIGNAL-", "AWARE-", "SPACE-"))
        assert all("hackriff:clip_count" in c for c in meta["captures"])
        roles = {a[sigmf.TRUTH_KEY]["role"] for a in meta["annotations"] if sigmf.TRUTH_KEY in a}
        assert "scenario" in roles and roles <= annotate.ROLES
        for a in meta["annotations"]:
            t = a[sigmf.TRUTH_KEY]
            assert "payload_hex" not in t and "bits_hex" not in t
