"""The key-free mocks of ``--mock-providers``: same numbers as the brisvox_sim mocks.

Offline, no keys. The constants are checked against the Rust source (a changed brisvox default
fails here, so the two systems cannot drift apart silently), the pure helpers directly, the
services as built by the bot, and whole calls over a websocket in real time.
"""

import asyncio
import json
import re
import socket
import sys
import time
import unittest
import wave
from pathlib import Path

HERE = Path(__file__).resolve().parent.parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))

import uvicorn  # noqa: E402
from loguru import logger  # noqa: E402
from websockets.asyncio.client import connect  # noqa: E402

import agent_mapping  # noqa: E402
import bot  # noqa: E402
import mock_providers as mp  # noqa: E402

FRAME = 640  # 20 ms of 16 kHz pcm16

logger.remove()  # Pipecat logs every frame at DEBUG


def rust(path: str) -> str:
    return (REPO / path).read_text(encoding="utf-8")


def const_int(source: str, name: str) -> int:
    return int(re.search(rf"const {name}: \w+ = ([\d_]+);", source)[1].replace("_", ""))


def const_str(source: str, name: str) -> str:
    return re.search(rf'const {name}: &str = "([^"]*)";', source)[1]


class RustParity(unittest.TestCase):
    """Every number the mocks share with the brisvox mocks is read from the Rust source."""

    def test_answers_and_thresholds(self):
        engine = rust("crates/brisvox-bench/src/v2v/engine_target.rs")
        self.assertEqual(mp.ANSWER_EN, const_str(engine, "ANSWER_EN"))
        self.assertEqual(mp.ANSWER_TR, const_str(engine, "ANSWER_TR"))
        self.assertEqual(mp.SPEECH_RMS, float(re.search(r"SPEECH_RMS: f64 = ([\d.]+);", engine)[1]))

    def test_provider_latencies_equal_the_engine_target_defaults(self):
        engine = rust("crates/brisvox-bench/src/v2v/engine_target.rs")
        sim_load = rust("crates/brisvox-bench/src/sim_load.rs")
        defaults = re.search(r"impl Default for ProviderLatencies \{(.*?)\n\}\n", engine, re.S)[1]
        interim = int(re.search(r"stt_interim: (\d+) \* MS", defaults)[1])
        chunk = int(re.search(r"tts_chunk_interval: (\d+) \* MS", defaults)[1])
        c = mp.MockConfig()
        self.assertEqual(c.stt_interim_ms, interim)
        self.assertEqual(c.tts_chunk_interval_ms, chunk)
        self.assertEqual(c.stt_final_ms, const_int(sim_load, "STT_FINAL_MEDIAN_MS"))
        self.assertEqual(c.llm_first_token_ms, const_int(sim_load, "LLM_FIRST_TOKEN_MEDIAN_MS"))
        self.assertEqual(c.tts_first_byte_ms, const_int(sim_load, "TTS_FIRST_BYTE_MEDIAN_MS"))

    def test_vad_defaults_equal_the_sim_energy_detector(self):
        vad = rust("crates/brisvox-sim/src/vad.rs")
        block = re.search(r"impl Default for VadConfig \{(.*?)\n\}\n", vad, re.S)[1]
        self.assertEqual(mp.MockConfig().vad_rms, float(re.search(r"threshold: ([\d.]+)", block)[1]))
        self.assertEqual(mp.MockConfig().vad_start_ms, int(re.search(r"start_after: Duration::from_millis\((\d+)\)", block)[1]))
        self.assertEqual(mp.MockConfig().vad_stop_ms, int(re.search(r"stop_after: Duration::from_millis\((\d+)\)", block)[1]))

    def test_tts_shape_equals_the_mock_tts(self):
        tts = rust("crates/brisvox-sim/src/mock_tts.rs")
        self.assertIn(f"ms_per_char: {mp.MS_PER_CHAR},", tts)
        self.assertIn(f"chunk: Duration::from_millis({mp.TTS_CHUNK_MS}),", tts)
        peak = f"{mp.TONE_PEAK // 1000}_{mp.TONE_PEAK % 1000:03d}"
        self.assertIn(f"if (i / {mp.TONE_HALF_PERIOD_SAMPLES}) % 2 == 0 {{ {peak} }} else {{ -{peak} }}", tts)

    def test_the_floor_is_the_engine_targets_floor(self):
        # 300 (VAD stop) + 150 (STT final) + 350 (first token) + 150 (first byte), with the
        # smart-turn policy: EngineTargetSettings::floor_ms.
        self.assertEqual(mp.MockConfig().floor_ms, 950)
        engine = rust("crates/brisvox-bench/src/v2v/engine_target.rs")
        self.assertIn(".saturating_add(ms(l.llm_first_token))", engine)
        self.assertIn(".saturating_add(ms(l.tts_first_byte))", engine)


class Helpers(unittest.TestCase):
    def test_normalize_drops_punctuation_and_case(self):
        self.assertEqual(mp.normalize("The U.N. also hopes,  to finalize."), "the u n also hopes to finalize")

    def test_the_answer_follows_the_request_and_the_language(self):
        script = [mp.Utterance("Hello there, friend.", "en"), mp.Utterance("Merhaba dünya.", "tr")]
        self.assertEqual(mp.answer_for(script, "hello there friend"), mp.ANSWER_EN)
        self.assertEqual(mp.answer_for(script, "Merhaba dünya"), mp.ANSWER_TR)
        # A speculative request carries a prefix of the transcript (CallScript::responder).
        self.assertEqual(mp.answer_for(script, "merhaba"), mp.ANSWER_TR)
        self.assertIsNone(mp.answer_for(script, "something else"))
        self.assertIsNone(mp.answer_for(script, "   "))

    def test_request_text_is_the_last_user_message(self):
        params = {
            "input": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "Okay, got it."},
                {"role": "user", "content": [{"type": "input_text", "text": "second"}]},
            ]
        }
        self.assertEqual(mp.request_user_text(params), "second")
        self.assertEqual(mp.request_user_text({"input": [{"role": "user", "content": "x"}]}), "x")
        self.assertEqual(mp.request_user_text({}), "")

    def test_interims_are_growing_word_prefixes_without_the_full_text(self):
        self.assertEqual(mp.interims_of("a b c"), ["a", "a b"])
        self.assertEqual(mp.interims_of("single"), [])

    def test_audio_shapes(self):
        self.assertAlmostEqual(mp.rms16(b"\x00\x00" * 100), 0.0)
        self.assertAlmostEqual(mp.rms16((1000).to_bytes(2, "little", signed=True) * 50), 1000.0)
        self.assertEqual(mp.rms16(b""), 0.0)
        chunk = mp.tone_chunk(24000)
        self.assertEqual(len(chunk), 24000 * 40 // 1000 * 2)
        self.assertAlmostEqual(mp.rms16(chunk), 1000.0)
        # 13 characters x 60 ms = 780 ms = 19.5 chunks of 40 ms -> 20.
        self.assertEqual(mp.chunk_count(mp.ANSWER_EN), 20)
        self.assertEqual(mp.chunk_count(""), 1)

    def test_the_script_is_the_fixture_sidecar(self):
        script = mp.load_script(REPO / "fixtures", "en_tr_mixed")
        self.assertEqual([u.lang for u in script], ["en", "tr"])
        self.assertEqual(script[1].answer, mp.ANSWER_TR)


class Services(unittest.IsolatedAsyncioTestCase):
    def config(self, **kwargs) -> bot.BotConfig:
        settings = agent_mapping.load(HERE / "agent.toml")
        return bot.BotConfig(settings=settings, mock=mp.MockConfig(**kwargs))

    async def test_the_pipeline_is_the_real_one_over_mock_services(self):
        class _NoSocket:
            headers: dict = {}

        start = bot.Start(sample_rate=16000, encoding="pcm16", call_id="t", metadata={})
        config = self.config()
        transport = bot.build_transport(_NoSocket(), start)  # type: ignore[arg-type]
        pipeline = bot.build_pipeline(config, transport)
        names = [type(p).__name__ for p in pipeline.processors]
        self.assertEqual(
            names[1:-1],
            [
                "FastAPIWebsocketInputTransport",
                "MockSTTService",
                "LLMUserAggregator",
                "MockResponsesLLMService",
                "MockTTSService",
                "FastAPIWebsocketOutputTransport",
                "LLMAssistantAggregator",
            ],
        )

    async def test_the_energy_vad_counts_32_ms_windows_like_the_brisvox_worker(self):
        analyzer = mp.energy_vad_analyzer(mp.MockConfig())
        analyzer.set_sample_rate(16000)
        self.assertEqual(analyzer.num_frames_required(), 512)
        # 40 ms start -> 2 windows; 300 ms stop -> 10 windows (320 ms): "silence >= 300 ms" at
        # the first window boundary, as in the brisvox VAD worker.
        self.assertEqual((analyzer._vad_start_frames, analyzer._vad_stop_frames), (2, 10))
        loud = (3000).to_bytes(2, "little", signed=True) * 512
        quiet = b"\x00\x00" * 512
        self.assertEqual(analyzer.voice_confidence(loud), 1.0)
        self.assertEqual(analyzer.voice_confidence(quiet), 0.0)

    async def test_the_mock_smart_turn_runs_the_features_and_answers_complete(self):
        import numpy as np

        analyzer = mp.mock_smart_turn_analyzer(3.0)
        self.assertIsNone(getattr(analyzer, "_session", None))  # no ONNX model loaded
        analyzer.set_sample_rate(16000)
        result = analyzer._predict_endpoint(np.zeros(16000, dtype=np.float32))
        self.assertEqual(result, {"prediction": 1, "probability": 1.0})

    async def test_the_tts_speaks_a_square_wave_with_the_first_byte_delay(self):
        from pipecat.frames.frames import TTSAudioRawFrame

        config = mp.MockConfig(tts_first_byte_ms=60, tts_chunk_interval_ms=10)
        tts = mp._tts_class()(config, sample_rate=24000)
        begun = time.perf_counter()
        stamps, frames = [], []
        async for frame in tts.run_tts(mp.ANSWER_EN, "ctx"):
            stamps.append(time.perf_counter() - begun)
            frames.append(frame)
        self.assertEqual(len(frames), 20)
        self.assertTrue(all(isinstance(f, TTSAudioRawFrame) for f in frames))
        self.assertGreaterEqual(stamps[0], 0.055)  # the first byte delay
        self.assertGreaterEqual(stamps[-1] - stamps[0], 19 * 0.009)  # one chunk per interval
        self.assertEqual(len(frames[0].audio), 24000 * 40 // 1000 * 2)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def fixture_pcm() -> bytes:
    with wave.open(str(REPO / "fixtures" / "en_two_utterances.wav"), "rb") as w:
        return w.readframes(w.getnframes())


def peak(pcm: bytes) -> int:
    return max((abs(int.from_bytes(pcm[i : i + 2], "little", signed=True)) for i in range(0, len(pcm) - 1, 2)), default=0)


class Calls(unittest.IsolatedAsyncioTestCase):
    """A whole call in real time against the mock pipeline, measured like brisvox-bench v2v."""

    async def asyncSetUp(self):
        settings = agent_mapping.load(HERE / "agent.toml")
        config = bot.BotConfig(settings=settings, mock=mp.MockConfig())
        self.port = free_port()
        self.server = uvicorn.Server(
            uvicorn.Config(bot.create_app(config), host="127.0.0.1", port=self.port, log_level="error")
        )
        self.serve = asyncio.create_task(self.server.serve())
        for _ in range(200):
            if self.server.started:
                break
            await asyncio.sleep(0.02)
        self.assertTrue(self.server.started)

    async def asyncTearDown(self):
        self.server.should_exit = True
        await asyncio.wait_for(self.serve, 10)

    async def test_the_first_utterance_is_answered_about_a_floor_after_its_end(self):
        first_end = 106_720  # end of the first utterance (fixtures/en_two_utterances.json)
        pcm = fixture_pcm()
        async with connect(f"ws://127.0.0.1:{self.port}/call") as ws:
            await ws.send(json.dumps({"type": "start", "version": 1, "sample_rate": 16000, "call_id": "m1"}))
            started = json.loads(await asyncio.wait_for(ws.recv(), 10))
            self.assertEqual(started["type"], "started")
            arrivals: list[float] = []

            async def receive():
                async for message in ws:
                    if isinstance(message, bytes) and peak(message) >= 200:
                        arrivals.append(time.perf_counter())

            reader = asyncio.create_task(receive())
            begun = time.perf_counter()
            total = (first_end + 24_000) * 2  # the utterance, then 1.5 s of silence
            for k, i in enumerate(range(0, total, FRAME)):
                await asyncio.sleep(max(0.0, begun + k * 0.02 - time.perf_counter()))
                await ws.send(pcm[i : i + FRAME])
            await asyncio.sleep(0.3)
            await ws.send(json.dumps({"type": "stop"}))
            reader.cancel()
        self.assertTrue(arrivals, "no audible answer")
        v2v = arrivals[0] - (begun + first_end / 16000)
        # The nominal floor is 0.95 s (VAD stop 0.3 + STT final 0.15 + first token 0.35 + first
        # byte 0.15), plus the pipeline's own overhead; the energy VAD's window rounding adds
        # up to 0.03 s. Wide bounds: a loaded test machine must not fail this.
        self.assertGreater(v2v, 0.93)
        self.assertLess(v2v, 1.8)


if __name__ == "__main__":
    unittest.main()
