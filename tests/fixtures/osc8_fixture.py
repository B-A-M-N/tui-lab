#!/usr/bin/env python3
"""OSC8 hyperlink fixture: renders one hyperlink then plain text."""
import sys, time
sys.stdout.write("\x1b[H\x1b[2J")
sys.stdout.write("before ")
sys.stdout.write("\x1b]8;;https://example.com/docs\x1b\\docs\x1b]8;;\x1b\\")
sys.stdout.write(" after\n")
sys.stdout.flush()
time.sleep(30)
