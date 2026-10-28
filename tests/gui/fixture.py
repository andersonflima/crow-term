#!/usr/bin/env python3
"""Deterministic pane content for the GUI journeys: prints the fixture, then idles.

Run as the pane program (`crow-term start -- python fixture.py`), so there is no
shell prompt, history or clock in the frame. Every line starts with a label the
checks use to find it in `capture-pane --json`, independent of how a platform's
PTY (ConPTY) reflows the output.
"""
import sys
import time

TOFU = "\U0010FFFD"  # private use, plane 16: no font maps it -> renderer's missing glyph
LINES = [
    "LIGA -> != === <= |>",
    "SOLO - > ! = = = = < = | >",
    "ACCENT é ç ã ñ ü",
    "CJK 界 漢字",
    "EMOJI 🚀",
    "NERD  ",
    "TOFU " + TOFU,
    "ZWJ 👩‍💻",
    "END",
]


def main():
    out = sys.stdout
    out.write("\x1b[2J\x1b[H" + "\r\n".join(LINES))
    out.flush()
    while True:
        time.sleep(3600)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        pass
