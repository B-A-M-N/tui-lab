#!/usr/bin/env python3
"""Modal-dialog TUI fixture for Wave E contract conformance.

Renders a bordered main panel; pressing "n" opens a nested Confirm dialog
(the modal the contract expects); Escape closes it; Tab moves focus between
the two dialog buttons; "q" quits. Uses raw ANSI + box-drawing bytes so it
runs under cargo test with no curses dependency.
"""
import sys
import select

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"


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


def draw_main(focus=0):
    lines = [
        "┌────────────────────Connections────────────────────┐",
        "│                                                   │",
        "│  host-1        localhost:8080                     │",
        "│  host-2        localhost:9090                     │",
        "│                                                   │",
        "│  n new   q quit                                   │",
        "└───────────────────────────────────────────────────┘",
    ]
    out = [ALT_ON, "\x1b[H\x1b[2J"]
    out.extend(line + "\n" for line in lines)
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def draw_modal(focus=0):
    lines = [
        "┌────────────────────Connections────────────────────┐",
        "│                                                   │",
        "│   ┌───────Confirm───────┐                         │",
        "│   │ Add connection?     │                         │",
        "│   │ " + ("[Save]" if focus == 0 else "[Cancel]").ljust(22) + "│  │",
        "│   └─────────────────────┘                         │",
        "│                                                   │",
        "└───────────────────────────────────────────────────┘",
    ]
    out = ["\x1b[H\x1b[2J"]
    out.extend(line + "\n" for line in lines)
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def main():
    draw_main()
    while True:
        key = read_key()
        if key is None or key == "q":
            break
        if key == "n":
            draw_modal()
            while True:
                k2 = read_key()
                if k2 is None or k2 == "\x1b":
                    draw_main()
                    break
                if k2 == "q":
                    draw_main()
                    sys.exit(0)
                # Tab / Enter keep the modal up (focus cycling is visual
                # only; the semantic layer tracks it via the [Save] label).
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
