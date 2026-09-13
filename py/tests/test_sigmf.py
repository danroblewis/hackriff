import json
import re
from pathlib import Path

import pytest

from hkpy import sigmf

REPO = Path(__file__).resolve().parents[2]


def _prov(**overrides):
    args = dict(
        center_hz=433.92e6,
        sample_rate_hz=2e6,
        lna_db=32,
        vga_db=20,
        amp_on=False,
        bandwidth_hz=1.75e6,
        antenna_port="A1",
    )
    args.update(overrides)
    return sigmf.provenance("hackrf:test", **args)


def test_round_trip_with_extension_and_unknown_keys(tmp_path):
    meta = sigmf.new_meta("ci8", 2e6, hw="HackRF One", provenance=_prov())
    meta["global"]["antenna:model"] = "whip"
    sigmf.add_capture(
        meta,
        0,
        frequency=433.92e6,
        datetime="2026-09-13T12:00:00Z",
        provenance=_prov(overload=True, quantisation_limited=True),
        clip_count=12,
        extra={"core:global_index": 3},
    )
    sigmf.add_annotation(
        meta,
        4000,
        sample_count=2000,
        freq_lower_edge=433.90e6,
        freq_upper_edge=433.94e6,
        label="fsk-burst",
        truth={"symbol_rate": 4800, "modulation": "2fsk"},
    )
    sigmf.add_annotation(meta, 100, sample_count=10, label="earlier")

    path = tmp_path / "burst.sigmf-meta"
    sigmf.write_meta(meta, path)
    back = sigmf.read_meta(path)

    assert back["global"]["core:extensions"][0]["name"] == "hackriff"
    assert back["global"]["antenna:model"] == "whip"
    assert back["global"][sigmf.PROVENANCE_KEY]["tune"]["lna_db"] == 32.0
    assert back["captures"][0]["core:global_index"] == 3
    assert back["captures"][0][sigmf.PROVENANCE_KEY]["overload"] is True
    assert back["captures"][0][sigmf.PROVENANCE_KEY]["quantisation_limited"] is True
    assert back["captures"][0][sigmf.CLIP_COUNT_KEY] == 12
    assert "clip_count" not in back["captures"][0][sigmf.PROVENANCE_KEY]
    # Written sorted by sample_start; the caller's dict is not reordered.
    assert [a["core:label"] for a in back["annotations"]] == ["earlier", "fsk-burst"]
    assert meta["annotations"][0]["core:label"] == "fsk-burst"
    assert back["annotations"][1][sigmf.TRUTH_KEY]["symbol_rate"] == 4800
    assert "temperature_c" not in back["global"][sigmf.PROVENANCE_KEY]


@pytest.mark.parametrize(
    "mutate, message",
    [
        (lambda m: m["global"].update({"core:datatype": "i8"}), "datatype"),
        (lambda m: m["global"].pop("core:version"), "core:version"),
        (lambda m: m["captures"].append({"core:frequency": 1.0}), "sample_start"),
        (lambda m: sigmf.add_capture(m, 0, clip_count=-1), "clip_count"),
        (lambda m: m["global"][sigmf.PROVENANCE_KEY].pop("quantisation_limited"), "quantisation_limited"),
        (lambda m: m["global"][sigmf.PROVENANCE_KEY].pop("tune"), "tune"),
        (lambda m: m["global"][sigmf.PROVENANCE_KEY].update({"clock_source": "tcxo"}), "clock_source"),
        (
            lambda m: m["global"][sigmf.PROVENANCE_KEY].update({"timestamp_method": "host_arrival"}),
            "timestamp_method",
        ),
    ],
)
def test_validate_rejects(mutate, message):
    meta = sigmf.new_meta("ci8", 2e6, provenance=_prov())
    mutate(meta)
    with pytest.raises(sigmf.SigmfError, match=message):
        sigmf.validate(meta)


def test_bytes_per_sample_and_data_path():
    assert sigmf.bytes_per_sample("ci8") == 2
    assert sigmf.bytes_per_sample("ri16_le") == 2
    assert sigmf.bytes_per_sample("cf32_le") == 8
    assert sigmf.data_path("a/b/tone.sigmf-meta") == Path("a/b/tone.sigmf-data")


def test_datatypes_match_rust():
    rust = (REPO / "crates/hk-model/src/sigmf.rs").read_text(encoding="utf-8")
    rust_types = set(re.findall(r'=> "(\w+)", \d+, (?:true|false);', rust))
    assert rust_types == set(sigmf.DATATYPES)


def test_provenance_enums_match_rust():
    time_rs = (REPO / "crates/hk-model/src/time.rs").read_text(encoding="utf-8")
    prov_rs = (REPO / "crates/hk-model/src/provenance.rs").read_text(encoding="utf-8")

    def kebab_variants(src, enum):
        body = re.search(r"pub enum " + enum + r" \{(.*?)\n\}", src, re.S).group(1)
        names = re.findall(r"^\s+([A-Z]\w*),", body, re.M)
        return {re.sub(r"(?<!^)([A-Z])", r"-\1", n).lower() for n in names}

    assert kebab_variants(time_rs, "TimestampMethod") == sigmf.TIMESTAMP_METHODS
    assert kebab_variants(prov_rs, "ClockSource") == sigmf.CLOCK_SOURCES


def test_committed_tiny_fixture_is_valid():
    meta_path = REPO / "fixtures/tiny/tone.sigmf-meta"
    meta = sigmf.read_meta(meta_path)
    assert meta["global"][sigmf.PROVENANCE_KEY]["timestamp_method"] == "synthetic"
    assert meta["annotations"][0][sigmf.TRUTH_KEY]["kind"] == "cw"
    data = sigmf.data_path(meta_path).read_bytes()
    if data.startswith(b"version https://git-lfs"):
        pytest.skip("fixture data is an unfetched Git LFS pointer")
    assert len(data) == meta["annotations"][0]["core:sample_count"] * 2
    # The document is plain JSON that the Rust side parses too (hk-cli test).
    json.loads(meta_path.read_text(encoding="utf-8"))
