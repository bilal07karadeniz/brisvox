# OpenAI-compatible speech (local TTS servers)

Facts fetched 2026-10-14 (URLs under "Sources"). The pages were read through a summarising
fetch tool: field lists and quoted strings are its output, not verbatim wire captures.
**Status.** The sections up to "UNVERIFIED / open items" were written from documentation before
any server ran, and their UNVERIFIED marks stand unless a later section says otherwise. The
sections "Live" and "Cold first request and the model warm-up" report what was run on
2026-10-14: Kokoro-FastAPI (the `kokoro` preset; first audio 50-164 ms on an RTX 5070) and
Chatterbox-TTS-Server (Turkish, about 3 s to the first byte, not streamed). Speaches and vLLM-Omni
have not been run, and the Docker images named in "Try it" were not used for those runs (a source
checkout was). Code: `brisvox_providers::openai_speech` (feature `provider-openai-speech`);
config name `provider = "openai_speech"`.

- Docs: see "Sources".
- Auth: none for the local servers read (Kokoro-FastAPI: "The API key can be any placeholder
  string, since the local server doesn't check it"); an optional `Authorization: Bearer <key>`
  is supported by the adapter for gateways and Speaches (which has optional auth).
- Endpoint: `POST {base_url}/audio/speech`, `base_url` carries the `/v1` prefix.
- Protocol: HTTP request/response; the response body is raw audio, chunked.
- Keepalive / timeouts: none documented; the adapter applies its own (below).
- Finalize / flush / cancel: one request per text piece; cancel = abandon the HTTP response.
- Timestamps / interim results: none requested (see "Words").
- Concurrency / rate limits: not documented by any server read; Kokoro-FastAPI handles requests
  concurrently on one model (README), throughput unmeasured.
- Turkish: Kokoro has none; Chatterbox Multilingual has (see `local-speech.md` section 4).
- Gotchas: see "Per-server facts" (Chatterbox has no `pcm`, no streaming; Speaches PCM carries no
  rate).

## The protocol

OpenAI text-to-speech guide (`developers.openai.com/api/docs/guides/text-to-speech`, via
`local-speech.md` section 1): body `{model, input, voice, response_format, speed, ...}`; `pcm` is
"raw 24kHz, 16-bit signed samples, little-endian ... without the header"; the response streams
chunked. The whole text is one request, so incremental text-in is done the only way the protocol
allows: one request per segment.

Request the adapter sends (`Content-Type: application/json`, key order not significant):

```json
{"model":"kokoro","input":"Hello there.","voice":"af_heart","response_format":"pcm","stream":true}
```

`speed` only when configured; `stream: true` only for the Kokoro preset; `sample_rate` only for
the Speaches preset (the session's rate); `extra_body` fields on top (the adapter's own keys
cannot be overridden).

## Per-server facts

| Server | Route and fields | `pcm`? | Streams? | Sample rate | Errors | Cancel |
|---|---|---|---|---|---|---|
| **Kokoro-FastAPI** | `POST /v1/audio/speech` (`/v1` from the mount; router `openai_compatible.py`). `OpenAISpeechRequest`: `model` (default `"kokoro"`; accepted: `tts-1`, `tts-1-hd`, `kokoro`, `gpt-4o-mini-tts`, anything else is 400 `invalid_model`), `input`, `voice` (default from settings; `+`/weight combinations), `response_format` mp3 (default) / opus / aac / flac / wav / pcm, `speed` 0.25-4.0, `stream` (default **True**), `lang_code`, `volume_multiplier`, `normalization_options` | yes, media type `audio/pcm` | yes: `StreamingResponse`, headers `Transfer-Encoding: chunked`, `X-Accel-Buffering: no`, `Cache-Control: no-cache`; one body chunk per generated audio chunk, empty ones skipped | 24 kHz (`StreamingAudioWriter(.., sample_rate=24000)`); README's PCM example: 16-bit mono 24000 Hz | before streaming: 400 `invalid_model` / `validation_error` (voice, tags, pause budget, "speakable" check), 403 voice tags disabled, 500 `processing_error`; after the first byte the status cannot change | `stream_audio_chunks` checks `client_request.is_disconnected` before each chunk and stops; generators are closed |
| **Speaches** | `POST /v1/audio/speech`: `model` (required), `input`, `voice` (required), `response_format` (pcm, mp3, wav, flac, opus, aac), `speed` (model dependent; 422 when unsupported), `stream_format` `"audio"` (default) or `"sse"`, `sample_rate` (int, "If omitted, the model's default sample rate is used") | yes, `audio/pcm` | always a `StreamingResponse` | model default unless `sample_rate` is sent (the adapter sends the session rate) | 422 for unsupported speed; others not shown | not documented |
| **Chatterbox-TTS-Server** (`devnen`, port 8004 in the README examples) | `POST /v1/audio/speech`, handler `openai_speech_endpoint`: `model` (required, unused), `input`, `voice` (required), `response_format` **only `wav` (default), `opus`, `mp3`**, `speed`, `seed`, `language`. `stream` does not exist on this model (it belongs to `/tts`) | **no** (validation rejects it, status not shown; FastAPI usually 422) | **no**: the audio is synthesized, stitched and encoded first, then sent as a `StreamingResponse` over an in-memory buffer | from config (`get_audio_sample_rate()`); the config value was not on the page (UNVERIFIED, expected 24000 = the model's native rate) | 400 invalid voice, 404 voice file not found, 503 model not loaded, 500 engine errors (the handler's `except Exception` also turns its own 400/404 raised inside the try into 500 with the old status in the detail) | not applicable (nothing streams); the connection close does not stop synthesis |
| **vLLM-Omni (Voxtral TTS)** | `POST {BASE_URL}/audio/speech {input, model, response_format, voice}` (`local-speech.md` section 3) | yes | yes (card) | 24 kHz | not read | not read |

`voice` meaning differs: a Kokoro voice name (`af_heart`; combinations like
`af_bella(2)+af_sky(1)` are Kokoro's), a Chatterbox file name from its voices directory
(`GET /v1/audio/voices` lists them; the preset's example `Emily.wav` is UNVERIFIED), a Speaches
voice of the loaded model.

### Presets

`OpenAiSpeechConfig::preset` / `[tts.openai_speech] preset`; none has a default base URL.

| Preset | model | format | other |
|---|---|---|---|
| `generic` | required | `pcm` | nothing extra |
| `kokoro` | `kokoro` | `pcm` | `stream: true`; example voice `af_heart` |
| `chatterbox` | `chatterbox` | `wav` | header read, rate checked; one whole segment per response |
| `speaches` | `speaches-ai/Kokoro-82M-v1.0-ONNX` | `pcm` | `sample_rate` = session rate (any rate) |
| `vllm_omni` | `mistralai/Voxtral-4B-TTS-2603` | `pcm` | weights non-commercial; set `model` to what the server serves |

## Design

- **No connection at `connect`.** The HTTP client comes from the shared pool
  (`net::shared_client`, keyed by origin); `warm()` is an unauthenticated `HEAD` that parks a
  connection in it. A refusal or an unreachable server therefore shows up as `TtsEvent::Error`
  after the first text, and the session ends (the engine reconnects at its next round). To let the
  contract suite judge that, `TtsHarness` got `faults_at_connect()` (default `true`); this adapter
  says `false`, and the checks then demand the same errors as events (`Rejected { status }`,
  `Core(Timeout)` at the deadline) after the first text.
- **One request per text piece, sequential.** The TTS stage sends the segmenter's chunks as
  `Text` and a `Flush` per response. Jobs run FIFO across contexts, so audio is never interleaved
  and a context that never flushes holds nothing back. While a request is in flight, pending
  pieces of the same context are joined (up to `coalesce_chars`, default 300) so a slow server does
  not fall behind with one request per clause; the first piece of an utterance always goes out
  alone and at once. `Flush` is answered when every earlier piece has been received; it triggers no
  request. Not done: sending request N+1 before N ends (it would buffer audio and double GPU
  load); a streaming server finishes a segment faster than real time, so the first-byte latency of
  the next request hides behind the previous segment's playback.
- **Audio.** Body chunks become `Audio` events as they arrive, cut to whole samples (the one
  trailing byte of an odd chunk waits for the next; a copy only then). `pcm` and a `wav` data
  section are PCM16 mono; rate must be 24000 (or the Speaches preset's `sample_rate`): `connect`
  refuses another, the WAV header's rate is checked against the session, and the server config
  rejects `tts.sample_rate != 24000` at load. The engine resamples to the call.
- **Words.** None requested: no `Word` events, so played-text truncation after a barge-in uses the
  engine's estimate from text length and audio duration (as for Deepgram Aura). Kokoro-FastAPI's
  `/dev/captioned_speech` returns JSON items `{audio (base64), audio_format, timestamps: [..]}`
  per chunk (`development.py`, `CaptionedSpeechResponse`), but the pages read do not show how the
  items are framed on the wire (newline-delimited? `JSONStreamingResponse` is in a module not
  read), nor the `WordTimestamp` fields, and the route is under `/dev`. Left as a follow-up that
  needs a live server: then add `format = "captioned"` parsing base64 audio and
  mapping timestamps onto `Word`.
- **Cancel.** Served before everything (also during send and body read): the context's queued
  text and undelivered events are dropped, `Cancelled` is queued at once, the response is dropped
  (RST_STREAM on HTTP/2, connection close on HTTP/1.1; the next request opens a new connection).
  Input for a cancelled context is ignored afterwards.
- **Refusals.** Non-2xx ends the session with `Rejected { status }` and the body is not read,
  except 400 and 422: those refuse one piece (Kokoro-FastAPI 400s a segment with nothing
  speakable), which is skipped with a warning; two in a row end the session. A 200 with a JSON,
  text or XML content type is `Protocol`.
- **Timeouts** (injected clock): `connect_timeout` 3 s; `first_audio_timeout` 15 s from sending to
  the first audio byte (a CPU Kokoro needs seconds; Chatterbox answers only after synthesizing the
  whole segment: raise it); `idle_timeout` 8 s between chunks; `max_request_duration` 120 s.
- **Backpressure.** The body is not read while 8 events wait for the consumer; the input channel is
  not read while 128 pieces queue; channels are the standard bounded ones; the cancel channel is
  never gated.
- **Plain HTTP.** `http://` is allowed for loopback, and for any host while no API key is
  configured (nothing secret is sent: a GPU box on the LAN works); with a key, `https://` or
  `allow_insecure_http`. The server config checks the same at load.

- **Model warm-up.** `warm()` (HEAD) is not enough for a GPU server that has idled: the
  first synthesis after idle is slow (below). `warm()` therefore also spawns, not awaited (the
  engine's TTS warm budget is `min(connect_timeout, 400 ms)`, tts.rs `warm_provider`), one tiny
  `POST` with `warm_text` whose audio is drained and dropped. `connect` starts the same warm-up (the
  first call of a process has no voice at `warm()` time). One is in flight at most; the next starts
  only after `warm_interval` (default 60 s, injected clock, one shared state per provider, so many
  calls do not each fire one); it is bounded by `warm_timeout` (10 s) and the provider's shutdown
  token; a failure is logged at debug and does not use up the interval. The voice is the latest
  session's, else `warm_voice`. Default on for the `kokoro` preset (`"Hi."`, voice `af_heart`),
  off for the others: Chatterbox-TTS-Server is not streaming and takes ~3 s of GPU per sentence, so a
  warm-up per minute would compete with real turns (opt in with `warm_text` and a long
  `warm_interval_ms`); Speaches and vLLM-Omni have no measurement yet. `warm_text = ""` turns it off.

## Config (`docs/CONFIG.md`)

```toml
[tts]
provider = "openai_speech"        # api_key_env is optional for this provider only
voice = "af_heart"

[tts.openai_speech]
preset = "kokoro"
base_url = "http://localhost:8880/v1"
# model, speed (0.25..=4), format ("pcm"|"wav"), first_audio_timeout_ms (500..=120000)
# warm_text = "Hi."        # background model warm-up (kokoro preset default; "" = off)
# warm_interval_ms = 60000 # at most one warm-up per interval, across calls
```

Example: `examples/agents/local-kokoro.toml` (and `.yaml`): Deepgram STT + Ollama LLM + Kokoro TTS.

## Try it with Kokoro-FastAPI

Commands from the project README (fetched 2026-10-14). The images below were not run for this
page (the live runs used a source checkout), but the server and the adapter were. `-p
127.0.0.1:8880:8880` keeps the unauthenticated port on this machine; the README's own examples
publish it on every interface.

```bash
# NVIDIA GPU (README: ships CUDA 12.6)
docker run --gpus all -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-gpu:latest
# NVIDIA RTX 50-series / Blackwell (README: ships CUDA 12.8) - the RTX 5070 of the live runs
docker run --gpus all -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-gpu:latest-cu128
# CPU only
docker run -p 127.0.0.1:8880:8880 ghcr.io/remsky/kokoro-fastapi-cpu:latest
```

The README recommends a pinned release tag over `:latest` for stable use. API docs at
`http://localhost:8880/docs`, voices at `/v1/audio/voices`. Then:

```bash
BRISVOX_LIVE_TESTS=1 LOCAL_TTS_BASE_URL=http://localhost:8880/v1 \
  cargo nextest run -p brisvox-providers --features provider-openai-speech \
  --test openai_speech live_local_tts_server --no-capture
```

or run the server with `examples/agents/local-kokoro.toml`.

## Live check

`live_local_tts_server` in `crates/brisvox-providers/tests/openai_speech.rs`
(`BRISVOX_LIVE_TESTS=1` and `LOCAL_TTS_BASE_URL`; optional `LOCAL_TTS_PRESET` kokoro (default) /
chatterbox / speaches / vllm_omni / generic, `LOCAL_TTS_VOICE`, `LOCAL_TTS_MODEL`,
`LOCAL_TTS_API_KEY`, never printed; skips without them). It prints the time to first audio of a
two-piece utterance, the cancel acknowledgement time and the first audio after a barge-in on the
same session, and asserts the audio is whole samples.

## UNVERIFIED / open items for a live run

1. All wire shapes above were read from source summaries, none observed: confirm `pcm` bytes are
   headerless 24 kHz on Kokoro-FastAPI `latest-cu128`.
2. Kokoro-FastAPI first-chunk latency (README claims about 300 ms GPU at chunk size 400, about
   3.5 s on an older i7) and whether the server stops generating on disconnect in practice.
3. Chatterbox-TTS-Server: the output sample rate, the real voice names (`GET /v1/audio/voices`),
   Turkish selection through `language` (the server mapping sends the TTS language's primary tag,
   e.g. `tr`, for the `chatterbox` preset only; the accepted codes are unconfirmed), and its time
   per segment; its `/tts` route with
   `stream: true` (chunk-level WAV) might beat `/v1/audio/speech` and is not used.
4. Speaches: PCM rate with `sample_rate` sent, auth, Blackwell build.
5. Word timestamps through Kokoro's captioned route (see "Words").
6. HTTP/1.1 vs HTTP/2 cancel behaviour of each server (the adapter only drops the response).

## Sources (fetched 2026-10-14)

- `raw.githubusercontent.com/remsky/Kokoro-FastAPI/master/api/src/routers/openai_compatible.py`
- `raw.githubusercontent.com/remsky/Kokoro-FastAPI/master/api/src/routers/development.py`
- `raw.githubusercontent.com/remsky/Kokoro-FastAPI/master/api/src/structures/schemas.py`
- `raw.githubusercontent.com/remsky/Kokoro-FastAPI/master/README.md`
- `raw.githubusercontent.com/devnen/Chatterbox-TTS-Server/main/server.py`
- `github.com/devnen/Chatterbox-TTS-Server` (README; the page was truncated before the
  `/v1/audio/speech` section, so the route facts come from `server.py`)
- `raw.githubusercontent.com/speaches-ai/speaches/master/src/speaches/routers/speech.py`
- `docs/providers/local-speech.md` (the survey this adapter was chosen from), vLLM-Omni row.

## Live (2026-10-14)

Kokoro-FastAPI (git main, `uv sync --extra gpu-cu128`, PyTorch 2.8.0+cu128, CUDA on the RTX 5070,
WSL2) through `openai_speech` (`kokoro` preset), 3 runs of the live test: time to first audio 164 ms
(cold), then 52 and 50 ms; 4.84 s of audio done after 102-228 ms; cancel acknowledged in 0.06-0.2 ms.
Cartesia's first audio over the internet is ~200 ms.

Chatterbox-TTS-Server (git main, Python 3.10 venv via uv, `requirements-nvidia-cu128.txt` + chatterbox-v2
`--no-deps`; protobuf had to be raised to 4.25 for onnx 1.16; `model.repo_id: chatterbox-multilingual`,
`language: tr`), RTX 5070: a two-sentence Turkish input ("Merhaba, yarın saat üçte randevunuz var. Başka
bir konuda yardımcı olabilir miyim?") took ~3.06-3.10 s to the first byte (the whole WAV; the route does
not stream) for 4.3-5.1 s of audio. Usable for offline/batch Turkish speech, too slow for a live turn's
first audio; the adapter's live test (15 s first-audio limit) timed out on the cold first request.
Requests with non-ASCII text must be sent as UTF-8 (a cp1254 body got HTTP 400 "error parsing the body").


## Cold first request and the model warm-up (2026-10-14)

The maintainers' local call showed `tts_ttfb` 2.78 s on the first response after idle and 52-104 ms on
the later ones (Kokoro-FastAPI GPU, RTX 5070, WSL2). Measured with a script against the running
server (time from sending the request to the first audio byte; the server is shared with other
work, so these are not lab numbers):

| state | first audio |
|---|---|
| after 420 s idle, no warm-up | 524 ms, 464 ms |
| right after, same request again | 56 ms, 83 ms |
| after 420 s idle, `"Hi."` warm-up first (the warm-up request itself took 463 ms and 514 ms, in the background) | the real request then 59 ms, 47 ms |
| after 200 s idle (total time of the request, curl), no warm-up | 687, 322, 105 ms against 55-71 ms warm |

So the penalty grows with the idle time (about 0.1-0.7 s after 3-7 minutes here, 2.8 s in the
maintainers' run, which came after a much longer idle). The warm-up pays it once, in the background,
while the call connects and the caller starts to speak (the engine's first user turn takes
seconds to reach the TTS, so a 0.5-3 s warm-up is finished well before it).
Tests (mock server, real sockets): `warm_returns_at_once_and_the_synthesis_runs_in_the_background`,
`warm_ups_are_one_at_a_time_and_spaced_by_the_interval`,
`the_warm_up_uses_the_latest_sessions_voice_and_empty_text_turns_it_off`,
`a_failed_warm_up_is_silent_and_retried_by_the_next_warm`, and the preset/limit unit test.
The example `examples/agents/local/local-en.toml` now sets `preset = "kokoro"`; a definition with
the `generic` preset and `model = "kokoro"` has no warm-up unless it sets `warm_text`.
