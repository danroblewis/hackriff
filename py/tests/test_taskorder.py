"""`just task order` (py/hkpy/taskorder.py): bottlenecks by what they hold behind them."""

from hkpy import taskorder, tasks


def t(i, status="todo", deps=(), ms="M1", **kw):
    return {"id": i, "status": status, "depends_on": list(deps), "milestone": ms, "title": f"title {i}", **kw}


BOARD = [
    t("T-800", "done", ms="MMAP"),
    t("T-801", "in-progress", ["T-800"], ms="MMAP"),              # the bottleneck
    t("T-802", deps=["T-801"], ms="MMAP"), t("T-803", deps=["T-802"], ms="MMAP"),
    t("T-804", deps=["T-801", "T-802"], ms="MPLAY"),
    t("T-565", "in-progress", ms="MAUTO", blocked_on="user: ADR-0015"),
    t("T-566", deps=["T-565"], ms="MAUTO"),
    t("T-568", deps=["T-568"], ms="MAUTO"), t("T-576", deps=["T-568"], ms="MAUTO"),   # a self-loop
    t("T-900"), t("T-901", "cancelled"), t("T-902", deps=["T-901", "T-999"]),        # frontier
    t("T-903", needs="user"),
]


def test_value_weighs_what_is_released_and_states_its_formula():
    board = [t("A"), t("B", deps=["A"], priority="high", use_cases=["X-1", "X-2"]),
             t("C", deps=["B"], priority="low"), t("D", deps=["A"], found_by="user 2026-09-23"),
             t("E", deps=["A"], priority="ahead of queued work (user)")]
    a = taskorder.analyse(board)
    by = {r["id"]: r for r in a["rows"]}
    # downstream of A: B (2 + 2*0.5) + C (0.5) + D (user 3) + E (priority says user: 3)
    assert by["A"]["unblocks"] == 4 and by["A"]["value"] == 9.5
    assert by["A"]["downstream_milestones"] == {"M1": 4}
    assert "3 if user-requested" in a["formula"] and "0.5 per use-case id" in a["formula"]


def test_depth_groups_are_landings_away_and_gates_say_what_they_wait_on():
    a = taskorder.analyse(BOARD)
    by = {r["id"]: r for r in a["rows"]}
    assert (by["T-801"]["depth"], by["T-802"]["depth"], by["T-803"]["depth"], by["T-804"]["depth"]) == (0, 1, 2, 2)
    assert by["T-804"]["gate"] == ["T-801", "T-802"]
    assert any(g.startswith("blocked:") for g in by["T-565"]["gate"]) and by["T-903"]["gate"] == ["needs user"]
    assert by["T-568"]["depth"] is None and "cycle" in a["groups"]
    assert a["groups"]["0"][0] == a["order"][0]


def test_the_committed_board_is_the_gates_base_while_a_batch_gates(tmp_path):
    ops = tmp_path / "ops"
    ops.mkdir()
    assert taskorder.committed_ref("/x", str(ops)) == "main"
    (ops / "bulk-in-progress").write_text("base=abc123\nbranches=a b\n")
    assert taskorder.committed_ref("/x", str(ops)) == "abc123"


def test_the_bottleneck_is_ranked_by_everything_behind_it():
    a = taskorder.analyse(BOARD)
    roots = [(r["id"], r["unblocks"], r["moves"]) for r in a["roots"]]
    assert roots[0] == ("T-801", 3, ["MMAP", "MPLAY"])          # 802, 803, 804 - counted once
    assert ("T-565", 1, ["MAUTO"]) in roots
    assert "T-802" not in [r[0] for r in roots]                  # a link in the chain, not a root
    by = {r["id"]: r for r in a["rows"]}
    assert by["T-802"]["unblocks"] == 2 and by["T-802"]["waits_on"] == ["T-801"]


def test_the_frontier_is_what_could_start_now():
    a = taskorder.analyse(BOARD)
    assert sorted(a["frontier"]) == ["T-900", "T-902"]          # T-902's deps: cancelled + not on the board
    assert "T-903" not in a["frontier"] and "T-802" not in a["frontier"]


def test_a_self_dependency_is_a_cycle_and_is_named():
    a = taskorder.analyse(BOARD)
    assert a["self_deps"] == ["T-568"] and set(a["cycle"]) == {"T-568", "T-576"}
    assert "self-dependency: T-568" in taskorder.render(a)[0]
    order = a["order"]
    assert order.index("T-801") < order.index("T-802") < order.index("T-803")


def test_the_cli_is_read_only(tmp_path, capsys):
    import yaml
    board = tmp_path / "tasks.yaml"
    board.write_text(yaml.safe_dump({"tasks": BOARD}))
    before = board.read_bytes()
    assert tasks.main(["order", "--working", "--file", str(board), "--top", "3"]) == 0
    out = capsys.readouterr().out
    assert "T-801" in out.splitlines()[4] and "frontier:" in out and "depth 1 (1 landing(s) away)" in out
    assert board.read_bytes() == before
