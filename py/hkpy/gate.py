"""`just gate` — the one diff-aware merge gate (T-396).

Looks at what actually changed, **classifies** it, prints the decision, and runs exactly
the suites that class needs. The "which tests do I run" rule lives here, in the runner,
rather than in each agent's judgement, so the same change gets the same gate whoever ran it.

The rule that makes this safe is that classification **fails closed**: a path that matches
no known class runs the **full** gate, never the cheapest one. That is the repo's own
principle applied to its own tooling — `BiasTee::Unknown` is not `Off`, `Coverage::Unobserved`
is not quiet, `Encryption::Unknown` is not clear. Nothing said is never permissive. A new
top-level directory nobody thought to classify must not silently inherit the ui-only
treatment; it gets the expensive path until someone classifies it on purpose.

Three classes a naive version gets wrong, each deliberate here:

* `fixtures/` is **not** ui/docs-ish — it is acceptance *input*, and the suites read fixture
  metadata (T-317, T-373, T-382 all changed it).
* the `justfile` and `.github/` are **the gate**. A change to them must be verified by the
  expensive path or the gate could weaken itself and certify its own weakening.
* this file and its tests are the gate too, for the same reason — so they are full, not the
  `py/` class they would otherwise land in.

`plugins/`, `spikes/`, `tests/`, `.config/`, `recipes/`, `Cargo.*` and repo-root files are
unclassified and therefore full.

The decision is always printed before anything runs. A silent classifier is a worse version
of the judgement it replaces: an agent or a human has to be able to see the choice and
challenge it, which means seeing the deciding files, not just the verdict.

**Two subjects, two sources (T-424).** The default source — merge base with `main` *plus*
everything uncommitted, untracked included — answers *"what is in my tree that isn't on
main?"*. That is the right question for an **agent** checking its own work: an untracked
`newdir/thing.rs` it has not `git add`ed yet is still going to be committed, so it is part
of the change and must be classified. The **coordinator's per-merge gate asks a different
question** — *"what will this merge put on main?"* — and `--merge` answers exactly that by
classifying the **index during an in-progress merge**, which git itself built as the merge
result versus HEAD. Untracked files are, by construction, not in that index and cannot reach
main through that merge; the permanently-untracked `tools/` and diagnostic captures in the
coordinator's checkout were degrading every merge gate to full for files that can never be
part of any merge.

That narrowing is guarded, not assumed:

* `--merge` **requires an in-progress merge** (`MERGE_HEAD`, or `SQUASH_MSG` for a squash).
  Without one there is no guarantee the index is a merge result, so it **forces the full
  gate** rather than classifying whatever happens to be staged. Fail-closed, as everywhere.
* Every uncommitted path it did *not* classify is **printed**, with the class it would have
  had. Nothing is ignored silently; there is no ignore list, and `fixtures/` staged in a
  merge is still full.

The counterexample worth stating: an untracked fixture *can* change what a suite sees when
that suite runs. Classifying untracked paths as FULL never protected against that — the
suites read the working tree, so a stray file changes their result at whatever class was
chosen. Fail-closed on untracked paths buys **cost, not safety**, for the merge subject. It
buys real safety for the agent subject, where untracked means not-yet-added-but-will-land,
which is why the default keeps it.

**The suites this file launches build with T-144's flags (T-400).** Every agent brief sets
`CARGO_INCREMENTAL=0` (and the rest of T-144) so worktree targets stay close to the APFS clone
they were seeded from, but `main()` used to shell out to `just lint`/`just test`/etc. without
them, so the coordinator's own gate regenerated `target/debug/incremental` on every run — state
every agent is configured to avoid. `GATE_BUILD_ENV`/`suite_env()` fix that at the one place
this file spawns a suite, not in the justfile: a developer's own `just test` still gets whatever
incremental behaviour they want.

Stdlib only, so it can run as `python3 py/hkpy/gate.py` as well as `just gate`.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass

# ---------------------------------------------------------------------------
# Classes
# ---------------------------------------------------------------------------

FULL = "full"
UI = "ui"
DOCS = "docs"
PY = "py"

#: Classes in the order they are reported when a change spans more than one.
CLASS_ORDER = (FULL, UI, DOCS, PY)

#: Phases. CI runs the two separately (two jobs, one recipe call each — T-358); a local
#: `just gate` runs both. The classification is identical either way: the phase only says
#: which half of the chosen suites this process runs.
PHASE_CHECK = "check"
PHASE_ACCEPTANCE = "acceptance"
PHASE_ALL = "all"
PHASES = (PHASE_ALL, PHASE_CHECK, PHASE_ACCEPTANCE)

#: What each class runs, per phase. `full` deliberately uses `acceptance-ci` (the census +
#: M0 slice + harness targets) rather than bare `acceptance`, so the gate an agent runs
#: locally and the gate CI runs are one definition rather than two similar ones.
SUITES: dict[str, dict[str, tuple[tuple[str, ...], ...]]] = {
    FULL: {
        PHASE_CHECK: (("just", "lint"), ("just", "test")),
        PHASE_ACCEPTANCE: (("just", "acceptance-ci"), ("just", "test-ui-e2e")),
    },
    UI: {
        # `just test-ui` is npm ci + build + `tsc --noEmit` + the ui/test suites. The
        # repo's `just lint` is lint-rust + lint-py and there is no JS/TS linter here, so
        # the UI's lint equivalent is the typecheck already inside test-ui; running clippy
        # over the workspace for a change to a .ts file proves nothing it could break.
        PHASE_CHECK: (("just", "test-ui"),),
        # `just test-ui-e2e` (T-455) is the browser tier: headless Chrome over a real
        # `hk serve`, asserting on what /surface draws and what it requests. It is in the
        # ACCEPTANCE phase, not CHECK, because it needs the `hk` binary and a browser
        # while `test-ui` needs neither — putting it in CHECK would make every `.ts` typo
        # pay for a Rust build. It runs for the `full` class too: this is the only suite
        # that notices when a backend change breaks the page that consumes it.
        #
        # It is not optional, and that is the point of the ticket that added it. Two
        # defects in two days (T-450's CSP throw at module scope, T-454's 503 reaching the
        # user) passed every suite this repo had, and a verification tier nobody is
        # obliged to run is worse than none, because it looks like coverage.
        PHASE_ACCEPTANCE: (("just", "test-ui-e2e"),),
    },
    DOCS: {
        # No link checker and no markdown linter exist in this repo, and adding a
        # dependency to satisfy a word in a ticket is the wrong trade. Docs-only is
        # honestly a no-op today; the gate says so out loud rather than implying a check.
        PHASE_CHECK: (),
        PHASE_ACCEPTANCE: (),
    },
    PY: {
        PHASE_CHECK: (("just", "lint-py"), ("just", "test-py")),
        PHASE_ACCEPTANCE: (),
    },
}

#: Canonical ordering for the union of several classes' suites.
_COMMAND_ORDER = (
    ("just", "lint"),
    ("just", "lint-py"),
    ("just", "test"),
    ("just", "test-ui"),
    ("just", "test-py"),
    ("just", "acceptance-ci"),
    ("just", "test-ui-e2e"),
)

#: T-400: the T-144 build flags, applied only to the suite subprocesses the gate itself
#: launches — never written into the justfile globally, because a developer running `just
#: build`/`just test`/`just lint` by hand may want incremental compilation. This is what "the
#: rule lives in the runner" means here, exactly as T-396 moved the which-suites rule into
#: this file instead of each agent's judgement.
#:
#: Every T-144 flag is included, each for its own reason — not copied by reflex:
#:
#:   CARGO_INCREMENTAL=0                        the ticket's own trigger. Without it, every
#:                                               `just gate` regenerates target/debug/incremental
#:                                               in the coordinator's OWN checkout — 3.6 GB in one
#:                                               session — diverging it from the clean state every
#:                                               future worktree clones via `cp -c -R -p`.
#:   CARGO_PROFILE_DEV_DEBUG=line-tables-only    every worktree already builds with this (T-144).
#:                                               If the gate's own suites built full debug info
#:                                               instead, the coordinator's target/ would carry a
#:                                               different profile fingerprint than what worktrees
#:                                               build against, which is its own source of
#:                                               unwanted rebuild divergence — the same failure
#:                                               mode this ticket exists to close, one layer up.
#:   CARGO_BUILD_JOBS=6                          CLAUDE.md's Coordination section counts "the
#:                                               coordinator's full check" as one of the "at most
#:                                               4 Rust-building agents" T-144 caps at 6 jobs each
#:                                               (4 x 6 ~= 28 cores). Left unbounded, a `just gate`
#:                                               run would oversubscribe on top of up to three
#:                                               concurrently building worktrees exactly as an
#:                                               agent that skipped the cap would.
#:
#: Deliberately NOT here: nextest's `--test-threads` — T-436 already pins that in
#: `.config/nextest.toml` for every nextest invocation, gate included, so repeating it here would
#: be a second, driftable copy of a rule that already lives in the runner.
GATE_BUILD_ENV: dict[str, str] = {
    "CARGO_INCREMENTAL": "0",
    "CARGO_PROFILE_DEV_DEBUG": "line-tables-only",
    "CARGO_BUILD_JOBS": "6",
}


def suite_env(base: dict[str, str]) -> dict[str, str]:
    """`base` (normally `os.environ`) with `GATE_BUILD_ENV` applied on top.

    A pure function of its input so it's testable without touching the real environment or
    spawning anything: it must add exactly the T-144 flags and change nothing else, in
    particular never removing or overriding an unrelated variable the caller already set.
    """
    return {**base, **GATE_BUILD_ENV}


_GATE_SELF = "the gate itself — it must not be able to weaken itself"

#: Ordered rules: (kind, pattern, class, reason). First match wins. `prefix` matches a
#: directory prefix, `exact` a whole path. Anything falling off the end is FULL.
_RULES: tuple[tuple[str, str, str, str], ...] = (
    ("exact", "justfile", FULL, _GATE_SELF + " (justfile)"),
    ("prefix", ".github/", FULL, _GATE_SELF + " (CI workflow)"),
    ("exact", "py/hkpy/gate.py", FULL, _GATE_SELF + " (classifier)"),
    ("exact", "py/tests/test_gate.py", FULL, _GATE_SELF + " (classifier tests)"),
    ("prefix", "crates/", FULL, "Rust workspace — the signal path"),
    ("exact", "docs/api.md", FULL, "the client/server contract (T-079)"),
    ("prefix", "fixtures/", FULL, "acceptance input — the suites read fixture metadata"),
    ("prefix", "tests/", FULL, "the hk-e2e acceptance crate"),
    ("prefix", "plugins/", FULL, "decoder manifests/wrappers the plugin host loads"),
    ("prefix", "recipes/", FULL, "pipeline recipes the suites load"),
    ("prefix", ".config/", FULL, "nextest thread cap + serial groups — how the suite runs"),
    ("exact", "Cargo.toml", FULL, "workspace manifest"),
    ("exact", "Cargo.lock", FULL, "workspace dependency lock"),
    ("prefix", "ui/", UI, "web client — thin presentation layer"),
    ("prefix", "docs/", DOCS, "documentation"),
    ("prefix", "py/", PY, "Python tooling (orchestration/research only)"),
)

_UNCLASSIFIED = "unclassified path — the gate fails closed"


def normalize(path: str) -> str:
    """Repo-relative, forward slashes, no `./` prefix."""
    p = path.strip().replace("\\", "/")
    while p.startswith("./"):
        p = p[2:]
    return p.strip("/") if p.endswith("/") else p


def classify_path(path: str) -> tuple[str, str]:
    """Classify one changed path. Returns `(class, reason)`.

    Unknown paths are FULL. This is the fail-closed half of the whole design: adding a
    top-level directory must cost the expensive gate until someone adds a rule above.
    """
    p = normalize(path)
    for kind, pattern, klass, reason in _RULES:
        if kind == "exact" and p == pattern:
            return klass, reason
        if kind == "prefix" and p.startswith(pattern):
            return klass, reason
    return FULL, _UNCLASSIFIED


@dataclass(frozen=True)
class Decision:
    """What the gate decided, and why."""

    #: One of the classes, or several joined by `+` (e.g. `ui+docs`), or `empty`.
    label: str
    #: The classes present, in CLASS_ORDER.
    classes: tuple[str, ...]
    #: `(path, class, reason)` for every changed file, sorted by path.
    files: tuple[tuple[str, str, str], ...]
    #: Set when the gate could not work out what changed and forced the full gate.
    forced_reason: str | None = None

    @property
    def is_full(self) -> bool:
        return FULL in self.classes

    @property
    def is_empty(self) -> bool:
        return not self.classes

    def deciding(self) -> tuple[tuple[str, str, str], ...]:
        """The files that decided the class.

        For a full gate those are the files that forced it — the interesting subset when
        one `fixtures/` edit drags 200 doc files up with it. Otherwise, all of them.
        """
        if self.is_full:
            return tuple(f for f in self.files if f[1] == FULL)
        return self.files

    def commands(self, phase: str = PHASE_ALL) -> list[list[str]]:
        """The suites to run, deduped and in canonical order."""
        wanted = (
            (PHASE_CHECK, PHASE_ACCEPTANCE) if phase == PHASE_ALL else (phase,)
        )
        chosen: set[tuple[str, ...]] = set()
        for klass in self.classes:
            for ph in wanted:
                chosen.update(SUITES[klass][ph])
        ordered = [cmd for cmd in _COMMAND_ORDER if cmd in chosen]
        # Defensive: a suite added to SUITES but not to _COMMAND_ORDER still runs.
        ordered += [cmd for cmd in sorted(chosen) if cmd not in _COMMAND_ORDER]
        return [list(cmd) for cmd in ordered]


def classify(paths) -> Decision:
    """Classify a set of changed paths. Pure: no git, no environment, no side effects."""
    seen: dict[str, tuple[str, str]] = {}
    for raw in paths:
        p = normalize(raw)
        if not p:
            continue
        seen[p] = classify_path(p)
    if not seen:
        return Decision(label="empty", classes=(), files=())
    files = tuple(sorted((p, k, r) for p, (k, r) in seen.items()))
    present = {k for _, k, _ in files}
    if FULL in present:
        return Decision(label=FULL, classes=(FULL,), files=files)
    classes = tuple(k for k in CLASS_ORDER if k in present)
    return Decision(label="+".join(classes), classes=classes, files=files)


def forced_full(reason: str) -> Decision:
    """The full gate, chosen because the gate could not tell what changed."""
    return Decision(label=FULL, classes=(FULL,), files=(), forced_reason=reason)


# ---------------------------------------------------------------------------
# Working out what changed
# ---------------------------------------------------------------------------


def _git(args: list[str], root: str) -> tuple[int, str]:
    proc = subprocess.run(
        ["git", *args], cwd=root, capture_output=True, text=True, check=False
    )
    return proc.returncode, proc.stdout


def _git_ok(args: list[str], root: str) -> str | None:
    rc, out = _git(args, root)
    return out if rc == 0 else None


def _split_z(out: str) -> list[str]:
    return [t for t in out.split("\0") if t]


def committed_changes(root: str, base_ref: str) -> list[str] | None:
    """Paths changed since the **merge base** with `base_ref`.

    Merge base, not `base_ref` itself: diffing against a moved main makes every file
    someone else touched look like part of this branch, which would push every branch to
    the full gate for no reason (and, worse, teach people to distrust the classifier).
    `--no-renames` so a rename counts as touching both the old and the new path, and
    deletions count as touching the path.
    """
    base = _git_ok(["merge-base", base_ref, "HEAD"], root)
    if base is None:
        return None
    base = base.strip()
    out = _git_ok(["diff", "--name-only", "-z", "--no-renames", base, "HEAD"], root)
    if out is None:
        return None
    return _split_z(out)


def staged_changes(root: str) -> list[str] | None:
    out = _git_ok(["diff", "--cached", "--name-only", "-z", "--no-renames"], root)
    return None if out is None else _split_z(out)


def worktree_changes(root: str) -> list[str] | None:
    """Everything uncommitted: staged, unstaged, and untracked.

    Untracked files count. A brand-new `newdir/thing.rs` that nobody has `git add`ed yet is
    still a change the gate must see — leaving it out is the one way this could fail open.
    The cost is that stray untracked scratch files force the full gate; they are printed as
    deciding files, so that is visible and answerable rather than mysterious.
    """
    out = _git_ok(
        ["status", "--porcelain=v1", "-z", "--untracked-files=all"], root
    )
    if out is None:
        return None
    tokens = out.split("\0")
    paths: list[str] = []
    i = 0
    while i < len(tokens):
        entry = tokens[i]
        i += 1
        if not entry:
            continue
        status, _, path = entry[:2], entry[2:3], entry[3:]
        if path:
            paths.append(path)
        # Renames/copies emit `XY new\0orig\0`: both paths were touched.
        if status[0] in "RC" or status[1] in "RC":
            if i < len(tokens) and tokens[i]:
                paths.append(tokens[i])
            i += 1
    return paths


def merge_state(root: str) -> str | None:
    """Name the in-progress merge, or `None` if there isn't one.

    `git merge --no-ff --no-commit` leaves `MERGE_HEAD`; `git merge --squash` leaves
    `SQUASH_MSG` and no `MERGE_HEAD`. Either one means **git** built the index from a merge,
    which is the whole precondition `--merge` rests on: the index is then the merge result
    versus HEAD, not an arbitrary pile of `git add`s.
    """
    if _git_ok(["rev-parse", "-q", "--verify", "MERGE_HEAD"], root) is not None:
        return "MERGE_HEAD"
    path = _git_ok(["rev-parse", "--git-path", "SQUASH_MSG"], root)
    if path and os.path.exists(os.path.join(root, path.strip())):
        return "SQUASH_MSG"
    return None


#: What `--merge` classifies, said out loud in the printed decision.
MERGE_SUBJECT = "merge index vs HEAD — exactly what this merge puts on main"

#: The misuse guard. `--merge` outside a merge cannot justify its own narrowing, so it
#: fails closed to the full gate rather than classifying whatever happens to be staged.
NOT_A_MERGE = (
    "--merge outside an in-progress merge: nothing guarantees the index is a merge result, "
    "so the narrowing is unjustified and the full gate runs"
)


@dataclass(frozen=True)
class Source:
    """Where the changed-file list came from, for printing."""

    description: str
    paths: list[str] | None
    forced: str | None = None
    #: Uncommitted paths deliberately left out of the classified set (`--merge` only).
    #: Printed with the class they would have had, so the narrowing is visible, never silent.
    outside: tuple[str, ...] = ()


def merge_source(
    staged: list[str] | None, uncommitted: list[str] | None, state: str | None
) -> Source:
    """Build the `--merge` Source from three already-gathered git facts. Pure.

    `staged` is `git diff --cached` (the merge result vs HEAD), `uncommitted` is the whole
    working-tree change set including untracked, `state` names the in-progress merge.

    The narrowing happens here and nowhere else, so it is one testable function: classify
    `staged`, and carry everything in `uncommitted` that is not in it as `outside` — printed,
    never classified, never silently dropped.
    """
    if state is None:
        return Source("merge index", None, forced=NOT_A_MERGE)
    if staged is None or uncommitted is None:
        return Source(
            "merge index", None, forced="cannot read the index or the working tree"
        )
    inside = {normalize(p) for p in staged if normalize(p)}
    outside = tuple(sorted({normalize(p) for p in uncommitted if normalize(p)} - inside))
    return Source(f"{MERGE_SUBJECT} [{state}]", list(staged), outside=outside)


def resolve_source(args, root: str) -> Source:
    """Pick the diff to classify, and say so.

    Explicit wins: `--merge`, `--files`, `--staged`, `--base`, `--worktree`. Otherwise:

    * In GitHub Actions on a pull request, the PR base (`origin/$GITHUB_BASE_REF`).
    * In GitHub Actions on a push, **the full gate**. There is no base to compare against
      that is worth trusting, and main is the branch everything else is measured from, so
      it gets verified whole. PRs get the cheap classified gate; main never does.
    * Locally, the merge base with `main` **plus** everything uncommitted — which on `main`
      itself degenerates to just the uncommitted set, exactly as it should.
    """
    if args.merge:
        return merge_source(
            staged_changes(root), worktree_changes(root), merge_state(root)
        )

    if args.files is not None:
        return Source("explicit --files", list(args.files))

    if args.staged:
        return Source("staged changes", staged_changes(root))

    if args.base:
        paths = committed_changes(root, args.base)
        if paths is None:
            return Source(
                f"merge base with {args.base}",
                None,
                forced=f"cannot resolve a merge base with {args.base!r}",
            )
        return Source(f"merge base with {args.base}", paths)

    if args.worktree:
        return Source("uncommitted (staged + unstaged + untracked)", worktree_changes(root))

    if os.environ.get("GITHUB_ACTIONS") == "true":
        base_ref = os.environ.get("GITHUB_BASE_REF", "").strip()
        if not base_ref:
            return Source(
                "CI push build",
                None,
                forced="CI push build: no pull-request base, so the full gate runs",
            )
        remote_ref = f"origin/{base_ref}"
        paths = committed_changes(root, remote_ref)
        if paths is None:
            return Source(
                f"CI pull request, merge base with {remote_ref}",
                None,
                forced=(
                    f"cannot resolve {remote_ref!r} — the checkout needs fetch-depth: 0"
                ),
            )
        return Source(f"CI pull request, merge base with {remote_ref}", paths)

    default_branch = args.default_branch
    committed = committed_changes(root, default_branch)
    if committed is None:
        return Source(
            f"merge base with {default_branch} + uncommitted",
            None,
            forced=f"cannot resolve a merge base with {default_branch!r}",
        )
    uncommitted = worktree_changes(root)
    if uncommitted is None:
        return Source(
            f"merge base with {default_branch} + uncommitted",
            None,
            forced="cannot read the working tree status",
        )
    return Source(
        f"merge base with {default_branch} + uncommitted", committed + uncommitted
    )


# ---------------------------------------------------------------------------
# Printing and running
# ---------------------------------------------------------------------------

_MAX_PRINTED = 25


def render(decision: Decision, source: Source, phase: str) -> list[str]:
    """The decision, as printed lines. Always printed, before anything runs."""
    out = [f"gate: source   = {source.description}"]
    if decision.forced_reason:
        out.append(f"gate: FULL     = {decision.forced_reason}")
    else:
        out.append(f"gate: changed  = {len(decision.files)} file(s)")
    out.append(f"gate: class    = {decision.label}")

    deciding = decision.deciding()
    if deciding:
        what = "deciding files" if not decision.is_full else "files forcing the full gate"
        out.append(f"gate: {what}:")
        for path, klass, reason in deciding[:_MAX_PRINTED]:
            out.append(f"gate:   {path}  [{klass}] {reason}")
        if len(deciding) > _MAX_PRINTED:
            out.append(f"gate:   ... and {len(deciding) - _MAX_PRINTED} more")

    if source.outside:
        out.append(
            f"gate: outside  = {len(source.outside)} uncommitted path(s) NOT in this merge, "
            "listed with the class they would have had:"
        )
        for path in source.outside[:_MAX_PRINTED]:
            klass, reason = classify_path(path)
            out.append(f"gate:   {path}  [would be {klass}] {reason}")
        if len(source.outside) > _MAX_PRINTED:
            out.append(f"gate:   ... and {len(source.outside) - _MAX_PRINTED} more")
        out.append(
            "gate:            none of these can reach main through this merge. They CAN "
            "change what a suite that does run sees, so judge the run, not the class."
        )

    commands = decision.commands(phase)
    phase_note = "" if phase == PHASE_ALL else f" (phase: {phase})"
    if decision.is_empty:
        out.append(f"gate: suites   = none — nothing changed{phase_note}")
    elif commands:
        out.append(
            "gate: suites   = " + "; ".join(" ".join(c) for c in commands) + phase_note
        )
    else:
        out.append(f"gate: suites   = none for this class{phase_note}")
        if DOCS in decision.classes:
            out.append(
                "gate:            docs-only: this repo has no link checker and no "
                "markdown linter, so there is nothing to run."
            )
    return out


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="just gate",
        description=(
            "Classify the current diff and run exactly the suites it needs. "
            "Unclassified paths run the full gate."
        ),
    )
    src = parser.add_argument_group("what to classify (default: merge base with main + uncommitted)")
    src.add_argument(
        "--merge",
        action="store_true",
        help=(
            "the coordinator's per-merge gate: classify the index of an in-progress merge "
            "(what this merge puts on main). Requires a merge in progress, or it runs full."
        ),
    )
    src.add_argument("--base", metavar="REF", help="diff against the merge base with REF")
    src.add_argument("--staged", action="store_true", help="classify the staged set only")
    src.add_argument(
        "--worktree", action="store_true", help="classify uncommitted changes only"
    )
    src.add_argument(
        "--files", nargs="*", metavar="PATH", help="classify this explicit path list"
    )
    src.add_argument("--default-branch", default="main", help="default: main")
    parser.add_argument(
        "--phase",
        choices=PHASES,
        default=PHASE_ALL,
        help="run only half the suites: `check` (lint/test) or `acceptance`",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="print the decision and the suites, run nothing",
    )
    parser.add_argument(
        "--root", default=None, help="repo root (default: the git toplevel)"
    )
    args = parser.parse_args(argv)

    root = args.root
    if root is None:
        rc, out = _git(["rev-parse", "--show-toplevel"], os.getcwd())
        root = out.strip() if rc == 0 and out.strip() else os.getcwd()

    source = resolve_source(args, root)
    if source.forced or source.paths is None:
        decision = forced_full(source.forced or "could not read the diff")
    else:
        decision = classify(source.paths)

    for line in render(decision, source, args.phase):
        print(line, flush=True)

    commands = decision.commands(args.phase)
    if args.dry_run or not commands:
        return 0

    if shutil.which("just") is None:
        print("gate: `just` is not on PATH — cannot run the suites.", file=sys.stderr)
        return 1

    # T-400: the suites the gate itself launches get T-144's build flags (env only — this
    # changes nothing about *which* commands run or what they assert, only how cargo builds
    # while they run). Printed so the override is visible, not a silent side effect.
    env = suite_env(dict(os.environ))
    print(
        "gate: build env = "
        + " ".join(f"{k}={v}" for k, v in GATE_BUILD_ENV.items())
        + " (T-400, this process only)",
        flush=True,
    )

    for cmd in commands:
        print(f"gate: running {' '.join(cmd)}", flush=True)
        rc = subprocess.run(cmd, cwd=root, env=env, check=False).returncode
        if rc != 0:
            print(f"gate: FAILED {' '.join(cmd)} (exit {rc})", file=sys.stderr)
            return rc
    print(f"gate: passed ({decision.label})", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
