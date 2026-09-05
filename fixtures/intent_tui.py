#!/usr/bin/env python3
"""A tiny TUI for tui_intent end-to-end tests.

Draws two buttons ([Cancel] [ Save ]) on the alternate screen with mouse
reporting ENABLED (1000 press+release, SGR encoding). Tab MOVES FOCUS
between the buttons (redrawn in reverse video) WITHOUT activating —
this is what lets the intent executor's focus-secured plan (a Tab hop
plus an AssertFocus guard) verify the focus move landed before the
payload. A click also moves focus (never activates on its own), so both
focus primitives are non-activating here. Activating Save (any key
while focused) prints "saved." and exits; the app logs each activation
to ACTIVATIONS so the double-activation test can count them exactly.
"""
import sys
import select
import tty
import os

# Debug trace (path override via env; off by default).
_dbg = os.environ.get("INTENT_TUI_LOG")
# Activation ledger (path override via env; off by default): every
# activation of Save appends a line, so tests can count activations
# EXACTLY (finding 3B: the plan must activate once, never twice).
_act = os.environ.get("INTENT_TUI_ACTIVATIONS")


def log(msg):
    if _dbg:
        with open(_dbg, "a") as f:
            f.write(msg + "\n")


def record_activation():
    if _act:
        with open(_act, "a") as f:
            f.write("save-activated\n")


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
    for _ in range(8):
        ev = read_event()
        if ev is None:
            break
        if ev[0] == "click":
            _, x, y, release = ev
            if not release:
                continue  # act on release, like most toolkits
            # Save sits at columns 12..18, Cancel at 2..10 (1-based, row 1).
            # A click MOVES FOCUS (never activates): the harness's
            # focus-secured plans rely on focus primitives not firing the
            # control (finding 3B).
            if y == 1 and 12 <= x <= 18:
                focus = "save"
                draw(focus)
            elif y == 1 and 2 <= x <= 10:
                focus = "cancel"
                draw(focus)
        elif ev[0] == "key":
            key = ev[1]  # a 1-char str (latin1) or a bytes escape tail
            # Tab is pure focus traversal: Cancel <-> Save, never
            # activating (finding 3A: a focus verb must not fire the
            # target). `read_event` returns the raw byte for a bare key
            # (latin1-decoded str) or a bytes tail after ESC.
            if key in ("\t", b"\t"):
                focus = "save" if focus == "cancel" else "cancel"
                draw(focus)
                continue
            # Any other key activates the focused control.
            if focus == "save":
                record_activation()
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
