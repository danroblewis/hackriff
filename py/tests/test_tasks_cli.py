"""Tests for the task-board CLI (`py/hkpy/tasks.py`).

Most tests operate on a small SYNTHETIC board written to a temp file — never the real
`docs/tasks.yaml` — so a bug here cannot corrupt the real file. A separate group at the bottom
runs read-only commands (`show`, `list --ready`, `validate`) against the real file, to prove the
CLI actually handles it (real quoting, real folded titles, real size).
"""

from __future__ import annotations

from pathlib import Path

import pytest
import yaml

from hkpy import tasks

REPO = Path(__file__).resolve().parents[2]
REAL_BOARD = REPO / "docs" / "tasks.yaml"

FIXTURE = """\
# a tiny synthetic board for testing the task CLI
version: 1
tasks:
  - id: T-001
    milestone: M0
    title: First ticket
    status: done
    deps: []
    model: opus
    effort: medium
    acceptance: |
      does the thing
  - id: T-002
    milestone: M0
    title: Second ticket, depends on the first
    status: todo
    deps:
    - T-001
    model: sonnet
    effort: low
    priority: normal
    parallel_group: A
    acceptance: |
      does another thing
  - id: T-003
    milestone: M1
    title: Third ticket, blocked
    status: blocked
    blocked_on: "user decision"
    deps: []
    model: opus
    effort: high
    notes: |
      first note line
notes:
  - "a top-level note, not a ticket"
"""


@pytest.fixture()
def board(tmp_path: Path) -> Path:
    p = tmp_path / "tasks.yaml"
    p.write_text(FIXTURE, encoding="utf-8")
    return p


def run(argv: list[str]) -> int:
    return tasks.main(argv)


def _text(board: Path) -> str:
    return board.read_text(encoding="utf-8")


# --------------------------------------------------------------------------------------------
# show
# --------------------------------------------------------------------------------------------


def test_show_prints_the_raw_block(board: Path, capsys) -> None:
    assert run(["show", "T-002", "--file", str(board)]) == 0
    out = capsys.readouterr().out
    assert "- id: T-002" in out
    assert "Second ticket" in out


def test_show_unknown_id_fails(board: Path, capsys) -> None:
    assert run(["show", "T-999", "--file", str(board)]) != 0
    assert "no such ticket" in capsys.readouterr().err


# --------------------------------------------------------------------------------------------
# list
# --------------------------------------------------------------------------------------------


def test_list_all(board: Path, capsys) -> None:
    assert run(["list", "--file", str(board)]) == 0
    out = capsys.readouterr().out
    assert "T-001" in out
    assert "T-002" in out
    assert "T-003" in out


def test_list_status_filter(board: Path, capsys) -> None:
    run(["list", "--status", "blocked", "--file", str(board)])
    out = capsys.readouterr().out
    assert "T-003" in out
    assert "T-001" not in out
    assert "T-002" not in out


def test_list_ready_excludes_unmet_deps_and_blocked(board: Path, capsys) -> None:
    # T-001 is done (not todo -> excluded). T-002 is todo with T-001(done) satisfied -> ready.
    # T-003 is blocked -> excluded regardless of deps.
    run(["list", "--ready", "--file", str(board)])
    out = capsys.readouterr().out
    assert "T-002" in out
    assert "T-001" not in out
    assert "T-003" not in out


def test_list_ready_excludes_unmet_dependency(board: Path, capsys) -> None:
    board.write_text(_text(board).replace("status: done", "status: todo", 1), encoding="utf-8")
    run(["list", "--ready", "--file", str(board)])
    out = capsys.readouterr().out
    # T-002 depends on T-001, which is no longer done -> T-002 should drop out of --ready.
    assert "T-002" not in out


def test_list_json(board: Path, capsys) -> None:
    import json

    run(["list", "--json", "--file", str(board)])
    rows = json.loads(capsys.readouterr().out)
    ids = {r["id"] for r in rows}
    assert ids == {"T-001", "T-002", "T-003"}


# --------------------------------------------------------------------------------------------
# set
# --------------------------------------------------------------------------------------------


def test_set_replaces_an_existing_scalar_field(board: Path) -> None:
    assert run(["set", "T-002", "status=in-progress", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["status"] == "in-progress"


def test_set_adds_a_new_field(board: Path) -> None:
    assert run(["set", "T-002", "branch=task-t2", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["branch"] == "task-t2"


def test_set_multiple_fields_in_one_call(board: Path) -> None:
    assert run(["set", "T-002", "status=done", "commit=abc1234", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["status"] == "done"
    assert t2["commit"] == "abc1234"


def test_set_quotes_a_value_that_would_read_as_a_number(board: Path) -> None:
    """An all-digit commit SHA must round-trip as a STRING (test_task_board.py's own worry)."""
    assert run(["set", "T-002", "commit=0567143", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["commit"] == "0567143"
    assert isinstance(t2["commit"], str)


def test_set_quotes_a_value_containing_colon_space(board: Path) -> None:
    assert run(["set", "T-002", "note_field=looks like FM: broadcast", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["note_field"] == "looks like FM: broadcast"


def test_set_refuses_unknown_status(board: Path, capsys) -> None:
    before = _text(board)
    assert run(["set", "T-002", "status=in_review", "--file", str(board)]) != 0
    assert "unknown status" in capsys.readouterr().err
    assert _text(board) == before  # refused before writing anything


def test_set_refuses_blocked_without_blocked_on(board: Path, capsys) -> None:
    before = _text(board)
    assert run(["set", "T-002", "status=blocked", "--file", str(board)]) != 0
    assert "blocked_on" in capsys.readouterr().err
    assert _text(board) == before


def test_set_allows_blocked_with_blocked_on_in_the_same_call(board: Path) -> None:
    rc = run(
        [
            "set",
            "T-002",
            "status=blocked",
            "blocked_on=waiting on hardware",
            "--file",
            str(board),
        ]
    )
    assert rc == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["status"] == "blocked"
    assert t2["blocked_on"] == "waiting on hardware"


def test_set_unknown_ticket_fails(board: Path, capsys) -> None:
    assert run(["set", "T-999", "status=done", "--file", str(board)]) != 0
    assert "no such ticket" in capsys.readouterr().err


def test_set_refuses_to_clobber_a_multiline_field(board: Path, capsys) -> None:
    before = _text(board)
    assert run(["set", "T-001", "acceptance=one line now", "--file", str(board)]) != 0
    assert _text(board) == before


# --------------------------------------------------------------------------------------------
# result / note
# --------------------------------------------------------------------------------------------


def test_result_sets_a_fresh_block(board: Path) -> None:
    assert run(["result", "T-002", "--text", "worked fine.", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["result"].strip() == "worked fine."


def test_result_replaces_an_existing_block(board: Path) -> None:
    run(["result", "T-001", "--text", "first result.", "--file", str(board)])
    run(["result", "T-001", "--text", "second, replacing.", "--file", str(board)])
    doc = yaml.safe_load(_text(board))
    t1 = next(t for t in doc["tasks"] if t["id"] == "T-001")
    assert "first result" not in t1["result"]
    assert "second, replacing." in t1["result"]


def test_note_appends_to_an_existing_block(board: Path) -> None:
    assert run(["note", "T-003", "--text", "second note line", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t3 = next(t for t in doc["tasks"] if t["id"] == "T-003")
    assert "first note line" in t3["notes"]
    assert "second note line" in t3["notes"]


def test_note_creates_the_block_when_absent(board: Path) -> None:
    assert run(["note", "T-002", "--text", "a note", "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert "a note" in t2["notes"]


def test_result_from_a_file(board: Path, tmp_path: Path) -> None:
    report = tmp_path / "report.txt"
    report.write_text("multi\nline\nresult\n", encoding="utf-8")
    assert run(["result", "T-002", "--from", str(report), "--file", str(board)]) == 0
    doc = yaml.safe_load(_text(board))
    t2 = next(t for t in doc["tasks"] if t["id"] == "T-002")
    assert t2["result"].splitlines() == ["multi", "line", "result"]


def test_result_requires_body(board: Path, capsys) -> None:
    assert run(["result", "T-002", "--file", str(board)]) != 0
    assert "nothing to write" in capsys.readouterr().err


# --------------------------------------------------------------------------------------------
# new
# --------------------------------------------------------------------------------------------


def test_new_allocates_next_id_from_the_working_file(board: Path, capsys, monkeypatch) -> None:
    # Neutralise the branch-tip scan: no `refs/heads` in an empty git repo initialized here would
    # also work, but stubbing is more direct and keeps this test independent of git state.
    monkeypatch.setattr(tasks, "next_id", lambda text: "T-004")
    rc = run(
        [
            "new",
            "--title",
            "A new ticket",
            "--milestone",
            "M2",
            "--file",
            str(board),
        ]
    )
    assert rc == 0
    new_id = capsys.readouterr().out.strip()
    assert new_id == "T-004"
    doc = yaml.safe_load(_text(board))
    t4 = next(t for t in doc["tasks"] if t["id"] == "T-004")
    assert t4["title"] == "A new ticket"
    assert t4["status"] == "todo"
    assert t4["milestone"] == "M2"


def test_new_id_allocation_uses_working_file_max(tmp_path: Path) -> None:
    text = FIXTURE  # highest id is T-003
    assert tasks._max_id_num(text) == 3


def test_new_records_deps_and_use_cases_and_found_by(board: Path, capsys, monkeypatch) -> None:
    monkeypatch.setattr(tasks, "next_id", lambda text: "T-005")
    run(
        [
            "new",
            "--title",
            "Depends on stuff",
            "--milestone",
            "M2",
            "--depends-on",
            "T-001,T-002",
            "--use-cases",
            "SIGNAL-01,SIGNAL-02",
            "--found-by",
            "T-002",
            "--file",
            str(board),
        ]
    )
    doc = yaml.safe_load(_text(board))
    t5 = next(t for t in doc["tasks"] if t["id"] == "T-005")
    assert t5["deps"] == ["T-001", "T-002"]
    assert t5["use_cases"] == ["SIGNAL-01", "SIGNAL-02"]
    assert "FOUND BY T-002." in t5["notes"]


def test_new_title_with_colon_space_is_quoted_correctly(board: Path, monkeypatch) -> None:
    monkeypatch.setattr(tasks, "next_id", lambda text: "T-006")
    run(
        [
            "new",
            "--title",
            "looks like FM: broadcast allocation nearby",
            "--milestone",
            "M2",
            "--file",
            str(board),
        ]
    )
    doc = yaml.safe_load(_text(board))
    t6 = next(t for t in doc["tasks"] if t["id"] == "T-006")
    assert t6["title"] == "looks like FM: broadcast allocation nearby"


# --------------------------------------------------------------------------------------------
# validate
# --------------------------------------------------------------------------------------------


def test_validate_passes_on_the_fixture(board: Path, capsys) -> None:
    assert run(["validate", "--file", str(board)]) == 0
    assert "OK" in capsys.readouterr().out


def test_validate_catches_a_blocked_ticket_with_no_blocked_on(board: Path, capsys) -> None:
    bad = _text(board).replace(
        '    blocked_on: "user decision"\n', "", 1
    )
    board.write_text(bad, encoding="utf-8")
    assert run(["validate", "--file", str(board)]) != 0
    assert "blocked" in capsys.readouterr().err


def test_validate_catches_duplicate_ids(board: Path, capsys) -> None:
    bad = _text(board).replace("- id: T-002", "- id: T-001", 1)
    board.write_text(bad, encoding="utf-8")
    assert run(["validate", "--file", str(board)]) != 0
    assert "duplicate" in capsys.readouterr().err


def test_validate_catches_unknown_status(board: Path, capsys) -> None:
    bad = _text(board).replace("status: todo", "status: in_review", 1)
    board.write_text(bad, encoding="utf-8")
    assert run(["validate", "--file", str(board)]) != 0
    assert "vocabulary" in capsys.readouterr().err


# --------------------------------------------------------------------------------------------
# a broken write is never trusted: restore-on-failure
# --------------------------------------------------------------------------------------------


def test_write_checked_restores_original_on_duplicate_id(tmp_path: Path) -> None:
    p = tmp_path / "b.yaml"
    original = FIXTURE
    p.write_text(original, encoding="utf-8")
    broken = original.replace("- id: T-002", "- id: T-001", 1)
    with pytest.raises(SystemExit):
        tasks.write_checked(p, original, broken)
    assert p.read_text(encoding="utf-8") == original


def test_write_checked_restores_original_on_invalid_yaml(tmp_path: Path) -> None:
    p = tmp_path / "b.yaml"
    original = FIXTURE
    p.write_text(original, encoding="utf-8")
    broken = original + "\n  bad: [unclosed\n"
    with pytest.raises(SystemExit):
        tasks.write_checked(p, original, broken)
    assert p.read_text(encoding="utf-8") == original


# --------------------------------------------------------------------------------------------
# against the REAL board, read-only
# --------------------------------------------------------------------------------------------


@pytest.mark.skipif(not REAL_BOARD.exists(), reason="docs/tasks.yaml not present")
def test_real_board_show_list_validate() -> None:
    # Pick a real id off the real board rather than hardcoding one that might get renumbered
    # (ids are never renumbered per CLAUDE.md, but this is a cheap way to avoid ever caring).
    doc = yaml.safe_load(REAL_BOARD.read_text(encoding="utf-8"))
    some_id = str(doc["tasks"][0]["id"])
    assert tasks.main(["show", some_id, "--file", str(REAL_BOARD)]) == 0
    assert tasks.main(["list", "--ready", "--file", str(REAL_BOARD)]) == 0
    assert tasks.main(["validate", "--file", str(REAL_BOARD)]) == 0


# --------------------------------------------------------------------------------------------
# set key=[a, b] (lists) and unset — 2026-09-23: turning a `blocked` ticket into `todo` with
# `depends_on` needed both, and `set` alone wrote `depends_on` as a quoted string (whose `list()`
# is its characters, so the runner saw no real dependency) and `blocked_on=null` as the string
# 'null' (truthy, so the runner still treated the ticket as blocked).
# --------------------------------------------------------------------------------------------


def _t(board: Path, tid: str) -> dict:
    return next(t for t in yaml.safe_load(_text(board))["tasks"] if t["id"] == tid)


def test_set_writes_a_bracketed_value_as_a_real_list(board: Path) -> None:
    assert run(["set", "T-002", "depends_on=[T-001, T-003]", "--file", str(board)]) == 0
    assert _t(board, "T-002")["depends_on"] == ["T-001", "T-003"]


def test_set_writes_an_empty_list(board: Path) -> None:
    assert run(["set", "T-002", "depends_on=[]", "--file", str(board)]) == 0
    assert _t(board, "T-002")["depends_on"] == []


def test_set_list_items_are_quoted_only_when_they_must_be(board: Path) -> None:
    assert run(["set", "T-002", "tags=[plain, 0123, a: b]", "--file", str(board)]) == 0
    assert _t(board, "T-002")["tags"] == ["plain", "0123", "a: b"]


def test_unset_removes_a_scalar_field(board: Path) -> None:
    assert run(["unset", "T-002", "priority", "--file", str(board)]) == 0
    assert "priority" not in _t(board, "T-002")


def test_unset_removes_a_block_field_and_its_continuation(board: Path) -> None:
    assert run(["unset", "T-002", "acceptance", "--file", str(board)]) == 0
    t2 = _t(board, "T-002")
    assert "acceptance" not in t2 and t2["effort"] == "low"
    assert _t(board, "T-003")["blocked_on"] == "user decision"  # neighbours untouched


def test_blocked_to_todo_with_depends_on_in_two_calls(board: Path) -> None:
    assert run(["set", "T-003", "status=todo", "depends_on=[T-001]", "--file", str(board)]) == 0
    assert run(["unset", "T-003", "blocked_on", "--file", str(board)]) == 0
    t3 = _t(board, "T-003")
    assert t3["status"] == "todo" and t3["depends_on"] == ["T-001"] and "blocked_on" not in t3


def test_unset_refuses_blocked_on_while_still_blocked(board: Path, capsys) -> None:
    assert run(["unset", "T-003", "blocked_on", "--file", str(board)]) != 0
    assert _t(board, "T-003")["blocked_on"] == "user decision"


def test_unset_refuses_required_keys_and_missing_keys(board: Path) -> None:
    assert run(["unset", "T-002", "status", "--file", str(board)]) != 0
    assert run(["unset", "T-002", "no_such_key", "--file", str(board)]) != 0


def test_set_true_and_false_are_real_booleans(board: Path) -> None:
    assert run(["set", "T-002", "core_interface=true", "is_hil=false", "--file", str(board)]) == 0
    t2 = _t(board, "T-002")
    assert t2["core_interface"] is True and t2["is_hil"] is False


def test_ready_honours_depends_on_as_well_as_deps(board: Path, capsys) -> None:
    assert run(["set", "T-003", "status=todo", "depends_on=[T-002]", "--file", str(board)]) == 0
    assert run(["unset", "T-003", "blocked_on", "--file", str(board)]) == 0
    capsys.readouterr()
    assert run(["list", "--ready", "--file", str(board)]) == 0
    assert "T-003" not in capsys.readouterr().out  # T-002 is still todo
