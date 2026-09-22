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

def test_high_effort_opus_or_fable_tickets_justify_it(records) -> None:
    """The symmetric check to the one above: over-tiering, not just under-tiering (T-563).

    The existing test stops a core_interface ticket going to a cheap model. Nothing stopped the
    opposite mistake, which is the one the /perf measurement actually blamed: a ticket landing on
    Opus at high/xhigh effort by habit rather than by the rubric in `prompts/model-selection.md`.
    Measured on 2026-09-21: 5 of 86 open tickets were `model: opus`, `effort: high|xhigh`, with
    `core_interface` not `true` and no stated reason - T-216, T-275, T-276, T-277, T-278. Three
    matched the rubric's own Sonnet/medium patterns ("bulk mapping and classification work with a
    fixed rubric", "wrapping existing decoders as plugins") and were retiered; one turned out to
    genuinely touch the C04 attention scheduler and was correctly reflagged core_interface: true;
    one (Jetson/TensorRT, blocked on hardware) got an explicit `high_effort_reason` because it is
    real GPU-provider design work, not a bounded change.

    So the rule: an open, non-cheap-model-exempt ticket at `effort: high` or `xhigh` on `opus` or
    `fable` must carry either `core_interface: true` or a non-empty reason field explaining why
    (`high_effort_reason:`, or any `*_reason:`/`retiered_*:` key recording the same judgment call).
    This is deliberately a presence check, not a correctness check - it cannot tell a good reason
    from a bad one, only that someone stated one instead of defaulting to the expensive tier. That
    is enough to make the choice reviewable, which is what CLAUDE.md's tiering conventions ask for.

    Checked against the whole open board before landing: this flagged exactly the 5 tickets above
    and nothing else, so it isn't a rule that would fail on legitimate work - the standard T-563
    itself sets ("if a rule would fail on many existing legitimate tickets, that rule is wrong").
    """
    expensive = {"opus", "fable"}
    for tid, rec in records:
        if rec.get("status") in {"done", "cancelled", "deferred", "reverted"}:
            continue
        if rec.get("model") not in expensive:
            continue
        if rec.get("effort") not in {"high", "xhigh"}:
            continue
        if rec.get("core_interface") == "true":
            continue
        has_reason = any(
            key.endswith("_reason") or key.startswith("retiered_") for key in rec
        )
        assert has_reason, (
            f"{tid} is {rec.get('model')}/{rec.get('effort')} but is not core_interface and states "
            "no reason (a *_reason or retiered_* field); either justify the tier explicitly or "
            "drop to the rubric's default in prompts/model-selection.md"
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

def test_no_ticket_carries_another_tickets_body() -> None:
    """Misattribution is the corruption a body-presence check cannot see.

    The 2026-09-21 marker-stripping damage had two shapes. One truncated a block, which
    `test_no_ticket_lost_its_body_to_a_merge` catches. The other SWAPPED bodies between adjacent
    blocks — T-598 ended up carrying T-595's acceptance verbatim, and T-581 carried T-583's. Every
    block still had an acceptance, so that guard passed while the board was wrong: the ticket said
    one thing in its title and another in its body, and only a person reading both noticed.

    Only PROVENANCE-prefixed bodies are compared. "FOUND BY T-321. …" names one specific discovery
    and cannot legitimately sit under two ids. Definition-of-done boilerplate is shared across whole
    families of tickets on purpose and is not evidence of anything.

    Reads the raw file rather than the shared scanner: the scanner keeps only a block scalar's
    marker, and a fingerprint built from it proved too easy to mis-attribute across blocks.
    """
    import re

    text = TASKS.read_text()
    blocks = re.split(r"^  - id: ", text, flags=re.M)[1:]
    first_lines: dict[str, list[str]] = {}
    for blk in blocks:
        tid = blk.split("\n", 1)[0].strip()
        m = re.search(r"^    (?:acceptance|notes): \|.*\n(\s+.*)$", blk, re.M)
        if not m:
            continue
        line = m.group(1).strip()
        if line.startswith(("FOUND BY", "MEASURED BY", "FILED BY", "SURFACED BY", "USER FIELD")):
            first_lines.setdefault(line, []).append(tid)
    shared = {b: ids for b, ids in first_lines.items() if len(ids) > 1}
    assert not shared, (
        "these tickets open their body with the same provenance sentence, so at least one is "
        "carrying another ticket's text: "
        + "; ".join(f"{ids} -> {b[:70]!r}" for b, ids in shared.items())
    )

def test_no_ticket_block_carries_a_key_twice() -> None:
    """A lost `- id:` line merges two tickets into one, and both other guards miss it.

    The third corruption shape from 2026-09-21. `test_no_ticket_lost_its_body_to_a_merge` passes
    because the merged block HAS a body; `test_no_ticket_carries_another_tickets_body` passes
    because the first provenance line is unique. What gives it away is that the swallowed ticket's
    keys are now a second copy inside its neighbour: two `acceptance:` keys, two `use_cases:`.

    YAML itself will not object - a duplicate key silently keeps the last value - so the swallowed
    ticket's body wins and the host ticket's is discarded on load. That is how T-596's block came
    to end with T-594's text: everything the host declared before the duplicate was live in the
    file and dead in the parse.
    """
    import re
    from collections import Counter

    text = TASKS.read_text()
    blocks = re.split(r"^  - id: ", text, flags=re.M)[1:]
    offenders: list[str] = []
    for blk in blocks:
        tid = blk.split("\n", 1)[0].strip()
        keys = Counter(re.findall(r"^    ([a-z_]+):", blk, re.M))
        dupes = sorted(k for k, n in keys.items() if n > 1)
        if dupes:
            offenders.append(f"{tid}: {dupes}")
    assert not offenders, (
        "these blocks declare a key more than once, which is what a lost `- id:` line looks like "
        "(the second ticket's keys land inside the first): " + "; ".join(offenders)
    )


def test_status_is_from_the_boards_own_vocabulary(records) -> None:
    """An off-vocabulary status hides a ticket from the tool that exists to find it.

    `hkpy.reconcile` selects in-progress work by matching the string `in-progress` exactly. So a
    ticket written `in_progress` — one underscore — is not merely cosmetically wrong: it is
    INVISIBLE to the check whose entire job is catching stale in-progress claims. That happened
    (T-690, 2026-09-21), and it happened in the ticket that was itself blocking eight merges.
    Two more spellings of the same idea had accumulated unnoticed: `in-review` and `review`.

    This is the board's version of a rule the codebase keeps restating: a value that is *usually*
    from a small set is not checked against that set anywhere, so a typo degrades silently instead
    of failing. Same family as `commit:` parsing as a YAML number, and as a nextest override whose
    filter names a package no run can see — the config is wrong and nothing says so.

    `reverted` is deliberately IN the vocabulary: it is a real, distinct outcome (T-484), not a
    misspelling. The point of the guard is that adding a state must be a decision taken here,
    not something that arrives by typo.
    """
    allowed = {"todo", "in-progress", "blocked", "done", "deferred", "cancelled", "reverted"}
    bad = [(i, f["status"]) for i, f in records if f.get("status") not in allowed]
    assert not bad, (
        "these tickets carry a status outside the board's vocabulary "
        f"({sorted(allowed)}):\n  "
        + "\n  ".join(f"{i}: {s!r}" for i, s in bad)
        + "\nA status reconcile does not recognise makes the ticket invisible to it."
    )


def test_the_board_parses_as_strict_yaml_and_ids_are_unique() -> None:
    """The board must load under the SAME strict parser its consumers use.

    Every other guard in this file is textual. `ops/monitor.py` (the task graph and the "Up next"
    queue) and any future tool that reaches for `yaml.safe_load` reject the whole file on ANY
    strict-YAML fault - an unquoted `: ` inside a scalar (T-640's title, 2026-09-22, which blanked
    the dashboard while every textual guard passed), a tab, a bad indent, an unquoted `#` or `%`,
    a stray `- `. So parse it for real, with the loader the dashboard uses. Fault-injected: T-640's
    original title, a tab indent and a duplicate id are each caught.

    While the document is loaded, check the one property a text scan cannot see across a merge:
    ids are unique. Two sessions allocating `max+1` on different branches (2026-09-22: T-761..T-764
    filed on two branches at once) merge textually clean and only collide as a parsed document.

    `pyyaml` (MIT) is a declared dev dependency of `py/` for exactly this; run under `uv`.
    """
    yaml = pytest.importorskip("yaml", reason="pyyaml is a declared dev dependency of py/; run under uv")
    try:
        doc = yaml.safe_load(TASKS.read_text())
    except yaml.YAMLError as e:  # pragma: no cover - the message IS the test output
        pytest.fail(f"docs/tasks.yaml is not strict YAML; the dashboard cannot load it:\n{e}")
    tasks = doc.get("tasks") or []
    assert len(tasks) > 100, f"strict parse found only {len(tasks)} tasks"
    ids = [str(t.get("id")) for t in tasks]
    dupes = sorted({i for i in ids if ids.count(i) > 1})
    assert not dupes, f"duplicate ticket ids after a merge: {dupes} - renumber one side (never reuse an id)"


def test_no_scalar_field_reads_as_a_nested_mapping(records) -> None:
    """An unquoted scalar containing ``": "`` is a MAPPING to a YAML loader, not a string.

    T-640's title contained ``sources[iq-ring].available: true``. Unquoted, `yaml.safe_load`
    reads that as a nested key and rejects the document: "mapping values are not allowed here".
    The board still worked everywhere that parses it leniently — this file's own guards and
    `hkpy.reconcile` both tolerated it — so the break reached `main` invisibly and surfaced only
    as a dashboard render error, which is the worst way to find out.

    That is the gap this closes: **every other guard here is textual, so a file that strict YAML
    rejects could still pass all of them.** A checker that cannot see the failure mode it exists
    to prevent is the same shape as a nextest override naming a package no run can see, and as a
    staged-bulk marker written where its only reader never looks.

    Deliberately stdlib, like the rest of this file: `pyyaml` is not a dependency of `py/`
    (`hkpy.boardmerge` says so explicitly), and adding one to run a linter would be a heavier
    answer than the defect deserves. This catches the specific construct that broke it — a
    plain scalar whose value contains a colon-space — which is the only way this has ever failed.
    """
    bad = [
        (i, k, v)
        for i, f in records
        for k, v in f.items()
        if isinstance(v, str)
        and v[:1] not in ("'", '"', "|", ">", "[")
        and ": " in v
    ]
    assert not bad, (
        "these values contain ': ' and are not quoted, so a YAML loader reads them as a nested "
        "mapping and rejects the file:\n  "
        + "\n  ".join(f"{i}: {k}: {v[:80]}" for i, k, v in bad)
        + "\nQuote the value."
    )
