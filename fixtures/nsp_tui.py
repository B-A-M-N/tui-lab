#!/usr/bin/env python3
"""NativeSemanticProtocol demo fixture (Wave F items 58-63).

A minimal cooperative TUI: renders two buttons whose focus state it KNOWS,
and declares that knowledge through the TUI_LAB_SEMANTIC side channel.
Arrow keys move focus; Enter "activates" (prints a marker); q quits.

Layout (deliberately simple for deterministic tests):
  ┌──────────────────────────────┐
  │  [ Save ]   [ Cancel ]       │
  └──────────────────────────────┘
Focus is reverse-video on the active button.
"""
import os
import sys
import select
import termios
import tty

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import nsproto

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"


def read_key(timeout=10.0):
    fd = sys.stdin.fileno()
    saved = termios.tcgetattr(fd)
    buf = sys.stdin.buffer
    try:
        tty.setraw(fd)
        r, _, _ = select.select([fd], [], [], timeout)
        if not r:
            return None
        # Read through the BufferedReader: bytes beyond the first stay in
        # its internal buffer, so the follow-up read never blocks on the fd
        # (select() alone would misreport — the readahead already emptied
        # the fd into the buffer).
        first = buf.read(1)
        if first == b"\x1b":
            rest = buf.read(2)
            key = (first + rest).decode("latin1")
            if key in ("\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D"):
                return key
            return "\x1b"
        return first.decode("latin1")
    except (AttributeError, IndexError):
        return None
    finally:
        termios.tcsetattr(fd, termios.TCSADRAIN, saved)


def draw(focus):
    save = f"{REV}[ Save ]{RESET}" if focus == 0 else "[ Save ]"
    cancel = f"{REV}[ Cancel ]{RESET}" if focus == 1 else "[ Cancel ]"
    lines = [
        "┌────────────────────────────────┐",
        "│                                │",
        f"│   {save}    {cancel}      │",
        "│                                │",
        "└────────────────────────────────┘",
    ]
    sys.stdout.write("\x1b[H\x1b[2J" + "".join(l + "\n" for l in lines))
    sys.stdout.flush()


def declare(ns, focus):
    """Tell the harness the truth inference cannot know."""
    ns.snapshot(nsproto.node(
        "#root", "screen", label="nsp-demo",
        children=[
            nsproto.node(
                "#save", "button", label="Save",
                bounds=[4, 2, 8, 1], actions=["activate"], focusable=True,
                focused=focus == 0,
            ),
            nsproto.node(
                "#cancel", "button", label="Cancel",
                bounds=[16, 2, 10, 1], actions=["activate"], focusable=True,
                focused=focus == 1,
            ),
        ],
    ))


def main():
    ns = nsproto.Client("nsp-demo", framework="raw-ansi")
    focus = 0
    draw(focus)
    declare(ns, focus)
    while True:
        key = read_key()
        if key is None or key == "q":
            break
        if key in ("\x1b[C", "\x1b[B"):
            focus = (focus + 1) % 2
        elif key in ("\x1b[A", "\x1b[D"):
            focus = (focus - 1) % 2
        elif key == "\r":
            ns.event("activate", "#save" if focus == 0 else "#cancel")
        draw(focus)
        declare(ns, focus)
    sys.stdout.write(ALT_OFF)
    sys.stdout.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
