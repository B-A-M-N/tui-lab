#!/usr/bin/env python3
"""A minimal TUI with ONE extractable input field (the sensitive-type E2E).

The field line sits OUTSIDE any border, so the semantic extractor's
field heuristic (`Label: value`, colon not inside a bordered region) sees
it as a Field control — the kind a semantic `type` verb applies to.
Any key received exits quietly.
"""
import sys
import select
import time

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"


def draw():
    sys.stdout.write(ALT_ON + "\x1b[H\x1b[2J")
    sys.stdout.write("Login\n")
    sys.stdout.write("Password: \n")
    sys.stdout.write("[ Submit ]\n")
    sys.stdout.flush()


def main():
    draw()
    # One input is enough for the E2E (the executor's settle); then leave.
    select.select([sys.stdin], [], [], 20)
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()


if __name__ == "__main__":
    main()
