"""T-631: the nextest overrides and the justfile's nextest scope must agree.

Two kinds of test here, and the second is the one that matters. The first asserts the real
tree is consistent *now*; on its own that is a test that passes for as long as nobody
changes anything, and it would have passed happily on the tree that shipped the inert
`package(hk-e2e)` override if the checker were wrong. So the rest of the file drives the
checker against synthetic trees, including a faithful replica of the T-631 defect, and
asserts it goes RED for each.
"""

from __future__ import annotations

from pathlib import Path

import pytest

from hkpy.nextest_config import REPO, check, nextest_scopes, overrides, workspace_members

CARGO = """
[workspace]
resolver = "3"
members = ["crates/*", "tests/e2e"]
"""


def tree(tmp_path: Path, justfile: str, config: str, packages=("hk-dsp", "hk-e2e")) -> Path:
    (tmp_path / "Cargo.toml").write_text(CARGO)
    (tmp_path / ".config").mkdir()
    (tmp_path / ".config" / "nextest.toml").write_text(config)
    (tmp_path / "justfile").write_text(justfile)
    for name in packages:
        d = tmp_path / ("tests/e2e" if name == "hk-e2e" else f"crates/{name}")
        d.mkdir(parents=True)
        (d / "Cargo.toml").write_text(f'[package]\nname = "{name}"\n')
    return tmp_path


WORKSPACE_JUST = """
test-rust:
    #!/usr/bin/env bash
    scope=$(just _crate-scope)
    cargo nextest run $scope
"""

EXCLUDING_JUST = """
test-rust:
    #!/usr/bin/env bash
    scope=$(just _crate-scope hk-e2e)
    cargo nextest run $scope

test-crate crate:
    cargo nextest run -p {{crate}}
"""

SERIAL_OVERRIDE = """
[test-groups.heavy-serial]
max-threads = 1

[[profile.default.overrides]]
filter = 'package(hk-e2e)'
test-group = 'heavy-serial'
"""


# --------------------------------------------------------------------------- the real tree


def test_the_repository_has_no_inert_nextest_override():
    report = check(REPO)
    assert report.ok, "\n".join(report.problems)


def test_the_repository_still_has_a_gate_nextest_run_to_be_measured_against():
    """A tree with no unparameterised nextest run would pass vacuously; assert it is not one."""
    real = [s for s in nextest_scopes(REPO / "justfile") if not s.parameterised]
    assert real, "no unparameterised `cargo nextest run` in the justfile"


def test_workspace_members_are_read_from_the_manifests():
    members = workspace_members(REPO)
    assert {"hk-e2e", "hk-pipeline", "hk-api", "hk-cli"} <= members


# --------------------------------------------------------------------------- the defect


def test_the_t631_defect_is_caught(tmp_path):
    """The exact shape that shipped: a serial group pinned to a package every run excludes."""
    report = check(tree(tmp_path, EXCLUDING_JUST, SERIAL_OVERRIDE))
    assert not report.ok
    assert any("hk-e2e" in p and "INERT" in p for p in report.problems), report.problems


def test_running_the_package_under_nextest_resolves_it(tmp_path):
    """Answer (a): the same override, with a run that can see the package."""
    report = check(tree(tmp_path, EXCLUDING_JUST + "\nacceptance:\n    cargo nextest run -p hk-e2e\n", SERIAL_OVERRIDE))
    assert report.ok, report.problems


def test_deleting_the_override_resolves_it(tmp_path):
    """Answer (b): no override names the excluded package."""
    report = check(tree(tmp_path, EXCLUDING_JUST, "[test-groups.heavy-serial]\nmax-threads = 1\n"))
    assert report.ok, report.problems


def test_a_parameterised_run_is_not_evidence(tmp_path):
    """`just test-crate <crate>` could name any package, so it proves nothing.

    This is the trap: counting it would have made the check pass on the broken tree.
    """
    just = "test-crate crate:\n    cargo nextest run -p {{crate}}\n"
    report = check(tree(tmp_path, just, SERIAL_OVERRIDE))
    assert not report.ok
    assert any("no unparameterised" in p for p in report.problems), report.problems


def test_a_package_that_does_not_exist_is_caught(tmp_path):
    """The same defect by typo: a filter that can never match a test."""
    config = SERIAL_OVERRIDE.replace("package(hk-e2e)", "package(hk-e2ee)")
    report = check(tree(tmp_path, WORKSPACE_JUST, config))
    assert not report.ok
    assert any("no workspace member" in p for p in report.problems), report.problems


def test_a_workspace_run_sees_every_member(tmp_path):
    report = check(tree(tmp_path, WORKSPACE_JUST, SERIAL_OVERRIDE))
    assert report.ok, report.problems


# --------------------------------------------------------------------------- parsing


def test_overrides_reports_every_package_token():
    named = overrides(REPO / ".config" / "nextest.toml")
    assert "hk-pipeline" in named
    # Multi-package filters (`package(hk-api) or package(hk-cli)`) contribute both.
    assert {"hk-api", "hk-cli"} <= set(named)


def test_a_commented_out_command_is_not_a_run(tmp_path):
    just = "# cargo nextest run -p hk-e2e\n" + EXCLUDING_JUST
    report = check(tree(tmp_path, just, SERIAL_OVERRIDE))
    assert not report.ok, "a comment mentioning the command must not count as running it"


@pytest.mark.parametrize("selector", ["cargo nextest run", "cargo nextest run --no-fail-fast"])
def test_an_unrecognised_selector_fails_closed(tmp_path, selector):
    """No package selector means no proof: the check must not treat it as full coverage."""
    report = check(tree(tmp_path, f"t:\n    {selector}\n", SERIAL_OVERRIDE))
    assert not report.ok
