#!/usr/bin/env python3
"""The PreToolUse hook's reading of a Bash command (.claude/hooks/block-full-gate.sh).

    cmd_code.py            stdin: a command -> stdout: the same command with its DATA removed
    cmd_code.py --ports    stdin: a command -> exit 0 if a spec run / test server in it is on its
                           OWN ports (every one >= OWN_FLOOR), exit 1 otherwise

CODE (user, 2026-09-23: "the hook should match commands, not text" - a tmux relay that QUOTED the
test-server command was blocked, and edit scripts whose file content named a recipe were denied
four times): the body of a heredoc not fed to a shell (`python3 - <<'EOF'`, `cat <<EOF`, `git commit
-F - <<EOF`) and the quoted argument of a data-carrying command (tmux send-keys, echo, printf, -m,
--body) are removed. KEPT, because they run: `bash -c '...'`, a heredoc fed to a shell (`bash <<EOF`,
`cat <<EOF | bash`), anything on a line piped into bash/sh/zsh/eval, a double-quoted argument that
holds `$(...)` or a backtick, and - if a heredoc never terminates - the whole text. Known and
accepted: an interpreter-fed heredoc (`python3 - <<EOF`) is treated as data; the guards exist
against accidents, not against a program written to evade them. On any error the hook falls back
to the raw text (stricter, never looser).

PORTS (user lead, 2026-09-23): the 2026-09-22 spec reds beside a gate were PORT sharing. The gate's
lanes are 8791 / 8951 / 8983 +32 per extra lane, each with backend.mjs's 24-port sweep, so even 9
lanes stay below 9216; canvas-journey takes its own HK_E2E_JOURNEY_PORT (default 8801). A run is on
its own ports only if EVERY port it names - each HK_E2E_PORT= / HK_E2E_JOURNEY_PORT= assignment and
each --bind port - is >= OWN_FLOOR, nothing unsets them, and a spec run has both (named, or from
the environment the work runner gives each worker - ops/work-runner.py e2e_port_for).
"""
import os
import re
import sys

OWN_FLOOR = 9216

SHELL_FED = re.compile(r"\b(bash|sh|zsh)\b[^\n|;&]*<<")
PIPED_TO_SHELL = re.compile(r"\|\s*(sudo\s+)?(bash|sh|zsh|eval)\b|\beval\s")
HEREDOC = re.compile(r"<<-?\s*(['\"]?)([A-Za-z_][A-Za-z0-9_]*)\1")
QUOTED = r"('(?:[^']|'\\'')*'|\"(?:\\.|[^\"\\])*\")"
DATA_ARG = re.compile(
    r"(\bsend-keys\b(?:\s+-[A-Za-z]+(?:\s+[^\s'\"]+)?)*\s+|\becho\s+(?:-[neE]+\s+)?|\bprintf\s+|"
    r"(?:^|\s)(?:-m|--body|--message|--notes|--text)\s+)" + QUOTED)


def strip_heredocs(text: str) -> str:
    lines, out, i = text.split("\n"), [], 0
    while i < len(lines):
        line = lines[i]
        out.append(line)
        m = HEREDOC.search(line)
        if m and not SHELL_FED.search(line) and not PIPED_TO_SHELL.search(line):
            tag, j = m.group(2), i + 1
            while j < len(lines) and lines[j].strip() != tag:
                j += 1
            if j >= len(lines):
                return text                   # never terminates: strip nothing
            out.append(lines[j])
            i = j
        i += 1
    return "\n".join(out)


def _strip_args(line: str) -> str:
    if PIPED_TO_SHELL.search(line):
        return line

    def repl(m):
        q = m.group(2)
        if q.startswith('"') and ("$(" in q or "`" in q):
            return m.group(0)                 # command substitution runs
        return m.group(1) + "''"
    prev = None
    while prev != line:
        prev, line = line, DATA_ARG.sub(repl, line)
    return line


def code(text: str) -> str:
    return "\n".join(_strip_args(ln) for ln in strip_heredocs(text).split("\n"))


def own_ports(text: str, env=os.environ) -> bool:
    c = code(text)
    if re.search(r"\benv\b[^|;&\n]*-u\s*HK_E2E_(JOURNEY_)?PORT\b|\bunset\s+[^;&|\n]*HK_E2E_(JOURNEY_)?PORT\b", c):
        return False
    named = re.findall(r"\b(HK_E2E_PORT|HK_E2E_JOURNEY_PORT)=(\S*)", c)
    for _, v in named:
        if not v.isdigit() or int(v) < OWN_FLOOR:
            return False
    for p in re.findall(r"--bind[= ]\S*?:(\d+)", c):
        if int(p) < OWN_FLOOR:
            return False
    if re.search(r"(^|[;&|(]|\s)hk serve|target/[a-z]*/hk serve", c) and not re.search(r"--bind[= ]\S*?:\d+", c):
        return False                          # hk serve without --bind: its default ports, not ours
    if re.search(r"npm run e2e|node e2e/run\.mjs", c):
        names = {k for k, _ in named}
        for var in ("HK_E2E_PORT", "HK_E2E_JOURNEY_PORT"):
            if var not in names:
                v = str(env.get(var, ""))
                if not v.isdigit() or int(v) < OWN_FLOOR:
                    return False
    return True


if __name__ == "__main__":
    src = sys.stdin.read()
    if sys.argv[1:] == ["--ports"]:
        sys.exit(0 if own_ports(src) else 1)
    sys.stdout.write(code(src))
