#!/usr/bin/env python3
"""A tiny TUI for tui_intent end-to-end tests.

Draws two buttons ([Cancel] [ Save ]) on the alternate screen with mouse
reporting ENABLED (1000 press+release, SGR encoding) so the intent
executor's EnsureFocus mouse click is protocol-legal. A click MOVES FOCUS
to the clicked button (redrawn in reverse video) — this is what lets the
plan's AssertFocus guard verify the focus move actually landed. Activating
Save (click when focused, or any key while focused) prints "saved." and
exits.
"""
import sys
import select
import tty
import os

# Debug trace (path override via env; off by default).
_dbg = os.environ.get("INTENT_TUI_LOG")


def log(msg):
    if _dbg:
        with open(_dbg, "a") as f:
            f.write(msg + "\n")


# Raw mode is required under a real PTY: the line discipline otherwise
# buffers reads until Enter, so mouse bytes would never reach the select
# loop (same constraint the dialog fixture documents).
try:
    tty.setraw(0)
    log("setraw ok")
except Exception as e:
    log(f"setraw failed: {e}")  # pipes (direct-drive tests) have no line discipline

ALT_ON = "\x1b[?1049h"
ALT_OFF = "\x1b[?1049l"
REV = "\x1b[7m"
RESET = "\x1b[0m"
# Enable mouse press+release tracking (1000) with SGR encoding (1006).
MOUSE_ON = "\x1b[?1000h\x1b[?1006h"
MOUSE_OFF = "\x1b[?1006l\x1b[?1000l"


def draw(focus):
    # focus: "cancel" | "save"
    out = [ALT_ON, MOUSE_ON, "\x1b[H\x1b[2J"]
    if focus == "cancel":
        out.append("\x1b[7m[ Cancel ]\x1b[0m [ Save ]\n")
    else:
        out.append("[ Cancel ] \x1b[7m[ Save ]\x1b[0m\n")
    sys.stdout.write("".join(out))
    sys.stdout.flush()


def read_event(timeout=10.0):
    r, _, _ = select.select([sys.stdin], [], [], timeout)
    if not r:
        return None
    # Raw fd reads: sys.stdin's text buffering can swallow bytes past the
    # first one, which eats a press+release pair that arrives back-to-back
    # (exactly what a harness click writes).
    ch = os.read(0, 1)
    if ch == b"\x1b":
        b2 = os.read(0, 1)
        if b2 == b"[":
            seq = b""
            while not seq.endswith((b"M", b"m")):
                c = os.read(0, 1)
                if not c:
                    return None
                seq += c
            trailer = seq[-1:].decode("latin1")
            text = seq[:-1].decode("latin1")
            parts = text.split(";")
            if trailer in ("M", "m") and parts[0].startswith("<"):
                b = int(parts[0].lstrip("<"))
                x = int(parts[1])
                y = int(parts[2])
                return ("click", x, y, trailer == "m")
            return ("garbage", text)
        return ("key", b2)
    return ("key", ch.decode("latin1"))



def main():
    focus = "cancel"
    draw(focus)
    for _ in range(4):
        ev = read_event()
        if ev is None:
            break
        if ev[0] == "click":
            _, x, y, release = ev
            if not release:
                continue  # act on release, like most toolkits
            # Save sits at columns 12..18, Cancel at 2..10 (1-based, row 1).
            if y == 1 and 12 <= x <= 18:
                if focus == "save":
                    sys.stdout.write("saved.\n")
                    sys.stdout.flush()
                    break
                focus = "save"
                draw(focus)
            elif y == 1 and 2 <= x <= 10:
                focus = "cancel"
                draw(focus)
        else:
            # Any key activates the focused control.
            if focus == "save":
                sys.stdout.write("saved.\n")
                sys.stdout.flush()
                break
            # Enter on cancel: just redraw (focus stays).
            draw(focus)
    sys.stdout.write(RESET + MOUSE_OFF + ALT_OFF)
    sys.stdout.flush()
    sys.exit(0)


if __name__ == "__main__":
    main()
