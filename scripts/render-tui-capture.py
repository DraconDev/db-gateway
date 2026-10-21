#!/usr/bin/env python3
"""Rebuild the screen a terminal program drew, from its raw pty output.

A terminal is a grid: the program moves a cursor around it and writes
characters at that position. This is a small model of that — enough to
rebuild the final screen of a captured `dg tui` session, so the
evidence pack holds something a person can read instead of a stream of
escape sequences.

Used by `scripts/capture-tui-evidence.sh`.
"""

import re
import sys

CURSOR_HOME = re.compile(r"\x1b\[(\d+);(\d+)H")
CURSOR_ELSEWHERE = re.compile(r"\x1b\[[0-9;?]*[a-zA-Z]")


class Screen:
    """A grid of characters, written the way a terminal writes them."""

    def __init__(self) -> None:
        self.rows: dict[int, dict[int, str]] = {}
        self.row = 1
        self.column = 1

    def move(self, row: int, column: int) -> None:
        self.row = max(1, row)
        self.column = max(1, column)

    def write(self, text: str) -> None:
        for character in text:
            if character == "\n":
                self.row += 1
                self.column = 1
                continue
            if character == "\r":
                self.column = 1
                continue
            if character == "\x1b":
                continue
            self.rows.setdefault(self.row, {})[self.column] = character
            self.column += 1

    def text(self) -> list[str]:
        """The screen as lines, with the trailing space trimmed."""
        if not self.rows:
            return []
        height = max(self.rows)
        lines = []
        for row in range(1, height + 1):
            cells = self.rows.get(row, {})
            if not cells:
                lines.append("")
                continue
            width = max(cells)
            line = "".join(cells.get(column, " ") for column in range(1, width + 1))
            lines.append(line.rstrip())
        return [line for line in lines if line.strip()]


def rebuild(raw: str) -> list[str]:
    """The final screen of a captured session."""
    screen = Screen()
    index = 0
    while index < len(raw):
        match = CURSOR_HOME.match(raw, index)
        if match:
            screen.move(int(match.group(1)), int(match.group(2)))
            index = match.end()
            continue
        if raw[index] == "\x1b":
            stripped = CURSOR_ELSEWHERE.match(raw, index)
            index = stripped.end() if stripped else index + 1
            continue
        screen.write(raw[index])
        index += 1
    return screen.text()


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: render-tui-capture.py RAW_LOG OUT_TEXT", file=sys.stderr)
        return 2
    raw = open(sys.argv[1], "rb").read().decode("utf-8", "replace")
    lines = rebuild(raw)
    with open(sys.argv[2], "w") as out:
        out.write("A real `dg tui` session against a real gateway.\n")
        out.write("Captured by scripts/capture-tui-evidence.sh: the raw pty\n")
        out.write("stream is in the .log beside this file, and the screen\n")
        out.write("below is what the terminal showed after the query ran.\n")
        out.write("\n")
        out.write("\n".join(lines))
        out.write("\n")
    print(f"rebuilt {len(lines)} lines into {sys.argv[2]}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
