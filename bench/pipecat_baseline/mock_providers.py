"""Key-free mock STT, LLM and TTS for Pipecat, shaped like the ``brisvox_sim`` mocks.

The capacity benchmark compares brisvox and Pipecat without provider keys and without provider
noise. brisvox runs its production engine over ``brisvox_sim`` mock providers
(``crates/brisvox-bench/src/v2v/engine_target.rs``); this module is the Pipecat side of
that comparison: the same fixed latencies, the same token and audio shapes, behind Pipecat's real
service classes, so Pipecat's own orchestration (frame processors, aggregators, turn strategies,
the TTS text aggregation, the transport serializer) is what the benchmark loads.

| brisvox mock | Pipecat mock here | latency / shape |
|---|---|---|
| ``MockStt`` | ``MockSTTService`` (a ``STTService``) | the interims of the next utterance, one per ``stt_interim_ms`` from the first loud audio frame (RMS >= ``vad_rms``): growing word prefixes; at the VAD stop the final transcript after ``stt_final_ms``, flagged ``finalized`` |
| ``MockLlm`` | ``MockResponsesLLMService`` (the real ``OpenAIResponsesHttpLLMService`` with only the HTTP stream replaced) | one text delta (the whole answer) ``llm_first_token_ms`` after the request; the answer is looked up from the user's text |
| ``MockTts`` | ``MockTTSService`` (a ``TTSService``) | first audio ``tts_first_byte_ms`` after the text, then a 40 ms chunk every ``tts_chunk_interval_ms``; 60 ms of audio per character; a quiet square wave (+-1000, 20-sample half period) |
| energy VAD (``brisvox-sim``) + ``FixedTurnModel`` | ``EnergyVADAnalyzer`` + ``MockSmartTurnAnalyzer`` | 512-sample (32 ms) windows like Silero, speech when the window's RMS is >= ``vad_rms``; the Smart Turn feature extraction runs, the model answers "complete" without an ONNX run |

Where the mock sits matters for fairness: brisvox's mocks replace the whole provider adapter
(wire protocol, JSON parsing); here the Pipecat mocks replace the network boundary of each
service (the Responses stream, the TTS audio, the STT transcript) and leave Pipecat's own code
above it in place. Neither side pays for TLS, HTTP or JSON of a real provider; both pay for
their own framework.

The defaults below equal the Rust constants (``tests/test_mock_providers.py`` fails when one of
them changes). Nothing here imports Pipecat at module import time except what the classes need,
so ``MockConfig`` and ``load_script`` work without it.
"""

from __future__ import annotations

import asyncio
import json
import math
import re
from dataclasses import dataclass, field
from functools import lru_cache
from pathlib import Path

ANSWER_EN = "Okay, got it."
"""What the brisvox mock LLM answers an English utterance (``engine_target::ANSWER_EN``)."""

ANSWER_TR = "Peki, anladım."
"""What it answers a Turkish utterance (``engine_target::ANSWER_TR``)."""

SPEECH_RMS = 500.0
"""RMS (PCM16 units) at or above which a frame is speech (``engine_target::SPEECH_RMS``)."""

VAD_WINDOW_SAMPLES = 512
"""The VAD window: 32 ms at 16 kHz, what Silero uses (and the brisvox VAD worker)."""

MS_PER_CHAR = 60
"""Audio per character of the mock TTS (``MockTts::new``)."""

TTS_CHUNK_MS = 40
"""Audio per chunk of the mock TTS (``MockTts::new``)."""

TONE_PEAK = 1000
TONE_HALF_PERIOD_SAMPLES = 20
"""The mock TTS square wave (``mock_tts::tone``)."""


@dataclass(frozen=True)
class MockConfig:
    """Latencies and thresholds of the mocks: the defaults of ``EngineTargetConfig``."""

    stt_interim_ms: int = 100
    stt_final_ms: int = 150
    llm_first_token_ms: int = 350
    tts_first_byte_ms: int = 150
    tts_chunk_interval_ms: int = 20
    vad_rms: float = SPEECH_RMS
    vad_start_ms: int = 40
    vad_stop_ms: int = 300
    fixture: str = "en_two_utterances"
    fixtures_dir: Path | None = None
    models: str = "energy"
    """``energy``: the energy VAD and the fixed Smart Turn model (like the brisvox sim target);
    ``real``: Pipecat's Silero VAD and Smart Turn v3 (a heavier Pipecat, not like for like)."""

    @property
    def floor_ms(self) -> int:
        """The latency floor of one turn: end of turn + first token + first byte.

        The end of turn is the VAD stop plus the STT final (the finals gate), as in
        ``EngineTargetSettings::floor_ms`` with the smart-turn policy.
        """
        return self.vad_stop_ms + self.stt_final_ms + self.llm_first_token_ms + self.tts_first_byte_ms


@dataclass(frozen=True)
class Utterance:
    """One utterance of the fixture: what the caller says and the language of the answer."""

    text: str
    lang: str

    @property
    def answer(self) -> str:
        return ANSWER_TR if self.lang == "tr" else ANSWER_EN


def normalize(text: str) -> str:
    """Lower-case words separated by single spaces (punctuation dropped), for matching."""
    return " ".join(re.sub(r"[^\w\s]", " ", text.lower()).split())


def load_script(fixtures_dir: Path, name: str) -> list[Utterance]:
    """The utterances of ``fixtures/NAME.json`` in order (the ground truth sidecar)."""
    with open(fixtures_dir / f"{name}.json", encoding="utf-8") as f:
        sidecar = json.load(f)
    return [Utterance(text=s["text"], lang=s.get("lang", "en")) for s in sidecar["segments"]]


def answer_for(script: list[Utterance], user_text: str) -> str | None:
    """The answer of the utterance whose text is ``user_text`` (or starts with it); else None.

    Same rule as ``CallScript::responder``: no match means no answer (the turn shows as
    ``no_response`` in the benchmark).
    """
    user = normalize(user_text)
    if not user:
        return None
    table = [(normalize(u.text), u.answer) for u in script]
    for text, answer in table:
        if text == user:
            return answer
    for text, answer in table:
        if text.startswith(user):
            return answer
    return None


def request_user_text(params: dict) -> str:
    """The text of the last user message of a Responses API request (``params["input"]``)."""
    for item in reversed(params.get("input") or []):
        if isinstance(item, dict) and item.get("role") == "user":
            content = item.get("content")
            if isinstance(content, str):
                return content
            if isinstance(content, list):
                return " ".join(part.get("text", "") for part in content if isinstance(part, dict))
    return ""


def interims_of(text: str) -> list[str]:
    """Growing word prefixes of ``text``, all but the full text (``SttUtterance::words``)."""
    words = text.split()
    return [" ".join(words[:n]) for n in range(1, len(words))]


def rms16(pcm: bytes) -> float:
    """RMS of little-endian PCM16 (0 for an empty buffer)."""
    import numpy as np

    samples = np.frombuffer(pcm, dtype="<i2")
    if samples.size == 0:
        return 0.0
    return float(np.sqrt(np.mean(samples.astype(np.float64) ** 2)))


def tone_chunk(sample_rate: int) -> bytes:
    """40 ms of the mock TTS square wave."""
    import numpy as np

    n = sample_rate * TTS_CHUNK_MS // 1000
    index = np.arange(n)
    wave = np.where((index // TONE_HALF_PERIOD_SAMPLES) % 2 == 0, TONE_PEAK, -TONE_PEAK)
    return wave.astype("<i2").tobytes()


def chunk_count(text: str) -> int:
    """How many 40 ms chunks the mock TTS makes of ``text``."""
    return max(1, math.ceil(len(text) * MS_PER_CHAR / TTS_CHUNK_MS))


# ---------------------------------------------------------------------------------------------
# Pipecat classes. Built lazily so this module imports without Pipecat.


def energy_vad_analyzer(config: MockConfig):
    """An energy VAD analyzer on 32 ms windows with the brisvox sim detector's thresholds.

    Pipecat counts whole windows: the stop needs ``round(stop_secs / 0.032)`` quiet windows, so
    300 ms is ten windows (320 ms), which is also when the brisvox VAD worker (windows of 32 ms,
    "silence >= min_silence") confirms a stop.
    """
    from pipecat.audio.vad.vad_analyzer import VADAnalyzer, VADParams

    window_secs = VAD_WINDOW_SAMPLES / 16000

    class EnergyVADAnalyzer(VADAnalyzer):
        """Speech when the window's RMS reaches the threshold (no model, no volume gate)."""

        def num_frames_required(self) -> int:
            return VAD_WINDOW_SAMPLES

        def voice_confidence(self, buffer: bytes) -> float:
            return 1.0 if rms16(buffer) >= config.vad_rms else 0.0

    # Whole windows, rounded up like a "at least this long" threshold.
    stop_windows = math.ceil(config.vad_stop_ms / 1000 / window_secs - 1e-9)
    start_windows = math.ceil(config.vad_start_ms / 1000 / window_secs - 1e-9)
    return EnergyVADAnalyzer(
        sample_rate=16000,
        params=VADParams(
            confidence=0.5,
            start_secs=start_windows * window_secs,
            stop_secs=stop_windows * window_secs,
            min_volume=0.0,
        ),
    )


def mock_smart_turn_analyzer(stop_secs: float):
    """Smart Turn v3 with the real feature extraction and a model that answers "complete".

    ``FixedTurnModel`` in brisvox: the mel features are computed (that is real CPU), the ONNX
    session is not created and not run.
    """
    import numpy as np
    from pipecat.audio.turn.smart_turn._whisper_features import compute_whisper_log_mel_features
    from pipecat.audio.turn.smart_turn.base_smart_turn import BaseSmartTurn, SmartTurnParams
    from pipecat.audio.turn.smart_turn.local_smart_turn_v3 import LocalSmartTurnAnalyzerV3

    class MockSmartTurnAnalyzer(LocalSmartTurnAnalyzerV3):
        def __init__(self, **kwargs):
            # Skip LocalSmartTurnAnalyzerV3.__init__: it loads the ONNX model.
            BaseSmartTurn.__init__(self, **kwargs)
            self._log_data = False

        def _predict_endpoint(self, audio_array):
            samples = 8 * 16000
            audio = self._resample_to_model_rate(audio_array)
            if len(audio) > samples:
                audio = audio[-samples:]
            elif len(audio) < samples:
                audio = np.pad(audio, (samples - len(audio), 0))
            compute_whisper_log_mel_features(audio, do_normalize=True)
            return {"prediction": 1, "probability": 1.0}

    return MockSmartTurnAnalyzer(params=SmartTurnParams(stop_secs=stop_secs))


@lru_cache(maxsize=None)
def _stt_class():
    from pipecat.frames.frames import (
        Frame,
        InterimTranscriptionFrame,
        TranscriptionFrame,
        VADUserStoppedSpeakingFrame,
    )
    from pipecat.processors.frame_processor import FrameDirection
    from pipecat.services.settings import STTSettings
    from pipecat.services.stt_service import STTService
    from pipecat.utils.time import time_now_iso8601

    class MockSTTService(STTService):
        """Plays the call's script: interims while the caller speaks, a final after the VAD stop."""

        def __init__(self, config: MockConfig, script: list[Utterance], **kwargs):
            super().__init__(
                # The final is "finalized" at once, so a turn never waits for the STT timeout.
                ttfs_p99_latency=config.stt_final_ms / 1000,
                settings=STTSettings(model=None, language=None),
                **kwargs,
            )
            self._mock_config = config
            self._mock_script = script
            self._mock_next = 0  # the utterance the next final belongs to
            self._mock_interim_task: asyncio.Task | None = None
            self._mock_final_task: asyncio.Task | None = None

        def can_generate_metrics(self) -> bool:
            return False

        async def run_stt(self, audio: bytes):
            # The first loud frame of an utterance starts its interims (as the brisvox mock).
            if (
                self._mock_interim_task is None
                and self._mock_final_task is None
                and self._mock_next < len(self._mock_script)
                and rms16(audio) >= self._mock_config.vad_rms
            ):
                self._mock_interim_task = self.create_task(self._mock_interims(self._mock_next), "mock_stt_interims")
            yield None

        async def process_frame(self, frame: Frame, direction: FrameDirection):
            await super().process_frame(frame, direction)
            if isinstance(frame, VADUserStoppedSpeakingFrame):
                await self._mock_finalize()

        async def _mock_finalize(self):
            """The VAD stop is the Finalize request: pending interims are dropped."""
            await self._mock_cancel(self._mock_interim_task)
            self._mock_interim_task = None
            if self._mock_final_task is None and self._mock_next < len(self._mock_script):
                index = self._mock_next
                self._mock_final_task = self.create_task(self._mock_final(index), "mock_stt_final")

        async def cleanup(self):
            """Ends the interim and final tasks with the call."""
            await self._mock_cancel(self._mock_interim_task)
            await self._mock_cancel(self._mock_final_task)
            self._mock_interim_task = self._mock_final_task = None
            await super().cleanup()

        async def _mock_cancel(self, task: asyncio.Task | None):
            if task is not None:
                await self.cancel_task(task)

        async def _mock_interims(self, index: int):
            for text in interims_of(self._mock_script[index].text):
                await asyncio.sleep(self._mock_config.stt_interim_ms / 1000)
                await self.push_frame(
                    InterimTranscriptionFrame(text=text, user_id=self._user_id, timestamp=time_now_iso8601())
                )

        async def _mock_final(self, index: int):
            try:
                await asyncio.sleep(self._mock_config.stt_final_ms / 1000)
                self._mock_next = index + 1
                await self.push_frame(
                    TranscriptionFrame(
                        text=self._mock_script[index].text,
                        user_id=self._user_id,
                        timestamp=time_now_iso8601(),
                        finalized=True,
                    )
                )
            finally:
                self._mock_final_task = None

    return MockSTTService


@lru_cache(maxsize=None)
def _llm_class():
    from openai.types.responses import ResponseTextDeltaEvent
    from pipecat.services.openai.responses.llm import OpenAIResponsesHttpLLMService

    class _MockStream:
        """Stands in for the SDK's ``AsyncStream``: one text delta after the first-token delay."""

        def __init__(self, delay_s: float, answer: str | None):
            self._delay_s = delay_s
            self._answer = answer

        def __aiter__(self):
            return self._events()

        async def _events(self):
            await asyncio.sleep(self._delay_s)
            if self._answer:
                yield ResponseTextDeltaEvent.model_construct(
                    type="response.output_text.delta",
                    delta=self._answer,
                    item_id="mock",
                    output_index=0,
                    content_index=0,
                    sequence_number=0,
                    logprobs=[],
                )

        async def close(self):
            return None

    class MockResponsesLLMService(OpenAIResponsesHttpLLMService):
        """The real Responses HTTP service; only the request to OpenAI is replaced."""

        def __init__(self, config: MockConfig, script: list[Utterance], **kwargs):
            super().__init__(api_key="mock-key-not-used", **kwargs)
            self._mock_config = config
            self._mock_script = script

        async def _create_stream(self, params: dict):
            answer = answer_for(self._mock_script, request_user_text(params))
            return _MockStream(self._mock_config.llm_first_token_ms / 1000, answer)

    return MockResponsesLLMService


@lru_cache(maxsize=None)
def _tts_class():
    from pipecat.frames.frames import TTSAudioRawFrame
    from pipecat.services.settings import TTSSettings
    from pipecat.services.tts_service import TTSService

    class MockTTSService(TTSService):
        """Speaks any text as a quiet square wave: the first chunk after the first-byte delay."""

        def __init__(self, config: MockConfig, *, sample_rate: int, **kwargs):
            super().__init__(
                sample_rate=sample_rate,
                push_start_frame=True,
                push_stop_frames=True,
                settings=TTSSettings(model="mock", voice="mock", language=None),
                **kwargs,
            )
            self._mock_config = config
            self._chunk = tone_chunk(sample_rate)

        def can_generate_metrics(self) -> bool:
            return False

        async def run_tts(self, text: str, context_id: str):
            await asyncio.sleep(self._mock_config.tts_first_byte_ms / 1000)
            for i in range(chunk_count(text)):
                if i:
                    await asyncio.sleep(self._mock_config.tts_chunk_interval_ms / 1000)
                yield TTSAudioRawFrame(self._chunk, self.sample_rate, 1, context_id=context_id)

    return MockTTSService


@dataclass
class MockServices:
    """The three mocks of one call."""

    stt: object
    llm: object
    tts: object
    script: list[Utterance] = field(default_factory=list)


def build_mock_services(config: MockConfig, fixtures_dir: Path, llm_kwargs: dict, tts_rate: int) -> MockServices:
    """The mock STT, LLM and TTS of one call, each with its own copy of the script.

    ``llm_kwargs`` are the keyword arguments of the real Responses service (its settings), so
    the mock LLM builds its requests from the agent file like the real one.
    """
    script = load_script(config.fixtures_dir or fixtures_dir, config.fixture)
    return MockServices(
        stt=_stt_class()(config, script),
        llm=_llm_class()(config, script, **llm_kwargs),
        tts=_tts_class()(config, sample_rate=tts_rate),
        script=script,
    )
