"""ops/stage.sh builds the demo from a snapshot of a landed commit, never from the live checkout.

2026-09-24 21:20: the stage build ran with cwd = main's checkout while the merge runner merged two batches
into it; hk-model compiled from one tree and hk-api from another, and the demo stayed on the 18:14 build.
"""

import pathlib
import re

STAGE = pathlib.Path(__file__).resolve().parents[2] / "ops" / "stage.sh"


def _build_fn() -> str:
    m = re.search(r"^build\(\)\{.*?^\}\n", STAGE.read_text(), re.M | re.S)
    assert m
    return m.group(0)


def test_the_stage_build_compiles_and_bundles_in_its_snapshot_worktree_only():
    fn = _build_fn()
    assert 'cd "$SRC" && CARGO_TARGET_DIR=' in fn and 'cd "$SRC/ui" && npm run build' in fn
    assert 'cd "$REPO"' not in fn and 'cd "$REPO/ui"' not in fn
    assert 'checkout -q --force --detach "$1"' in fn          # the snapshot is the commit it was asked for


def test_the_demo_serves_its_own_copy_of_the_bundle_not_mains_ui_dist():
    text = STAGE.read_text()
    assert '--ui-dist "$REPO/ui/dist"' not in text
    assert text.count('--ui-dist "$(ui_dist)"') == 2
    assert 'if [ -f "$DIST/index.html" ]; then echo "$DIST"' in text     # never a missing dir
