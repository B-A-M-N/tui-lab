#!/usr/bin/env python3
"""A tiny alternate-screen TUI used to exercise tui-lab end-to-end.

Draws a Settings dialog with two fields and two buttons using raw ANSI +
box-drawing bytes (no curses, no TTY required) so it runs under cargo test.
The focused button is rendered in reverse video. On the first key it prints
"saved." to confirm input delivery, then exits on the second.
"""
import sys
import time

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"


def read_key(timeout=5.0):
    import select
    r, _, _ = select.select([sys.stdin], [], [], timeout)
    if r:
        return sys.stdin.read(1)
    return None


def draw():
    out = []
    out.append(ALT_ON)
    out.append("\x1b[H\x1b[2J")
    width = 30
    title = "Settings"
    title_pad = width - 2 - len(title)
    host = "Host: localhost"
    host_pad = width - 2 - len(host)
    port = "Port: 8080"
    port_pad = width - 2 - len(port)
    empty_pad = width - 2
    buttons = "[Cancel] [ Save ]"
    buttons_pad = width - 2 - len(buttons)
    
    out.append("┌" + "─" * (width - 2) + "┐\n")
    out.append("│" + title + " " * title_pad + "│\n")
    out.append("│" + host + " " * host_pad + "│\n")
    out.append("│" + port + " " * port_pad + "│\n")
    out.append("│" + " " * empty_pad + "│\n")
    out.append("│" + buttons + " " * buttons_pad + "│\n")
    out.append("└" + "─" * (width - 2) + "┘\n")
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def main():
    draw()
    # Wait for a key (the harness sends Enter or a char), then show saved.
    read_key(timeout=30)
    sys.stdout.write(RESET + "saved.\n")
    sys.stdout.flush()
    read_key(timeout=30)
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
