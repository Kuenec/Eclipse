#!/usr/bin/env python3

import argparse
import json
import math
import struct
import sys
from dataclasses import dataclass
from pathlib import Path

MAGIC = b"ECLFRAME"
HEADER = struct.Struct("<8sQQ40x")
ENTRY = struct.Struct("<QII")
NANOS_PER_SECOND = 1_000_000_000
NANOS_PER_MS = 1_000_000
OUTLIER_MS = 100
MEDIAN_MULTIPLE = 2
MS_DIGITS = 3


class RingError(Exception):
    pass


@dataclass(frozen=True)
class Entry:
    entered_ns: int
    seam_ns: int
    driver_ns: int


@dataclass(frozen=True)
class Ring:
    entries: tuple
    dropped: int


def nearest_rank(values, percent):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(percent / 100 * len(ordered)) - 1)]


def read_ring(path):
    data = Path(path).read_bytes()
    if len(data) < HEADER.size:
        raise RingError(
            f"{path} holds {len(data)} bytes, fewer than the "
            f"{HEADER.size}-byte frame-time log header"
        )
    magic, capacity, written = HEADER.unpack_from(data)
    if magic != MAGIC:
        raise RingError(f"{path} is not an Eclipse frame-time log (magic {magic!r})")
    expected = HEADER.size + ENTRY.size * capacity
    if capacity == 0 or len(data) != expected:
        raise RingError(
            f"{path} holds {len(data)} bytes, but a frame-time log of "
            f"{capacity} entries holds {expected}"
        )
    entries = []
    dropped = 0
    previous = 0
    for index in range(written - min(written, capacity), written):
        offset = HEADER.size + ENTRY.size * (index % capacity)
        entry = Entry(*ENTRY.unpack_from(data, offset))
        if entry.entered_ns <= previous:
            dropped += 1
            continue
        entries.append(entry)
        previous = entry.entered_ns
    return Ring(tuple(entries), dropped)


def milliseconds(nanos):
    return round(nanos / NANOS_PER_MS, MS_DIGITS)


def percentiles(values, percents):
    if not values:
        return {f"p{percent}": None for percent in percents} | {"max": None}
    summary = {
        f"p{percent}": milliseconds(nearest_rank(values, percent))
        for percent in percents
    }
    summary["max"] = milliseconds(max(values))
    return summary


def summarize(ring, start_s, end_s):
    window = []
    if ring.entries:
        first = ring.entries[0].entered_ns
        start = first + start_s * NANOS_PER_SECOND
        end = first + end_s * NANOS_PER_SECOND
        window = [entry for entry in ring.entries if start <= entry.entered_ns < end]
    intervals = [
        later.entered_ns - earlier.entered_ns
        for earlier, later in zip(window, window[1:])
    ]
    median = nearest_rank(intervals, 50) if intervals else None
    return {
        "presents": len(window),
        "mean_fps": (
            round(len(intervals) * NANOS_PER_SECOND / sum(intervals), 2)
            if intervals
            else None
        ),
        "interval_ms": percentiles(intervals, (50, 90, 99)),
        "over_twice_median": sum(
            interval > MEDIAN_MULTIPLE * median for interval in intervals
        ),
        "over_100_ms": sum(
            interval > OUTLIER_MS * NANOS_PER_MS for interval in intervals
        ),
        "seam_ms": percentiles([entry.seam_ns for entry in window], (50, 99)),
        "driver_ms": percentiles([entry.driver_ns for entry in window], (50, 99)),
        "dropped": ring.dropped,
    }


def parse_arguments(argv):
    parser = argparse.ArgumentParser(
        description="Summarise an ECLIPSE_FRAMETIME_LOG ring as JSON."
    )
    parser.add_argument("file", type=Path, metavar="FILE")
    parser.add_argument(
        "--from",
        dest="start",
        type=float,
        default=0.0,
        metavar="S",
        help="window start in seconds after the first present (default: 0)",
    )
    parser.add_argument(
        "--to",
        dest="end",
        type=float,
        default=math.inf,
        metavar="S",
        help="window end in seconds after the first present (default: the end)",
    )
    return parser.parse_args(argv)


def main(argv):
    arguments = parse_arguments(argv)
    try:
        ring = read_ring(arguments.file)
    except (OSError, RingError) as error:
        print(f"frametimes.py: {error}", file=sys.stderr)
        return 1
    print(json.dumps(summarize(ring, arguments.start, arguments.end), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
