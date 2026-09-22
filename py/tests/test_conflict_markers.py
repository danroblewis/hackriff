"""The conflict-marker checker. T-843."""

from __future__ import annotations

import subprocess
from pathlib import Path

from hkpy.conflictmarkers import offenders

REPO = Path(
    subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
    ).stdout.strip()
)


def test_the_tracked_tree_carries_no_conflict_markers() -> None:
    """The guard itself, over the real tree.

    Found on 2026-09-22: `docs/adr/README.md` had carried `<<<<<<< HEAD` / `>>>>>>> task-t549`
    on `main` since a merge weeks earlier. A `docs/`-only diff runs NO suite, so no gate could
    ever have caught it - the same shape as the board's YAML break, and as a nextest override
    naming a package no run can see.
    """
    bad = offenders(REPO)
    assert not bad, "unresolved conflict markers:\n  " + "\n  ".join(
        f"{p}:{n}: {line}" for p, n, line in bad
    )


def test_a_marker_is_detected_wherever_it_is(tmp_path) -> None:
    """Non-vacuity: the checker must FIND one, not merely fail to find any."""
    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
    f = tmp_path / "doc.md"
    f.write_text("fine\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> branch\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "doc.md"], check=True)
    bad = offenders(tmp_path)
    assert [p for p, _, _ in bad] == ["doc.md", "doc.md"]


def test_a_markdown_underline_is_not_a_conflict(tmp_path) -> None:
    """`=======` alone underlines a heading in markdown and rst. It must not trip the guard."""
    subprocess.run(["git", "init", "-q", str(tmp_path)], check=True)
    (tmp_path / "doc.md").write_text("A heading\n=========\n\nbody\n=======\n")
    subprocess.run(["git", "-C", str(tmp_path), "add", "doc.md"], check=True)
    assert offenders(tmp_path) == []
