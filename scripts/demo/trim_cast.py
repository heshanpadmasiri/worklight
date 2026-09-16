#!/usr/bin/env python3
"""Drop everything an asciinema cast recorded after a given number of seconds.

The demo recording ends by detaching the tmux client, which paints a cleared
screen and a "[detached]" line over the last frame. Trimming by wall clock cuts
those without having to pattern-match tmux's teardown output.

    trim_cast.py <cast> <seconds>
"""

import json
import sys


def main(path: str, limit: float) -> None:
    with open(path, encoding="utf-8") as handle:
        lines = handle.read().splitlines()

    kept, elapsed = [], 0.0
    for index, line in enumerate(lines):
        # The first line is the header; every later line is [delta, code, data].
        if index == 0:
            kept.append(line)
            continue
        event = json.loads(line)
        elapsed += event[0]
        if elapsed > limit:
            break
        kept.append(line)

    with open(path, "w", encoding="utf-8") as handle:
        handle.write("\n".join(kept) + "\n")


if __name__ == "__main__":
    main(sys.argv[1], float(sys.argv[2]))
