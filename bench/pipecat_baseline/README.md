# Pipecat baseline bot

A Pipecat bot that speaks the brisvox websocket protocol (`docs/WEBSOCKET_PROTOCOL.md`), so
`brisvox-bench v2v --url ws://...` measures Pipecat and brisvox-server with the same client, the
same fixtures and the same definition of V2V (`docs/LATENCY.md`). It runs the
providers, models, voice, language, prompt and turn settings of a brisvox agent file: it reads
the very TOML file that brisvox-server runs (`agent.toml` here by default) and maps it onto
Pipecat (`agent_mapping.py`, table below).

| file | what |
|---|---|
| `bot.py` | the server: FastAPI + uvicorn, one Pipecat pipeline per websocket call |
| `brisvox_protocol.py` | the protocol adapter: `start`/`started` handshake, a Pipecat `FrameSerializer` for audio, `clear`, `stop`, `dtmf` |
| `agent_mapping.py` | brisvox agent file -> Pipecat settings (pure Python, no Pipecat import) |
| `mock_providers.py` | key-free mock STT, LLM and TTS with the `brisvox_sim` latencies and shapes, for `bot.py --mock-providers` and the capacity benchmark (below) |
| `stage_probe.py` | a diagnostic wrapper around `bot.py`: logs the Pipecat stage timestamps (VAD stop, Smart Turn, STT final, end of turn, LLM first text, TTS first audio, first audio frame at the websocket) to `$PCTRACE_FILE`; patches Pipecat 1.12 internals, never used by the capacity runs |
| `agent_live_en.toml`, `agent_live_tr.toml` | the agents of the live comparison on real providers: OpenRouter as the OpenAI-compatible LLM, a one-sentence prompt (the English one also pins the OpenRouter upstream with `openrouter_order`/`openrouter_allow_fallbacks`: the bot sends `extra_body={"provider": {...}}`, brisvox the same object in the request body); `tts.voice` is a placeholder, the run copy carries the id of `CARTESIA_VOICE_ID` |
| `agent.toml` | the benchmark agent both systems run (`examples/agents/support-en.toml` without tools, speculation off) |
| `requirements.txt` / `requirements.lock.txt` | direct pins / the whole dependency tree, universal (`uv pip compile --universal`) |
| `tests/` | offline tests (no keys): protocol, mapping (checked against the Rust defaults), whole calls over a websocket |

## Pinned versions

| what | version | notes |
|---|---|---|
| Pipecat | `pipecat-ai[deepgram,websocket]==1.12.0` | newest release on PyPI on 2026-10-13 (uploaded 2026-09-26); requires Python >= 3.11 |
| Python | 3.13 (tested with 3.13.14 on Windows) | any 3.11..3.13 should work; the lock was compiled for 3.13 |
| uvicorn | 0.54.0 | ASGI server (`websockets` 17.2 underneath, from the lock) |
| Smart Turn | `smart-turn-v3.2-cpu.onnx`, bundled with Pipecat 1.12.0 | **the same file brisvox pins** (8 679 182 bytes, SHA-256 `2bb02631...e4f`; `tests/test_mapping.py` checks it), threshold 0.5 on both sides |
| Silero VAD | v6.2.3 `silero_vad.onnx` (SHA-256 `1a153a22...88e3`) when fetched, else Pipecat's bundled v6.0 | brisvox runs v6.2.3. `scripts/models/fetch_models.py` puts the file in `models/`; `bot.py --silero-model auto` (default) uses it and logs a warning when it falls back to v6.0 |

## Setup

Keep the venv outside the repository (it is large: onnxruntime, numba, the Deepgram SDK).

```sh
# from the repository root; uv (https://docs.astral.sh/uv/) or plain venv + pip
uv venv --python 3.13 ../pcvenv
uv pip sync --python ../pcvenv/bin/python bench/pipecat_baseline/requirements.lock.txt
# or: python3.13 -m venv ../pcvenv && ../pcvenv/bin/pip install -r bench/pipecat_baseline/requirements.lock.txt
python3 scripts/models/fetch_models.py        # Silero v6.2.3 (and Smart Turn) into models/
```

(On Windows the interpreter is `..\pcvenv\Scripts\python.exe`.)

### Keys

Only from the environment: the variables the agent file names in `api_key_env`
(`DEEPGRAM_API_KEY`, `OPENAI_API_KEY`, `CARTESIA_API_KEY` for `agent.toml`). `bot.py` loads
`.env` at the repository root when it exists (git-ignored; template `.env.example`), or the file
given with `--env-file`; variables already set win. A missing key stops the bot at startup with
the variable's name. Keys are never written to files or logs.

Before a live run, set `tts.voice` in `agent.toml` to a real Cartesia voice id (both systems read
it from there, so they speak with the same voice).

## Running

```sh
cd bench/pipecat_baseline
../../../pcvenv/bin/python bot.py --port 8765            # real providers (keys needed)
../../../pcvenv/bin/python bot.py --check                # build one call's pipeline and exit
../../../pcvenv/bin/python bot.py --fake --port 8765     # provider-free self-check, no keys
```

Options: `--host` (127.0.0.1), `--port` (8765), `--agent PATH` (`agent.toml`), `--env-file`,
`--llm-transport http|websocket` (below), `--silero-model auto|pipecat|PATH`,
`--smart-turn-threads N` (1, Pipecat's default), `--llm-base-url` / `--llm-model` (override the
`openai_compatible` agent's server), `--provider-metrics` (logs each service's TTFB as `PCMETRIC`
lines with the call id, the provider-latency columns of the live comparison; independent of the flag the
bot logs the upstream provider OpenRouter names on the first chunk of every LLM response as a
`PCUPSTREAM` line, the routing check), `--no-happy-eyeballs` (below),
`--vad-start-ms`, `--vad-stop-ms`,
`--vad-min-volume` (ablations, below), `--fake`, `--fake-delay-ms` (300), `--fake-reply-ms`
(600), `--log-level`. At startup the bot logs the mapped settings and one `mapping:` warning per
setting that has no exact Pipecat counterpart.

Like brisvox-server, the bot is unauthenticated and ignores the request path: keep it on
loopback or behind a proxy.

### Mock providers: `--mock-providers`

`bot.py --mock-providers` runs the same Pipecat pipeline with no keys and no network: STT, LLM
and TTS are mocks with the **same fixed latencies and token/audio shapes** as the `brisvox_sim`
mocks that the brisvox side of the capacity benchmark runs (`brisvox-bench capacity --sim`).
Everything above the network boundary of each service is Pipecat's own code: the
`MockResponsesLLMService` is the real `OpenAIResponsesHttpLLMService` with only its HTTP stream
replaced, the TTS is a real `TTSService` (sentence aggregation included), the STT a real
`STTService`, the user aggregator, turn strategies, transport and serializer are the live ones.
`mock_providers.py` documents each mock; `tests/test_mock_providers.py` reads every shared number
from the Rust source and fails when one drifts.

| what | mock | latency / shape (defaults = `EngineTargetConfig::default()`) |
|---|---|---|
| STT | `MockSTTService` | interims every 100 ms from the first loud frame (growing word prefixes of the fixture's next utterance), the final 150 ms after the VAD stop, flagged `finalized` |
| LLM | `MockResponsesLLMService` | one text delta (`Okay, got it.` / `Peki, anladım.`, chosen by the user's text like `CallScript::responder`) 350 ms after the request |
| TTS | `MockTTSService` | first audio 150 ms after the text, then 40 ms chunks every 20 ms, 60 ms of audio per character, a +-1000 square wave |
| VAD | energy VAD on 32 ms windows | RMS >= 500; start 40 ms (2 windows), stop 300 ms (10 windows, 320 ms: the first window boundary at or after 300 ms, as the brisvox VAD worker) |
| Smart Turn | `MockSmartTurnAnalyzer` | the real feature extraction (Whisper log-mel of the last 8 s), the ONNX model replaced by "complete" (brisvox: `FixedTurnModel`) |

The floor of a turn is therefore 300 + 150 + 350 + 150 = **950 ms** on both sides
(`MockConfig.floor_ms`), and what a measurement shows above it is the framework's own time.
`--mock-models real` keeps Pipecat's Silero VAD and Smart Turn v3 (a heavier Pipecat, not like
for like with the brisvox simulator). The script (which utterance is next) is the fixture's JSON
sidecar: `--mock-fixture` (default `en_two_utterances`, the capacity benchmark's) and
`--mock-fixtures-dir`; every latency has a `--mock-*-ms` flag.

```sh
cd bench/pipecat_baseline
../../../pcvenv/bin/python bot.py --mock-providers --port 8765 --pid-file /tmp/bot.pid
../../../pcvenv/bin/python bot.py --mock-providers --port 8765 --workers 4   # 4 processes
# from the repository root, against either system (docs/LATENCY.md, "Capacity benchmark"):
./target/release/brisvox-bench capacity --url ws://127.0.0.1:8765/ --pid $(cat /tmp/bot.pid) --floor-ms 950
```

`--workers N` starts N uvicorn worker processes (the command line is passed through the
environment, `BRISVOX_BOT_ARGV`); their CPU and memory add up under the main process, which is
the pid in `--pid-file` and the one `brisvox-bench capacity --pid` reads (it sums the whole
process tree). The bot builds one pipeline at startup in every process (`--no-prewarm` skips it):
the first pipeline of a Python process imports and compiles for seconds, which would fail the
first calls of a benchmark for nothing.

### How a call runs

1. The client connects (any path) and sends `start`. The bot validates it with the brisvox
   rules (version 1, the six sample rates, `pcm16`/`mulaw`, call id and metadata limits, a 5 s
   start timeout) and refuses a bad one with the brisvox `error` code and close code.
2. The bot builds the call's pipeline (Silero and Smart Turn models are loaded here), then
   answers `started` and runs it:
   `transport.input -> Deepgram STT -> user aggregator (Silero VAD, Smart Turn v3) -> OpenAI
   Responses LLM -> Cartesia TTS -> transport.output -> assistant aggregator`.
   Provider connections open when the pipeline starts, during the fixture's leading silence.
3. Client audio (call rate, pcm16 or mu-law) enters as 16 kHz PCM (the rate brisvox feeds its
   VAD, Smart Turn and STT). Bot audio leaves at the call's rate and encoding in 20 ms frames
   (`audio_out_10ms_chunks=2`); Pipecat resamples Cartesia's 24 kHz itself.
4. A Pipecat interruption is sent as `clear{epoch}` (epochs count from 1) only when it cuts bot
   audio the client may still be playing (modelled from the audio sent: real-time playout plus
   0.2 s), which is when brisvox-server clears. Pipecat's user aggregator interrupts on every
   user turn start, also when the bot is silent; those are not barge-ins and send nothing, so
   the bench's `clears` count compares like for like. No `mark` is sent:
   Pipecat trims its context from its own playout clock; `mark` echoes are ignored. `dtmf`
   becomes an `InputDTMFFrame` (A-D have no Pipecat keypad entry and are dropped).
5. The client's `stop` cancels the pipeline; the bot answers `stop{ended}` and closes. A client
   that disconnects cancels the pipeline.

## Mapping: brisvox setting -> Pipecat setting

Source of the brisvox values: `agent.toml`, else the brisvox default (the Rust constant named in
`agent_mapping.py`; `tests/test_mapping.py` fails when one of them changes in Rust).

| brisvox (agent file / engine) | value in `agent.toml` | Pipecat 1.12.0 | notes |
|---|---|---|---|
| `agent.language` | `en` | Deepgram `language`, Cartesia `language` | brisvox passes the tag as written to Deepgram and the primary subtag to Cartesia; so does the bot |
| `agent.system_prompt` | (text) | `OpenAIResponses*LLMService.Settings(system_instruction=...)` | identical text |
| `agent.tools` | none | none | brisvox-server refuses tools today; the bot refuses an agent with tools |
| `stt.provider` / `[stt.deepgram].model` | deepgram / `nova-3` | `DeepgramSTTService`, `Settings(model="nova-3")` | Pipecat's default would be `nova-3-general` |
| `[stt.deepgram].endpointing_ms` | 300 | `endpointing=300` | `0` (off) maps to `endpointing=False` |
| `[stt.deepgram].keyterms` | `["Example Shop"]` | `keyterm=["Example Shop"]` | |
| `[stt.deepgram].smart_format` | true (default) | `smart_format=True` | Pipecat default False |
| Deepgram `punctuate` (not configurable) | false | `punctuate=False` | Pipecat default True |
| Deepgram `interim_results` (not configurable) | true | `interim_results=True` | |
| `[stt.deepgram].utterance_end_ms` | unset | unset | |
| STT input | 16 kHz linear16 | 16 kHz linear16 (`audio_in_sample_rate=16000`) | |
| `llm.provider = "openai_compatible"`, `[llm.openai_compatible]` `base_url`, `model`, `max_tokens`, `temperature`, `reasoning_effort` | OpenRouter, `openai/gpt-6-luna`, 120, unset, `none` (`agent_live_en.toml`) | `OpenAILLMService` (Chat Completions, streaming, `stream_options.include_usage`) with `base_url`; `Settings(max_tokens=..., extra={"reasoning_effort": ...})` | brisvox's compatible adapter speaks the same wire (`/chat/completions`, `max_tokens`, `reasoning_effort` as a top-level field). `preset` is refused (give `base_url` and `model`); the key is optional like in brisvox. `--mock-providers` needs an `openai` agent |
| `llm.provider` / `[llm.openai].model` | openai / `gpt-6-luna` (brisvox default) | `OpenAIResponsesHttpLLMService`, `Settings(model="gpt-6-luna")` | Pipecat's default would be `gpt-4.1` |
| OpenAI API surface | Responses API over HTTP SSE, `store: false`, full history each turn | `OpenAIResponsesHttpLLMService` (same endpoint, `store` false, full context) | `--llm-transport websocket` runs Pipecat's recommended `OpenAIResponsesLLMService` (persistent `wss://.../v1/responses`, `previous_response_id`): a second Pipecat row, not the apples-to-apples one |
| `[llm.openai].reasoning_effort` | `low` | `reasoning=OpenAIResponsesReasoningConfig(effort="low")` | unset/`disabled` -> `"none"` (brisvox's adapter default) |
| `[llm.openai].max_output_tokens` | 400 | `max_completion_tokens=400` (sent as `max_output_tokens`) | |
| `[llm.openai].service_tier` | unset | `service_tier=None` | |
| `llm.context_window_tokens`, `max_tool_rounds` | defaults | not mapped | irrelevant for the short benchmark calls |
| `tts.provider` / `[tts.cartesia].model` | cartesia / `sonic-3.6` (brisvox default) | `CartesiaTTSService`, `Settings(model="sonic-3.6")` | websocket API on both sides |
| `tts.voice` | the voice id | `Settings(voice=...)` | identical id |
| `tts.sample_rate` | 24000 | `sample_rate=24000` | both resample to the call's rate |
| `[tts.cartesia].max_buffer_delay_ms` | 0 (brisvox default) | `max_buffer_delay_ms=0` | Pipecat's default in sentence mode is 0 too |
| text to TTS | brisvox segmenter (sentence/clause chunks) | Pipecat sentence aggregation (default `TextAggregationMode.SENTENCE`) | equivalent, not identical: brisvox may release a first clause earlier (`first_min_words`) |
| VAD model | Silero v6.2.3 (tract) | `SileroVADAnalyzer` on the same v6.2.3 file (`--silero-model auto`) | 512-sample (32 ms) windows on both sides |
| VAD `threshold` (engine, fixed) | 0.5 | `VADParams(confidence=0.5)` | Pipecat default 0.7 |
| VAD `min_silence` (engine, fixed; `interruption.vad_hangover_ms` mirrors it) | 100 ms | `VADParams(stop_secs=0.1)` | Pipecat default 0.2 s; `--vad-stop-ms` to change |
| VAD `min_speech` 64 ms + `interruption.min_speech_ms` | 64 ms + 300 ms (`recommended`) | `VADParams(start_secs=0.3)` | Pipecat interrupts when the VAD reports a start, so its start delay is the barge-in guard: `max(min_speech_ms, 64 ms)`; preset `off` gives 0.064 s. A start delay does not move V2V (measured from the end of speech) |
| (no volume gate) | | `VADParams(min_volume=0.6)` (Pipecat default kept) | `--vad-min-volume 0` removes Pipecat's gate |
| `turn.policy = "adaptive"` | adaptive | `TurnAnalyzerUserTurnStopStrategy(LocalSmartTurnAnalyzerV3)` (`wait_for_transcript=True`) | the same model and threshold; brisvox also holds the end for the STT final, Pipecat waits for a transcript |
| `turn.fallback_silence_ms` | 3000 | `SmartTurnParams(stop_secs=3.0)` | the end of an undecided pause |
| `turn.policy = "fixed_silence"`, `silence_ms` | | `SpeechTimeoutUserTurnStopStrategy(user_speech_timeout=max(0, silence_ms - vad_stop_ms)/1000)` | brisvox counts `silence_ms` from the start of the silence (backdated VAD stop); Pipecat starts its timer at the VAD stop, `stop_secs` later, so the mapped VAD stop (100 ms, or `--vad-stop-ms`) is subtracted: `silence_ms = 500` ends both turns 500 ms after the end of speech |
| `turn.final_grace_ms`, `final_wait_ms`, `require_final`, `response_deadline_ms` | defaults | none (Pipecat's `user_turn_stop_timeout` 5 s kept) | noted at startup when set |
| `interruption.min_words` | 0 | default start strategies (VAD + transcription) | `> 0` maps to `MinWordsUserTurnStartStrategy` |
| `interruption.echo_guard_ms` / `echo_min_words` | 400 / 1 | none | the benchmark client never feeds bot audio back |
| `interruption.backchannels` | `["en"]` | none | the fixtures have no backchannels; Pipecat would be interrupted by them |
| `speculation.enabled` | false | none | Pipecat speculates only on an STT's eager end of turn (`EagerUserTurnStrategies`, e.g. Deepgram Flux), not with nova-3 + Smart Turn |
| `early_analysis.enabled` | false (default) | none | |
| output frames | 20 ms, real-time pacer | 20 ms (`audio_out_10ms_chunks=2`), Pipecat's pacer (sends up to 2x real time ahead) | V2V takes the first audible frame, so the pacing difference after it does not count |
| websocket protocol | brisvox-server | `brisvox_protocol.py` | see "How a call runs" |

### Known differences that remain

- **Address-family racing.** `websockets` (under Pipecat's Cartesia and other
  websocket services) tries a host's addresses one at a time. On a host whose IPv6 route drops
  packets (the Windows machine of the live run: `api.cartesia.ai` has 8 AAAA and 4 A records)
  the first Cartesia handshake timed out after 10 s and the call's first turn got no audio.
  brisvox races families. The bot enables asyncio's `happy_eyeballs_delay=0.25` for the
  services' websocket connects (`enable_happy_eyeballs`, tested), so connection setup is not part
  of the comparison; `--no-happy-eyeballs` runs Pipecat as shipped.

- Pipecat releases resampled audio in bursts: its soxr stream resampler emits about 100 ms at a
  time (`tests/test_protocol.py` shows it). Cartesia's 24 kHz output resampled to a 16 kHz call
  goes through it, so the first audio of a reply can wait for the first burst. That is Pipecat
  as shipped; the ablation `tts.sample_rate = 16000` in `agent.toml` (both systems) removes the
  resampling on both sides.
- Pipecat runs Silero and Smart Turn on one-thread executors per call (one process, one asyncio
  event loop for all calls); brisvox runs them on its shared inference pool. That is the design
  under comparison (it matters most for the capacity run: `docs/LATENCY.md`, "Capacity benchmark",
  where Pipecat is also measured with `--workers N`, N processes).
- No backchannel or echo filtering in Pipecat (see the table); irrelevant to the V2V fixtures.

## Offline verification (done 2026-10-13, Windows, no keys)

- `python -m unittest discover -s tests` (from this directory; set `BRISVOX_MODELS_DIR` when
  `models/` is elsewhere): the tests: handshake validation and every serializer mapping; the
  mapping of `agent.toml`; every brisvox default the mapping assumes, read from the Rust source;
  the identical Smart Turn file; the services built with dummy keys carrying the mapped
  settings; and whole calls over a real websocket against `--fake` (started, an answer, a
  barge-in that arrives as `clear{epoch:1}` while the turn start before it, with the bot silent,
  sends none, `stop{ended}` and the close, refusals with the
  brisvox codes, an 8 kHz mu-law call). Mutation-checked: dropping the `clear` or the client
  `stop` mapping fails them.
- `bot.py --check` with dummy keys builds the full live pipeline (Deepgram, OpenAI Responses
  HTTP, Cartesia, Silero, Smart Turn v3) without connecting.
- The real benchmark client against the fake bot:
  `brisvox-bench v2v --url ws://127.0.0.1:8799 --fixtures en_two_utterances --runs 3` ->
  6/6 turns answered, V2V 423-457 ms over a floor of about 400 ms (VAD stop 100 ms + fake delay
  300 ms; debug build, Windows timer). This is the Pipecat-side analogue of
  `brisvox-bench v2v --sim --target mock-bot`.

- `tests/test_mock_providers.py`: every number the mocks share with the `brisvox_sim`
  mocks (answers, latencies, VAD thresholds, TTS shape, the 950 ms floor) read from the Rust
  source; the pure helpers; the pipeline built over the mocks; the energy VAD's window counts; a
  whole call in real time (the first utterance answered about 1.0-1.1 s after its end).

What could not be verified offline: the provider connections and their wire behaviour, which
need keys (live phase).

## Live comparison (live phase): exact commands

(The capacity comparison needs no keys: it runs `--mock-providers`, above.)

Run on Linux (Windows timers add up to ~25 ms of send lateness), both systems on the same
machine, one at a time, same network. Fixtures: the English ones (the agent is English; a Turkish
utterance would be transcribed poorly by both and may end as `no_response`).

**Smart Turn (brisvox side).** Build brisvox-server with the `smart-turn-ort` feature (
`docs/OPERATIONS.md`) and have `models/smart-turn-v3.2-cpu.onnx` in place; without the feature its
adaptive turns end at the 3 s fallback. **Gap between utterances:** pass `--min-gap-ms 9000` to
`brisvox-bench v2v` with real providers: the default 4 s gap is shorter than a real LLM's answer
plus its speech, so the first reply would be cut by the second utterance (a barge-in, not the
latency being measured). The measured Windows runs are in `bench/results/`
(`v2v-live-*-windows-*.md`) and `docs/LATENCY.md`: a 30-turn run (one call at a time,
Cartesia) and a second run (`...-c5-windows-2026-10-14.*`: 100 turns, 5 concurrent calls, Deepgram Aura via
`agent_live_en_aura.toml`, OpenRouter upstream pinned; per block
`brisvox-bench v2v --url ... --fixtures en_two_utterances --runs 5 --concurrency 5 --min-gap-ms 9000
--keep-invalid`, blocks of the two systems alternating, then
`scripts/bench_live_report.py --tts deepgram_aura --concurrency 5 --pin openai`).

```sh
# 0. once: keys in .env, a real voice id in bench/pipecat_baseline/agent.toml, models fetched
python3 scripts/models/fetch_models.py
cargo build --release -p brisvox-bench
cargo build --release -p brisvox-server --features all-providers

# 1. brisvox-server
set -a; . ./.env; set +a
BRISVOX_AGENT_FILE=bench/pipecat_baseline/agent.toml BRISVOX_WS_ADDR=127.0.0.1:8080 \
  ./target/release/brisvox-server &
./target/release/brisvox-bench v2v --url ws://127.0.0.1:8080 --fixtures en_two_utterances \
  --runs 30 --min-gap-ms 9000 --out-md bench/results/v2v-live-brisvox.md --out-json bench/results/v2v-live-brisvox.json
kill %1

# 2. Pipecat baseline (same agent file, same keys)
(cd bench/pipecat_baseline && ../../../pcvenv/bin/python bot.py --port 8765) &
./target/release/brisvox-bench v2v --url ws://127.0.0.1:8765 --fixtures en_two_utterances \
  --runs 30 --min-gap-ms 9000 --out-md bench/results/v2v-live-pipecat.md --out-json bench/results/v2v-live-pipecat.json
kill %1
```

Rows worth adding to the comparison:

| row | change |
|---|---|
| brisvox + speculation | `[speculation] enabled = true` in a copy of `agent.toml` for brisvox-server only (Pipecat has no counterpart; the bot just notes it) |
| Pipecat, websocket LLM | `bot.py --llm-transport websocket` (Pipecat's recommended OpenAI mode) |
| Pipecat defaults | `bot.py --vad-stop-ms 200 --silero-model pipecat` (Pipecat's shipped VAD stop and Silero) |
| no TTS resampling | `tts.sample_rate = 16000` in `agent.toml` (both systems) |

Interleave the systems over several rounds if the network drifts, and record the commands, the
machine and the Pipecat/brisvox versions in the result files (the bench writes its own header).

## Updating Pipecat

```sh
pip index versions pipecat-ai           # or https://pypi.org/pypi/pipecat-ai/json
# edit requirements.txt, then:
uv pip compile requirements.txt --universal --python-version 3.13 -o requirements.lock.txt
uv pip sync --python ../../../pcvenv/bin/python requirements.lock.txt
python -m unittest discover -s tests    # Pipecat internals the bot relies on are tested here
```

The bot depends on Pipecat 1.x names (`PipelineWorker`, `CancelWorkerFrame`,
`UserTurnStrategies`, `OpenAIResponsesHttpLLMService`, the Silero analyzer's `_model`); a
release that changes them fails the tests above.
