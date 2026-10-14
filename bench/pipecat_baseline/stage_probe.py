"""Stage timestamps of the Pipecat baseline: a diagnostic wrapper around ``bot.py``.

It runs the bot unchanged (every ``bot.py`` option works) and appends one line per key frame to
the file named by ``$PCTRACE_FILE``::

    <perf_counter ms> <event> [<extra>]

Events, in the order a turn produces them: ``vad_start`` / ``vad_stop`` (the user aggregator's
VAD frames, with ``n_audio`` = the 20 ms input frames seen so far, which dates the stop in audio
time), ``smart_turn_start`` / ``smart_turn_end`` (the analyser call, awaited inline by the stop
strategy), ``stt_final`` (the STT's final transcript), ``llm_ctx`` (the user turn ended: the
context reached the LLM), ``llm_resp_start``, ``llm_first_text``, ``tts_started``,
``tts_first_audio`` (the first TTS audio frame), ``audio_out_first`` (the first bot audio frame
handed to the websocket by the serializer after at least 0.5 s of none).

It patches Pipecat 1.12 internals (``PipelineWorker.__init__``, ``BaseSmartTurn.analyze_end_of_turn``,
the brisvox serializer), so it is a measuring tool for the pinned version, not part of the
benchmark runs: no probe is active in ``bot.py`` itself and the capacity numbers never use it.
``docs/LATENCY.md`` ("Unloaded floor: brisvox vs Pipecat with the same mocks") shows how it was used::

    PCTRACE_FILE=/tmp/pc.txt python stage_probe.py --mock-providers --mock-models real --port 8765
"""

from __future__ import annotations

import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
os.chdir(HERE)

_OUT = open(os.environ["PCTRACE_FILE"], "w", buffering=1, encoding="utf-8")  # noqa: SIM115


def log(name: str, extra: str = "") -> None:
    _OUT.write(f"{time.perf_counter() * 1000:.3f} {name} {extra}\n")


import bot  # noqa: E402
import brisvox_protocol  # noqa: E402
from pipecat.audio.turn.smart_turn import base_smart_turn  # noqa: E402
from pipecat.frames.frames import (  # noqa: E402
    InputAudioRawFrame,
    InterimTranscriptionFrame,
    LLMContextFrame,
    LLMFullResponseStartFrame,
    OutputAudioRawFrame,
    TextFrame,
    TranscriptionFrame,
    TTSAudioRawFrame,
    TTSStartedFrame,
    UserStoppedSpeakingFrame,
    VADUserStartedSpeakingFrame,
    VADUserStoppedSpeakingFrame,
)
from pipecat.observers.base_observer import BaseObserver  # noqa: E402
from pipecat.pipeline import worker  # noqa: E402


class Probe(BaseObserver):
    """Logs the first push of each key frame (``observe_every_push=False``: one per hop)."""

    def __init__(self):
        super().__init__(observe_every_push=False)
        self.n_audio = 0
        self.seen_text = False
        self.seen_tts_audio = False

    async def on_push_frame(self, data):
        frame = data.frame
        src = type(data.source).__name__
        if isinstance(frame, InputAudioRawFrame):
            if "Input" in src:
                self.n_audio += 1
        elif isinstance(frame, VADUserStartedSpeakingFrame):
            log("vad_start", f"n_audio={self.n_audio} src={src}")
        elif isinstance(frame, VADUserStoppedSpeakingFrame):
            log("vad_stop", f"n_audio={self.n_audio} src={src}")
        elif isinstance(frame, InterimTranscriptionFrame):
            return
        elif isinstance(frame, TranscriptionFrame):
            log("stt_final", f"src={src}")
        elif isinstance(frame, UserStoppedSpeakingFrame):
            log("user_stopped", f"src={src}")
        elif isinstance(frame, LLMContextFrame):
            log("llm_ctx", f"src={src}")  # the assistant aggregator also pushes one: filter on src
            self.seen_text = self.seen_tts_audio = False
        elif isinstance(frame, LLMFullResponseStartFrame):
            log("llm_resp_start", f"src={src}")
        elif isinstance(frame, TextFrame) and "LLM" in src and not self.seen_text:
            self.seen_text = True
            log("llm_first_text", f"src={src}")
        elif isinstance(frame, TTSStartedFrame):
            log("tts_started", f"src={src}")
        elif isinstance(frame, TTSAudioRawFrame) and not self.seen_tts_audio:
            self.seen_tts_audio = True
            log("tts_first_audio", f"src={src}")


_worker_init = worker.PipelineWorker.__init__


def _init(self, *args, **kwargs):
    kwargs["observers"] = [*(kwargs.get("observers") or []), Probe()]
    _worker_init(self, *args, **kwargs)


worker.PipelineWorker.__init__ = _init

_analyze = base_smart_turn.BaseSmartTurn.analyze_end_of_turn


async def _timed_analyze(self):
    log("smart_turn_start")
    try:
        return await _analyze(self)
    finally:
        log("smart_turn_end")


base_smart_turn.BaseSmartTurn.analyze_end_of_turn = _timed_analyze

_serialize = brisvox_protocol.BrisvoxFrameSerializer.serialize
_last_audio = {"t": 0.0}


async def _timed_serialize(self, frame):
    out = await _serialize(self, frame)
    if isinstance(frame, OutputAudioRawFrame) and out is not None:
        now = time.perf_counter()
        if now - _last_audio["t"] > 0.5:
            log("audio_out_first", f"bytes={len(frame.audio)}")
        _last_audio["t"] = now
    return out


brisvox_protocol.BrisvoxFrameSerializer.serialize = _timed_serialize

if __name__ == "__main__":
    sys.exit(bot.main(sys.argv[1:]))
