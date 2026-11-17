"""Checks what `avb-endpointd log` read against what fake_board wrote.

    log-check.py CASE BOARD READ JSON|- MIN_LINES [NAME]

Every line read must be one the board wrote, in the order it wrote them.
A board writes `I (ms) fake: line N ...`, numbering from 1 in each boot.
NAME takes only the lines read under that board's name, as when two are
followed. The case decides what else must hold.
"""

import json
import sys


def lines(path):
    with open(path, encoding="utf-8", errors="replace") as file:
        return [line.rstrip("\n") for line in file]


def main():
    case, board_path, read_path, json_path, least = sys.argv[1:6]
    name = sys.argv[6] if len(sys.argv) > 6 else None
    least = int(least)
    board = lines(board_path)
    read = lines(read_path)
    if name is not None:
        read = [line[len(name) + 2:] for line in read if line.startswith(name + ": ")]
    markers = [line for line in read if line.startswith("[")]
    text = [line for line in read if not line.startswith("[")]
    problems = []

    # In order: each line read is the board's next or a later one, a new
    # boot starting the search over.
    at = 0
    for line in text:
        try:
            at = board.index(line, at) + 1
        except ValueError:
            try:
                at = board.index(line) + 1
            except ValueError:
                problems.append(f"never written: {line!r}")
    if len(text) < least:
        problems.append(f"only {len(text)} lines read, {least} wanted")

    lost = [m for m in markers if "lost" in m]
    restarted = [m for m in markers if "restarted" in m]
    dropped = [m for m in markers if "dropped" in m]
    if case in ("follow", "once", "two"):
        if markers:
            problems.append(f"unexpected markers: {markers}")
        if text != board[: len(text)]:
            problems.append("the lines read are not the board's first lines, all of them")
    if case == "lost" and not lost:
        problems.append("nothing said lost")
    if case == "restart" and len(restarted) != 1:
        problems.append(f"restarts marked: {restarted}")
    if case == "dropped" and not dropped:
        problems.append("no dropped lines said")
    if json_path != "-":
        with open(json_path, encoding="utf-8") as file:
            stats = json.load(file)["boards"][0]
        for key in ("retries", "timeouts", "refused", "lost_octets"):
            if stats[key] != 0:
                problems.append(f"{key} {stats[key]}")
        if stats["lines"] < least or stats["latency_ms_max"] is None:
            problems.append(f"stats: {stats}")

    label = f"{case}{' ' + name if name else ''}"
    print(f"{label}: {len(text)} lines read, {len(board)} written, markers {markers[:3]}"
          f"{' ...' if len(markers) > 3 else ''}")
    for problem in problems[:10]:
        print(f"FAIL {label}: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
