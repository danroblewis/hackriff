"""The board's own invariants, asserted rather than remembered.

`docs/tasks.yaml` is the single source of truth for task state (CLAUDE.md, "Coordination"), and a
fresh session resumes from it. So a field that is only filled in when someone remembers to fill it in
is a field that will be empty exactly when a new session needs it.

The rule these tests enforce comes from the user, 2026-09-16: **a ticket may not be `blocked` without
recording what it is blocked on.** The motivation was concrete — five tickets were `blocked` and only
one said why, so the board could show *that* work was stalled but not *what would unstall it*.

There is a second-order point in the same instruction, and it is the more useful half: *"if a blocked
ticket has no real blocker, it should be todo/deferred instead, not blocked."* `blocked` is a claim
about the world, not a way of parking something. Requiring a reason makes the claim checkable — it is
hard to write `blocked_on` for a ticket nobody is actually waiting on.

**Stdlib only, deliberately.** `pyyaml` is not a dependency of this project and one is not worth
adding to check a textual invariant of one file — the same reasoning that keeps `hkpy.gate`'s
classifier dependency-free so it can run before anything compiles. The scan below reads the block
structure directly, which is enough for "does this record have that key" and cannot break when an
optional dependency is missing.
"""

from __future__ import annotations

from pathlib import Path

import pytest

TASKS = Path(__file__).resolve().parents[2] / "docs" / "tasks.yaml"


def _records(text: str) -> list[tuple[str, dict[str, str]]]:
    """Every `- id:` block, as (id, {top-level key: first-line value}).

    Only the two-space-indented keys of each task are read; nested mappings and block scalars are
    skipped, which is all this file needs. A block scalar's *marker* (`>-`, `|`) is kept as the value
    so a key's presence is still visible.
    """
    out: list[tuple[str, dict[str, str]]] = []
    cur: dict[str, str] | None = None
    cur_id = ""
    for line in text.splitlines():
        if line.startswith("  - id:"):
            if cur is not None:
                out.append((cur_id, cur))
            cur_id, cur = line.split(":", 1)[1].strip(), {}
            continue
        if cur is None:
            continue
        if line and not line.startswith(" "):  # left the tasks list entirely
            out.append((cur_id, cur))
            cur = None
            continue
        if line.startswith("    ") and not line.startswith("     "):
            key, _, val = line[4:].partition(":")
            if key and not key.startswith(("-", "#")):
                cur[key.strip()] = val.strip()
    if cur is not None:
        out.append((cur_id, cur))
    return out


@pytest.fixture(scope="module")
def records() -> list[tuple[str, dict[str, str]]]:
    recs = _records(TASKS.read_text())
    assert len(recs) > 100, f"parsed only {len(recs)} tasks — the scan is broken, not the board"
    return recs


def test_every_blocked_task_records_what_it_is_blocked_on(records) -> None:
    """A `blocked` ticket names its blocker, so the board can say what would unstall it."""
    missing = [i for i, f in records if f.get("status") == "blocked" and not f.get("blocked_on")]
    assert not missing, (
        f"blocked without a blocked_on: {missing}. Record what each is waiting on — a user "
        "decision, hardware, or another ticket — or set it to todo/deferred if nothing is "
        "actually blocking it."
    )


def test_only_blocked_tasks_carry_a_blocker(records) -> None:
    """A stale `blocked_on` on a running or finished ticket is a lie the board tells quietly.

    This is the direction that rots: a ticket gets unblocked, its status moves on, and the reason it
    *used* to be stuck stays behind looking current.
    """
    stale = [(i, f.get("status")) for i, f in records if f.get("blocked_on") and f.get("status") != "blocked"]
    assert not stale, f"blocked_on left behind after unblocking: {stale}"
