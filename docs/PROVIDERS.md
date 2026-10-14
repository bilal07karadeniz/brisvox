# Providers

Rule: facts here and in `docs/providers/*.md` come from documentation fetched during
development (URL + date). Never from memory.

## Priority
1. v1: Deepgram (STT), OpenAI (LLM), Cartesia (TTS)
2. Next: Anthropic, Gemini, OpenAI-compatible (Groq/Together/Fireworks/vLLM/Ollama), ElevenLabs
3. Backlog: further STT, TTS and LLM providers (the lists below and `docs/providers/`)

## Per-provider checklist
- streaming protocol and auth · keepalive · finalize/flush semantics · cancel semantics
- word/char timestamps (TTS) · interim results (STT) · tool-call delta format (LLM)
- rate/concurrency limits · regions/endpoints · Turkish support · pricing page link

Template: `docs/providers/_TEMPLATE.md`.

Media formats: G.711 μ-law/A-law (telephony, Twilio/Telnyx) — `docs/providers/g711.md`; codec
in `brisvox_audio::codec`.

## Dialing (happy eyeballs)

`tokio_tungstenite::connect_async*` resolves the host and tries the addresses one after the other,
so an IPv6-first host behind a route that silently drops packets costs the whole handshake
deadline (found by the Cartesia live run: `api.cartesia.ai` has 8 IPv6 + 4 IPv4 addresses).
Every WebSocket connect therefore goes through `brisvox_providers::net::connect_ws` (feature
`ws`; deepgram, deepgram-aura, elevenlabs, cartesia, assemblyai, gladia, gemini-live,
openai-realtime, sherpa-onnx, openai-realtime-stt and the agent link):
- `net::dial(host, port, clock)` resolves and races the connects (RFC 8305): families alternate
  starting with the family of the first answer, a new attempt starts 250 ms after the previous
  one or at once when it failed, at most 8 addresses, the first connected socket wins and the
  rest are dropped; Nagle is off. The delay comes from the injected `Clock`.
- `net::connect_ws(request, ws_config, connector, clock)` dials, then runs TLS and the
  WebSocket handshake (`client_async_tls_with_config`) over that socket and returns the same
  `(WebSocketStream<MaybeTlsStream<TcpStream>>, Response)` / `tungstenite::Error` as
  `connect_async_tls_with_config`, so each adapter's error mapping is unchanged.
- There is no deadline inside: each adapter wraps the call in its own timeout on the injected
  clock (name lookup, dial, TLS and handshake share it). Dropping the future cancels every
  attempt. `localhost` mock servers keep working (`::1` is refused at once, `127.0.0.1` wins).
- HTTP/SSE adapters use reqwest 0.13.5, whose hyper-util 0.1.21 `HttpConnector` races the two
  address families by default (`happy_eyeballs_timeout` 300 ms); the shared client sets no
  `local_address` (that would disable it), so no change was needed.

## Anthropic LLM adapter
Code: `brisvox_providers::anthropic` (feature `provider-anthropic`), facts in
`docs/providers/anthropic.md`. `POST /v1/messages` with `stream: true`, stateless full-history
replay. Defaults for a voice loop: model `claude-haiku-5-5`, `thinking: disabled`, `max_tokens`
1024, `cache_control` on the last tool, the last system block and the last stable history turn
(5-minute TTL). Thinking blocks round-trip through `Message::provider_data`; usage reports
`input = input + cache_creation + cache_read` and `cached = cache_read`. Not retried by the
adapter; `ping` events never count as activity; `warm()` is an unauthenticated `HEAD` (a
`max_tokens: 0` cache warm is unverified with streaming and is not done). A late `System`
message is folded into the next user turn by default; `mid_conversation_system` (opt-in, model
dependent, wire shape unverified) sends it as a `role:"system"` message when the docs allow the
position. An open `thinking` block is allowed `first_output_timeout` of silence (display
`omitted` streams nothing until the signature). `xhigh`/`max` effort with thinking disabled or
`between_tools` is refused at construction. Live check:
`tests/anthropic/live.rs` (`BRISVOX_LIVE_TESTS=1` + `ANTHROPIC_API_KEY`).

## Deepgram Aura TTS adapter
Code: `brisvox_providers::deepgram_aura` (feature `provider-deepgram-aura`), facts in
`docs/providers/deepgram-aura.md` (fetched 2026-10-08). One WebSocket per session to
`wss://api.deepgram.com/v1/speak?model=<TtsConfig.voice>&encoding=linear16&sample_rate=<rate>`,
`Authorization: Token <key>`. Aura has one text buffer and one unlabelled audio stream per
connection, so brisvox contexts are served strictly one at a time (a FIFO; the next context's
text goes out after the previous `Flushed`). Summary of the choices (details in the module docs):
- **No words.** `/v1/speak` has no timestamps: the adapter never emits `TtsEvent::Word`; played-text tracking
  falls back to its char-rate estimate (barge-in truncation is coarse). The contract suite is
  told so by `TtsHarness::emits_words() == false`.
- **No Turkish.** Aura-2 covers en/es/de/fr/nl/it/ja only (2026-10-08). A Turkish voice
  (`aura-2-..-tr`) is refused at `connect`, `DeepgramAuraConfig::language = "tr"` at `new`
  (`Error::Protocol`). Use Cartesia for Turkish.
- **Cancel is local first.** `Clear` is the only cancel and `Cleared` comes after the remaining
  audio: `Cancelled` is emitted at once, audio and `Flushed` are dropped until `Cleared`, and the
  next context is held until then (a lost `Cleared` is given up on after `io_timeout`).
- **Flush once per turn** (`FlushPolicy::PerTurn`, default; Deepgram allows 20 per 60 s and warns
  about quality). `PerText` flushes after every text for servers that do not auto-generate and
  so that a silent server is a stall from the first text; it keeps 4 flushes in reserve. The
  20/60 s flush and 2400 chars/min budgets are tracked client-side (sliding windows on the
  injected clock); a send over budget waits, it is never dropped. When more than 1800
  characters of one context would sit between two flushes, a flush is made before the next
  `Speak` (the 2000-character limit may be per flush; 1009 would end the call). Under `PerTurn`
  the silence of a server that holds unflushed text is bounded by a liveness `Ping` (half an
  `io_timeout` after the last sign of life; a `Pong` restarts the deadline). Whether Aura
  generates without a `Flush` (what `PerTurn` bets on) is open item 3: the live test prints it;
  if it does not, switch the default to `PerText`.
- **Clear only when the server may hold text** (written on this connection, unflushed or with a
  `Flushed` owed): a cancel during a reconnect, or of a fully answered context, sends none.
- Text is sent as `Speak` pieces of at most 1000 characters (sentence, then space boundaries,
  spaces kept). No keepalive message (none documented); idle closes reconnect lazily (also
  with a flushed `PerText` context still open: nothing is owed); connections are replaced after
  55 minutes (server limit 60). Connect, send, close and the shutdown token are served together
  with cancels, so neither a barge-in nor a shutdown waits for a stalled socket.
- `warm()` parks a socket for the latest session's voice and rate (Aura fixes both per socket;
  `DeepgramAuraConfig::warm_target` covers the first call).
Live check: `live_deepgram_aura_session` in `tests/deepgram_aura.rs` (`BRISVOX_LIVE_TESTS=1` +
`DEEPGRAM_API_KEY`, optional `DEEPGRAM_AURA_MODEL`); it prints the measurements for the open
items at the end of the provider doc.

## OpenAI-compatible speech TTS adapter
Code: `brisvox_providers::openai_speech` (feature `provider-openai-speech`, config name
`openai_speech`), facts in `docs/providers/openai-speech.md` (fetched 2026-10-14; wire shapes
read from sources, nothing run yet; a live check is pending). `POST {base_url}/audio/speech` for local servers:
Kokoro-FastAPI, Speaches, Chatterbox-TTS-Server (WAV only, no streaming), vLLM-Omni. One HTTP
request per text piece the TTS stage sends, run in order (pending pieces of a context are joined
while the server is busy), `Audio` events as the chunked body arrives (whole PCM16 samples, 24 kHz;
`wav` header read and rate-checked for Chatterbox). `connect` opens nothing, so refusals and dead
servers are `Error` events after the first text (`TtsHarness::faults_at_connect() == false`, the
contract suite judges them there); `warm()` is a `HEAD` that pools a connection. Cancel drops the
response (Kokoro-FastAPI stops generating on disconnect) and acknowledges at once; 400/422 skip a
piece (two in a row end the session); no `Word` events (estimated downstream); the API key is
optional and plain `http://` is allowed to any host while no key is set. Presets: `generic`,
`kokoro`, `chatterbox`, `speaches`, `vllm_omni`. Live check: `live_local_tts_server` in
`tests/openai_speech.rs` (`BRISVOX_LIVE_TESTS=1` + `LOCAL_TTS_BASE_URL`; prints time to first
audio).

## AssemblyAI STT adapter
Code: `brisvox_providers::assemblyai` (feature `provider-assemblyai`), facts and adapter notes in
`docs/providers/assemblyai.md`. v3 universal streaming (`wss://streaming.assemblyai.com/v3/ws`), model
`universal-3-6-pro` (Turkish), `Authorization: <key>` without `Bearer`. `Turn` partials are interims, the final turn is
one `speech_final` transcript; `Finalize` is `ForceEndpoint` with a bounded wait and a synthetic final. Audio is batched
to >= 50 ms chunks (the server closes on shorter ones), `session_heartbeat` feeds the silence watchdog, reconnect replays
the unfinished turn's audio, hang-up sends `Terminate` and drains until `Termination`. Live check:
`tests/assemblyai.rs` (`BRISVOX_LIVE_TESTS=1` + `ASSEMBLYAI_API_KEY`).

## sherpa-onnx STT adapter
Code: `brisvox_providers::sherpa_onnx` (feature `provider-sherpa-onnx`, `provider = "sherpa_onnx"`), facts and the
Finalize design in `docs/providers/sherpa-onnx.md` (server source read 2026-10-14). Self-hosted, CPU-first streaming
transducers (English zipformers, the community Turkish `seda-v0.1`), no key. Audio is `float32` LE at the model rate
(resampled here), results are the open segment's hypothesis (Python `{text, segment}`, C++ with `is_final`/`is_eof`),
the server's own endpoint becomes a final segment. The protocol has no mid-stream finalize: `Finalize` sends `Done`,
the last result is the answer and the spent connection is replaced when audio follows (audio that arrives meanwhile is
held and replayed in order); an unanswered `Done` ends with the latest hypothesis after `finalize_timeout`. Mock
servers of both flavours + the provider contract suite: `crates/brisvox-providers/tests/sherpa_onnx.rs`. Not
live-tested (needs a running server).

## OpenAI-Realtime-transcription STT adapter
Code: `brisvox_providers::openai_realtime_stt` (feature `provider-openai-realtime-stt`, `provider =
"openai_realtime_stt"`), the dialect table and per-dialect facts in `docs/providers/openai-realtime-stt.md`
(documentation and server source read 2026-10-14). One client for OpenAI (24 kHz, `gpt-live-transcribe`), NVIDIA Speech
NIM (16 kHz, `?intent=transcription`), Speaches (24 kHz in, completed-only, VAD that cannot be disabled) and vLLM
(16 kHz, `transcription.delta/done`, commit = start, `final` commit = end). Our VAD drives turns: `Finalize` is
`input_audio_buffer.commit`, answered by the `completed` of the committed item (empty buffer or too little audio is
answered empty without a round trip). Liveness uses WebSocket pings because these servers do not answer every chunk.
Tests: `crates/brisvox-providers/tests/openai_realtime_stt.rs` (mocks of the four dialects, contract suite for OpenAI,
NIM and vLLM). Not live-tested.

## Shared WebSocket STT session core
`brisvox_providers::wsstt` (private; compiled with either local-speech feature). One task per session owns the
socket, a bounded replay ring (`replay_window`, oldest dropped first, one chunk per engine frame), the finalize queue
and the timers; an adapter implements only `Wire` (setup messages, audio encoding, finalize, server messages, close
handling). The core does: resampling to the wire rate, in-order finalize requests answered exactly once (protocol
answer or timeout answer), liveness (sign of life within `io_timeout` after audio, a ping half-way, ping + suspended
wait after `keepalive_interval` without audio), reconnect with replay and a pending finalize asked again after the
replay, a `rotate` request (sherpa-onnx's spent connection) that waits for the next input before dialing, bounded
event delivery (interims dropped when the channel is full, finals wait `io_timeout`), hang-up. Connects go through
`net::connect_ws` (happy eyeballs).

## Provider interface
Code: `brisvox_providers::{stt, llm, tts}`. Sessions are channel-based so any task can drive
them and dropping a handle cancels the provider work. Every trait has `warm()` (default no-op).
- STT: `connect(SttConfig) -> SttSession { input: Sender<SttInput>, events: Receiver<SttEvent> }`;
  inputs `Audio(Bytes)` / `Finalize`; events `Transcript { text, is_final, speech_final,
  from_finalize }` / `Error`. An utterance can have several final segments; `speech_final` marks
  its end (provider endpoint or the end of a requested finalize).
- LLM: `stream(LlmRequest) -> LlmStream`; events `TextDelta`, `ToolCallDelta { index, id?,
  name?, arguments }` (fragments of different calls may interleave), `Done { finish, usage }`,
  `Error`. A stream that ends without `Done`/`Error` was cut off. Errors before streaming (e.g.
  `Rejected { status }`) come from `stream()` itself.
- TTS: `connect(TtsConfig) -> TtsSession { input, cancel, events }`. Utterances are identified by
  an engine-chosen `ContextId` echoed on every event (`Audio`, `Word { start }` relative to the
  context's first sample, `Flushed`, `Cancelled`), so stale output after a cancel is detectable.
  `cancel` is a separate small channel providers handle before queued text (barge-in never waits
  behind text).
- Channels: input/events 64 (backpressure), TTS cancel 8. Errors: `Rejected`, `Disconnected`,
  `Protocol` (never containing credentials), plus core timeouts/cancellation.

## OpenAI-compatible LLM
`brisvox_providers::openai_compatible::{OpenAiCompatibleLlm, OpenAiCompatibleConfig, Preset,
Quirks}` (feature `provider-openai-compatible`): Chat Completions streaming for Groq, Together,
Fireworks, vLLM, Ollama, OpenRouter and any other server with that API. `OpenAiCompatibleConfig::preset(p)`
fills in the base URL, a model from the vendor's docs and the quirks; base URL, auth header and
scheme, extra headers, extra body fields and quirks are all configurable. Details and the
per-vendor deviations: `docs/providers/openai-compatible.md`. Shared HTTP/SSE helpers live in
`src/http_llm.rs` and `src/sse.rs` (also used by the OpenAI adapter).

### OpenRouter (and any other hosted OpenAI-compatible service)

OpenRouter is the `openai_compatible` provider with `preset = "openrouter"` (it fills in the base
URL `https://openrouter.ai/api/v1` and a default model) and its key in an environment variable of
your choice. Setting `base_url` and `model` without a preset is the same thing spelled out:

```toml
[llm]
provider = "openai_compatible"
api_key_env = "OPENROUTER_API_KEY"          # any variable name; sent as `Authorization: Bearer`

[llm.openai_compatible]
preset = "openrouter"                       # base_url = "https://openrouter.ai/api/v1" without it
model = "openai/gpt-6-luna"                 # OpenRouter's id: <author>/<model>, as on its model page
max_tokens = 120
reasoning_effort = "none"                   # only for a reasoning model that thinks by default
```

- `base_url` is `https://openrouter.ai/api/v1` (the adapter appends `/chat/completions`); plain
  `http://` is accepted only for loopback, a hosted service is always `https://`.
- `model` is passed through verbatim. OpenRouter ids are `<author>/<model>` (`openai/gpt-6-luna`,
  the model the repository's benchmark and live tests use); copy the id from OpenRouter's model
  page. The adapter does not check it: a wrong id is the service's 4xx on the first turn.
- `api_key_env` names the variable that holds the key (`export OPENROUTER_API_KEY=...`); the key is
  never read from the agent file. `brisvox-server check <file> --require-secrets` reports an unset
  one before the first call.
- `reasoning_effort = "none"` is sent as the request field of that name. A reasoning model that
  thinks by default adds about a second to the first token, which a voice turn cannot afford; a
  model that does not know the field may refuse the request, so leave it out for those.
- `openrouter_order = ["openai"]` with `openrouter_allow_fallbacks = false` pins the upstream. These
  two keys are **OpenRouter-only**: without `preset = "openrouter"` or a `base_url` on openrouter.ai the
  agent file is rejected (`brisvox-server check` reports the error with the key's line), so a
  pin can never be silently ignored by another server. The
  request body gets `"provider": {"order": ["openai"], "allow_fallbacks": false}` (OpenRouter's
  provider routing; slugs as on the model's page, a base slug such as `openai` does not select the
  `openai/fast` or `openai/flex` service-tier endpoints). A wrong slug is a 404 from OpenRouter on
  the first turn, never a silent fallback. The adapter logs, once per response, which upstream the
  gateway reports on the first stream chunk (`brisvox::llm`, `llm upstream`, fields
  `upstream_provider` and `upstream_model`).
- What has been run through this path: streaming, parallel and chained tool calls, retried invalid
  arguments, a slow tool with a filler and a barge-in, with `openai/gpt-6-luna` (`docs/TOOLS.md`,
  "Live results"; a full agent file is `bench/pipecat_baseline/agent_live_tr.toml`). Whether a given
  model handles your language and your tools well (Turkish included) is not established by that
  run: choose by a live test with your prompt. First-token time depends on the upstream provider
  OpenRouter routes to and varies between runs (`docs/LATENCY.md`, the live section).
- The same shape fits any other hosted service with the Chat Completions API: change `base_url`,
  `model` and `api_key_env` (the vendors with a preset are listed above).

## Mocks (brisvox-sim)
`brisvox_sim::latency::{LatencyProfile, LatencySampler}` (fixed, normal, log-normal with seed,
scripted per call; SplitMix64 PRNG, so runs are reproducible) and `brisvox_sim::mock_llm`
(`MockLlm` + `LlmScript` with connect / first-token / inter-token latency and faults
`ErrorResponse`, `Disconnect`, `ProtocolError`, `Stall`, `MalformedToolArguments`,
`InterleavedToolDeltas`; usage estimated at 4 chars/token).
`brisvox_sim::mock_stt::MockStt` (`SttUtterance` scripts: interims spaced by the interim
latency after the first audio; `Finalize` drops pending interims and emits the finals after the
final latency, flagged `from_finalize`, last one `speech_final`) and
`brisvox_sim::mock_tts::MockTts` (synthetic audio of `ms_per_char` per character in `chunk`
pieces, `Word` offsets, `Flushed` after a context's audio, priority cancel -> `Cancelled`,
late input for cancelled contexts ignored). Both take `StreamFault`s per session
(`ConnectError`; `Disconnect` right after the N-th event; `Stall`, during which input, cancel
and drop stay live) and connect latency. A `Word` event is not "spoken": it carries the word's
offset into the context audio and is delivered with the text's first audio. Consumers must drop
events of a cancelled context already queued before `Cancelled`, and keep draining events.
MockStt ignores a `Finalize` without new audio and has no provider-side endpointing.

## Resilience layer
Code: `brisvox_providers::resilience` (no feature: it wraps any adapter). `ResilientStt`,
`ResilientLlm` and `ResilientTts` implement the provider traits over a primary and an optional
secondary provider of the same stage, with a process-wide circuit breaker per provider, retry
budgets (per call and per process), mid-call recovery and the idle refresh of TTS sessions
(`TtsProvider::idle_limit`, which Cartesia sets to its documented 5 minutes). The full contract
is in the module docs and `docs/PIPELINE.md` ("Resilience layer"). Simulator outages:
`brisvox_sim::outage::{Outage, OutageSwitch}` (refused with 503 while down, or blackholed: pending
until the caller's deadline while hanging). The server wires it for every call (
`brisvox_server::resilient::ResilientProviders`, `docs/CONFIG.md` "Secondary providers").

## OpenAI Realtime speech-to-speech adapter
Code: `brisvox_providers::openai_realtime` (feature `provider-openai-realtime`) behind the
provider-neutral `brisvox_providers::s2s` interface; facts in `docs/providers/openai-realtime.md`
(fetched 2026-10-13), design and the engine stage in `docs/S2S.md`. One WebSocket per session
(`wss://api.openai.com/v1/realtime?model=gpt-realtime-2.1`, `Authorization: Bearer`, no beta header),
24 kHz PCM16 both ways, one `session.update` at connect. The adapter maps events, delivers a tool call
once per `call_id` and swallows the expected races of `response.cancel` and
`conversation.item.truncate`; it does not decide turns, truncate by itself or reconnect (the engine
does; after a reconnect or rotation the engine seeds the new session with `S2sConfig::history`, which
the adapter sends as `conversation.item.create` items, and `session_limit` is 60 minutes). Not
live-tested (needs `OPENAI_API_KEY`).

## Gemini Live speech-to-speech adapter
Code: `brisvox_providers::gemini_live` (feature `provider-gemini-live`) on the same
`brisvox_providers::s2s` interface; facts in `docs/providers/gemini-live.md` (fetched 2026-10-13,
UNVERIFIED items marked), mapping and limits in `docs/S2S.md`. One `BidiGenerateContent` WebSocket per
session (`?key=` query by default, `x-goog-api-key` header opt-in), model `gemini-3.8-live`, caller audio
`audio/pcm;rate=<hz>`, model audio 24 kHz, JSON in text or binary frames. Gemini has no response ids, no
cancel and no history truncation: the adapter mints ids, ends a cancelled response locally and ignores
`Truncate` (the engine's mirror is the truth). The connection lives about 10 minutes (`goAway` becomes
`S2sEvent::SessionEnding`); the engine rotates to a re-seeded session. Contract tests against a mock
server: `crates/brisvox-providers/tests/gemini_live.rs`. Not live-tested (needs a
`GEMINI_API_KEY`).
