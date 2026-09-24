"""The board merge driver automates one case and refuses the rest (T-582).

The refusals are the important half. A driver that guesses wrong on the board is worse than the
nine conflicts it replaces: a malformed board already reached main once because `docs/` ran no
suite, and a union merge of YAML can produce a file that parses and is silently wrong.
"""

from hkpy.boardmerge import merge, split

BASE = """version: 1
tasks:
  - id: T-001
    title: first
    status: done
  - id: T-002
    title: second
    status: todo
notes:
  - a note
"""


def _with(*blocks: str, head: str = "version: 1\ntasks:\n", tail: str = "notes:\n  - a note\n") -> str:
    return head + "".join(blocks) + tail


T1 = "  - id: T-001\n    title: first\n    status: done\n"
T2 = "  - id: T-002\n    title: second\n    status: todo\n"


def _ids(text: str) -> list[str]:
    return split(text)[2]


def test_both_sides_append_is_the_case_it_automates():
    ours = _with(T1, T2, "  - id: T-003\n    title: ours\n    status: todo\n")
    theirs = _with(T1, T2, "  - id: T-004\n    title: theirs\n    status: todo\n")
    out = merge(BASE, ours, theirs)
    assert out is not None
    assert _ids(out) == ["T-001", "T-002", "T-003", "T-004"]


def test_one_side_appends_and_the_other_does_not():
    ours = _with(T1, T2, "  - id: T-003\n    title: ours\n    status: todo\n")
    out = merge(BASE, ours, BASE)
    assert out is not None and _ids(out) == ["T-001", "T-002", "T-003"]


def test_an_untouched_block_survives_byte_for_byte():
    """Blocks move verbatim: formatting, folding and comments are not re-serialised."""
    odd = "  - id: T-002\n    title: second\n    status: todo\n    # a hand-written comment\n"
    base = _with(T1, odd)
    ours = _with(T1, odd, "  - id: T-003\n    title: ours\n    status: todo\n")
    theirs = _with(T1, odd, "  - id: T-004\n    title: theirs\n    status: todo\n")
    out = merge(base, ours, theirs)
    assert out is not None and "# a hand-written comment" in out


def test_a_status_flip_on_one_side_is_taken():
    flipped = "  - id: T-002\n    title: second\n    status: done\n"
    ours = _with(T1, flipped)
    out = merge(BASE, ours, BASE)
    assert out is not None and "status: done" in out.split("T-002")[1]


def test_the_same_ticket_edited_on_both_sides_is_refused():
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: done\n")
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: blocked\n")
    assert merge(BASE, ours, theirs) is None


def test_the_same_ticket_changed_in_DIFFERENT_fields_on_each_side_merges_field_by_field():
    """The work runner's own two writes (2026-09-23): `status`+`branch` on main at dispatch,
    `result:` on the branch at hand-back. Seven landings that day conflicted on exactly this,
    and every resolution a person typed was the union of the two edits."""
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: in-progress\n    branch: task-t002\n")
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n    result: |\n      DONE (work-runner).\n      Tests: just test-crate hk-blocks -> exit 0\n")
    out = merge(BASE, ours, theirs)
    assert out is not None
    block = split(out)[1]["T-002"]
    # A split block ends where the next `- id:` (or the trailer) begins: no trailing newline.
    assert block == ("  - id: T-002\n    title: second\n    status: in-progress\n    branch: task-t002\n"
                     "    result: |\n      DONE (work-runner).\n      Tests: just test-crate hk-blocks -> exit 0"), block


def test_a_multi_line_field_moves_verbatim_through_a_field_merge():
    """A `result: |` and its continuation lines are one field: folding and indentation survive."""
    res = "    result: |\n      line one\n\n      line three, after a blank\n      # and a comment\n"
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: review\n")
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n" + res)
    out = merge(BASE, ours, theirs)
    assert out is not None and res.rstrip("\n") in out and "status: review" in split(out)[1]["T-002"]


def test_the_same_field_changed_two_ways_is_still_refused():
    """Field-level merging does not make a status disagreement disappear."""
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: done\n    branch: x\n")
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: blocked\n    result: r\n")
    assert merge(BASE, ours, theirs) is None


def test_a_field_removed_on_one_side_and_untouched_on_the_other_is_removed():
    base = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n    branch: old\n")
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n")  # branch: dropped
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n    branch: old\n    result: r\n")
    out = merge(base, ours, theirs)
    assert out is not None
    assert split(out)[1]["T-002"] == "  - id: T-002\n    title: second\n    status: todo\n    result: r"


def test_a_field_removed_on_one_side_and_changed_on_the_other_is_refused():
    base = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n    branch: old\n")
    ours = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n")
    theirs = _with(T1, "  - id: T-002\n    title: second\n    status: todo\n    branch: new\n")
    assert merge(base, ours, theirs) is None


def test_a_field_merge_of_a_real_ticket_on_the_real_board_validates():
    """The 2026-09-23 shape against the actual file: a dispatch flip on main, a hand-back result on
    the branch, for the same ticket - and the result must pass the driver's own validation."""
    from pathlib import Path

    from hkpy.boardmerge import _validate

    board = Path(__file__).resolve().parents[2] / "docs" / "tasks.yaml"
    base = board.read_text(encoding="utf-8")
    head, blocks, order, tail = split(base)
    tid = next(t for t in order if "\n    status: todo\n" in blocks[t] and "\n    result:" not in blocks[t])
    flipped = blocks[tid].replace("\n    status: todo\n", "\n    status: in-progress\n    branch: task-x\n", 1)
    resulted = blocks[tid] + "\n    result: |\n      DONE (work-runner, from handback.json).\n      Tests: exit 0"
    join = lambda b: head + "\n" + "\n".join(b[t] for t in order) + "\n" + tail  # noqa: E731
    out = merge(base, join({**blocks, tid: flipped}), join({**blocks, tid: resulted}))
    assert out is not None
    got = split(out)[1][tid]
    assert "status: in-progress" in got and "branch: task-x" in got and "DONE (work-runner" in got
    assert _validate(out, set(order)) is None


def test_a_deleted_ticket_is_refused():
    ours = _with(T1)  # T-002 removed
    theirs = _with(T1, T2, "  - id: T-003\n    title: theirs\n    status: todo\n")
    assert merge(BASE, ours, theirs) is None


def test_a_change_outside_the_task_list_is_refused():
    ours = _with(T1, T2, tail="notes:\n  - a note\n  - ours added a note\n")
    theirs = _with(T1, T2, tail="notes:\n  - a note\n  - theirs added a note\n")
    assert merge(BASE, ours, theirs) is None


def test_a_header_change_is_refused():
    ours = _with(T1, T2, head="version: 2\ntasks:\n")
    theirs = _with(T1, T2, "  - id: T-003\n    title: theirs\n    status: todo\n")
    assert merge(BASE, ours, theirs) is None


def test_both_sides_filing_the_SAME_new_id_is_refused():
    """The id collision that must never be papered over.

    Two branches each picking the next free number is not hypothetical — it happened on
    2026-09-21, when two branches both took T-571..T-573. A union would keep one branch's ticket
    and drop the other's silently, losing work without telling anyone.
    """
    ours = _with(T1, T2, "  - id: T-003\n    title: ours\n    status: todo\n")
    theirs = _with(T1, T2, "  - id: T-003\n    title: theirs\n    status: todo\n")
    assert merge(BASE, ours, theirs) is None


def test_the_same_new_id_with_identical_content_is_fine():
    """The benign case: the same ticket reached both branches (a shared ancestor commit)."""
    same = "  - id: T-003\n    title: same\n    status: todo\n"
    out = merge(BASE, _with(T1, T2, same), _with(T1, T2, same))
    assert out is not None and _ids(out) == ["T-001", "T-002", "T-003"]


def test_validation_refuses_a_result_that_lost_a_ticket():
    from hkpy.boardmerge import _validate

    assert "lost in the merge" in _validate(BASE, {"T-001", "T-002", "T-999"})


def test_validation_refuses_a_merge_that_truncated_the_list():
    """A block bleeding to column 0 ends the task list early, so later tickets vanish."""
    from hkpy.boardmerge import _validate

    bad = _with(T1, "  - id: T-002\n    title: second\nstatus: todo\n")
    assert "lost in the merge" in (_validate(bad, {"T-001", "T-002", "T-003"}) or "")


def test_a_suffixed_id_is_a_block_of_its_own():
    """The real board carries `T-022a`; an id regex that misses it absorbs a whole ticket."""
    ours = _with(T1, T2, "  - id: T-022a\n    title: sub\n    status: done\n")
    assert _ids(ours) == ["T-001", "T-002", "T-022a"]


def test_validation_accepts_the_real_board():
    """The driver must not refuse the file it exists to merge."""
    from pathlib import Path

    from hkpy.boardmerge import _validate

    board = Path(__file__).resolve().parents[2] / "docs" / "tasks.yaml"
    text = board.read_text(encoding="utf-8")
    assert _validate(text, set()) is None
    assert len(split(text)[2]) > 500


def test_a_real_append_to_the_real_board_merges():
    """End to end on the actual file, which is where the nine conflicts happened."""
    from pathlib import Path

    board = Path(__file__).resolve().parents[2] / "docs" / "tasks.yaml"
    base = board.read_text(encoding="utf-8")
    head, blocks, order, tail = split(base)
    ours = head + "\n" + "\n".join(blocks[t] for t in order) + "\n" + \
        "  - id: T-9001\n    title: ours\n    status: todo\n" + tail
    theirs = head + "\n" + "\n".join(blocks[t] for t in order) + "\n" + \
        "  - id: T-9002\n    title: theirs\n    status: todo\n" + tail
    out = merge(base, ours, theirs)
    assert out is not None
    ids = split(out)[2]
    assert ids[-2:] == ["T-9001", "T-9002"] and len(set(ids)) == len(ids)
