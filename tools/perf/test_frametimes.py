import json
import struct
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from frametimes import Entry, Ring, RingError, nearest_rank, read_ring, summarize

SCRIPT = Path(__file__).resolve().with_name("frametimes.py")
MS = 1_000_000
SECOND = 1_000_000_000
START = 5 * SECOND


def ring_bytes(capacity, written, slots, magic=b"ECLFRAME"):
    entries = list(slots) + [(0, 0, 0)] * (capacity - len(slots))
    return struct.pack("<8sQQ40x", magic, capacity, written) + b"".join(
        struct.pack("<QII", *entry) for entry in entries
    )


def ring_of(entered, seam=0, driver=0):
    return Ring(tuple(Entry(at, seam, driver) for at in entered), 0)


class ReadRingTest(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.path = Path(temp.name) / "ring.bin"

    def read(self, data):
        self.path.write_bytes(data)
        return read_ring(self.path)

    def test_entries_follow_write_order_after_a_wrap(self):
        slots = [(START + 4 * MS, 4, 40), (START + 5 * MS, 5, 50)]
        slots += [(START + 2 * MS, 2, 20), (START + 3 * MS, 3, 30)]
        ring = self.read(ring_bytes(4, 6, slots))
        self.assertEqual(
            [(entry.entered_ns - START) // MS for entry in ring.entries], [2, 3, 4, 5]
        )
        self.assertEqual([entry.seam_ns for entry in ring.entries], [2, 3, 4, 5])
        self.assertEqual([entry.driver_ns for entry in ring.entries], [20, 30, 40, 50])
        self.assertEqual(ring.dropped, 0)

    def test_a_ring_that_never_wrapped_reads_only_written_slots(self):
        ring = self.read(ring_bytes(4, 2, [(START, 1, 2), (START + MS, 3, 4)]))
        self.assertEqual(ring.entries, (Entry(START, 1, 2), Entry(START + MS, 3, 4)))

    def test_torn_and_stale_slots_are_dropped(self):
        slots = [(START + 4 * MS, 0, 0), (START + 1 * MS, 0, 0)]
        slots += [(START + 2 * MS, 0, 0), (0, 0, 0)]
        ring = self.read(ring_bytes(4, 6, slots))
        self.assertEqual(
            [(entry.entered_ns - START) // MS for entry in ring.entries], [2, 4]
        )
        self.assertEqual(ring.dropped, 2)

    def test_wrong_magic_is_rejected(self):
        with self.assertRaises(RingError) as raised:
            self.read(ring_bytes(4, 0, [], magic=b"NOTFRAME"))
        self.assertIn("not an Eclipse frame-time log", str(raised.exception))

    def test_a_truncated_file_is_rejected(self):
        with self.assertRaises(RingError) as raised:
            self.read(ring_bytes(4, 1, [(START, 0, 0)])[:-1])
        self.assertIn("a frame-time log of 4 entries holds 128", str(raised.exception))

    def test_a_file_shorter_than_the_header_is_rejected(self):
        with self.assertRaises(RingError) as raised:
            self.read(b"ECLFRAME")
        self.assertIn("fewer than the 64-byte", str(raised.exception))


class SummarizeTest(unittest.TestCase):
    def test_percentiles_and_outlier_counts_on_a_known_series(self):
        intervals = [10] * 16 + [25, 30, 150, 200]
        entered = [START]
        for interval in intervals:
            entered.append(entered[-1] + interval * MS)
        ring = Ring(
            tuple(
                Entry(at, (index + 1) * 1000, (index + 1) * MS)
                for index, at in enumerate(entered)
            ),
            3,
        )
        summary = summarize(ring, 0, 60)
        self.assertEqual(summary["presents"], 21)
        self.assertEqual(
            summary["interval_ms"],
            {"p50": 10.0, "p90": 30.0, "p99": 200.0, "max": 200.0},
        )
        self.assertEqual(summary["over_twice_median"], 4)
        self.assertEqual(summary["over_100_ms"], 2)
        self.assertEqual(summary["mean_fps"], round(20 / 0.565, 2))
        self.assertEqual(summary["seam_ms"], {"p50": 0.011, "p99": 0.021, "max": 0.021})
        self.assertEqual(summary["driver_ms"], {"p50": 11.0, "p99": 21.0, "max": 21.0})
        self.assertEqual(summary["dropped"], 3)

    def test_the_window_is_relative_to_the_first_present(self):
        ring = ring_of([START + second * SECOND for second in range(0, 20)])
        early = summarize(ring, 0, 10)
        late = summarize(ring, 10, 70)
        self.assertEqual(early["presents"], 10)
        self.assertEqual(late["presents"], 10)
        self.assertEqual(late["interval_ms"]["p50"], 1000.0)

    def test_an_empty_window_has_no_statistics(self):
        summary = summarize(ring_of([START, START + MS]), 10, 70)
        self.assertEqual(summary["presents"], 0)
        self.assertIsNone(summary["mean_fps"])
        self.assertEqual(
            summary["interval_ms"], {"p50": None, "p90": None, "p99": None, "max": None}
        )
        self.assertEqual(summary["over_twice_median"], 0)

    def test_nearest_rank_picks_an_observed_value(self):
        self.assertEqual(nearest_rank([4, 1, 3, 2], 50), 2)
        self.assertEqual(nearest_rank([4, 1, 3, 2], 95), 4)
        self.assertEqual(nearest_rank([7], 1), 7)


class CommandLineTest(unittest.TestCase):
    def test_prints_the_window_summary_as_json(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "ring.bin"
            slots = [(START + second * SECOND, 0, 0) for second in range(4)]
            path.write_bytes(ring_bytes(8, 4, slots))
            completed = subprocess.run(
                [sys.executable, SCRIPT, path, "--from", "1", "--to", "3"],
                capture_output=True,
                encoding="utf-8",
                check=True,
                timeout=30,
            )
        self.assertEqual(json.loads(completed.stdout)["presents"], 2)

    def test_an_unreadable_ring_exits_1_with_the_reason(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "ring.bin"
            path.write_bytes(b"short")
            completed = subprocess.run(
                [sys.executable, SCRIPT, path],
                capture_output=True,
                encoding="utf-8",
                timeout=30,
            )
        self.assertEqual(completed.returncode, 1)
        self.assertIn("frametimes.py:", completed.stderr)
        self.assertIn("header", completed.stderr)


if __name__ == "__main__":
    unittest.main()
