"""LiveKit Agents worker for the brisvox capacity comparison: real LiveKit pipeline, mock providers.

    python agent.py start --log-level warn          # production mode: the framework's defaults

The worker registers with a LiveKit server (``LIVEKIT_URL``, ``LIVEKIT_API_KEY``,
``LIVEKIT_API_SECRET``) and is dispatched to every new room (no agent name = automatic
dispatch). Each job runs one ``AgentSession``:

- STT / LLM / TTS: ``lk_mock_providers`` (key-free, the latencies and shapes of the brisvox and
  Pipecat mocks, behind LiveKit's plugin interfaces);
- VAD: the session default, ``inference.VAD(model="silero")`` (Silero v5 on the native
  ``livekit-local-inference`` runtime; ``min_silence_duration`` 0.25 s);
- turn detection: the session default, ``inference.TurnDetector()``, which resolves to the local
  audio model ``v1-mini`` outside LiveKit Cloud (the cloud model ``v1`` needs LiveKit Inference);
- worker options: the framework's defaults (a process per job, ``num_idle_processes`` = the
  container's CPUs up to 4, ``load_threshold`` 0.7, forkserver start) unless the ``LK_*``
  variables below override them (the "tuned" variant of the benchmark).

No ``Agent.on_enter`` greeting is generated: the first thing the agent says is the answer to the
caller's first utterance, as in the other two systems.

Environment (all optional; unset = the framework's default):

| variable | meaning |
|---|---|
| ``LK_LOAD_THRESHOLD`` | ``AgentServer(load_threshold=...)`` |
| ``LK_NUM_IDLE_PROCESSES`` | ``AgentServer(num_idle_processes=...)`` |
| ``LK_EXECUTOR`` | ``process`` (default) or ``thread``: ``AgentServer(job_executor_type=...)`` |
| ``LK_UNLIKELY_THRESHOLD`` | ``inference.TurnDetector(unlikely_threshold=...)``: below this end-of-turn probability the session waits ``max_delay`` instead of ``min_delay`` (the model still runs on every turn) |
| ``LK_MIN_ENDPOINTING`` / ``LK_MAX_ENDPOINTING`` | ``turn_handling["endpointing"]`` (seconds) |
| ``LK_VAD_MIN_SILENCE`` | ``inference.VAD(min_silence_duration=...)`` (seconds) |
| ``LK_PREEMPTIVE`` | ``0`` turns preemptive generation off |
| ``BRISVOX_FIXTURES_DIR`` / ``BRISVOX_MOCK_FIXTURE`` | the script the mocks play (default ``en_two_utterances``) |
"""

from __future__ import annotations

import logging
import os

from livekit.agents import Agent, AgentServer, AgentSession, JobContext, cli, inference
from livekit.agents.worker import JobExecutorType

from lk_mock_providers import MockLLM, MockSetup, MockSTT, MockTTS

logger = logging.getLogger("brisvox-bench-agent")


def _env_float(name: str) -> float | None:
    value = os.environ.get(name)
    return float(value) if value not in (None, "") else None


def _env_int(name: str) -> int | None:
    value = os.environ.get(name)
    return int(value) if value not in (None, "") else None


def build_server() -> AgentServer:
    kwargs: dict = {}
    if (threshold := _env_float("LK_LOAD_THRESHOLD")) is not None:
        kwargs["load_threshold"] = threshold
    if (idle := _env_int("LK_NUM_IDLE_PROCESSES")) is not None:
        kwargs["num_idle_processes"] = idle
    if os.environ.get("LK_EXECUTOR") == "thread":
        kwargs["job_executor_type"] = JobExecutorType.THREAD
    return AgentServer(**kwargs)


server = build_server()


def build_session() -> AgentSession:
    setup = MockSetup.from_env()
    session_kwargs: dict = {}
    turn_handling: dict = {}
    endpointing: dict = {}
    if (v := _env_float("LK_MIN_ENDPOINTING")) is not None:
        endpointing["min_delay"] = v
    if (v := _env_float("LK_MAX_ENDPOINTING")) is not None:
        endpointing["max_delay"] = v
    if endpointing:
        turn_handling["endpointing"] = endpointing
    if os.environ.get("LK_PREEMPTIVE") == "0":
        turn_handling["preemptive_generation"] = {"enabled": False}
    if (v := _env_float("LK_UNLIKELY_THRESHOLD")) is not None:
        turn_handling["turn_detection"] = inference.TurnDetector(unlikely_threshold=v)
    if turn_handling:
        session_kwargs["turn_handling"] = turn_handling
    if (v := _env_float("LK_VAD_MIN_SILENCE")) is not None:
        session_kwargs["vad"] = inference.VAD(model="silero", min_silence_duration=v)
    return AgentSession(stt=MockSTT(setup), llm=MockLLM(setup), tts=MockTTS(setup), **session_kwargs)


def attach_trace(session: AgentSession, room_name: str) -> None:
    """LK_TRACE=1: one line per session event with the wall-clock time, for the latency breakdown."""
    import time

    def emit(kind: str, detail: str = "") -> None:
        print(f"TRACE {room_name} {time.time():.4f} {kind} {detail}", flush=True)

    session.on("user_state_changed", lambda ev: emit("user_state", ev.new_state))
    session.on("agent_state_changed", lambda ev: emit("agent_state", ev.new_state))
    session.on("user_input_transcribed", lambda ev: emit("transcript", f"final={ev.is_final}"))


@server.rtc_session()
async def entrypoint(ctx: JobContext) -> None:
    session = build_session()
    if os.environ.get("LK_TRACE") == "1":
        attach_trace(session, ctx.room.name)
    await session.start(
        agent=Agent(instructions="You are the voice assistant of Example Shop. Answer in one short sentence."),
        room=ctx.room,
    )


if __name__ == "__main__":
    cli.run_app(server)
