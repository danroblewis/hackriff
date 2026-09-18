"""Tests for `hkpy.reconcile` (T-477).

The one that matters is `test_a_fresh_branch_is_not_merged`: reachability alone cannot tell a
landed branch from one freshly cut off main, and the first draft of this tool got it wrong in
exactly that way — it reported three brand-new worktree branches as MERGED because a branch at main
is trivially an ancestor of main. A tool whose job is to catch a false claim on the board must not
make one itself.
"""

from __future__ import annotations

import hkpy.reconcile as rec

BOARD = """\
tasks:
  - id: T-001
    title: a done thing
    status: done
  - id: T-002
    title: something in flight
    status: in-progress
  - id: T-003
    title: not started
    status: todo
  - id: T-004
    title: another in flight
    status: in-progress
notes: |
  trailing
"""


def test_tickets_parses_id_status_and_title():
    got = {t.id: (t.status, t.title) for t in rec.tickets(BOARD)}
    assert got["T-001"] == ("done", "a done thing")
    assert got["T-002"] == ("in-progress", "something in flight")
    assert got["T-004"] == ("in-progress", "another in flight")
    assert len(got) == 4


def test_tickets_parses_a_board_with_a_merge_conflict_in_it():
    """It must answer when the board does not parse as YAML — that is when it is most needed."""
    conflicted = BOARD.replace(
        "    status: in-progress\n  - id: T-003",
        "<<<<<<< HEAD\n    status: in-progress\n=======\n    status: done\n>>>>>>> other\n  - id: T-003",
    )
    ids = [t.id for t in rec.tickets(conflicted)]
    assert ids == ["T-001", "T-002", "T-003", "T-004"]


def test_branch_name_maps_ticket_id_to_the_repo_convention():
    """Naming is pure and separate from existence — `branch_for` adds the repo lookup."""
    assert rec.branch_name("T-451") == "task-t451"
    assert rec.branch_name("T-007") == "task-t7"
    assert rec.branch_name("T-1234") == "task-t1234"


def test_a_fresh_branch_is_not_merged(monkeypatch):
    """A branch cut from main and never committed to is NO WORK, never MERGED.

    Reachability says ancestor-of-main for both a landed branch and a fresh one. The discriminator
    is whether main carries a merge commit naming the branch tip as a parent.
    """
    calls = {"parents": "aaa bbb\nccc ddd"}  # main's merge commits: none name our tip

    def fake_git(*args):
        if args[:2] == ("rev-parse",):
            return "freshtip"
        if args[0] == "rev-parse":
            return "freshtip"
        if args[0] == "log" and "--merges" in args:
            return calls["parents"]
        if args[0] == "rev-list":
            return "0"
        return ""

    monkeypatch.setattr(rec, "_git", fake_git)
    assert rec._merged_via_commit("task-t999") is False

    calls["parents"] = "aaa freshtip\nccc ddd"
    assert rec._merged_via_commit("task-t999") is True


def test_findings_flag_only_merged_and_ahead_for_attention():
    t = rec.Ticket("T-002", "in-progress", "x")
    assert rec.Finding(t, "b", "MERGED", 0, 1.0).needs_attention
    assert rec.Finding(t, "b", "AHEAD", 3, 1.0).needs_attention
    assert not rec.Finding(t, "b", "NO WORK", 0, 1.0).needs_attention
    assert not rec.Finding(t, None, "NO BRANCH", 0, None).needs_attention


def test_render_names_the_merged_tickets_and_says_what_to_do():
    t = rec.Ticket("T-458", "in-progress", "a merged thing")
    out = rec.render([rec.Finding(t, "task-t458", "MERGED", 0, 60.0)])
    assert "T-458" in out
    assert "MERGED" in out
    assert "done" in out  # the instruction, not just the state


def test_render_handles_an_empty_board_without_claiming_a_problem():
    out = rec.render([])
    assert "no in-progress" in out
    assert "!!" not in out


def test_age_is_human_and_monotone():
    assert rec._age(None) == "?"
    assert rec._age(30).endswith("s")
    assert rec._age(600).endswith("m")
    assert rec._age(7200).endswith("h")
