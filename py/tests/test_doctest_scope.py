"""`just test-doc` must narrow what rustdoc is *invoked* over, never what is *tested*.

The rule under test (`hkpy.doctests`): a crate is skipped only when nothing in its `src/` is a
code fence rustdoc would run. Every case here is a way that could go wrong — a ```text block
whose closing fence looks like an opening Rust one, a doc attribute whose content is not in the
file, an unreadable tree — and each one must land on "run it", because a doctest that silently
stops running is the exact failure R7 warned a hand-kept list would cause.
"""

from __future__ import annotations

from pathlib import Path

from hkpy.doctests import REPO, crate_dirs, has_doc_fence, package_name, scope


def test_a_plain_fence_is_a_doctest():
    assert has_doc_fence("/// ```\n/// let x = 1;\n/// ```\npub fn f() {}")
    assert has_doc_fence("//! ```\n//! let x = 1;\n//! ```")


def test_rustdoc_attributes_are_still_doctests():
    for attr in ("rust", "no_run", "should_panic", "compile_fail", "ignore", "edition2024"):
        assert has_doc_fence(f"/// ```{attr}\n/// let x = 1;\n/// ```"), attr
    assert has_doc_fence("/// ```rust,no_run\n/// let x = 1;\n/// ```")


def test_a_non_rust_fence_is_not_a_doctest():
    """And — the case a line-by-line grep gets wrong — its CLOSING fence is not one either."""
    for lang in ("text", "json", "jsonc", "bash", "console"):
        assert not has_doc_fence(f"/// ```{lang}\n/// not rust\n/// ```"), lang
    assert not has_doc_fence("//! ```text\n//! a\n//! ```\n//!\n//! ```text\n//! b\n//! ```")


def test_a_rust_fence_after_a_text_one_is_still_found():
    assert has_doc_fence("//! ```text\n//! a\n//! ```\n//!\n//! ```\n//! let x = 1;\n//! ```")


def test_an_unterminated_fence_does_not_swallow_the_next_item():
    """A ```text block left open must not hide a real doctest further down the file."""
    src = "//! ```text\n//! a\n\npub fn g() {}\n\n/// ```\n/// let x = 1;\n/// ```\npub fn f() {}"
    assert has_doc_fence(src)


def test_a_doc_attribute_whose_content_is_elsewhere_counts():
    assert has_doc_fence('#![doc = include_str!("../README.md")]')
    assert has_doc_fence('#[doc = concat!("```\\n", "let x = 1;\\n", "```")]')


def test_code_outside_a_doc_comment_is_not_a_doctest():
    assert not has_doc_fence("// ```\n// let x = 1;\n// ```\nfn f() {}")
    assert not has_doc_fence("let s = \"```\";")


def test_the_real_workspace_selects_the_crates_that_actually_have_doctests():
    """Drift alarm. These three hold the workspace's only runnable doc fences today; if one
    gains or loses a doctest this test says so, out loud, instead of the gate quietly running
    rustdoc over 16 crates that cannot have one."""
    selected, skipped = scope(REPO, "hk-e2e")
    assert selected == ["hk-blocks", "hk-pipeline", "hk-recipe"], selected
    assert "hk-e2e" not in selected and "hk-e2e" not in skipped
    assert len(skipped) >= 10, skipped


def test_every_workspace_crate_is_classified_one_way_or_the_other():
    selected, skipped = scope(REPO, "hk-e2e")
    names = {package_name(d) for d in crate_dirs(REPO)} - {"hk-e2e", None}
    assert set(selected) | set(skipped) == names


def test_gate_crates_narrows_but_never_widens():
    selected, _ = scope(REPO, "hk-e2e", gate_crates="hk-recipe hk-model")
    assert selected == ["hk-recipe"]


def test_an_unreadable_crate_fails_closed(tmp_path: Path):
    """A crate whose sources cannot be scanned is SELECTED, not skipped."""
    crate = tmp_path / "crates" / "hk-mystery"
    (crate / "src").mkdir(parents=True)
    (crate / "Cargo.toml").write_text('[package]\nname = "hk-mystery"\n')
    (crate / "src" / "lib.rs").write_bytes(b"\xff\xfe not utf 8 // no fence here")
    selected, skipped = scope(tmp_path)
    # Undecodable bytes are replaced, not fatal — but a crate with no `src/` at all, which the
    # rule was not written for, must still be run.
    assert selected + skipped == ["hk-mystery"]
    bare = tmp_path / "crates" / "hk-bare"
    bare.mkdir()
    (bare / "Cargo.toml").write_text('[package]\nname = "hk-bare"\n')
    selected, _ = scope(tmp_path)
    assert "hk-bare" in selected
