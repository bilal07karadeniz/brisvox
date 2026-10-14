"""Tests of scripts/bench_live_report.py (python -m unittest, stdlib only): provider timers must be
matched to the bench's turns by call when calls overlap, not by position."""

import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import bench_live_report as r  # noqa: E402

ESC = "\x1b[2m"


def brisvox_log(path: Path) -> None:
    """Two overlapping calls (ws-1 is session s2, ws-2 is session s1); the turns of the two
    sessions interleave in the file and the first summary of a session has no span (it is
    written by the task that starts the next turn: the span comes from the recorder)."""
    lines = [
        'INFO brisvox::server: call admitted transport="websocket" session=s1 call_id=ws-2',
        'INFO brisvox::server: call admitted transport="websocket" session=s2 call_id=ws-1',
        'INFO brisvox::llm: llm upstream provider="openai-compatible" upstream_provider="OpenAI" upstream_model="openai/gpt-6-luna"',
        'INFO session{id=s1}: brisvox::timeline: turn finished epoch=0 interrupted=false llm_rounds=1 stt_final_delay_us=300000 llm_ttft_us=1100000 llm_first_sentence_us=1200000 tts_ttfb_us=200000 response_latency_us=1700000',
        'INFO session{id=s2}: brisvox::timeline: turn finished epoch=0 interrupted=false llm_rounds=1 stt_final_delay_us=310000 llm_ttft_us=900000 llm_first_sentence_us=950000 tts_ttfb_us=210000 response_latency_us=1500000',
        'INFO brisvox::llm: llm upstream provider="openai-compatible" upstream_provider="OpenAI" upstream_model="openai/gpt-6-luna"',
        'INFO session{id=s2}: brisvox::timeline: turn finished epoch=1 interrupted=false llm_rounds=1 stt_final_delay_us=320000 llm_ttft_us=1500000 llm_first_sentence_us=1600000 tts_ttfb_us=220000 response_latency_us=2100000',
        'INFO session{id=s1}: brisvox::timeline: turn finished epoch=1 interrupted=false llm_rounds=1 stt_final_delay_us=330000 llm_ttft_us=700000 llm_first_sentence_us=750000 tts_ttfb_us=230000 response_latency_us=1300000',
        'INFO llm request finished',
        'INFO llm request finished',
        'INFO llm request finished',
        'INFO llm request finished',
    ]
    path.write_text("\n".join(f"{ESC}2026{ESC} {line}" for line in lines) + "\n", encoding="utf-8")


def pipecat_log(path: Path) -> None:
    def metric(kind: str, instance: int, ms: float, call: str) -> str:
        # Deepgram TTS has no model name in Pipecat's metric ("-"), its values are real
        model = "-" if kind == "DeepgramTTS" else "some-model"
        return f"INFO | PCMETRIC ttfb {kind}Service#{instance} {model} {ms} call={call}"

    lines = [
        # the zero metrics at start are ignored
        "INFO | PCMETRIC ttfb DeepgramSTTService#1 - 0.0 call=cA",
        metric("DeepgramSTT", 1, 300.0, "cA"),
        metric("DeepgramSTT", 2, 310.0, "cB"),
        metric("OpenAILLM", 2, 900.0, "cB"),
        metric("OpenAILLM", 1, 1100.0, "cA"),
        metric("CartesiaTTS", 1, 200.0, "cA"),
        metric("DeepgramTTS", 2, 210.0, "cB"),
        "INFO | PCUPSTREAM OpenAILLMService#1 provider=OpenAI model=openai/gpt-6-luna",
        "INFO | PCUPSTREAM OpenAILLMService#2 provider=OpenAI model=openai/gpt-6-luna",
        metric("DeepgramSTT", 2, 320.0, "cB"),
        metric("OpenAILLM", 2, 1500.0, "cB"),
        metric("DeepgramSTT", 1, 330.0, "cA"),
        metric("OpenAILLM", 1, 700.0, "cA"),
        metric("DeepgramTTS", 2, 220.0, "cB"),
        metric("DeepgramTTS", 1, 230.0, "cA"),
    ]
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def bench_json(path: Path, call_ids: list[str], v2v: dict[str, list[float]]) -> None:
    runs = []
    for n, call_id in enumerate(call_ids):
        runs.append(
            {
                "fixture": "f",
                "run": n,
                "call_id": call_id,
                "failure": None,
                "turns": [
                    {"index": i, "lang": "en", "outcome": "answered", "v2v_us": ms * 1000}
                    for i, ms in enumerate(v2v[call_id])
                ],
            }
        )
    path.write_text(
        json.dumps({"runs": runs, "failed_runs": 0, "settings": {"runs": len(call_ids)}, "overall": {}}),
        encoding="utf-8",
    )
    path.with_suffix(".md").write_text("# bench\n\n## Summary\n", encoding="utf-8")


class OverlappingCalls(unittest.TestCase):
    def test_brisvox_turns_carry_their_call(self):
        with tempfile.TemporaryDirectory() as d:
            log = Path(d) / "b.log"
            brisvox_log(log)
            turns = r.brisvox_turns(log)
            self.assertEqual([t["call_id"] for t in turns], ["ws-2", "ws-1", "ws-1", "ws-2"])
            counts = r.upstream_counts("brisvox", log)
            self.assertEqual(counts["providers"], {"OpenAI": 2})
            self.assertEqual(counts["requests"], 4)

    def test_pipecat_turns_group_by_call_not_by_position(self):
        with tempfile.TemporaryDirectory() as d:
            log = Path(d) / "p.log"
            pipecat_log(log)
            turns = r.pipecat_turns(log)
            by_call = {}
            for t in turns:
                by_call.setdefault(t["call_id"], []).append(t)
            self.assertEqual([t["stt_ttfb_ms"] for t in by_call["cA"]], [300.0, 330.0])
            self.assertEqual([t["llm_ttfb_ms"] for t in by_call["cB"]], [900.0, 1500.0])
            self.assertEqual([t["tts_ttfb_ms"] for t in by_call["cB"]], [210.0, 220.0])
            self.assertEqual(r.upstream_counts("pipecat", log)["providers"], {"OpenAI": 2})

    def test_matching_follows_the_bench_call_ids(self):
        runs = [
            {"call_id": "ws-1", "turns": [{}, {}]},
            {"call_id": "ws-2", "turns": [{}, {}]},
        ]
        prov = [
            {"call_id": "ws-2", "x": "a"},
            {"call_id": "ws-1", "x": "b"},
            {"call_id": "ws-1", "x": "c"},
            {"call_id": "ws-2", "x": "d"},
        ]
        matched = r.match_provider_turns(runs, prov)
        self.assertEqual([m["x"] for m in matched], ["b", "c", "a", "d"])
        # a call the log does not know gives None, not someone else's turn
        runs.append({"call_id": "ws-9", "turns": [{}]})
        self.assertIsNone(r.match_provider_turns(runs, prov)[-1])
        # logs without call ids (one call at a time): by position
        plain = [{"call_id": None, "x": i} for i in range(4)]
        self.assertEqual([m["x"] for m in r.match_provider_turns(runs[:2], plain)], [0, 1, 2, 3])

    def run_report(self, system: str) -> dict:
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            if system == "brisvox":
                calls = ["ws-1", "ws-2"]
                v2v = {"ws-1": [2000.0, 2600.0], "ws-2": [2200.0, 1900.0]}
                brisvox_log(d / "b1.brisvox.log")
            else:
                calls = ["cA", "cB"]
                v2v = {"cA": [1700.0, 1600.0], "cB": [1500.0, 2100.0]}
                pipecat_log(d / "b1.pipecat.log")
            bench_json(d / "b1.json", calls, v2v)
            out = d / "out"
            argv = ["bench_live_report.py", "--system", system, "--dir", str(d), "--blocks", "b1",
                    "--out-prefix", str(out), "--concurrency", "2", "--tts", "deepgram_aura", "--pin", "openai"]
            with mock.patch.object(sys, "argv", argv):
                self.assertEqual(r.main(), 0)
            self.assertIn("2 concurrent calls", out.with_suffix(".md").read_text(encoding="utf-8"))
            return json.loads(out.with_suffix(".json").read_text(encoding="utf-8"))

    def test_the_brisvox_report_uses_each_calls_own_timers(self):
        doc = self.run_report("brisvox")
        self.assertEqual(doc["concurrency"], 2)
        self.assertEqual(doc["tts"], "deepgram_aura")
        by = {(t["run"], t["turn"]): t for t in doc["turns"]}
        # ws-1 (run 0) is session s2: its first turn had ttft 900 ms, its second 1500 ms
        self.assertEqual(by[(0, 0)]["llm_ttft_ms"], 900.0)
        self.assertEqual(by[(0, 1)]["llm_ttft_ms"], 1500.0)
        # ws-2 (run 1) is session s1
        self.assertEqual(by[(1, 0)]["llm_ttft_ms"], 1100.0)
        self.assertEqual(by[(1, 1)]["llm_ttft_ms"], 700.0)
        self.assertEqual(doc["failures"]["turns without a provider timeline"], 0)
        self.assertTrue(doc["routing"]["all_pinned"])
        self.assertEqual(doc["routing"]["responses_with_upstream"], 2)
        self.assertIn("p99", doc["v2v"]["pooled"]["all"])

    def test_the_pipecat_report_uses_each_calls_own_timers(self):
        doc = self.run_report("pipecat")
        by = {(t["run"], t["turn"]): t for t in doc["turns"]}
        self.assertEqual(by[(0, 0)]["llm_ttfb_ms"], 1100.0)  # call cA
        self.assertEqual(by[(1, 1)]["llm_ttfb_ms"], 1500.0)  # call cB
        self.assertEqual(by[(1, 0)]["tts_ttfb_ms"], 210.0)
        self.assertEqual(doc["failures"]["turns without a provider timeline"], 0)
        self.assertTrue(doc["routing"]["all_pinned"])


class LogFailureCounters(unittest.TestCase):
    def test_error_and_warning_lines_are_counted(self):
        # the word-boundary patterns once held literal backspace characters and never matched
        with tempfile.TemporaryDirectory() as d:
            d = Path(d)
            log = d / "b1.pipecat.log"
            pipecat_log(log)
            with open(log, "a", encoding="utf-8") as f:
                f.write("ERROR | CartesiaTTSService#1 error: concurrency limit of 2\n")
                f.write("WARNING | something odd\n")
            bench_json(d / "b1.json", ["cA", "cB"], {"cA": [1.0, 1.0], "cB": [1.0, 1.0]})
            out = d / "out"
            argv = ["bench_live_report.py", "--system", "pipecat", "--dir", str(d), "--blocks", "b1",
                    "--out-prefix", str(out)]
            with mock.patch.object(sys, "argv", argv):
                r.main()
            doc = json.loads(out.with_suffix(".json").read_text(encoding="utf-8"))
            self.assertEqual(doc["failures"]["ERROR log lines"], 1)
            self.assertEqual(doc["failures"]["WARN log lines"], 1)
            self.assertEqual(doc["failures"]["provider concurrency / 429 refusals"], 1)


if __name__ == "__main__":
    unittest.main()
