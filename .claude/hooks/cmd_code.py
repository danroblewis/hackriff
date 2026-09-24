#!/usr/bin/env python3
"""stdin: a Bash tool command. stdout: the same command with its DATA removed, so the hooks'
patterns match what would RUN, not text the command merely carries (user, 2026-09-23: "the hook
should match commands, not text" - a tmux relay that QUOTED the test-server command was blocked,
and edit scripts whose file content mentioned a forbidden recipe were denied three times).

Removed: the body of a heredoc not fed to a shell (`python3 - <<'EOF' ... EOF`, `cat <<EOF`,
`git commit -F - <<EOF`), and the quoted argument of a data-carrying command (`tmux send-keys ...
'...'`, `echo '...'`, `printf '...'`, `-m '...'`, `--body '...'`, `--message '...'`). Kept: every
command word, `bash -c '...'` / `sh -c` / `zsh -c` payloads and heredocs fed to a shell - those
ARE commands. Stdlib only; on any error the hook falls back to the raw text (stricter, never looser).
"""
import re
import sys

SHELL_FED = re.compile(r"\b(bash|sh|zsh)\b[^\n|;&]*<<")
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
        if m and not SHELL_FED.search(line):
            tag = m.group(2)
            i += 1
            while i < len(lines) and lines[i].strip() != tag:
                i += 1
            if i < len(lines):
                out.append(lines[i])
        i += 1
    return "\n".join(out)


def code(text: str) -> str:
    text = strip_heredocs(text)
    prev = None
    while prev != text:                       # several data args on one line
        prev, text = text, DATA_ARG.sub(lambda m: m.group(1) + "''", text)
    return text


if __name__ == "__main__":
    sys.stdout.write(code(sys.stdin.read()))
