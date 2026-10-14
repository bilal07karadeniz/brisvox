"""The brisvox websocket protocol (docs/WEBSOCKET_PROTOCOL.md, version 1) for Pipecat.

Two parts:

- :func:`parse_start` validates the client's ``start`` message, the handshake that ``bot.py``
  answers with ``started`` before the Pipecat pipeline runs (Pipecat serializers map one message
  to one frame and cannot answer a handshake themselves).
- :class:`BrisvoxFrameSerializer` maps the rest of the call onto Pipecat frames: binary audio in
  both directions (pcm16 or mu-law at the call's rate), ``clear`` on a Pipecat interruption
  that cuts bot audio the client may still be playing, ``stop`` when the pipeline ends, ``dtmf`` as a keypad frame and the client's ``stop`` as a
  pipeline cancel.

Marks are not sent: Pipecat trims its LLM context from its own playout clock, not from client
echoes, so a ``mark`` from the client is ignored. Everything here is the server side of the
protocol only, as far as the V2V benchmark client (``brisvox-bench v2v``) needs it.
"""

from __future__ import annotations

import json
import time
from collections.abc import Callable
from dataclasses import dataclass

from loguru import logger
from pipecat.audio.dtmf.types import KeypadEntry
from pipecat.audio.utils import create_stream_resampler, pcm_to_ulaw, ulaw_to_pcm
from pipecat.frames.frames import (
    AudioRawFrame,
    CancelFrame,
    CancelWorkerFrame,
    EndFrame,
    Frame,
    InputAudioRawFrame,
    InputDTMFFrame,
    InterruptionFrame,
    OutputTransportMessageFrame,
    OutputTransportMessageUrgentFrame,
)
from pipecat.processors.frame_processor import FrameProcessorSetup
from pipecat.serializers.base_serializer import FrameSerializer

PROTOCOL_VERSION = 1
"""The protocol version this adapter speaks (``start.version``)."""

SUPPORTED_RATES = frozenset({8000, 16000, 22050, 24000, 44100, 48000})
"""Sample rates a ``start`` may ask for."""

ENCODINGS = frozenset({"pcm16", "mulaw"})
"""Audio encodings a ``start`` may ask for."""

MAX_CALL_ID_BYTES = 128
"""Longest accepted ``start.call_id``."""

MAX_CONTROL_BYTES = 16 * 1024
"""Longest accepted text frame (the brisvox server's default ``max_control_bytes``)."""

MAX_AUDIO_BYTES = 64 * 1024
"""Longest accepted binary frame (the brisvox server's default ``max_audio_bytes``)."""

PLAYOUT_GRACE_SECS = 0.2
"""How long after the modelled end of the bot audio sent so far an interruption still counts as
a barge-in: covers the client's buffering and the network between serializing and playing."""


class ProtocolError(Exception):
    """A protocol fault: ``code`` is the brisvox error code, ``close_code`` the websocket close."""

    def __init__(self, code: str, message: str, close_code: int = 1008):
        super().__init__(message)
        self.code = code
        self.message = message
        self.close_code = close_code

    def to_json(self) -> str:
        """The fatal ``error`` message for this fault."""
        return error_message(self.code, self.message, fatal=True)


@dataclass(frozen=True)
class Start:
    """A validated ``start``."""

    sample_rate: int
    encoding: str
    call_id: str
    metadata: dict[str, str]


def error_message(code: str, message: str, fatal: bool) -> str:
    """An ``error`` text frame."""
    return json.dumps({"type": "error", "code": code, "message": message, "fatal": fatal})


def started_message(start: Start) -> str:
    """The ``started`` answer to a validated ``start``."""
    return json.dumps(
        {
            "type": "started",
            "version": PROTOCOL_VERSION,
            "call_id": start.call_id,
            "sample_rate": start.sample_rate,
            "encoding": start.encoding,
        }
    )


def stop_message(reason: str = "ended") -> str:
    """A ``stop`` text frame (``ended`` or ``shutdown``)."""
    return json.dumps({"type": "stop", "reason": reason})


def parse_start(text: str | bytes, fallback_call_id: str) -> Start:
    """Validates the first message of a call, which must be ``start``.

    Raises :class:`ProtocolError` with the code the brisvox server would use.
    """
    if isinstance(text, bytes):
        raise ProtocolError("audio_before_start", "a binary frame before start")
    if len(text.encode("utf-8")) > MAX_CONTROL_BYTES:
        raise ProtocolError("frame_too_large", "control frame too large", close_code=1009)
    try:
        message = json.loads(text)
    except json.JSONDecodeError:
        raise ProtocolError("invalid_json", "not JSON", close_code=1002) from None
    if not isinstance(message, dict) or not isinstance(message.get("type"), str):
        raise ProtocolError("invalid_json", "not a JSON object with a string type", 1002)
    if message["type"] != "start":
        raise ProtocolError("invalid_message", "the first message must be start")
    version = message.get("version")
    if version != PROTOCOL_VERSION:
        raise ProtocolError("unsupported_version", f"version {version!r} is not supported")
    rate = message.get("sample_rate")
    if not isinstance(rate, int) or isinstance(rate, bool) or rate not in SUPPORTED_RATES:
        raise ProtocolError("unsupported_sample_rate", f"sample rate {rate!r} is not supported")
    encoding = message.get("encoding", "pcm16")
    if encoding not in ENCODINGS:
        raise ProtocolError("invalid_message", "encoding must be pcm16 or mulaw")
    call_id = message.get("call_id") or fallback_call_id
    if not isinstance(call_id, str) or len(call_id.encode("utf-8")) > MAX_CALL_ID_BYTES:
        raise ProtocolError("invalid_message", "call_id must be a string of at most 128 bytes")
    metadata = message.get("metadata") or {}
    if not isinstance(metadata, dict) or not all(
        isinstance(k, str) and isinstance(v, str) for k, v in metadata.items()
    ):
        raise ProtocolError("invalid_message", "metadata must be a string map")
    return Start(sample_rate=rate, encoding=encoding, call_id=call_id, metadata=dict(metadata))


class BrisvoxFrameSerializer(FrameSerializer):
    """Pipecat frames to and from the brisvox websocket protocol, after the handshake.

    Input audio arrives at the call's rate and encoding and leaves as 16-bit PCM at the
    pipeline's input rate (``PipelineParams.audio_in_sample_rate``, 16 kHz: the rate brisvox
    feeds its VAD, Smart Turn and STT). Output audio reaches the serializer already resampled
    to the call's rate by Pipecat's output transport (``audio_out_sample_rate`` is set to the
    call's rate); mu-law calls are encoded here.

    A ``clear`` (and a new epoch) goes out only for an interruption that cuts bot audio the
    client may still be playing, which is when brisvox-server sends one. Pipecat's user
    aggregator broadcasts an interruption on every user turn start, also when the bot is silent;
    those are not barge-ins and are not sent. Whether audio may still be playing is modelled
    from the audio serialized so far: each chunk plays for its duration after the previous one
    ends (Pipecat's websocket output paces chunks in real time), plus
    :data:`PLAYOUT_GRACE_SECS`. ``clock`` (monotonic seconds) is injectable for tests.
    """

    def __init__(self, start: Start, clock: Callable[[], float] = time.monotonic, **kwargs):
        super().__init__(**kwargs)
        self._start = start
        self._in_rate = start.sample_rate
        self._clock = clock
        self._playout_end: float | None = None  # None: no bot audio since the last clear
        self._epoch = 0
        self._stopped = False
        self._input_resampler = create_stream_resampler(
            clear_after_secs=self._params.resampler_clear_after_secs
        )
        self._output_resampler = create_stream_resampler(
            clear_after_secs=self._params.resampler_clear_after_secs
        )

    @property
    def epoch(self) -> int:
        """The epoch of the last ``clear`` sent (0 before the first)."""
        return self._epoch

    async def setup(self, setup: FrameProcessorSetup):
        """Takes the pipeline's input rate."""
        await super().setup(setup)
        self._in_rate = setup.audio_in_sample_rate or self._start.sample_rate

    async def serialize(self, frame: Frame) -> str | bytes | None:
        """A frame going to the client."""
        if self.should_ignore_frame(frame):
            return None
        if isinstance(frame, InterruptionFrame):
            if not self._bot_audio_playing():
                # A user turn start while the bot is silent: nothing to clear.
                return None
            self._playout_end = None
            self._epoch += 1
            return json.dumps({"type": "clear", "epoch": self._epoch})
        if isinstance(frame, (EndFrame, CancelFrame)):
            if self._stopped:
                return None
            self._stopped = True
            return stop_message("ended")
        if isinstance(frame, AudioRawFrame):
            if not frame.audio:
                return None
            self._note_bot_audio(frame)
            if self._start.encoding == "mulaw":
                return await pcm_to_ulaw(
                    frame.audio, frame.sample_rate, self._start.sample_rate, self._output_resampler
                )
            if frame.sample_rate != self._start.sample_rate:
                return await self._output_resampler.resample(
                    frame.audio, frame.sample_rate, self._start.sample_rate
                )
            return frame.audio
        if isinstance(frame, (OutputTransportMessageFrame, OutputTransportMessageUrgentFrame)):
            return json.dumps(frame.message)
        return None

    def _note_bot_audio(self, frame: AudioRawFrame) -> None:
        bytes_per_sec = 2 * max(frame.num_channels, 1) * max(frame.sample_rate, 1)
        now = self._clock()
        start = now if self._playout_end is None else max(self._playout_end, now)
        self._playout_end = start + len(frame.audio) / bytes_per_sec

    def _bot_audio_playing(self) -> bool:
        if self._playout_end is None:
            return False
        return self._clock() < self._playout_end + PLAYOUT_GRACE_SECS

    async def deserialize(self, data: str | bytes) -> Frame | None:
        """A message from the client."""
        if isinstance(data, (bytes, bytearray)):
            return await self._audio(bytes(data))
        if len(data) > MAX_CONTROL_BYTES:
            logger.warning("brisvox protocol: control frame too large, ignored")
            return None
        try:
            message = json.loads(data)
        except json.JSONDecodeError:
            logger.warning("brisvox protocol: a text frame that is not JSON, ignored")
            return None
        if not isinstance(message, dict):
            return None
        kind = message.get("type")
        if kind == "stop":
            # The client hangs up: cancel the pipeline at once. The output transport then sends
            # `stop{ended}` (serialize(CancelFrame)) and closes the socket.
            return CancelWorkerFrame(reason="client stop")
        if kind == "dtmf":
            digit = str(message.get("digit", ""))
            try:
                return InputDTMFFrame(KeypadEntry(digit))
            except ValueError:
                # A-D are valid in the protocol but Pipecat has no keypad entry for them.
                logger.debug(f"brisvox protocol: DTMF digit {digit!r} has no Pipecat entry")
                return None
        # `mark` echoes (no marks are sent), a second `start` and unknown types: ignored.
        return None

    async def _audio(self, data: bytes) -> Frame | None:
        if not data or len(data) > MAX_AUDIO_BYTES:
            return None
        if self._start.encoding == "mulaw":
            pcm = await ulaw_to_pcm(
                data, self._start.sample_rate, self._in_rate, self._input_resampler
            )
        else:
            if len(data) % 2:
                logger.warning("brisvox protocol: odd-length pcm16 frame, ignored")
                return None
            pcm = data
            if self._start.sample_rate != self._in_rate:
                pcm = await self._input_resampler.resample(
                    data, self._start.sample_rate, self._in_rate
                )
        if not pcm:
            return None
        return InputAudioRawFrame(audio=pcm, sample_rate=self._in_rate, num_channels=1)
