# Changelog

All notable changes to brisvox are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project follows
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). The Rust crates, the Python package
`brisvox-agent` and the npm packages `@brisvox/agent` and `@brisvox/web`, and the umbrella packages
named plain `brisvox` on crates.io, PyPI and npm, are released together under one version number.

## [Unreleased]

## [0.1.1] - 2026-10-15

### Added

- **Usage and cost per call**: the engine counts the provider billing units of every call (LLM
  tokens including discarded speculative responses, STT audio seconds sent, TTS characters sent)
  and the server delivers one per-call summary (duration, turns, average and worst stage latency,
  units with the configured provider and model) as the agent link event `session.usage` before
  `session.end`, as the `summary` of the transcript's `end` line, as one `call summary` log line
  and as the Prometheus families `brisvox_llm_tokens_total`, `brisvox_stt_audio_seconds_total`,
  `brisvox_tts_characters_total` and `brisvox_call_duration_seconds`; `bot.llm_stopped` carries
  the tokens of each turn, and the Python and TypeScript agent SDKs have a typed `session.usage`
  event. Units, not money (`docs/OPERATIONS.md`, "Usage and cost").
- **Learned admission ceiling** (`BRISVOX_OVERLOAD_CEILING`, on by default). Pool pressure shows
  only when callers speak, so a fast ramp used to be admitted far past capacity (on 4 vCPU, about
  730 calls held for a capacity of about 250, every call at 3.7 s p50). The server now learns how
  many calls it serves well from the pressure signals and refuses new calls (503) above that,
  without touching running calls. In a load test with 900 callers dialing into 4 vCPU: 120 to 400
  calls held at 0.9 to 1.5 s p95, none failed.
- **Cautious start**: a fresh server admits 16 calls and doubles per calm window until the pool is
  first under pressure, so a rush right after a restart cannot overshoot.
- **A second agent per server** (`BRISVOX_WS_AGENT_FILE`): the generic websocket listener runs its
  own agent, sharing the process's pools and admission control.
- `BRISVOX_DEFAULT_AGENT_FILE`: the container image's default agent path, used only when neither
  `BRISVOX_AGENT_FILE` nor `BRISVOX_AGENT_DEFINITION` is set.

### Changed

- Library API (`brisvox-engine`, `brisvox-agent-link`, `brisvox-server`): `EngineEvent::BotLlmStopped`
  has a new `usage` field (patterns need `..`); `CallGuard::new` takes a `summarizer` argument;
  `Metrics::observe_call` and `CallObserver::call_ended` take a `summary` argument;
  `observability::metrics::Histogram::new` is no longer `const` (use `Histogram::with_bounds` for
  other buckets); new `AgentLink::close_with` (`close` is unchanged).
- `BRISVOX_MAX_SESSIONS` defaults to 256 per core (at least 256) instead of 256.
- Pool pressure also watches the Smart Turn executor (its queue waits): a late end-of-turn verdict
  delays the reply as much as a late VAD window. `/readyz` reports `overloaded` at the learned
  ceiling and when the Smart Turn executor is behind, so a load balancer sends calls elsewhere.

### Fixed

- An admitted call could stay silent on a saturated pool: the output resampler is now built off
  the pool when the pool is full or late.
- The container image set `BRISVOX_AGENT_FILE`, so `BRISVOX_AGENT_DEFINITION` alone (the way to
  pass an agent on a platform that cannot mount a file) failed at startup.
- The release workflow's npm publish step (`./pkg/*.tgz`): 0.1.0 reached GitHub, GHCR and PyPI
  only; crates.io and npm start at 0.1.1.

## [0.1.0] - 2026-10-15

First public release.

### Added

- **Engine.** A Rust-native real-time voice pipeline, one isolated session per call: transport
  pump, VAD, STT, turn driver, LLM runner, TTS and transport out. Turn epochs drop stale frames
  everywhere, so an interruption never leaves old audio or text behind. After an interruption the
  LLM context holds only what was actually played (played-text truncation). Every channel is
  bounded, every network await has a timeout, every task is cancellable, and one session's panic or
  provider failure cannot touch another.
- **Server.** `brisvox-server`, a multi-tenant session server: one TOML or YAML file per agent
  (`brisvox-server check <file>` validates it offline with line and column for every problem),
  health and readiness endpoints, Prometheus metrics, OpenTelemetry traces (feature `otlp`),
  admission control, graceful drain, recordings encrypted at rest with age (feature
  `recording-encryption`), a container image and a compose file.
- **Transports.** Generic websocket, Twilio Media Streams, Telnyx media streaming, RTVI (Pipecat's
  client SDKs), WebRTC (feature `webrtc`, WHIP-style signaling, Opus) and a SIP trunk listener
  (feature `sip`). G.711 mu-law and A-law, any call rate with any provider rate.
- **Providers.** STT: Deepgram, AssemblyAI, Gladia, sherpa-onnx (on your machine) and servers that
  speak the OpenAI Realtime transcription protocol. LLM: OpenAI, Anthropic, Gemini and any
  OpenAI-compatible endpoint (Groq, Together, Fireworks, vLLM, Ollama, OpenRouter). TTS: Cartesia,
  ElevenLabs, Deepgram Aura and any server of the OpenAI `/v1/audio/speech` shape. Speech-to-speech:
  OpenAI Realtime and Gemini Live. Per-stage failover, circuit breakers and retry budgets.
- **On-device models.** Silero VAD (compiled into the binary, MIT) and Smart Turn v3.2 end-of-turn
  detection on ONNX Runtime (feature `smart-turn-ort`), on a dedicated inference pool.
- **Conversation.** Adaptive turn taking, interruption policy with backchannel lists, speculative
  LLM start, tool calls with `read_only` and `side_effect` kinds (a side-effect tool never runs
  speculatively and never runs twice), filler phrases for slow tools, DTMF in and out, call
  transfer, answering machine handling. English and Turkish.
- **SDKs.** The Python package `brisvox-agent` and the npm package `@brisvox/agent` for an
  out-of-process agent (your tools and application logic in your own process, the server runs the
  call), and `@brisvox/web`, a dependency-free browser module for a "Talk to us" button: WebRTC
  first, automatic WebSocket fallback, microphone, playback, barge-in and clean hang-up.
- **Rust crates.** `brisvox-core`, `brisvox-audio`, `brisvox-inference`, `brisvox-agent`,
  `brisvox-providers`, `brisvox-sim`, `brisvox-transport`, `brisvox-engine`, `brisvox-agent-link`
  and `brisvox-server` on crates.io; `brisvox_server::app::run_with_tools` is a whole `main` for a
  server with your own Rust tools.
- **Umbrella packages.** The plain name `brisvox` is a real package on all three registries, so
  `cargo add brisvox`, `pip install brisvox` and `npm i brisvox` work next to the specific names.
  The crate re-exports `brisvox-server`, the tools API (`brisvox-agent`), the provider adapters and
  the core types and forwards the server's Cargo features (`brisvox::run_with_tools(...)` is a
  whole `main`). The PyPI package depends on `brisvox-agent` of the same release and re-exports its
  API (`from brisvox import Agent`). The npm package depends on `@brisvox/agent` and `@brisvox/web`
  and re-exports them (`brisvox` is the agent SDK for Node, `brisvox/web` the browser client).
- **Benchmarks.** A harness for voice-to-voice latency and capacity with real and simulated
  providers, with the raw results and the method committed under `bench/`.
- **Documentation.** The book (architecture, quickstart, configuration, telephony, operations,
  benchmarks, protocols and provider notes) and `llms.txt` / `llms-full.txt` for AI coding agents.
- **Release.** Prebuilt `brisvox-server` binaries for Linux (x86_64, aarch64), macOS (aarch64)
  and Windows (x86_64) with checksums, and a multi-architecture image at
  `ghcr.io/bilal07karadeniz/brisvox-server`.

[Unreleased]: https://github.com/bilal07karadeniz/brisvox/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/bilal07karadeniz/brisvox/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bilal07karadeniz/brisvox/releases/tag/v0.1.0
