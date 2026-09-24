"""The dashboard's main page never hides a card (user, 2026-09-24).

A tall card pushed the cards below it out of a fixed-height column the page could not scroll:
#mergecard on 09-22 (fixed by hand, for that card only), then Work trees, Merge queue and Recent
commits on 09-24 - measured in headless Chrome with 20 merge-queue rows and 10 agents, their last
rows were unreachable at 1440x900, 1280x720, 1920x1080, 1000x700 and 768x1024. The rule is for
EVERY card: it may shrink, it scrolls, the column scrolls as the backstop, and below 1000 px the
page itself scrolls.
"""
from __future__ import annotations

import importlib.util
import pathlib
import re

_MON = pathlib.Path(__file__).resolve().parents[2] / "ops" / "monitor.py"
_spec = importlib.util.spec_from_file_location("hk_monitor_layout", _MON)
M = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(M)
PAGE = M.PAGE
CSS = PAGE[PAGE.index("<style>"):PAGE.index("</style>")]


def _rule(sel: str, css: str = CSS) -> str:
    m = re.search(r"(?:^|[}\n])" + re.escape(sel) + r"\{([^}]*)\}", css)
    assert m, sel
    return m.group(1)


def test_no_card_refuses_to_shrink():
    cards = re.findall(r'<div class="card[^"]*"[^>]*>', PAGE)
    assert len(cards) >= 9
    for tag in cards:
        style = (re.search(r'style="([^"]*)"', tag) or [None, ""])[1].replace(" ", "")
        assert "flex:00" not in style and "flex:none" not in style and "flex-shrink:0;" not in style + ";", tag
        assert not re.search(r"(^|;)(min-)?height:\d", style) or "height:190px" in style, tag   # a fixed floor never hides a card


def test_every_card_scrolls_and_the_column_is_the_backstop():
    card, col = _rule(".card"), _rule(".col")
    assert "flex:0 1 auto" in card and "overflow:auto" in card and "min-height:" in card
    assert "overflow-y:auto" in col and "min-height:0" in col
    assert "overflow:auto" in _rule(".bd") and "min-height:0" in _rule(".bd")


def test_below_1000px_the_page_itself_scrolls():
    mid = CSS[CSS.index("@media(max-width:1000px)"):CSS.index("@media(max-width:640px)")]
    assert "body{overflow:auto" in mid and "height:auto" in mid
