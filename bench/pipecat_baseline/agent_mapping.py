"""Read a brisvox agent definition (docs/CONFIG.md) and derive the Pipecat bot's settings.

The Pipecat baseline reads the very file brisvox-server runs (``BRISVOX_AGENT_FILE``), so the
providers, models, voice, language, prompt and turn settings of both systems come from one
source. This module is pure (no Pipecat import): ``bot.py`` turns :class:`BaselineSettings`
into Pipecat services. The mapping table with the reasons is in README.md.

Only the subset the benchmark needs is supported: Deepgram STT, an OpenAI LLM (Responses API) or
an OpenAI-compatible one (Chat Completions, e.g. OpenRouter), Cartesia TTS, no
tools. Anything else is refused with :class:`MappingError`, so a comparison never runs on a
silently different setup. Settings without a Pipecat counterpart are listed in
:attr:`BaselineSettings.notes` and logged at startup.
"""

from __future__ import annotations

import tomllib
from dataclasses import dataclass, field
from pathlib import Path

# brisvox defaults for keys the agent file may leave out. tests/test_mapping.py checks each one
# against the Rust source (path in the comment), so a changed default fails the test.
BRISVOX_OPENAI_MODEL = "gpt-6-luna"  # crates/brisvox-providers/src/openai/config.rs DEFAULT_MODEL
BRISVOX_CARTESIA_MODEL = "sonic-3.6"  # crates/brisvox-providers/src/cartesia/mod.rs DEFAULT_MODEL
BRISVOX_DEEPGRAM_MODEL = "nova-3"  # crates/brisvox-providers/src/deepgram/config.rs (default)
BRISVOX_DEEPGRAM_ENDPOINTING_MS = 300  # deepgram/config.rs default endpointing
BRISVOX_DEEPGRAM_SMART_FORMAT = True  # deepgram/config.rs default
BRISVOX_DEEPGRAM_PUNCTUATE = False  # deepgram/config.rs default
BRISVOX_DEEPGRAM_INTERIM_RESULTS = True  # deepgram/config.rs default
BRISVOX_CARTESIA_MAX_BUFFER_DELAY_MS = 0  # cartesia/mod.rs default max_buffer_delay_ms
BRISVOX_TTS_SAMPLE_RATE = 24000  # docs/CONFIG.md `tts.sample_rate` default

# The production VAD (crates/brisvox-inference/src/vad.rs, VadParams::default; the server uses
# it unchanged through VadWorkerConfig::from_engine). Not configurable in the agent file.
BRISVOX_VAD_THRESHOLD = 0.5
BRISVOX_VAD_MIN_SPEECH_MS = 64
BRISVOX_VAD_MIN_SILENCE_MS = 100

# Turn and interruption defaults (crates/brisvox-agent/src/turn.rs, brisvox-engine config.rs).
BRISVOX_FALLBACK_SILENCE_MS = 3000
BRISVOX_RECOMMENDED_MIN_SPEECH_MS = 300
BRISVOX_RECOMMENDED_ECHO_GUARD_MS = 400

# brisvox `llm.openai.reasoning_effort` to the Responses API `reasoning.effort` value; unset is
# the adapter's default, "none" (crates/brisvox-providers/src/openai/config.rs).
REASONING_EFFORT = {
    "disabled": "none",
    "low": "low",
    "medium": "medium",
    "high": "high",
    "xhigh": "xhigh",
    "max": "max",
}


class MappingError(ValueError):
    """The agent file uses something the baseline cannot reproduce in Pipecat."""


@dataclass(frozen=True)
class VadSettings:
    """Pipecat ``VADParams`` (Silero)."""

    confidence: float
    start_secs: float
    stop_secs: float
    min_volume: float | None  # None: Pipecat's default (0.6), brisvox has no volume gate


@dataclass(frozen=True)
class TurnSettings:
    """End-of-turn strategy: ``smart_turn`` (Smart Turn v3) or ``speech_timeout``."""

    policy: str
    smart_turn_stop_secs: float  # Smart Turn's fallback silence (brisvox fallback_silence_ms)
    speech_timeout_secs: float  # fixed_silence only
    min_words: int  # 0: VAD/transcription start strategies (Pipecat default), else MinWords


@dataclass(frozen=True)
class SttSettings:
    """Deepgram live STT."""

    model: str
    language: str
    endpointing_ms: int | None  # None: off ("false")
    utterance_end_ms: int | None
    smart_format: bool
    punctuate: bool
    interim_results: bool
    keyterms: tuple[str, ...]
    api_key_env: str


@dataclass(frozen=True)
class LlmSettings:
    """OpenAI Responses API (``provider = "openai"``) or an OpenAI-compatible Chat Completions
    server (``provider = "openai_compatible"``, ``base_url`` set)."""

    model: str
    system_prompt: str
    reasoning_effort: str
    max_output_tokens: int | None
    service_tier: str | None
    api_key_env: str | None  # None only for an openai_compatible server without a key
    provider: str = "openai"
    base_url: str | None = None  # openai_compatible only, with the version prefix
    temperature: float | None = None  # openai_compatible only
    # openai_compatible only: OpenRouter `provider` routing object of the request body, from
    # `openrouter_order` / `openrouter_allow_fallbacks` (None = not sent)
    provider_routing: dict | None = None


@dataclass(frozen=True)
class TtsSettings:
    """Cartesia websocket TTS (``provider = "cartesia"``) or Deepgram Aura websocket TTS
    (``provider = "deepgram_aura"``: ``voice`` is the Aura model string, ``model`` repeats it)."""

    model: str
    voice: str
    language: str
    sample_rate: int
    max_buffer_delay_ms: int
    api_key_env: str
    provider: str = "cartesia"
    speed: float | None = None  # deepgram_aura only


@dataclass(frozen=True)
class BaselineSettings:
    """Everything the Pipecat bot takes from the agent file."""

    name: str
    language: str
    vad: VadSettings
    turn: TurnSettings
    stt: SttSettings
    llm: LlmSettings
    tts: TtsSettings
    notes: tuple[str, ...] = field(default_factory=tuple)

    def api_key_envs(self) -> tuple[str, ...]:
        """The environment variables holding the STT, LLM and TTS keys (no LLM entry for a keyless
        OpenAI-compatible server)."""
        llm = (self.llm.api_key_env,) if self.llm.api_key_env else ()
        return (self.stt.api_key_env, *llm, self.tts.api_key_env)


def load(path: str | Path, **overrides) -> BaselineSettings:
    """Reads a TOML agent definition and maps it (see :func:`map_agent`)."""
    with open(path, "rb") as f:
        return map_agent(tomllib.load(f), **overrides)


def _table(doc: dict, key: str) -> dict:
    value = doc.get(key, {})
    if not isinstance(value, dict):
        raise MappingError(f"`{key}` must be a table")
    return value


def _primary(tag: str) -> str:
    return tag.split("-")[0].lower()


def map_agent(
    doc: dict,
    *,
    vad_start_ms: int | None = None,
    vad_stop_ms: int | None = None,
    vad_min_volume: float | None = None,
    llm_base_url: str | None = None,
    llm_model: str | None = None,
) -> BaselineSettings:
    """Maps a parsed agent definition onto the Pipecat baseline's settings.

    The keyword arguments override the derived VAD values (ablations; README.md lists them) and,
    for an ``openai_compatible`` agent, the LLM server's base URL and model (``--llm-base-url``,
    ``--llm-model``).
    """
    notes: list[str] = []
    agent = _table(doc, "agent")
    name = agent.get("name", "agent")
    language = agent.get("language")
    if not isinstance(language, str) or not language:
        raise MappingError("`agent.language` is required")
    prompt = agent.get("system_prompt")
    if not isinstance(prompt, str) or not prompt.strip():
        raise MappingError("`agent.system_prompt` is required")
    if agent.get("tools"):
        raise MappingError(
            "`agent.tools` is not supported by the baseline (and brisvox-server refuses tools "
            "too); use an agent without tools, such as bench/pipecat_baseline/agent.toml"
        )

    # --- interruption -> VAD start (Pipecat interrupts when the VAD reports a start) -----------
    interruption = _table(doc, "interruption")
    preset = interruption.get("preset", "recommended")
    if preset not in ("recommended", "off"):
        raise MappingError(f"unknown `interruption.preset` {preset!r}")
    recommended = preset == "recommended"
    min_speech_ms = interruption.get(
        "min_speech_ms", BRISVOX_RECOMMENDED_MIN_SPEECH_MS if recommended else 0
    )
    min_words = int(interruption.get("min_words", 0))
    echo_guard_ms = interruption.get(
        "echo_guard_ms", BRISVOX_RECOMMENDED_ECHO_GUARD_MS if recommended else 0
    )
    backchannels = interruption.get("backchannels", ["en", "tr"] if recommended else [])
    hangover_ms = interruption.get("vad_hangover_ms", BRISVOX_VAD_MIN_SILENCE_MS)
    if hangover_ms != BRISVOX_VAD_MIN_SILENCE_MS:
        notes.append(
            f"interruption.vad_hangover_ms={hangover_ms} only tells the brisvox policy the VAD's "
            f"stop delay; the brisvox VAD itself stops after {BRISVOX_VAD_MIN_SILENCE_MS} ms, "
            "which is what the Pipecat VAD gets"
        )
    if echo_guard_ms:
        notes.append(
            f"interruption.echo_guard_ms={echo_guard_ms}: no Pipecat counterpart (the benchmark "
            "client does not feed bot audio back, so it does not matter there)"
        )
    if backchannels:
        notes.append(
            f"interruption.backchannels={backchannels}: no Pipecat counterpart (backchannels "
            "would interrupt the Pipecat bot)"
        )
    if min_words:
        notes.append(
            f"interruption.min_words={min_words}: mapped to MinWordsUserTurnStartStrategy, "
            "which also gates turn starts when the bot is silent"
        )
    start_ms = max(int(min_speech_ms), BRISVOX_VAD_MIN_SPEECH_MS)
    if vad_start_ms is not None:
        start_ms = vad_start_ms
        notes.append(f"VAD start overridden: {vad_start_ms} ms")
    stop_ms = BRISVOX_VAD_MIN_SILENCE_MS
    if vad_stop_ms is not None:
        stop_ms = vad_stop_ms
        notes.append(f"VAD stop overridden: {vad_stop_ms} ms")
    if vad_min_volume is not None:
        notes.append(f"VAD min_volume overridden: {vad_min_volume}")
    else:
        notes.append("VAD min_volume: Pipecat default (0.6); brisvox has no volume gate")
    vad = VadSettings(
        confidence=BRISVOX_VAD_THRESHOLD,
        start_secs=start_ms / 1000,
        stop_secs=stop_ms / 1000,
        min_volume=vad_min_volume,
    )

    # --- turn ----------------------------------------------------------------------------------
    turn_doc = _table(doc, "turn")
    policy = turn_doc.get("policy", "adaptive")
    if policy == "adaptive":
        fallback = turn_doc.get("fallback_silence_ms", BRISVOX_FALLBACK_SILENCE_MS)
        turn = TurnSettings("smart_turn", fallback / 1000, 0.0, min_words)
    elif policy == "fixed_silence":
        silence = turn_doc.get("silence_ms")
        if not isinstance(silence, int) or silence <= 0:
            raise MappingError("`turn.silence_ms` is required with fixed_silence")
        # brisvox counts `silence_ms` from the VAD stop backdated to the start of the silence
        # (crates/brisvox-agent/src/turn.rs, TurnPolicy::FixedSilence). Pipecat's
        # SpeechTimeoutUserTurnStopStrategy starts its timer when VADUserStoppedSpeakingFrame
        # arrives, which is `stop_secs` into the silence. Subtracting the mapped VAD stop makes
        # both end the turn `silence_ms` after the start of the silence.
        timeout_ms = max(0, silence - stop_ms)
        turn = TurnSettings("speech_timeout", 0.0, timeout_ms / 1000, min_words)
        notes.append(
            f"turn.fixed_silence: silence_ms={silence} counts from the start of the silence in "
            f"brisvox; Pipecat's speech timeout starts at the VAD stop, so it gets "
            f"{timeout_ms} ms (silence_ms minus the {stop_ms} ms VAD stop)"
        )
    else:
        raise MappingError(f"unknown `turn.policy` {policy!r}")
    for key in ("require_final", "final_grace_ms", "final_wait_ms", "response_deadline_ms"):
        if key in turn_doc:
            notes.append(f"turn.{key}: no exact Pipecat counterpart (Pipecat waits for a transcript)")

    speculation = _table(doc, "speculation")
    if speculation.get("enabled"):
        notes.append(
            "speculation.enabled: Pipecat speculates only on an STT's eager end of turn "
            "(EagerUserTurnStrategies, Deepgram Flux); nova-3 + Smart Turn has none, so the "
            "baseline runs without speculation"
        )
    early = _table(doc, "early_analysis")
    if early.get("enabled"):
        notes.append("early_analysis.enabled: no Pipecat counterpart")
    filler = _table(doc, "filler")
    if filler:
        notes.append("[filler]: only used with tools; ignored")

    # --- providers ------------------------------------------------------------------------------
    stt_doc = _table(doc, "stt")
    if stt_doc.get("provider") != "deepgram":
        raise MappingError("the baseline supports `stt.provider = \"deepgram\"` only")
    dg = _table(stt_doc, "deepgram")
    endpointing = dg.get("endpointing_ms", BRISVOX_DEEPGRAM_ENDPOINTING_MS)
    if dg.get("region") not in (None, "global") or dg.get("base_url"):
        raise MappingError("`stt.deepgram.region`/`base_url` are not supported by the baseline")
    stt = SttSettings(
        model=dg.get("model", BRISVOX_DEEPGRAM_MODEL),
        language=stt_doc.get("language", language),
        endpointing_ms=endpointing or None,
        utterance_end_ms=dg.get("utterance_end_ms"),
        smart_format=dg.get("smart_format", BRISVOX_DEEPGRAM_SMART_FORMAT),
        punctuate=BRISVOX_DEEPGRAM_PUNCTUATE,
        interim_results=BRISVOX_DEEPGRAM_INTERIM_RESULTS,
        keyterms=tuple(dg.get("keyterms", ())),
        api_key_env=_key_env(stt_doc, "stt"),
    )

    llm_doc = _table(doc, "llm")
    llm_provider = llm_doc.get("provider")
    if llm_provider not in ("openai", "openai_compatible"):
        raise MappingError(
            "the baseline supports `llm.provider = \"openai\"` or \"openai_compatible\" only"
        )
    if llm_provider == "openai" and (llm_base_url or llm_model):
        raise MappingError(
            "--llm-base-url/--llm-model apply to an `openai_compatible` agent (the `openai` "
            "provider is Pipecat's Responses service on api.openai.com)"
        )
    if "context_window_tokens" in llm_doc or "max_tool_rounds" in llm_doc:
        notes.append("llm.context_window_tokens/max_tool_rounds: not mapped (short benchmark calls)")
    if llm_provider == "openai":
        oa = _table(llm_doc, "openai")
        if oa.get("base_url"):
            raise MappingError("`llm.openai.base_url` is not supported by the baseline")
        effort = oa.get("reasoning_effort", "disabled")
        if effort not in REASONING_EFFORT:
            raise MappingError(f"unknown `llm.openai.reasoning_effort` {effort!r}")
        if "parallel_tool_calls" in oa:
            notes.append("llm.openai.parallel_tool_calls: no tools in the benchmark; ignored")
        llm = LlmSettings(
            model=oa.get("model", BRISVOX_OPENAI_MODEL),
            system_prompt=prompt,
            reasoning_effort=REASONING_EFFORT[effort],
            max_output_tokens=oa.get("max_output_tokens"),
            service_tier=oa.get("service_tier"),
            api_key_env=_key_env(llm_doc, "llm"),
        )
    else:
        oc = _table(llm_doc, "openai_compatible")
        if oc.get("preset"):
            raise MappingError(
                "`llm.openai_compatible.preset` is not supported by the baseline: set `base_url` "
                "and `model`"
            )
        base_url = llm_base_url or oc.get("base_url")
        model = llm_model or oc.get("model")
        if not base_url or not model:
            raise MappingError(
                "`llm.openai_compatible` needs `base_url` and `model` (or --llm-base-url and "
                "--llm-model)"
            )
        key_env = llm_doc.get("api_key_env")
        if key_env is not None and (not isinstance(key_env, str) or not key_env):
            raise MappingError("`llm.api_key_env` must name an environment variable")
        # brisvox sends no service tier, and a reasoning parameter only when the agent file sets
        # `reasoning_effort` (the top-level request field of that name, request.rs / map.rs);
        # Pipecat's chat-completions service gets the same field through `settings.extra`. Its own
        # `stream_options.include_usage` matches the brisvox default quirks.
        effort = oc.get("reasoning_effort")
        routing = None
        order = oc.get("openrouter_order")
        if order is not None:
            if not isinstance(order, list) or not order or not all(isinstance(x, str) and x for x in order):
                raise MappingError("`llm.openai_compatible.openrouter_order` must be a list of slugs")
            routing = {"order": list(order)}
            if "openrouter_allow_fallbacks" in oc:
                routing["allow_fallbacks"] = bool(oc["openrouter_allow_fallbacks"])
        elif "openrouter_allow_fallbacks" in oc:
            raise MappingError("`openrouter_allow_fallbacks` is only sent with `openrouter_order`")
        notes.append(
            "llm.openai_compatible: Pipecat's OpenAILLMService (Chat Completions, streaming) with "
            f"base_url; reasoning_effort={effort!r} sent as the same top-level request field"
        )
        llm = LlmSettings(
            model=model,
            system_prompt=prompt,
            reasoning_effort=effort or "",
            max_output_tokens=oc.get("max_tokens"),
            service_tier=None,
            api_key_env=key_env,
            provider="openai_compatible",
            base_url=base_url,
            temperature=oc.get("temperature"),
            provider_routing=routing,
        )

    tts_doc = _table(doc, "tts")
    if tts_doc.get("provider") == "deepgram_aura":
        da = _table(tts_doc, "deepgram_aura")
        if da.get("url"):
            raise MappingError("`tts.deepgram_aura.url` is not supported by the baseline")
        voice = tts_doc.get("voice")
        if not isinstance(voice, str) or not voice:
            raise MappingError("`tts.voice` is required (the Aura model string, e.g. aura-2-thalia-en)")
        notes.append(
            "tts.deepgram_aura: Pipecat's DeepgramTTSService (websocket /v1/speak, linear16), "
            "Speak per sentence and one Flush at the end of the response, like brisvox's per-turn flush"
        )
        return BaselineSettings(
            name=name,
            language=language,
            vad=vad,
            turn=turn,
            stt=stt,
            llm=llm,
            tts=TtsSettings(
                model=voice,
                voice=voice,
                language=_primary(tts_doc.get("language", language)),
                sample_rate=int(tts_doc.get("sample_rate", BRISVOX_TTS_SAMPLE_RATE)),
                max_buffer_delay_ms=0,
                api_key_env=_key_env(tts_doc, "tts"),
                provider="deepgram_aura",
                speed=da.get("speed"),
            ),
            notes=tuple(notes),
        )
    if tts_doc.get("provider") != "cartesia":
        raise MappingError(
            "the baseline supports `tts.provider = \"cartesia\"` or `\"deepgram_aura\"` only"
        )
    ca = _table(tts_doc, "cartesia")
    if ca.get("url"):
        raise MappingError("`tts.cartesia.url` is not supported by the baseline")
    voice = tts_doc.get("voice")
    if not isinstance(voice, str) or not voice:
        raise MappingError("`tts.voice` is required")
    if voice.startswith("replace-with"):
        notes.append(f"tts.voice is the placeholder {voice!r}: set a real Cartesia voice id")
    tts = TtsSettings(
        model=ca.get("model", BRISVOX_CARTESIA_MODEL),
        voice=voice,
        language=_primary(tts_doc.get("language", language)),
        sample_rate=int(tts_doc.get("sample_rate", BRISVOX_TTS_SAMPLE_RATE)),
        max_buffer_delay_ms=int(ca.get("max_buffer_delay_ms", BRISVOX_CARTESIA_MAX_BUFFER_DELAY_MS)),
        api_key_env=_key_env(tts_doc, "tts"),
    )

    return BaselineSettings(
        name=name,
        language=language,
        vad=vad,
        turn=turn,
        stt=stt,
        llm=llm,
        tts=tts,
        notes=tuple(notes),
    )


def _key_env(section: dict, stage: str) -> str:
    env = section.get("api_key_env")
    if not isinstance(env, str) or not env:
        raise MappingError(f"`{stage}.api_key_env` is required")
    return env
