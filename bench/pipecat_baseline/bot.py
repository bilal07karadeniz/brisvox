"""Pipecat baseline bot for the brisvox V2V benchmark.

One websocket connection is one call, in the brisvox websocket protocol
(docs/WEBSOCKET_PROTOCOL.md), so ``brisvox-bench v2v --url ws://HOST:PORT`` measures this bot
exactly like brisvox-server. Each call gets its own Pipecat pipeline:

    FastAPIWebsocketTransport.input -> Deepgram STT -> user aggregator (Silero VAD, Smart Turn v3)
      -> OpenAI Responses LLM -> Cartesia TTS -> FastAPIWebsocketTransport.output
      -> assistant aggregator

Providers, models, voice, language, prompt and turn settings come from the brisvox agent file
(``--agent``, the same file brisvox-server runs), mapped by ``agent_mapping.py``; README.md has
the table. API keys come from the environment variables the agent file names (``.env`` at the
repository root is loaded when present); they are never written anywhere.

``--fake`` replaces STT, LLM and TTS with a provider-free responder (Pipecat's Silero VAD, then a
tone after a fixed delay): a self-check of this adapter and Pipecat's transport, the analogue of
``brisvox-bench v2v --sim --target mock-bot``. It needs no keys.

``--mock-providers`` keeps the real Pipecat pipeline (transport, aggregators, turn
strategies, the real Responses LLM service and the TTS text aggregation) and replaces only the
network boundary of STT, LLM and TTS with mocks that have the same fixed latencies and shapes as
the ``brisvox_sim`` mocks (``mock_providers.py``), so ``brisvox-bench capacity`` can compare the
two systems without keys. ``--workers N`` runs N processes (uvicorn) for capacity runs on
multi-core containers.

Run ``python bot.py --help`` for the options.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import math
import os
import sys
import uuid
from dataclasses import dataclass
from pathlib import Path

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from loguru import logger

import agent_mapping
from agent_mapping import BaselineSettings
from brisvox_protocol import BrisvoxFrameSerializer, ProtocolError, Start, parse_start, started_message
from mock_providers import MockConfig

HERE = Path(__file__).resolve().parent
REPO_ROOT = HERE.parent.parent
DEFAULT_AGENT = HERE / "agent.toml"

PIPELINE_IN_RATE = 16000
"""The rate the pipeline's input runs at: what brisvox feeds its VAD, Smart Turn and STT."""

START_TIMEOUT_SECS = 5.0
"""How long a client has to send ``start`` (the brisvox server's default ``start_timeout``)."""

SILERO_V623_SHA256 = "1a153a22f4509e292a94e67d6f9b85e8deb25b4988682b7e174c65279d8788e3"
"""SHA-256 of ``silero_vad.onnx`` at silero-vad tag v6.2.3, the model brisvox runs (its git
blob is ``80c5592e...``, docs/providers/silero-vad.md). Pipecat 1.12 bundles v6.0."""


@dataclass(frozen=True)
class FakeConfig:
    """The provider-free responder of ``--fake``."""

    delay_ms: int = 300
    reply_ms: int = 600
    tone_rate: int = 24000


@dataclass(frozen=True)
class BotConfig:
    """What every call of this process runs."""

    settings: BaselineSettings
    llm_transport: str = "http"  # "http" (same wire as brisvox) or "websocket" (Pipecat's pick)
    fake: FakeConfig | None = None
    mock: MockConfig | None = None  # --mock-providers: key-free mocks, same latencies as brisvox_sim
    smart_turn_threads: int = 1
    provider_metrics: bool = False  # --provider-metrics: log each service's TTFB (STT/LLM/TTS)
    silero_model: Path | None = None  # None: the file bundled with Pipecat (see find_silero_model)


def _language(tag: str):
    from pipecat.transcriptions.language import Language

    try:
        return Language(tag)
    except ValueError:
        return tag


def _key(env: str) -> str:
    value = os.environ.get(env, "")
    if not value:
        raise RuntimeError(f"environment variable {env} is not set (see README.md, Keys)")
    return value


def check_keys(settings: BaselineSettings) -> None:
    """Fails early, naming the variable, when a provider key is missing."""
    for env in settings.api_key_envs():
        _key(env)


def find_silero_model(choice: str) -> Path | None:
    """The Silero ONNX file the VAD runs: ``None`` for the one bundled with Pipecat.

    ``auto`` takes brisvox's pinned v6.2.3 file (``silero_vad.onnx`` in ``BRISVOX_MODELS_DIR``
    or the repository's ``models/``, fetched by ``scripts/models/fetch_models.py``) when it is
    there and its SHA-256 matches, else Pipecat's bundled file; ``pipecat`` forces the bundled
    one; anything else is a path, which must match the pinned SHA-256.
    """
    if choice == "pipecat":
        return None
    if choice != "auto":
        path = Path(choice)
        if _sha256(path) != SILERO_V623_SHA256:
            raise RuntimeError(f"{path} is not Silero VAD v6.2.3 (SHA-256 mismatch)")
        return path
    models = os.environ.get("BRISVOX_MODELS_DIR")
    for directory in ([Path(models)] if models else []) + [REPO_ROOT / "models"]:
        path = directory / "silero_vad.onnx"
        if path.is_file() and _sha256(path) == SILERO_V623_SHA256:
            return path
    return None


def _sha256(path: Path) -> str:
    import hashlib

    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def build_vad(settings: BaselineSettings, silero_model: Path | None = None):
    """Silero VAD with the mapped parameters, on ``silero_model`` when given."""
    from pipecat.audio.vad.silero import SileroOnnxModel, SileroVADAnalyzer
    from pipecat.audio.vad.vad_analyzer import VADParams

    v = settings.vad
    kwargs = dict(confidence=v.confidence, start_secs=v.start_secs, stop_secs=v.stop_secs)
    if v.min_volume is not None:
        kwargs["min_volume"] = v.min_volume
    analyzer = SileroVADAnalyzer(params=VADParams(**kwargs))
    if silero_model is not None:
        # Pipecat 1.12 has no model-path option; its analyzer keeps the ONNX session in
        # `_model`. Swap it for brisvox's pinned file (same class, same 512-sample windows).
        analyzer._model = SileroOnnxModel(str(silero_model), force_onnx_cpu=True)
    return analyzer


def build_turn_strategies(
    settings: BaselineSettings, smart_turn_threads: int = 1, mock: MockConfig | None = None
):
    """User turn start/stop strategies equivalent to the agent's turn settings.

    With ``mock`` on its ``energy`` models the Smart Turn model is replaced by the mock analyzer
    (real feature extraction, no ONNX run), like brisvox's ``FixedTurnModel``.
    """
    from pipecat.turns.user_start import MinWordsUserTurnStartStrategy
    from pipecat.turns.user_stop import (
        SpeechTimeoutUserTurnStopStrategy,
        TurnAnalyzerUserTurnStopStrategy,
    )
    from pipecat.turns.user_turn_strategies import UserTurnStrategies

    t = settings.turn
    start = [MinWordsUserTurnStartStrategy(min_words=t.min_words)] if t.min_words else None
    if t.policy == "smart_turn":
        from pipecat.audio.turn.smart_turn.base_smart_turn import SmartTurnParams
        from pipecat.audio.turn.smart_turn.local_smart_turn_v3 import LocalSmartTurnAnalyzerV3

        if mock is not None and mock.models == "energy":
            from mock_providers import mock_smart_turn_analyzer

            analyzer = mock_smart_turn_analyzer(t.smart_turn_stop_secs)
        else:
            analyzer = LocalSmartTurnAnalyzerV3(
                cpu_count=smart_turn_threads,
                params=SmartTurnParams(stop_secs=t.smart_turn_stop_secs),
            )
        stop = [TurnAnalyzerUserTurnStopStrategy(turn_analyzer=analyzer)]
    else:
        stop = [SpeechTimeoutUserTurnStopStrategy(user_speech_timeout=t.speech_timeout_secs)]
    return UserTurnStrategies(start=start, stop=stop)


def _compatible_llm(s: BaselineSettings):
    """Pipecat's Chat Completions service pointed at an OpenAI-compatible server (OpenRouter ...)
    with the agent file's ``base_url``, model, ``max_tokens`` and ``temperature``."""
    from pipecat.services.openai.llm import OpenAILLMService

    llm_settings = dict(model=s.llm.model, system_instruction=s.llm.system_prompt)
    if s.llm.max_output_tokens is not None:
        llm_settings["max_tokens"] = s.llm.max_output_tokens
    if s.llm.temperature is not None:
        llm_settings["temperature"] = s.llm.temperature
    extra: dict = {}
    if s.llm.reasoning_effort:
        extra["reasoning_effort"] = s.llm.reasoning_effort
    if s.llm.provider_routing:
        # Not a parameter of the OpenAI SDK's create(): its `extra_body` merges it into the JSON
        # body, where brisvox's `extra_body` puts the same `provider` object (OpenRouter routing).
        extra["extra_body"] = {"provider": s.llm.provider_routing}
    if extra:
        llm_settings["extra"] = extra
    api_key = _key(s.llm.api_key_env) if s.llm.api_key_env else "not-needed"
    return _upstream_logging_llm_class()(
        api_key=api_key,
        base_url=s.llm.base_url,
        settings=OpenAILLMService.Settings(**llm_settings),
    )


def _upstream_logging_llm_class():
    """``OpenAILLMService`` that logs, once per response, which upstream provider and model the
    gateway (OpenRouter) reports on the first stream chunk: ``PCUPSTREAM <service> provider=<p>
    model=<m>``. The live comparison checks that every request was served by the pinned provider."""
    from pipecat.services.openai.llm import OpenAILLMService as Base

    class _Stream:
        def __init__(self, inner, label: str):
            self._inner = inner
            self._label = label

        def __aiter__(self):
            return self._chunks()

        async def _chunks(self):
            first = True
            async for chunk in self._inner:
                if first:
                    first = False
                    extra = getattr(chunk, "model_extra", None) or {}
                    logger.info(
                        f"PCUPSTREAM {self._label} provider={extra.get('provider', '-')} "
                        f"model={getattr(chunk, 'model', None) or '-'}"
                    )
                yield chunk

        async def close(self):
            closer = getattr(self._inner, "close", None) or getattr(self._inner, "aclose", None)
            if closer is not None:
                await closer()

    class OpenAILLMService(Base):  # same name: Pipecat's metrics and logs name a service by class
        async def get_chat_completions(self, context):
            return _Stream(await super().get_chat_completions(context), str(self))

    return OpenAILLMService


def _llm_kwargs(s: BaselineSettings, llm_cls) -> dict:
    """The keyword arguments (service tier, settings) of the OpenAI Responses service."""
    from pipecat.services.openai.responses.llm import OpenAIResponsesReasoningConfig

    llm_settings = dict(
        model=s.llm.model,
        system_instruction=s.llm.system_prompt,
        reasoning=OpenAIResponsesReasoningConfig(effort=s.llm.reasoning_effort),
    )
    if s.llm.max_output_tokens is not None:
        llm_settings["max_completion_tokens"] = s.llm.max_output_tokens
    return dict(service_tier=s.llm.service_tier, settings=llm_cls.Settings(**llm_settings))


def build_mock_services(config: BotConfig):
    """The mock STT, the Responses LLM over a mock stream, and the mock TTS (``--mock-providers``)."""
    from pipecat.services.openai.responses.llm import OpenAIResponsesHttpLLMService

    import mock_providers

    assert config.mock is not None
    s = config.settings
    if s.llm.provider != "openai":
        raise RuntimeError(
            "--mock-providers mocks the OpenAI Responses service; use an `openai` agent file"
        )
    mocks = mock_providers.build_mock_services(
        config.mock,
        REPO_ROOT / "fixtures",
        _llm_kwargs(s, OpenAIResponsesHttpLLMService),
        s.tts.sample_rate,
    )
    return mocks.stt, mocks.llm, mocks.tts


def build_services(config: BotConfig):
    """Deepgram STT, OpenAI Responses LLM and Cartesia TTS from the mapped settings."""
    if config.mock is not None:
        return build_mock_services(config)
    from pipecat.services.cartesia.tts import CartesiaTTSService
    from pipecat.services.deepgram.stt import DeepgramSTTService
    from pipecat.services.openai.responses.llm import (
        OpenAIResponsesHttpLLMService,
        OpenAIResponsesLLMService,
    )

    s = config.settings
    stt = DeepgramSTTService(
        api_key=_key(s.stt.api_key_env),
        # "" is the SDK's own host; a Deepgram-compatible server otherwise (bench/fake_providers)
        base_url=s.stt.base_url or "",
        settings=DeepgramSTTService.Settings(
            model=s.stt.model,
            language=_language(s.stt.language),
            endpointing=s.stt.endpointing_ms if s.stt.endpointing_ms else False,
            utterance_end_ms=s.stt.utterance_end_ms,
            smart_format=s.stt.smart_format,
            punctuate=s.stt.punctuate,
            interim_results=s.stt.interim_results,
            keyterm=list(s.stt.keyterms) or None,
        ),
    )

    if s.llm.provider == "openai_compatible":
        llm = _compatible_llm(s)
    else:
        llm_cls = OpenAIResponsesHttpLLMService
        if config.llm_transport == "websocket":
            llm_cls = OpenAIResponsesLLMService
        llm = llm_cls(
            api_key=_key(s.llm.api_key_env),
            **_llm_kwargs(s, llm_cls),
        )

    if s.tts.provider == "deepgram_aura":
        from pipecat.services.deepgram.tts import DeepgramTTSService

        aura_settings = dict(voice=s.tts.voice)
        if s.tts.speed is not None:
            aura_settings["speed"] = s.tts.speed
        # Websocket /v1/speak, linear16 at the agent's rate: what brisvox's Aura adapter opens.
        tts = DeepgramTTSService(
            api_key=_key(s.tts.api_key_env),
            **({"base_url": s.tts.base_url} if s.tts.base_url else {}),
            sample_rate=s.tts.sample_rate,
            encoding="linear16",
            settings=DeepgramTTSService.Settings(**aura_settings),
        )
        return stt, llm, tts
    tts = CartesiaTTSService(
        api_key=_key(s.tts.api_key_env),
        sample_rate=s.tts.sample_rate,
        max_buffer_delay_ms=s.tts.max_buffer_delay_ms,
        settings=CartesiaTTSService.Settings(
            model=s.tts.model,
            voice=s.tts.voice,
            language=_language(s.tts.language),
        ),
    )
    return stt, llm, tts


def build_transport(websocket: WebSocket, start: Start):
    """The FastAPI websocket transport with the brisvox serializer for this call."""
    from pipecat.transports.websocket.fastapi import (
        FastAPIWebsocketParams,
        FastAPIWebsocketTransport,
    )

    return FastAPIWebsocketTransport(
        websocket=websocket,
        params=FastAPIWebsocketParams(
            audio_in_enabled=True,
            audio_out_enabled=True,
            audio_in_sample_rate=PIPELINE_IN_RATE,
            audio_out_sample_rate=start.sample_rate,
            # 20 ms frames, like brisvox (Pipecat's default is 40 ms).
            audio_out_10ms_chunks=2,
            add_wav_header=False,
            serializer=BrisvoxFrameSerializer(start),
        ),
    )


def build_pipeline(config: BotConfig, transport):
    """The call's pipeline: the real one, or the provider-free one of ``--fake``."""
    from pipecat.pipeline.pipeline import Pipeline

    if config.fake is not None:
        from pipecat.processors.audio.vad_processor import VADProcessor

        return Pipeline(
            [
                transport.input(),
                VADProcessor(vad_analyzer=build_vad(config.settings, config.silero_model)),
                FakeResponder(config.fake),
                transport.output(),
            ]
        )

    from pipecat.processors.aggregators.llm_context import LLMContext
    from pipecat.processors.aggregators.llm_response_universal import (
        LLMContextAggregatorPair,
        LLMUserAggregatorParams,
    )

    stt, llm, tts = build_services(config)
    if config.mock is not None and config.mock.models == "energy":
        from mock_providers import energy_vad_analyzer

        vad = energy_vad_analyzer(config.mock)
    else:
        vad = build_vad(config.settings, config.silero_model)
    aggregators = LLMContextAggregatorPair(
        LLMContext(),
        user_params=LLMUserAggregatorParams(
            vad_analyzer=vad,
            user_turn_strategies=build_turn_strategies(
                config.settings, config.smart_turn_threads, config.mock
            ),
        ),
    )
    return Pipeline(
        [
            transport.input(),
            stt,
            aggregators.user(),
            llm,
            tts,
            transport.output(),
            aggregators.assistant(),
        ]
    )


HAPPY_EYEBALLS_DELAY_SECS = 0.25
"""RFC 8305 "connection attempt delay": the next address is tried after 250 ms (brisvox:
``brisvox_providers::net::dial``; httpx/anyio and reqwest race address families the same
way)."""


def enable_happy_eyeballs(delay: float = HAPPY_EYEBALLS_DELAY_SECS) -> None:
    """Makes Pipecat's websocket services race address families like every other client.

    ``websockets.asyncio.client.connect`` tries a host's addresses one after another, each until
    the OS connect timeout. On a host whose IPv6 route silently drops packets (this Windows
    machine; ``api.cartesia.ai`` has 8 AAAA and 4 A records) the Cartesia handshake therefore
    times out before an IPv4 address is tried, and the first turns of a call get no audio.
    ``create_connection(happy_eyeballs_delay=...)`` is asyncio's own racer; this
    sets it for the services' ``_websocket_connect`` and nothing else. ``--no-happy-eyeballs``
    measures Pipecat as shipped."""
    import pipecat.services.websocket_service as websocket_service

    original = websocket_service.websocket_connect
    if getattr(original, "_brisvox_happy_eyeballs", False):
        return

    def connect(uri, **kwargs):
        kwargs.setdefault("happy_eyeballs_delay", delay)
        return original(uri, **kwargs)

    connect._brisvox_happy_eyeballs = True  # type: ignore[attr-defined]
    websocket_service.websocket_connect = connect


def _metrics_observer(call_id: str = "-"):
    """An observer that logs every TTFB metric as ``PCMETRIC ttfb <service> <model> <ms>`` (INFO).

    Pipecat's own service timers: STT = the end of speech (VAD stop) to the final transcript, LLM
    = the request to the first token, TTS = the text to the first audio. The provider-latency
    columns of the live comparison read them (``--provider-metrics``)."""
    from pipecat.frames.frames import MetricsFrame
    from pipecat.metrics.metrics import TTFBMetricsData
    from pipecat.observers.base_observer import BaseObserver, FramePushed

    class _Metrics(BaseObserver):
        def __init__(self):
            super().__init__(observe_every_push=False)

        async def on_push_frame(self, data: FramePushed):
            if not isinstance(data.frame, MetricsFrame):
                return
            for item in data.frame.data:
                if isinstance(item, TTFBMetricsData):
                    logger.info(
                        f"PCMETRIC ttfb {item.processor} {item.model or '-'} "
                        f"{item.value * 1000:.1f} call={call_id}"
                    )

    return _Metrics()


def _fake_responder_class():
    from pipecat.frames.frames import (
        Frame,
        OutputAudioRawFrame,
        VADUserStartedSpeakingFrame,
        VADUserStoppedSpeakingFrame,
    )
    from pipecat.processors.frame_processor import FrameDirection, FrameProcessor

    class _FakeResponder(FrameProcessor):
        """Answers every VAD stop with a tone after a fixed delay; a VAD start cancels it.

        Every VAD start is an interruption, as Pipecat's user aggregator makes every user turn
        start one: the serializer decides whether it is a barge-in that needs a ``clear``.
        """

        def __init__(self, fake: FakeConfig, **kwargs):
            super().__init__(**kwargs)
            self._fake = fake
            self._reply_task: asyncio.Task | None = None
            self._chunks = _tone_chunks(fake.reply_ms, fake.tone_rate)

        async def process_frame(self, frame: Frame, direction: FrameDirection):
            await super().process_frame(frame, direction)
            if isinstance(frame, VADUserStartedSpeakingFrame):
                await self._cancel_reply()
                await self.broadcast_interruption()
            elif isinstance(frame, VADUserStoppedSpeakingFrame):
                await self._cancel_reply()
                self._reply_task = self.create_task(self._reply())
            await self.push_frame(frame, direction)

        async def _cancel_reply(self):
            if self._reply_task is not None:
                await self.cancel_task(self._reply_task)
                self._reply_task = None

        async def _reply(self):
            await asyncio.sleep(self._fake.delay_ms / 1000)
            for chunk in self._chunks:
                await self.push_frame(
                    OutputAudioRawFrame(audio=chunk, sample_rate=self._fake.tone_rate, num_channels=1)
                )

    return _FakeResponder


def _tone_chunks(reply_ms: int, rate: int) -> list[bytes]:
    """A 440 Hz tone at half scale, in 20 ms chunks."""
    per_chunk = rate // 50
    total = rate * reply_ms // 1000
    samples = bytearray()
    for n in range(total):
        value = int(16000 * math.sin(2 * math.pi * 440 * n / rate))
        samples += value.to_bytes(2, "little", signed=True)
    step = per_chunk * 2
    return [bytes(samples[i : i + step]) for i in range(0, len(samples), step)]


def FakeResponder(fake: FakeConfig):  # noqa: N802 (a class built lazily, after Pipecat imports)
    """The ``--fake`` responder processor."""
    return _fake_responder_class()(fake)


# Optional shared token (the listening test's public real-call process, bench/railway): when set,
# a call must present it like a brisvox client does, as the subprotocol `brisvox-token.<token>` (a
# browser) or `?token=`; otherwise the upgrade is refused (HTTP 403). Unset: open, as before.
ACCESS_TOKEN_ENV = "BOT_ACCESS_TOKEN"


def _token_ok(websocket: WebSocket) -> bool:
    expected = os.environ.get(ACCESS_TOKEN_ENV, "")
    if not expected:
        return True
    presented = websocket.query_params.get("token", "")
    for proto in websocket.scope.get("subprotocols", []):
        if proto.startswith("brisvox-token."):
            presented = proto[len("brisvox-token.") :]
    import hmac

    return hmac.compare_digest(presented.encode(), expected.encode())


async def run_call(websocket: WebSocket, config: BotConfig) -> None:
    """One call: the handshake, then the pipeline until the call ends."""
    if not _token_ok(websocket):
        await websocket.close(code=1008)
        return
    offered = websocket.scope.get("subprotocols", [])
    await websocket.accept(subprotocol="brisvox" if "brisvox" in offered else None)
    try:
        first = await asyncio.wait_for(_receive_first(websocket), START_TIMEOUT_SECS)
        start = parse_start(first, fallback_call_id=uuid.uuid4().hex)
    except asyncio.TimeoutError:
        await _refuse(websocket, ProtocolError("start_timeout", "no start in time"))
        return
    except ProtocolError as error:
        await _refuse(websocket, error)
        return
    except WebSocketDisconnect:
        return

    from pipecat.pipeline.runner import PipelineRunner
    from pipecat.pipeline.worker import PipelineParams, PipelineWorker

    transport = build_transport(websocket, start)
    pipeline = build_pipeline(config, transport)
    worker = PipelineWorker(
        pipeline,
        params=PipelineParams(
            audio_in_sample_rate=PIPELINE_IN_RATE,
            audio_out_sample_rate=start.sample_rate,
            enable_metrics=config.provider_metrics,
        ),
        observers=[_metrics_observer(start.call_id)] if config.provider_metrics else None,
        enable_rtvi=False,
        idle_timeout_secs=None,
    )

    @transport.event_handler("on_client_disconnected")
    async def on_client_disconnected(_transport, _websocket):
        await worker.cancel()

    # Everything is built (models loaded): open the call. The pipeline starts right after, while
    # the client's first frames wait in the socket.
    await websocket.send_text(started_message(start))
    logger.info(f"call {start.call_id}: started ({start.sample_rate} Hz {start.encoding})")
    await PipelineRunner(handle_sigint=False).run(worker)
    logger.info(f"call {start.call_id}: ended")


async def _receive_first(websocket: WebSocket) -> str | bytes:
    message = await websocket.receive()
    if message.get("type") == "websocket.disconnect":
        raise WebSocketDisconnect(message.get("code", 1000))
    if message.get("text") is not None:
        return message["text"]
    return message.get("bytes") or b""


async def _refuse(websocket: WebSocket, error: ProtocolError) -> None:
    logger.warning(f"call refused: {error.code}: {error.message}")
    try:
        await websocket.send_text(error.to_json())
        await websocket.close(code=error.close_code)
    except Exception:  # the client may be gone already
        pass


def create_app(config: BotConfig) -> FastAPI:
    """The ASGI app: a websocket call on any path."""
    app = FastAPI()

    async def call(websocket: WebSocket):
        await run_call(websocket, config)

    # brisvox-server ignores the path; so does the baseline.
    app.add_api_websocket_route("/", call)
    app.add_api_websocket_route("/{path:path}", call)
    return app


def _load_env(path: Path | None) -> None:
    from dotenv import load_dotenv

    env = path or REPO_ROOT / ".env"
    if env.is_file():
        load_dotenv(env, override=False)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(description="Pipecat baseline bot for brisvox-bench v2v")
    p.add_argument("--host", default="127.0.0.1", help="listen address (default 127.0.0.1)")
    p.add_argument("--port", type=int, default=8765, help="listen port (default 8765)")
    p.add_argument("--agent", type=Path, default=DEFAULT_AGENT, help="brisvox agent TOML")
    p.add_argument("--env-file", type=Path, default=None, help="dotenv file (default: repo .env)")
    p.add_argument(
        "--llm-transport",
        choices=("http", "websocket"),
        default="http",
        help="OpenAI Responses over HTTP (the wire brisvox uses; default) or Pipecat's websocket mode",
    )
    p.add_argument(
        "--silero-model",
        default="auto",
        help="auto (brisvox's pinned v6.2.3 file when fetched, else Pipecat's), pipecat, or a path",
    )
    p.add_argument("--smart-turn-threads", type=int, default=1, help="Smart Turn ONNX threads")
    p.add_argument("--vad-start-ms", type=int, default=None, help="override the mapped VAD start")
    p.add_argument("--vad-stop-ms", type=int, default=None, help="override the mapped VAD stop")
    p.add_argument("--vad-min-volume", type=float, default=None, help="override Pipecat's 0.6")
    p.add_argument(
        "--llm-base-url",
        default=None,
        help="override llm.openai_compatible.base_url (e.g. https://openrouter.ai/api/v1)",
    )
    p.add_argument("--llm-model", default=None, help="override llm.openai_compatible.model")
    p.add_argument(
        "--no-happy-eyeballs",
        action="store_true",
        help="connect provider websockets as Pipecat ships (one address at a time)",
    )
    p.add_argument(
        "--provider-metrics",
        action="store_true",
        help="log each service's TTFB (STT, LLM, TTS) as PCMETRIC lines (live comparison)",
    )
    p.add_argument("--fake", action="store_true", help="provider-free responder (no keys)")
    p.add_argument("--fake-delay-ms", type=int, default=300)
    p.add_argument("--fake-reply-ms", type=int, default=600)
    p.add_argument(
        "--mock-providers",
        action="store_true",
        help="key-free mock STT/LLM/TTS with the brisvox_sim latencies (capacity benchmark)",
    )
    p.add_argument("--mock-fixture", default=MockConfig.fixture, help="fixture the mock STT plays")
    p.add_argument("--mock-fixtures-dir", type=Path, default=None, help="default: <repo>/fixtures")
    p.add_argument(
        "--mock-models",
        choices=("energy", "real"),
        default=MockConfig.models,
        help="energy: energy VAD + fixed Smart Turn model (like the brisvox sim target); "
        "real: Silero + Smart Turn v3 (heavier, not like for like)",
    )
    p.add_argument("--mock-stt-interim-ms", type=int, default=MockConfig.stt_interim_ms)
    p.add_argument("--mock-stt-final-ms", type=int, default=MockConfig.stt_final_ms)
    p.add_argument("--mock-llm-first-token-ms", type=int, default=MockConfig.llm_first_token_ms)
    p.add_argument("--mock-tts-first-byte-ms", type=int, default=MockConfig.tts_first_byte_ms)
    p.add_argument("--mock-tts-chunk-interval-ms", type=int, default=MockConfig.tts_chunk_interval_ms)
    p.add_argument("--mock-vad-start-ms", type=int, default=MockConfig.vad_start_ms)
    p.add_argument("--mock-vad-stop-ms", type=int, default=MockConfig.vad_stop_ms)
    p.add_argument("--mock-vad-rms", type=float, default=MockConfig.vad_rms)
    p.add_argument(
        "--workers",
        type=int,
        default=1,
        help="uvicorn worker processes (default 1: one process, one event loop, as in a typical Pipecat deployment)",
    )
    p.add_argument(
        "--pid-file",
        type=Path,
        default=None,
        help="write the main process id here (the pid `brisvox-bench capacity --pid` reads)",
    )
    p.add_argument("--no-prewarm", action="store_true", help="skip building one pipeline at startup")
    p.add_argument("--check", action="store_true", help="build one call's pipeline and exit")
    p.add_argument("--log-level", default="INFO")
    return p.parse_args(argv)


def config_from_args(args: argparse.Namespace) -> BotConfig:
    settings = agent_mapping.load(
        args.agent,
        vad_start_ms=args.vad_start_ms,
        vad_stop_ms=args.vad_stop_ms,
        vad_min_volume=args.vad_min_volume,
        llm_base_url=args.llm_base_url,
        llm_model=args.llm_model,
    )
    if not args.no_happy_eyeballs:
        enable_happy_eyeballs()
    fake = FakeConfig(args.fake_delay_ms, args.fake_reply_ms) if args.fake else None
    mock = None
    if args.mock_providers:
        if args.fake:
            raise agent_mapping.MappingError("--fake and --mock-providers are exclusive")
        mock = MockConfig(
            stt_interim_ms=args.mock_stt_interim_ms,
            stt_final_ms=args.mock_stt_final_ms,
            llm_first_token_ms=args.mock_llm_first_token_ms,
            tts_first_byte_ms=args.mock_tts_first_byte_ms,
            tts_chunk_interval_ms=args.mock_tts_chunk_interval_ms,
            vad_rms=args.mock_vad_rms,
            vad_start_ms=args.mock_vad_start_ms,
            vad_stop_ms=args.mock_vad_stop_ms,
            fixture=args.mock_fixture,
            fixtures_dir=args.mock_fixtures_dir,
            models=args.mock_models,
        )
    return BotConfig(
        settings=settings,
        llm_transport=args.llm_transport,
        fake=fake,
        mock=mock,
        smart_turn_threads=args.smart_turn_threads,
        provider_metrics=args.provider_metrics,
        silero_model=find_silero_model(args.silero_model),
    )


def _llm_description(config: BotConfig) -> str:
    s = config.settings
    if s.llm.provider == "openai_compatible":
        return f"openai_compatible {s.llm.model} @ {s.llm.base_url} (Chat Completions)"
    return f"openai {s.llm.model} (reasoning {s.llm.reasoning_effort}, {config.llm_transport})"


def prewarm(config: BotConfig) -> None:
    """Builds one call's pipeline before serving, in this process: the imports and the first
    construction of every class happen at startup, not inside the first calls (which would time
    out the clients' ``started`` wait). A production server does the same."""
    try:
        check(config)
    except Exception as error:  # a failing warm-up must not stop the server
        logger.warning(f"prewarm failed: {error}")


def check(config: BotConfig) -> None:
    """Builds every part of one call (no network): the offline smoke test of ``--check``."""

    class _NoSocket:
        headers: dict = {}

    start = Start(sample_rate=16000, encoding="pcm16", call_id="check", metadata={})
    transport = build_transport(_NoSocket(), start)  # type: ignore[arg-type]
    pipeline = build_pipeline(config, transport)
    names = [type(p).__name__ for p in pipeline.processors]
    logger.info(f"pipeline ok: {' -> '.join(names)}")


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    logger.remove()
    logger.add(sys.stderr, level=args.log_level)
    _load_env(args.env_file)
    try:
        config = config_from_args(args)
        if config.fake is None and config.mock is None:
            check_keys(config.settings)
    except (agent_mapping.MappingError, RuntimeError, OSError) as error:
        logger.error(str(error))
        return 2
    s = config.settings
    logger.info(
        f"agent {s.name}: STT deepgram {s.stt.model} ({s.stt.language}), LLM {_llm_description(config)}, TTS cartesia {s.tts.model} "
        f"voice {s.tts.voice} @ {s.tts.sample_rate} Hz; VAD {s.vad}; turn {s.turn}"
        + ("; FAKE providers" if config.fake else "")
        + (
            f"; MOCK providers ({config.mock.models} models, floor {config.mock.floor_ms} ms)"
            if config.mock
            else ""
        )
    )
    for note in s.notes:
        logger.warning(f"mapping: {note}")
    if config.mock is not None and config.mock.models == "energy":
        logger.info("mock providers: energy VAD and fixed Smart Turn model (no Silero, no ONNX)")
    elif config.silero_model is None:
        logger.warning(
            "mapping: Silero VAD is Pipecat's bundled v6.0, not brisvox's v6.2.3 "
            "(run scripts/models/fetch_models.py or pass --silero-model)"
        )
    else:
        logger.info(f"Silero VAD v6.2.3 from {config.silero_model}")
    if args.check:
        check(config)
        return 0

    # The pid the capacity benchmark reads CPU and memory from (`brisvox-bench capacity --pid`);
    # worker processes (`--workers`) are its children, so the process tree is the whole bot.
    logger.info(f"pid {os.getpid()}: listening on {args.host}:{args.port} ({args.workers} worker process(es))")
    if args.pid_file is not None:
        args.pid_file.write_text(str(os.getpid()))
    if not args.no_prewarm and args.workers <= 1:  # worker processes prewarm themselves
        prewarm(config)

    import uvicorn

    options = dict(
        host=args.host, port=args.port, log_level="warning", ws_ping_interval=10, ws_ping_timeout=30
    )
    if args.workers > 1:
        # Worker processes import the app and build it from the same command line.
        os.environ[ARGV_ENV] = json.dumps(sys.argv[1:] if argv is None else list(argv))
        uvicorn.run("bot:app_factory", factory=True, workers=args.workers, app_dir=str(HERE), **options)
    else:
        uvicorn.run(create_app(config), **options)
    return 0


ARGV_ENV = "BRISVOX_BOT_ARGV"
"""The command line, for worker processes (``--workers N``)."""


def app_factory() -> FastAPI:
    """The app of a worker process: built from the command line the main process was given."""
    args = parse_args(json.loads(os.environ[ARGV_ENV]))
    logger.remove()
    logger.add(sys.stderr, level=args.log_level)
    _load_env(args.env_file)
    config = config_from_args(args)
    if not args.no_prewarm:
        prewarm(config)
    return create_app(config)


if __name__ == "__main__":
    sys.exit(main())
