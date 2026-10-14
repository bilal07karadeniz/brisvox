"""Offline tests of the pure parts of the load generator (no LiveKit, no network).

    python -m unittest discover -s bench/livekit_baseline/tests

They pin the V2V definition and the verdict rules to the Rust ones (crates/brisvox-bench:
``v2v/measure.rs``, ``capacity/mod.rs``, ``capacity/report.rs``).
"""

import sys
import unittest
from pathlib import Path

import numpy as np

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent))

import loadgen  # noqa: E402
from loadgen import Fixture, Judge, Ramp, Segment, Tracker, conclude, pad_gaps, summarize  # noqa: E402

RATE = 16000


def fixture(segments):
    total = max(s[1] for s in segments) + RATE
    return Fixture("t", RATE, np.ones(total, dtype="<i2"), [Segment(a, b, "en", "x") for a, b in segments])


JUDGE = Judge(limit_us=1_200_000, floor_ms=950, over_floor_ms=250, max_bad_rate=0.0, lateness_limit_ms=100, late_share_limit=0.01)


class TrackerTest(unittest.TestCase):
    def test_latency_is_first_audible_arrival_after_eos(self):
        fx = fixture([(0, RATE), (4 * RATE, 5 * RATE)])  # EOS at 1 s and 5 s
        t = Tracker(fx)
        t.observe(1.95)
        t.observe(1.98)
        t.observe(5.9)
        r = t.results()
        self.assertEqual((r[0]["outcome"], r[0]["v2v_us"]), ("answered", 950_000))
        self.assertEqual((r[1]["outcome"], r[1]["v2v_us"]), ("answered", 900_000))

    def test_arrival_after_next_utterance_started_is_talked_over(self):
        fx = fixture([(0, RATE), (2 * RATE, 3 * RATE)])
        t = Tracker(fx)
        t.observe(2.5)
        self.assertEqual(t.results()[0]["outcome"], "talked_over")

    def test_audio_within_the_guard_before_eos_is_bot_speaking(self):
        fx = fixture([(0, RATE)])
        t = Tracker(fx)
        t.observe(0.95)
        t.observe(1.5)
        self.assertEqual(t.results()[0]["outcome"], "bot_speaking")

    def test_no_audio_is_no_response(self):
        self.assertEqual(Tracker(fixture([(0, RATE)])).results()[0]["outcome"], "no_response")


class PadGapsTest(unittest.TestCase):
    def test_short_gaps_are_padded_and_boundaries_shift(self):
        fx = fixture([(RATE, 2 * RATE), (3 * RATE, 4 * RATE)])  # gap 1 s
        padded = pad_gaps(fx, 4.0)
        self.assertEqual(padded.segments[0].start, RATE)
        self.assertEqual(padded.segments[1].start - padded.segments[0].end, 4 * RATE)
        self.assertEqual(padded.segments[1].end - padded.segments[1].start, RATE)
        self.assertEqual(padded.pcm.size, fx.pcm.size + 3 * RATE)

    def test_long_gaps_are_kept(self):
        fx = fixture([(0, RATE), (6 * RATE, 7 * RATE)])
        self.assertEqual(pad_gaps(fx, 4.0).pcm.size, fx.pcm.size)


def call(v2vs, failure=None):
    return {
        "failure": failure,
        "turns": [{"index": i, "outcome": "answered" if v else "no_response", "v2v_us": v} for i, v in enumerate(v2vs)],
        "max_late_us": 0,
        "fixture_frames": 100,
        "late_frames": 0,
    }


class SummarizeTest(unittest.TestCase):
    def test_nearest_rank_percentiles(self):
        d = loadgen.dist(list(range(1, 101)))
        self.assertEqual((d["p50_us"], d["p95_us"], d["p99_us"], d["max_us"]), (50, 95, 99, 100))

    def test_pass_and_slo_failure(self):
        ok = summarize(1, 2, [call([1_000_000, 1_100_000])], JUDGE)
        self.assertEqual(ok["verdict"], "pass")
        slow = summarize(1, 2, [call([1_000_000, 1_300_000])], JUDGE)
        self.assertEqual(slow["verdict"], "fail")
        self.assertIn("p95 V2V 1300 ms is above the limit 1200 ms", slow["reasons"][0])

    def test_a_lost_turn_or_failed_call_fails_the_step(self):
        self.assertEqual(summarize(1, 2, [call([1_000_000, None])], JUDGE)["verdict"], "fail")
        self.assertEqual(summarize(1, 2, [call([1_000_000, 1_000_000], failure="x")], JUDGE)["verdict"], "fail")

    def test_late_generator_with_healthy_server_is_generator_limited(self):
        c = call([1_000_000, 1_000_000])
        c["late_frames"] = 5
        self.assertEqual(summarize(1, 2, [c], JUDGE)["verdict"], "generator_limited")


class RampTest(unittest.TestCase):
    def test_stops_at_first_failure_and_bisects(self):
        r = Ramp([5, 10], resolution=1, keep_going=False)
        self.assertEqual(r.next_step(), 5)
        r.record(5, "pass")
        self.assertEqual(r.next_step(), 10)
        r.record(10, "fail")
        self.assertEqual(r.next_step(), 7)
        r.record(7, "pass")
        self.assertEqual(r.next_step(), 8)
        r.record(8, "fail")
        self.assertIsNone(r.next_step())

    def test_conclusion(self):
        steps = [{"concurrent": n, "verdict": v} for n, v in [(5, "pass"), (7, "pass"), (8, "fail"), (10, "fail")]]
        self.assertEqual(conclude(steps), {"max_concurrent": 7, "first_failure": 8, "limit": "found"})
        self.assertEqual(conclude([{"concurrent": 1, "verdict": "fail"}])["limit"], "first_step_failed")


if __name__ == "__main__":
    unittest.main()
