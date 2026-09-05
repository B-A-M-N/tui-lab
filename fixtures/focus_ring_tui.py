#!/usr/bin/env python3
"""A three-button TUI whose focus ring MOVES with Tab (scaffold-explore E2E).

Renders three buttons; the focused one is drawn in reverse video — the
exact evidence the semantic focus inferencer keys on. Tab advances the
ring; Escape redraws the initial state; any other key is ignored. Plain
ANSI + box drawing so it runs under cargo test with no curses dep.
"""
import sys
import select

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"

BUTTONS = ["[ Alpha ]", "[ Beta ]", "[ Gamma ]"]


def read_key(timeout=10.0):
    import termios, tty
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    try:
        tty.setraw(fd)
        r, _, _ = select.select([sys.stdin], [], [], timeout)
        if r:
            return sys.stdin.read(1)
        return None
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


def draw(focus=0):
    out = [ALT_ON, "\x1b[H\x1b[2J"]
    out.append("Focus ring fixture\n")
    for i, b in enumerate(BUTTONS):
        if i == focus:
            out.append(REV + b + RESET + "\n")
        else:
            out.append(b + "\n")
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def main():
    focus = 0
    draw(focus)
    while True:
        key = read_key()
        if key is None or key == "q":
            break
        if key == "\t":
            focus = (focus + 1) % len(BUTTONS)
            draw(focus)
        elif key == "\x1b":
            focus = 0
            draw(focus)
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
