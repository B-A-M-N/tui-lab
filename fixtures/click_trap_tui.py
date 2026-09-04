#!/usr/bin/env python3
"""A TUI whose button ACTIVATES on click (the exploration-safety E2E target).

Audit P0-4 / finding 58: semantic exploration resolves a never-focused
control via a MOUSE CLICK. On a real button, that click fires the button.
This fixture draws one bordered panel with a single [ DETONATE ] button and
visibly mutates (button text becomes "[ ACTIVATED ]", plus an ACTIVATED
banner) the moment ANY input arrives — including the click that a
"click-focus" sends.

The E2E contract: `tui_explore mode=semantic max_risk=safe` must finish
without the banner ever appearing. (The legacy explorer labeled this
candidate `Safe` and clicked it; the honest risk class keeps it out of
safe-only runs.)
"""
import sys
import time

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"


def draw(activated=False):
    out = [ALT_ON, "\x1b[H\x1b[2J"]
    width = 34
    title = "Click Trap"
    title_pad = width - 2 - len(title)
    if activated:
        body = "ACTIVATED"
        button = "[ ACTIVATED ]"
    else:
        body = "press nothing"
        button = "[ DETONATE ]"
    body_pad = width - 2 - len(body)
    button_pad = width - 2 - len(button)
    out.append("┌" + "─" * (width - 2) + "┐\n")
    out.append("│" + title + " " * title_pad + "│\n")
    out.append("│" + body + " " * body_pad + "│\n")
    out.append("│" + button + " " * button_pad + "│\n")
    out.append("└" + "─" * (width - 2) + "┘\n")
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def main():
    draw()
    # Any byte — including the synthetic click's escape sequence — flips the
    # trap. We stay alive afterward so the harness can observe the mutated
    # screen; exit on a second input or timeout.
    import select
    r, _, _ = select.select([sys.stdin], [], [], 30)
    if r:
        draw(activated=True)
        select.select([sys.stdin], [], [], 5)
    time.sleep(0.2)
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()


if __name__ == "__main__":
    main()
