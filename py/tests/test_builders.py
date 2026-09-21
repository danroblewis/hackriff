"""`just builders` (T-559): whether it is safe to launch another Rust-building agent.

Filed because the 4-builder cap in CLAUDE.md counted cargo processes but not the `hk serve`
processes agents leave running (e2e harnesses, demo servers, replay servers) — invisible to
a "count the cargo processes" check, and the thing that turned a 4-agent day into load
129-211 and a 62-minute gate (T-543). Every assertion below is on the **counting rule**,
same shape as `test_gate.py`'s own docstring: a classifier that counts wrong and reports
"safe" anyway is exactly the failure this tool exists to prevent.
"""

from __future__ import annotations

import pytest

from hkpy.builders import (
    CAP,
    DEFAULT_CORES,
    DISK_FLOOR_GB,
    ProcInfo,
    assess,
    classify_process,
    hk_subcommand,
    render,
    server_port,
    worktree_of,
)

MAIN = "/Users/daniellewis/hackriff"
WT_A = f"{MAIN}/.claude/worktrees/t420"
WT_B = f"{MAIN}/.claude/worktrees/t421"
WT_C = f"{MAIN}/.claude/worktrees/t422"
WT_D = f"{MAIN}/.claude/worktrees/t423"


def cargo(pid: int, cwd: str, jobs: int = 1) -> list[ProcInfo]:
    """`jobs` cargo/rustc processes in one worktree — a building agent's `CARGO_BUILD_JOBS=6`

    spawns cargo plus several rustc children, and they must still cost one slot (see the
    `parallel_rustc_children_are_one_slot` test below), so most fixtures use `jobs=1`.
    """
    procs = [ProcInfo(pid=pid, command="cargo build -p hk-plugins --bins", cwd=cwd)]
    for i in range(jobs - 1):
        procs.append(
            ProcInfo(pid=pid + 1 + i, command="rustc --edition 2021 --crate-name hk_plugins", cwd=cwd)
        )
    return procs


def server(pid: int, cwd: str, port: str = "127.0.0.1:8899") -> ProcInfo:
    return ProcInfo(pid=pid, command=f"target/debug/hk serve --bind {port}", cwd=cwd)


def idle_disk(**kw):
    kw.setdefault("disk_free_gb", 100.0)
    kw.setdefault("loadavg1", 2.0)
    return kw


# --------------------------------------------------------------------------- classification


def test_cargo_build_is_classified_as_cargo_build():
    assert classify_process("cargo build -p hk-plugins --bins") == "cargo_build"
    assert classify_process("cargo nextest run -p hk-detect") == "cargo_build"
    assert classify_process("rustc --edition 2021 --crate-name hk_core") == "cargo_build"


def test_hk_serve_direct_invocation_is_hk_server():
    assert classify_process("target/debug/hk serve --bind 127.0.0.1:8899") == "hk_server"
    assert classify_process("/usr/bin/hk replay fixtures/x.sigmf-meta --serve") == "hk_server"
    assert classify_process("target/release/hackriffd --bind 127.0.0.1:9000") == "hk_server"


def test_cargo_run_of_the_hk_binary_is_hk_server_not_also_cargo_build():
    # This is a `cargo` process by argv[0], but what it's DOING is running the served
    # binary. Counting it in both buckets would double-charge one process against the cap.
    assert (
        classify_process("cargo run -p hk-cli --bin hk -- serve --bind 127.0.0.1:0")
        == "hk_server"
    )
    assert classify_process("cargo run -p hackriffd -- --bind 127.0.0.1:9100") == "hk_server"


def test_plain_cargo_build_of_hk_cli_is_not_hk_server():
    # Compiling hk-cli is not running it — must not be mistaken for a live server.
    assert classify_process("cargo build -p hk-cli --bins") == "cargo_build"


def test_unrelated_processes_are_ignored():
    assert classify_process("mds_stores") is None
    assert classify_process("/usr/libexec/syspolicyd") is None
    assert classify_process("node ui/e2e/run.mjs") is None


def test_hk_subcommand_and_port_extraction():
    assert hk_subcommand("target/debug/hk serve --bind 127.0.0.1:8899") == "serve"
    assert server_port("target/debug/hk serve --bind 127.0.0.1:8899") == "8899"
    assert hk_subcommand("cargo run -p hk-cli --bin hk -- serve --bind 127.0.0.1:0") == "serve"
    assert server_port("cargo run -p hk-cli --bin hk -- serve --bind 127.0.0.1:0") == "0"
    assert server_port("target/debug/hk replay fixtures/x.sigmf-meta") is None


# --------------------------------------------------------------------------- worktree grouping


def test_worktree_of_from_cwd():
    assert worktree_of("cargo build", WT_A) == "worktree:t420"


def test_worktree_of_falls_back_to_command_when_cwd_missing():
    cmd = f"cargo build --manifest-path {WT_B}/Cargo.toml"
    assert worktree_of(cmd, None) == "worktree:t421"


def test_worktree_of_defaults_to_main_not_unknown():
    # The coordinator's own checkout has no .claude/worktrees/ path in it at all — this is
    # exactly the "coordinator's full gate" case the acceptance criteria names.
    assert worktree_of("cargo build -p hk-plugins --bins", MAIN) == "main"
    assert worktree_of("cargo build -p hk-plugins --bins", None) == "main"


# --------------------------------------------------------------------------- the cap itself


def test_under_the_cap_is_safe():
    procs = cargo(1, WT_A) + cargo(10, WT_B)
    a = assess(processes=procs, **idle_disk())
    assert a.builder_count == 2
    assert a.safe
    assert "safe to launch" in a.verdict
    assert not a.reasons


def test_parallel_rustc_children_are_one_slot():
    # CARGO_BUILD_JOBS=6: one cargo parent plus five rustc children in the SAME worktree
    # must cost exactly one slot, or the cap would trip on a single agent's own parallelism.
    procs = cargo(1, WT_A, jobs=6)
    a = assess(processes=procs, **idle_disk())
    assert a.cargo_by_worktree == {"worktree:t420": 6}
    assert a.builder_count == 1
    assert a.safe


def test_at_the_cap_is_not_safe():
    procs = cargo(1, WT_A) + cargo(10, WT_B) + cargo(20, WT_C) + cargo(30, WT_D)
    a = assess(processes=procs, **idle_disk(), cap=CAP)
    assert a.builder_count == CAP == 4
    assert not a.safe
    assert any("at or over the cap" in r for r in a.reasons)
    assert "NOT safe" in a.verdict


def test_one_below_the_cap_is_still_safe():
    procs = cargo(1, WT_A) + cargo(10, WT_B) + cargo(20, WT_C)
    a = assess(processes=procs, **idle_disk(), cap=CAP)
    assert a.builder_count == CAP - 1
    assert a.safe


def test_hk_serve_processes_push_it_over_the_cap():
    # Two worktrees building (2 slots) plus THREE bare `hk serve` processes with no cargo
    # activity of their own (an e2e harness, a demo server, a replay server) — a
    # count-the-cargo-processes check would see 2 and call it safe. This is the T-543 defect.
    procs = (
        cargo(1, WT_A)
        + cargo(10, WT_B)
        + [
            server(100, WT_C, port="127.0.0.1:8899"),
            server(101, MAIN, port="127.0.0.1:8900"),
            server(102, WT_C, port="127.0.0.1:8901"),
        ]
    )
    a = assess(processes=procs, **idle_disk())
    assert len(a.cargo_by_worktree) == 2
    assert len(a.servers) == 3
    assert a.builder_count == 5
    assert not a.safe
    assert any("3 hk serve/run process" in r for r in a.reasons)


def test_hk_serve_alone_with_no_cargo_activity_still_counts():
    # The exact scenario named in the ticket: "an hk serve counts toward the cap" even when
    # nothing is compiling in that worktree.
    procs = [server(1, WT_A)]
    a = assess(processes=procs, **idle_disk())
    assert a.builder_count == 1
    assert a.cargo_by_worktree == {}
    assert len(a.servers) == 1


def test_coordinators_own_full_gate_counts_as_one_builder():
    # `just gate` from the main checkout spawns cargo plus CARGO_BUILD_JOBS=6 rustc
    # children, all with cwd == the main checkout, not a worktree. CLAUDE.md: "the
    # coordinator's full check counts as one [builder]".
    coordinator_gate = cargo(1, MAIN, jobs=6)
    three_agents = cargo(50, WT_A) + cargo(60, WT_B) + cargo(70, WT_C)
    a = assess(processes=coordinator_gate + three_agents, **idle_disk())
    assert a.cargo_by_worktree["main"] == 6
    assert a.builder_count == 4  # main (1) + three worktrees (3) == at the cap
    assert not a.safe


def test_unrelated_processes_do_not_affect_the_count():
    procs = cargo(1, WT_A) + [
        ProcInfo(pid=999, command="mds_stores", cwd="/"),
        ProcInfo(pid=998, command="/usr/libexec/syspolicyd", cwd="/"),
    ]
    a = assess(processes=procs, **idle_disk())
    assert a.builder_count == 1
    assert a.safe


# --------------------------------------------------------------------------- disk and load


def test_low_disk_is_not_safe_even_with_no_builders():
    a = assess(processes=[], loadavg1=1.0, disk_free_gb=12.0)
    assert not a.safe
    assert any("disk free" in r for r in a.reasons)
    assert a.builder_count == 0


def test_disk_exactly_at_the_floor_is_safe():
    a = assess(processes=[], loadavg1=1.0, disk_free_gb=DISK_FLOOR_GB)
    assert a.safe


def test_disk_just_under_the_floor_is_not_safe():
    a = assess(processes=[], loadavg1=1.0, disk_free_gb=DISK_FLOOR_GB - 0.1)
    assert not a.safe


def test_high_load_is_not_safe_even_with_no_builders():
    # T-543's own measurement: load average 129-211 on a 28-core box.
    a = assess(processes=[], loadavg1=150.0, disk_free_gb=100.0)
    assert not a.safe
    assert any("exceeds" in r for r in a.reasons)
    assert a.builder_count == 0


def test_load_at_the_core_budget_is_safe():
    a = assess(processes=[], loadavg1=float(DEFAULT_CORES), disk_free_gb=100.0)
    assert a.safe


def test_load_just_over_the_core_budget_is_not_safe():
    a = assess(processes=[], loadavg1=DEFAULT_CORES + 0.1, disk_free_gb=100.0)
    assert not a.safe


def test_custom_cores_budget_is_respected():
    a = assess(processes=[], loadavg1=10.0, disk_free_gb=100.0, cores=8)
    assert not a.safe
    a2 = assess(processes=[], loadavg1=10.0, disk_free_gb=100.0, cores=16)
    assert a2.safe


def test_multiple_reasons_are_all_reported():
    procs = cargo(1, WT_A) + cargo(10, WT_B) + cargo(20, WT_C) + cargo(30, WT_D)
    a = assess(processes=procs, loadavg1=200.0, disk_free_gb=5.0)
    assert not a.safe
    assert len(a.reasons) == 3


# --------------------------------------------------------------------------- rendering


def test_render_includes_worktrees_servers_load_disk_and_verdict():
    procs = cargo(1, WT_A) + [server(100, WT_B, port="127.0.0.1:8899")]
    a = assess(processes=procs, **idle_disk())
    lines = render(a)
    text = "\n".join(lines)
    assert "worktree:t420" in text
    assert "worktree:t421" in text
    assert "8899" in text
    assert "verdict" in text
    assert a.verdict in text


def test_render_says_none_running_when_empty():
    a = assess(processes=[], **idle_disk())
    lines = render(a)
    text = "\n".join(lines)
    assert "cargo/rustc: none running" in text
    assert "hk serve/run processes: none running" in text


@pytest.mark.parametrize("cap", [1, 4, 10])
def test_cap_is_configurable_and_boundary_is_inclusive(cap):
    procs = []
    for i in range(cap):
        procs += cargo(i * 10 + 1, f"{MAIN}/.claude/worktrees/w{i}")
    a = assess(processes=procs, **idle_disk(), cap=cap)
    assert a.builder_count == cap
    assert not a.safe
