"""The bot: services built from the agent file, and whole calls over a real websocket.

The calls run the provider-free ``--fake`` pipeline (Pipecat's transport, serializer, Silero VAD
and a tone responder) behind uvicorn, with a plain websocket client speaking the brisvox
protocol the way ``brisvox-bench v2v`` does. No keys and no network beyond loopback.
"""

import asyncio
import audioop  # the audioop-lts backport on Python 3.13 (a Pipecat dependency)
import json
import os
import socket
import sys
import unittest
import wave
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent.parent
REPO = HERE.parent.parent
sys.path.insert(0, str(HERE))

import uvicorn  # noqa: E402
from websockets.asyncio.client import connect  # noqa: E402
from websockets.exceptions import ConnectionClosed  # noqa: E402

import agent_mapping  # noqa: E402
import bot  # noqa: E402

FIXTURE = REPO / "fixtures" / "en_two_utterances.wav"
FIRST_END = 106_720  # end of the first utterance (fixtures/en_two_utterances.json)
SECOND_START, SECOND_END = 130_720, 174_880
FRAME = 640  # 20 ms of 16 kHz pcm16
DUMMY_KEYS = {"DEEPGRAM_API_KEY": "dummy", "OPENAI_API_KEY": "dummy", "CARTESIA_API_KEY": "dummy"}


def fixture_pcm() -> bytes:
    with wave.open(str(FIXTURE), "rb") as w:
        assert (w.getframerate(), w.getnchannels(), w.getsampwidth()) == (16000, 1, 2)
        return w.readframes(w.getnframes())


def peak(pcm: bytes) -> int:
    return max((abs(int.from_bytes(pcm[i : i + 2], "little", signed=True)) for i in range(0, len(pcm) - 1, 2)), default=0)


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Services(unittest.TestCase):
    def test_the_services_carry_the_mapped_settings(self):
        settings = agent_mapping.load(HERE / "agent.toml")
        with mock.patch.dict(os.environ, DUMMY_KEYS):
            stt, llm, tts = bot.build_services(bot.BotConfig(settings=settings))
            ws_llm = bot.build_services(bot.BotConfig(settings=settings, llm_transport="websocket"))[1]
        self.assertEqual(type(llm).__name__, "OpenAIResponsesHttpLLMService")
        self.assertEqual(type(ws_llm).__name__, "OpenAIResponsesLLMService")
        s = stt._settings
        self.assertEqual(
            (s.model, str(s.language), s.endpointing, s.smart_format, s.punctuate, s.interim_results),
            ("nova-3", "en", 300, True, False, True),
        )
        self.assertEqual(list(s.keyterm), ["Example Shop"])
        s = llm._settings
        self.assertEqual((s.model, s.max_completion_tokens), ("gpt-6-luna", 400))
        self.assertEqual(s.reasoning.effort, "low")
        self.assertTrue(s.system_instruction.strip().startswith("You are the voice assistant"))
        s = tts._settings
        self.assertEqual((s.model, s.voice, str(s.language)), ("sonic-3.6", settings.tts.voice, "en"))
        self.assertEqual(tts._init_sample_rate, 24000)

    def test_the_aura_agent_builds_pipecats_deepgram_websocket_tts(self):
        settings = agent_mapping.load(HERE / "agent_live_en_aura.toml")
        keys = {**DUMMY_KEYS, "OPENAI_COMPATIBLE_API_KEY": "dummy"}
        with mock.patch.dict(os.environ, keys):
            tts = bot.build_services(bot.BotConfig(settings=settings))[2]
        self.assertEqual(type(tts).__name__, "DeepgramTTSService")
        self.assertEqual(tts._settings.voice, "aura-2-thalia-en")
        self.assertEqual((tts._encoding, tts._init_sample_rate), ("linear16", 24000))
        self.assertIsNone(tts._settings.speed)

    def test_the_compatible_agent_builds_pipecats_chat_completions_service(self):
        settings = agent_mapping.load(HERE / "agent_live_en.toml")
        keys = {**DUMMY_KEYS, "OPENAI_COMPATIBLE_API_KEY": "dummy"}
        with mock.patch.dict(os.environ, keys):
            llm = bot.build_services(bot.BotConfig(settings=settings))[1]
            bot.check_keys(settings)
        self.assertEqual(type(llm).__name__, "OpenAILLMService")
        self.assertEqual(str(llm._client.base_url).rstrip("/"), "https://openrouter.ai/api/v1")
        s = llm._settings
        self.assertEqual((s.model, s.max_tokens), ("openai/gpt-6-luna", 120))
        self.assertEqual(
            s.extra,
            {
                "reasoning_effort": "none",
                "extra_body": {"provider": {"order": ["openai"], "allow_fallbacks": False}},
            },
        )
        self.assertTrue(s.system_instruction.strip().startswith("You are the voice assistant"))
        with mock.patch.dict(os.environ, {**keys, "OPENAI_COMPATIBLE_API_KEY": ""}):
            with self.assertRaisesRegex(RuntimeError, "OPENAI_COMPATIBLE_API_KEY"):
                bot.check_keys(settings)

    def test_websocket_services_race_address_families(self):
        import pipecat.services.websocket_service as websocket_service

        calls = []

        def recorder(uri, **kwargs):
            calls.append((uri, kwargs))
            return "connected"

        with mock.patch.object(websocket_service, "websocket_connect", recorder):
            bot.enable_happy_eyeballs()
            bot.enable_happy_eyeballs()  # idempotent: one wrapper layer
            self.assertEqual(websocket_service.websocket_connect("wss://x", a=1), "connected")
            websocket_service.websocket_connect("wss://y", happy_eyeballs_delay=1.0)
        self.assertEqual(
            calls,
            [
                ("wss://x", {"a": 1, "happy_eyeballs_delay": 0.25}),
                ("wss://y", {"happy_eyeballs_delay": 1.0}),
            ],
        )

    def test_missing_keys_are_named(self):
        settings = agent_mapping.load(HERE / "agent.toml")
        with mock.patch.dict(os.environ, {k: "" for k in DUMMY_KEYS}):
            with self.assertRaisesRegex(RuntimeError, "DEEPGRAM_API_KEY"):
                bot.check_keys(settings)

    def test_the_pinned_silero_file_is_used_when_present(self):
        models = Path(os.environ.get("BRISVOX_MODELS_DIR", REPO / "models"))
        if not (models / "silero_vad.onnx").is_file():
            self.skipTest("models/silero_vad.onnx not fetched (scripts/models/fetch_models.py)")
        with mock.patch.dict(os.environ, {"BRISVOX_MODELS_DIR": str(models)}):
            path = bot.find_silero_model("auto")
        self.assertEqual(path, models / "silero_vad.onnx")
        self.assertIsNone(bot.find_silero_model("pipecat"))
        settings = agent_mapping.load(HERE / "agent.toml")
        analyzer = bot.build_vad(settings, path)
        self.assertEqual(analyzer.params.confidence, 0.5)
        with self.assertRaises(RuntimeError):
            bot.find_silero_model(str(HERE / "agent.toml"))


class Calls(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self):
        settings = agent_mapping.load(HERE / "agent.toml")
        config = bot.BotConfig(settings=settings, fake=bot.FakeConfig(delay_ms=100, reply_ms=3000))
        self.port = free_port()
        self.server = uvicorn.Server(
            uvicorn.Config(bot.create_app(config), host="127.0.0.1", port=self.port, log_level="warning")
        )
        self.serve = asyncio.create_task(self.server.serve())
        for _ in range(200):
            if self.server.started:
                break
            await asyncio.sleep(0.02)
        self.assertTrue(self.server.started)
        self.url = f"ws://127.0.0.1:{self.port}/any/path"

    async def asyncTearDown(self):
        self.server.should_exit = True
        await asyncio.wait_for(self.serve, 10)

    async def expect_refusal(self, ws, code: str, close_code: int):
        error = json.loads(await asyncio.wait_for(ws.recv(), 5))
        self.assertEqual((error["type"], error["code"], error["fatal"]), ("error", code, True))
        with self.assertRaises(ConnectionClosed):
            await asyncio.wait_for(ws.recv(), 5)
        self.assertEqual(ws.close_code, close_code)

    async def test_a_bad_start_is_refused_like_brisvox(self):
        async with connect(self.url) as ws:
            await ws.send(json.dumps({"type": "start", "version": 2, "sample_rate": 16000}))
            await self.expect_refusal(ws, "unsupported_version", 1008)
        async with connect(self.url) as ws:
            await ws.send(b"\x00\x00")
            await self.expect_refusal(ws, "audio_before_start", 1008)

    async def test_no_start_in_time(self):
        with mock.patch.object(bot, "START_TIMEOUT_SECS", 0.3):
            async with connect(self.url) as ws:
                await self.expect_refusal(ws, "start_timeout", 1008)

    async def test_a_call_answers_clears_on_barge_in_and_stops(self):
        pcm = fixture_pcm()
        async with connect(self.url) as ws:
            await ws.send(
                json.dumps({"type": "start", "version": 1, "sample_rate": 16000, "call_id": "t1"})
            )
            started = json.loads(await asyncio.wait_for(ws.recv(), 10))
            self.assertEqual(
                started,
                {"type": "started", "version": 1, "call_id": "t1", "sample_rate": 16000, "encoding": "pcm16"},
            )

            async def send(begin: int, end: int):
                for i in range(begin * 2, end * 2, FRAME):
                    await ws.send(pcm[i : i + FRAME])
                    await asyncio.sleep(0.005)  # 4x real time: Silero does not care

            # The first utterance and 0.5 s of its trailing silence: the VAD stops, the tone follows.
            # The VAD start of the first utterance is an interruption too (the live user
            # aggregator interrupts on every turn start), but the bot is silent: no `clear`.
            await send(0, FIRST_END + 8000)
            before: list[str | bytes] = []
            audible = await self.read_until(
                ws, lambda m: isinstance(m, bytes) and peak(m) >= 200, seen=before
            )
            self.assertEqual(len(audible) % 2, 0)
            self.assertFalse([m for m in before if isinstance(m, str) and '"clear"' in m])

            # Talk over the tone: Pipecat interrupts, the client is told to clear.
            sender = asyncio.create_task(send(SECOND_START, SECOND_END))
            clear = await self.read_until(ws, lambda m: isinstance(m, str) and '"clear"' in m)
            self.assertEqual(json.loads(clear), {"type": "clear", "epoch": 1})
            await sender

            await ws.send(json.dumps({"type": "stop"}))
            stop = await self.read_until(ws, lambda m: isinstance(m, str) and '"stop"' in m)
            self.assertEqual(json.loads(stop), {"type": "stop", "reason": "ended"})
            with self.assertRaises(ConnectionClosed):
                while True:
                    await asyncio.wait_for(ws.recv(), 5)

    async def test_mulaw_8k_calls_get_mulaw_audio(self):
        async with connect(self.url) as ws:
            await ws.send(json.dumps({"type": "start", "version": 1, "sample_rate": 8000, "encoding": "mulaw"}))
            started = json.loads(await asyncio.wait_for(ws.recv(), 10))
            self.assertEqual((started["sample_rate"], started["encoding"]), (8000, "mulaw"))
            speech = fixture_pcm()[: (FIRST_END + 8000) * 2]
            narrow, _ = audioop.ratecv(speech, 2, 1, 16000, 8000, None)
            ulaw = audioop.lin2ulaw(narrow, 2)
            for i in range(0, len(ulaw), 160):
                await ws.send(ulaw[i : i + 160])
                await asyncio.sleep(0.005)
            # The tone comes back as 20 ms mu-law frames: 160 bytes at 8 kHz.
            frame = await self.read_until(
                ws, lambda m: isinstance(m, bytes) and peak(audioop.ulaw2lin(m, 2)) >= 200
            )
            self.assertEqual(len(frame), 160)
            await ws.send(json.dumps({"type": "stop"}))
            await self.read_until(ws, lambda m: isinstance(m, str) and '"stop"' in m)

    async def read_until(self, ws, wanted, timeout: float = 15.0, seen: list | None = None):
        async def loop():
            while True:
                message = await ws.recv()
                if seen is not None:
                    seen.append(message)
                if wanted(message):
                    return message

        return await asyncio.wait_for(loop(), timeout)


if __name__ == "__main__":
    unittest.main()
