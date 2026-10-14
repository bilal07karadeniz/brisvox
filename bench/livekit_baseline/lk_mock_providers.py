"""Key-free mock STT, LLM and TTS for LiveKit Agents, shaped like the ``brisvox_sim`` mocks.

The capacity benchmark compares brisvox, Pipecat and LiveKit Agents without provider keys and
without provider noise. ``bench/pipecat_baseline/mock_providers.py`` is the Pipecat side of that
comparison and documents the shared numbers (they equal the Rust constants of
``crates/brisvox-bench/src/v2v/engine_target.rs``; ``bench/pipecat_baseline/tests`` fails when one
drifts). This module is the LiveKit side: the same fixed latencies, the same token and audio
shapes, behind LiveKit's real plugin interfaces (``stt.STT``/``stt.SpeechStream``, ``llm.LLM``/
``llm.LLMStream``, ``tts.TTS``/``tts.ChunkedStream``), so LiveKit's own orchestration (the
audio-recognition state machine, VAD, the turn detector, preemptive generation, the sentence
stream adapter around a non-streaming TTS, the audio emitter, the room audio output) is what the
benchmark loads. The pure helpers (script, answer lookup, interims, tone) are imported from the
Pipecat module, so there is one definition of them.

| brisvox mock | LiveKit mock here | latency / shape |
|---|---|---|
| ``MockStt`` | ``MockSTT`` (an ``stt.STT``, streaming) | interims of the next utterance, one per ``stt_interim_ms`` from the first loud 32 ms window (RMS >= ``vad_rms``): growing word prefixes; the stream ends the utterance itself after ``vad_stop_ms`` of quiet windows (10 windows = 320 ms, the stop of the other mocks) and sends the final ``stt_final_ms`` later |
| ``MockLlm`` | ``MockLLM`` (an ``llm.LLM``) | one text delta (the whole answer) ``llm_first_token_ms`` after the request; the answer is looked up from the user's text |
| ``MockTts`` | ``MockTTS`` (a non-streaming ``tts.TTS``) | first audio ``tts_first_byte_ms`` after the text, then a 40 ms chunk every ``tts_chunk_interval_ms``; 60 ms of audio per character; a quiet square wave (+-1000, 20-sample half period); 24 kHz like Cartesia |

Where LiveKit differs from Pipecat, and the mock follows LiveKit: the framework does not tell a
streaming STT where the user's speech ended (a provider such as Deepgram decides that on its
side, by its endpointing). The mock STT therefore detects the end of the utterance itself, with
the 10 quiet windows (320 ms) that the VAD stop of the other two mocks uses, and answers 150 ms
after that, which is the same time the final transcript arrives on the other two sides.
"""

from __future__ import annotations

import asyncio
import os
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "pipecat_baseline"))
import mock_providers as shared  # noqa: E402  (the Pipecat side's pure helpers, no Pipecat import)

from livekit import rtc  # noqa: E402
from livekit.agents import llm, stt, tts  # noqa: E402
from livekit.agents.types import DEFAULT_API_CONNECT_OPTIONS, APIConnectOptions  # noqa: E402

MockConfig = shared.MockConfig
load_script = shared.load_script

TTS_SAMPLE_RATE = 24000
"""Cartesia's default and what ``bench/pipecat_baseline/agent.toml`` uses (``tts.sample_rate``)."""

WINDOW_MS = 32
"""The VAD window of the other mocks (512 samples at 16 kHz)."""


@dataclass(frozen=True)
class MockSetup:
    """What one call's mocks share: the latencies and the fixture's script."""

    config: MockConfig
    script: list

    @staticmethod
    def from_env() -> "MockSetup":
        fixtures_dir = Path(os.environ.get("BRISVOX_FIXTURES_DIR", str(HERE.parent.parent / "fixtures")))
        config = MockConfig(fixture=os.environ.get("BRISVOX_MOCK_FIXTURE", "en_two_utterances"))
        return MockSetup(config=config, script=shared.load_script(fixtures_dir, config.fixture))


def _frame_rms(samples) -> float:
    import numpy as np

    if samples.size == 0:
        return 0.0
    return float(np.sqrt(np.mean(samples.astype(np.float64) ** 2)))


# ---------------------------------------------------------------------------------------------
# STT


class MockSTT(stt.STT):
    def __init__(self, setup: MockSetup) -> None:
        super().__init__(capabilities=stt.STTCapabilities(streaming=True, interim_results=True))
        self._setup = setup

    @property
    def model(self) -> str:
        return "brisvox-mock"

    @property
    def provider(self) -> str:
        return "brisvox-mock"

    async def _recognize_impl(self, buffer, *, language=None, conn_options: APIConnectOptions):  # pragma: no cover
        raise NotImplementedError("the mock STT is streaming only")

    def stream(self, *, language=None, conn_options: APIConnectOptions = DEFAULT_API_CONNECT_OPTIONS) -> "MockSpeechStream":
        return MockSpeechStream(stt=self, setup=self._setup, conn_options=conn_options)


class MockSpeechStream(stt.SpeechStream):
    """Plays the call's script: interims while the caller speaks, a final after the stop."""

    def __init__(self, *, stt: MockSTT, setup: MockSetup, conn_options: APIConnectOptions) -> None:
        super().__init__(stt=stt, conn_options=conn_options)
        self._cfg = setup.config
        self._script = setup.script
        self._next = 0  # the utterance the next final belongs to
        self._interim_task: asyncio.Task | None = None
        self._final_task: asyncio.Task | None = None
        # 32 ms windows: samples per window depends on the pushed rate
        self._window_samples = 0
        self._pending = None  # numpy int16 remainder of the current window
        self._in_speech = False
        self._quiet_windows = 0
        self._stop_windows = -(-self._cfg.vad_stop_ms // WINDOW_MS)  # ceil: 300 ms -> 10 windows

    async def _run(self) -> None:
        import numpy as np

        try:
            async for item in self._input_ch:
                if isinstance(item, self._FlushSentinel):
                    continue
                frame: rtc.AudioFrame = item
                if self._window_samples == 0:
                    self._window_samples = frame.sample_rate * WINDOW_MS // 1000 * frame.num_channels
                data = np.frombuffer(frame.data, dtype=np.int16)
                if self._pending is not None and self._pending.size:
                    data = np.concatenate([self._pending, data])
                n = (data.size // self._window_samples) * self._window_samples
                for start in range(0, n, self._window_samples):
                    self._on_window(data[start : start + self._window_samples])
                self._pending = data[n:]
        finally:
            await self._cancel(self._interim_task)
            await self._cancel(self._final_task)

    def _on_window(self, window) -> None:
        loud = _frame_rms(window) >= self._cfg.vad_rms
        if loud:
            self._quiet_windows = 0
            if not self._in_speech and self._next < len(self._script) and self._final_task is None:
                self._in_speech = True
                self._interim_task = asyncio.create_task(self._interims(self._next))
        elif self._in_speech:
            self._quiet_windows += 1
            if self._quiet_windows >= self._stop_windows:
                # The utterance has ended: pending interims are dropped, the final follows.
                self._in_speech = False
                self._quiet_windows = 0
                self._cancel_nowait(self._interim_task)
                self._interim_task = None
                if self._final_task is None and self._next < len(self._script):
                    self._final_task = asyncio.create_task(self._final(self._next))

    @staticmethod
    def _cancel_nowait(task: asyncio.Task | None) -> None:
        if task is not None and not task.done():
            task.cancel()

    @staticmethod
    async def _cancel(task: asyncio.Task | None) -> None:
        if task is None:
            return
        task.cancel()
        try:
            await task
        except (asyncio.CancelledError, Exception):  # noqa: BLE001
            pass

    def _event(self, kind: stt.SpeechEventType, text: str, lang: str) -> stt.SpeechEvent:
        return stt.SpeechEvent(
            type=kind,
            request_id=self._request_id,
            alternatives=[stt.SpeechData(language=lang, text=text, confidence=1.0)],
        )

    _request_id = "mock"

    async def _interims(self, index: int) -> None:
        utterance = self._script[index]
        for text in shared.interims_of(utterance.text):
            await asyncio.sleep(self._cfg.stt_interim_ms / 1000)
            self._event_ch.send_nowait(self._event(stt.SpeechEventType.INTERIM_TRANSCRIPT, text, utterance.lang))

    async def _final(self, index: int) -> None:
        try:
            await asyncio.sleep(self._cfg.stt_final_ms / 1000)
            utterance = self._script[index]
            self._next = index + 1
            self._event_ch.send_nowait(self._event(stt.SpeechEventType.FINAL_TRANSCRIPT, utterance.text, utterance.lang))
        finally:
            self._final_task = None


# ---------------------------------------------------------------------------------------------
# LLM


class MockLLM(llm.LLM):
    def __init__(self, setup: MockSetup) -> None:
        super().__init__()
        self._setup = setup

    @property
    def model(self) -> str:
        return "brisvox-mock"

    @property
    def provider(self) -> str:
        return "brisvox-mock"

    def chat(
        self,
        *,
        chat_ctx: llm.ChatContext,
        tools=None,
        conn_options: APIConnectOptions = DEFAULT_API_CONNECT_OPTIONS,
        parallel_tool_calls=None,
        tool_choice=None,
        extra_kwargs=None,
    ) -> "MockLLMStream":
        return MockLLMStream(self, setup=self._setup, chat_ctx=chat_ctx, tools=tools or [], conn_options=conn_options)


def last_user_text(chat_ctx: llm.ChatContext) -> str:
    for item in reversed(chat_ctx.items):
        if getattr(item, "type", None) == "message" and getattr(item, "role", None) == "user":
            return item.text_content or ""
    return ""


class MockLLMStream(llm.LLMStream):
    def __init__(self, llm_: MockLLM, *, setup: MockSetup, chat_ctx, tools, conn_options) -> None:
        super().__init__(llm_, chat_ctx=chat_ctx, tools=tools, conn_options=conn_options)
        self._setup = setup

    async def _run(self) -> None:
        answer = shared.answer_for(self._setup.script, last_user_text(self._chat_ctx))
        await asyncio.sleep(self._setup.config.llm_first_token_ms / 1000)
        if answer:
            self._event_ch.send_nowait(
                llm.ChatChunk(id="mock", delta=llm.ChoiceDelta(role="assistant", content=answer))
            )


# ---------------------------------------------------------------------------------------------
# TTS


class MockTTS(tts.TTS):
    def __init__(self, setup: MockSetup) -> None:
        super().__init__(
            capabilities=tts.TTSCapabilities(streaming=False),
            sample_rate=TTS_SAMPLE_RATE,
            num_channels=1,
        )
        self._setup = setup
        self._chunk = shared.tone_chunk(TTS_SAMPLE_RATE)

    @property
    def model(self) -> str:
        return "brisvox-mock"

    @property
    def provider(self) -> str:
        return "brisvox-mock"

    def synthesize(self, text: str, *, conn_options: APIConnectOptions = DEFAULT_API_CONNECT_OPTIONS) -> "MockChunkedStream":
        return MockChunkedStream(tts=self, input_text=text, conn_options=conn_options)


class MockChunkedStream(tts.ChunkedStream):
    async def _run(self, output_emitter: tts.AudioEmitter) -> None:
        mock: MockTTS = self._tts  # type: ignore[assignment]
        cfg = mock._setup.config
        output_emitter.initialize(
            request_id=uuid.uuid4().hex[:8],
            sample_rate=TTS_SAMPLE_RATE,
            num_channels=1,
            mime_type="audio/pcm",
        )
        await asyncio.sleep(cfg.tts_first_byte_ms / 1000)
        for i in range(shared.chunk_count(self.input_text)):
            if i:
                await asyncio.sleep(cfg.tts_chunk_interval_ms / 1000)
            output_emitter.push(mock._chunk)
        output_emitter.flush()
