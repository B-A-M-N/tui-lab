#!/usr/bin/env python3
"""Probe E2E fixture: prints a banner, then consumes input SILENTLY.

Echo is disabled on the tty (termios), so a typed secret never appears in
the terminal surface — the E2E can then assert the secret appears nowhere
in the probe's response envelope (whose `after.viewport_text` is a live
screen read) nor in any evidence sink.
"""
import sys
import termios
import select
import time

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"


def main():
    fd = sys.stdin.fileno()
    old = termios.tcgetattr(fd)
    new = termios.tcgetattr(fd)
    new[3] &= ~termios.ECHO
    termios.tcsetattr(fd, termios.TCSANOW, new)
    try:
        sys.stdout.write(ALT_ON + "\x1b[H\x1b[2Jprobe-ready\n")
        sys.stdout.flush()
        # Consume up to ~10s of input without echoing or reacting.
        deadline = time.time() + 10
        while time.time() < deadline:
            r, _, _ = select.select([sys.stdin], [], [], 0.5)
            if r:
                if not sys.stdin.read(1):
                    break
    finally:
        sys.stdout.write(ALT_OFF)
        sys.stdout.flush()
        termios.tcsetattr(fd, termios.TCSADRAIN, old)


if __name__ == "__main__":
    main()
