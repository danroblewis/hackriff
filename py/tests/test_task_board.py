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


def test_a_commit_field_is_never_silently_a_number(records) -> None:
    """A short SHA of all digits is a YAML *number*, and a leading zero makes it octal.

    `commit: 0567143` parses as octal 192099 — the SHA is intact in the file and wrong in every
    tool that reads it through a YAML loader. `commit: 5684346` is luckier: it becomes an int
    whose digits happen to round-trip, so it reads correctly until someone compares types. Five
    entries were affected before this test existed, and the leading-zero one had genuinely lost
    its value.

    This is the board's own instance of the rule the codebase keeps rediscovering: a field that is
    *usually* a string is not a string, and nothing says so at the point of use. The fix is to
    quote it; the guard is to refuse the unquoted form.

    Deliberately a *textual* check, like the rest of this file: it reads the raw line, because the
    whole point is that the loader has already destroyed the evidence by the time it has a value.
    """
    unquoted = [
        (i, f["commit"])
        for i, f in records
        if f.get("commit")
        and f["commit"][0].isdigit()
        and f["commit"].strip('"').isdigit()
        and not f["commit"].startswith('"')
    ]
    assert not unquoted, (
        f"commit SHAs that YAML will read as numbers: {unquoted}. Quote them — an all-digit short "
        "SHA becomes an int, and a leading zero becomes octal (0567143 -> 192099)."
    )


def test_effort_is_a_known_tier_and_core_interface_never_goes_to_a_cheap_model(records) -> None:
    """`effort:` records the complexity tier chosen alongside `model:` (T-563).

    Reasoning time was measured at 184 h against 144 h for ALL tool and build time
    on 2026-09-20, because nearly every agent ran Opus at high effort regardless of
    the work. The field is advisory - the Agent tool exposes `model` but no effort
    parameter - so it exists to make the choice reviewable rather than implicit.

    Note what is NOT asserted: core_interface tickets may sit at `medium`, and 69 of
    them do. CLAUDE.md's rule is about the MODEL - core interfaces and the real-time
    path never go to Sonnet or Haiku alone - not about effort. A well-specified
    change to a core interface, whose mechanism a prior ticket already measured, is
    legitimately Opus-at-medium.
    """
    allowed = {"low", "medium", "high", "xhigh"}
    cheap = {"sonnet", "haiku"}
    for tid, rec in records:
        effort = rec.get("effort")
        if effort is not None:
            assert effort in allowed, f"{tid}: unknown effort {effort!r}"
        # The rule governs work about to be HANDED OUT, so it applies to open tickets.
        # One historical exception stands on purpose: T-370 is core_interface and was
        # assigned sonnet; it landed inside the T-502 batch, which measured its premise
        # to be wrong and correctly changed no contract. Rewriting that record would be
        # tidying history rather than learning from it.
        if rec.get("status") in {"done", "cancelled", "deferred"}:
            continue
        if rec.get("core_interface") == "true" and rec.get("model") in cheap:
            raise AssertionError(
                f"{tid} is core_interface but assigned model {rec['model']!r}; "
                "CLAUDE.md forbids core interfaces and the real-time path going to "
                "Sonnet or Haiku alone"
            )

def test_no_ticket_lost_its_body_to_a_merge(records) -> None:
    """A truncated ticket is the failure a YAML parser cannot see.

    On 2026-09-21 seven tickets reached `main` reduced to `id`/`milestone`/`title`: a
    conflict-marker-stripping resolution had eaten the rest of each block wherever a hunk boundary
    fell mid-ticket. The file still PARSED and every other assertion here still passed, so nothing
    noticed until a reconcile happened to print a ticket with no status. T-582's merge driver stops
    the cause; this stops the damage reaching `main` if anything else ever does it.

    A ticket worth filing is worth a reason: `status` is structural and a body (`acceptance` or
    `notes`) is what makes it actionable. A block with neither is not terse, it is damaged.
    """
    thin = [
        tid
        for tid, rec in records
        if rec.get("status") not in ("cancelled", "deferred")
        and not (rec.get("acceptance") or rec.get("notes") or rec.get("dod"))
    ]
    assert not thin, (
        f"{len(thin)} ticket(s) carry neither acceptance nor notes, which is what a merge that ate "
        f"a block looks like: {thin[:10]}"
    )
