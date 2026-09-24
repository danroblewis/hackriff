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
    assert tasks.main(["order", "--file", str(board), "--top", "3"]) == 0
    out = capsys.readouterr().out
    assert "T-801" in out.splitlines()[2] and "frontier:" in out
    assert board.read_bytes() == before
