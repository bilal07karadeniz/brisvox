# OpenAI Realtime transcription protocol and its dialects — STT (WebSocket)

Code: `brisvox_providers::openai_realtime_stt` (feature
`provider-openai-realtime-stt`, server feature of the same name, `provider =
"openai_realtime_stt"` with `[stt.openai_realtime_stt] dialect = "openai" | "nim" | "speaches" |
"vllm"`), on the shared session core `brisvox_providers::wsstt`. Survey and alternatives:
`docs/providers/local-speech.md`; the speech-to-speech use of the same API:
`docs/providers/openai-realtime.md`.

Read 2026-10-14 through a summarising fetch tool (documentation pages: field lists, not wire
captures) and, for Speaches and vLLM, from the server source itself (labelled "source").
**Status: not yet verified against a real server.** This file has no live results; nothing was
run against any of the four servers (the stack that was run locally uses sherpa-onnx instead:
`docs/providers/sherpa-onnx.md`). The mocks in
`crates/brisvox-providers/tests/openai_realtime_stt.rs` implement what is written here;
a live run (with the existing OpenAI key) must confirm it.

Sources:
- OpenAI: https://developers.openai.com/api/docs/guides/realtime-transcription (session shape,
  models, events) and https://developers.openai.com/api/docs/guides/voice-websockets (URL and
  auth of a Realtime WebSocket)
- NVIDIA Speech NIM: https://docs.nvidia.com/nim/speech/latest/reference/api-references/asr/realtime-asr.html
- Speaches (source): https://raw.githubusercontent.com/speaches-ai/speaches/master/src/speaches/
  `routers/realtime_ws.py`, `realtime/session.py`, `realtime/session_event_router.py`,
  `realtime/input_audio_buffer_event_router.py`, `types/realtime.py`
- vLLM (source): https://raw.githubusercontent.com/vllm-project/vllm/main/vllm/entrypoints/speech_to_text/realtime/
  `connection.py`, `protocol.py`; docs https://docs.vllm.ai/en/latest/serving/online_serving/speech_to_text/
  and https://docs.vllm.ai/en/latest/examples/speech_to_text/realtime/

## Dialect table

| | OpenAI | NVIDIA Speech NIM | Speaches | vLLM |
|---|---|---|---|---|
| URL | `wss://api.openai.com/v1/realtime?model=<model>` (the guide for transcription shows no connection line; the voice-websockets guide shows `?model=`; `?intent=transcription` as in the older API is the alternative, set it with `path_and_query`: UNVERIFIED which one the transcription models want) | `ws://<host>:9000/v1/realtime?intent=transcription`; missing/invalid intent closes with 1008 | `ws://<host>:8000/v1/realtime?intent=transcription&model=<stt model>[&language=<iso639-1>]` (source: `realtime_ws.py` parameters `model`, `intent`, `language`, `transcription_model`) | `ws://<host>:8000/v1/realtime` |
| auth | `Authorization: Bearer <key>`; no beta header | none by default for a self-hosted container (`REALTIME_AUTH_REQUIRE_TOKEN=false`); a token travels in `Sec-WebSocket-Protocol: realtime, realtime-token.<secret>` (not supported by the adapter) | optional API key checked before the upgrade (Bearer, `X-API-Key` or `api_key` query per the survey); failure = 1008/403 | none |
| audio in | `input_audio_buffer.append {audio: base64 PCM16}`, session format `{"type":"audio/pcm","rate":24000}` | same event; session `input_audio_params {sample_rate_hz: 16000, num_channels: 1}`, `input_audio_format: "pcm16"`; max 15 MB per append | same event; the base64 is read as **24 kHz** and resampled to 16 kHz server side (source) | same event, base64 PCM16 **16 kHz** mono |
| session message | `session.update {session: {type: "transcription", audio: {input: {format, transcription: {model, languages \| language, prompt}, turn_detection: null}}}}`; `gpt-live-transcribe` takes `languages` (not `language`); "omit `audio.input.turn_detection` or set it to `null`. The model doesn't support `server_vad` or `semantic_vad`" | `transcription_session.update {session: {modalities: ["text"], input_audio_format: "pcm16", input_audio_transcription: {language, model, prompt}, input_audio_params, recognition_config {enable_automatic_punctuation, ...}}}` (all `session.*` optional except `modalities`, `input_audio_format`; defaults rate 16000, language `en-US`, model `conformer`, punctuation off) | none sent: model and language come from the URL. `session.update` exists but is of no use here (below) | `session.update {model}` (required before a commit: "Model not validated") |
| server VAD | none (null) | the page documents `vad_states` in results and zero-default `endpointing_config`; whether server endpointing runs and can be disabled: UNVERIFIED | on by default for transcription: `server_vad`, threshold **0.9**, `prefix_padding_ms` 0, `silence_duration_ms` 550, `create_response` false (source: `session.py`). **Cannot be switched off**: `PartialSession.turn_detection` is `TurnDetection \| NotGiven` and every `TurnDetection` field is required (source: `types/realtime.py`), so `null` is refused | none |
| interims | `conversation.item.input_audio_transcription.delta {item_id, content_index, delta}`: "returns transcript deltas as speech arrives" (`gpt-live-transcribe`); `gpt-transcribe`: deltas "before the final completion event" | the same `delta` event "when response received from gRPC server"; whether a delta is an increment or the whole hypothesis: UNVERIFIED (the page's sample client assumes increments) | **none** (source: only `...completed` is published) | `transcription.delta {delta}` (incremental, source: `output_kind=DELTA`) |
| final | `...input_audio_transcription.completed {item_id, content_index, transcript}` after `input_audio_buffer.commit` | `...completed {transcript, words_info, vad_states, is_last_result, usage}`; several per stream are possible (`is_last_result`) | `...completed {item_id, transcript, usage}` after a commit or a VAD turn | `transcription.done {text, usage}` |
| commit | `input_audio_buffer.commit` = "end of the audio turn" with client VAD; `input_audio_buffer.committed` may be returned | `input_audio_buffer.commit` sends the buffer for inference; `input_audio_buffer.committed {previous_item_id, item_id}`; `input_audio_buffer.done` = no more audio | `input_audio_buffer.commit`; a buffer of less than **100 ms** is `invalid_request_error` "buffer too small" (source: `MIN_AUDIO_BUFFER_DURATION_MS = 100`) | `input_audio_buffer.commit` **starts** a generation that consumes the appended audio as it arrives; `commit` with `final: true` ends the audio and the generation answers with `transcription.done`; a new non-final `commit` starts the next (source: `connection.py`) |
| errors | `{"type":"error","error":{type,code,message,param}}` | same shape; `conversation.item.input_audio_transcription.failed` | same shape (`invalid_request_error`, `server_error`) | `{"type":"error","error":"<message>","code":"<code>"}` (string message) |
| limits | 60 minutes per session | page lists both 100 and 10,000 connections (inconsistent) | 30 minutes per connection (source: `OPENAI_REALTIME_SESSION_DURATION_SECONDS`) | not stated |
| models | `gpt-live-transcribe`, `gpt-transcribe` (the page names no others) | Nemotron ASR Streaming, Parakeet (per NIM docs) | any Speaches STT model (default English-only `Systran/faster-distil-whisper-small.en`; Turkish needs a multilingual one such as `Systran/faster-whisper-small`) | ASR models with a realtime class (Voxtral Mini 4B Realtime, Qwen3-ASR); `--enforce-eager` or the model card's compilation config |

## What the adapter does

- **One protocol, a small table.** `Dialect` fixes the URL, rate, session message(s), whether
  interims exist and the minimum commit. Event names are matched for all dialects at once
  (`...transcription.delta/completed`, `transcription.delta/done`, `error`, `...failed`,
  `input_audio_buffer.committed`); everything else (`session.created`, `rate_limits.updated`, ...)
  is ignored. The mock servers, not the table, decide what a server sends.
- **Turn control is ours.** The engine's VAD ends the turn; `Finalize` is `input_audio_buffer.commit`
  (vLLM: the `final: true` commit, with the audio gate shut until `transcription.done` and a new
  generation started by a plain `commit`; audio that arrives meanwhile is held in the ring and sent
  after it). Server-side turn ends (Speaches' VAD, NIM's endpointing) arrive as additional
  `speech_final` transcripts that are not `from_finalize`; the engine closes them with the next
  `UtteranceFinal` (`crates/brisvox-engine/src/stt.rs`).
- **Which transcript answers a commit.** The `item_id` of `input_audio_buffer.committed` pairs
  the commit with its `completed`; a `completed` of another item is a server turn. Servers that
  send no `committed` (vLLM has no items): the first transcript after the commit answers it.
- **Nothing to flush.** A `Finalize` with nothing appended since the last commit, or less than
  the minimum (`min_commit`: 100 ms OpenAI and Speaches, 0 NIM and vLLM), is answered empty at once
  and sends nothing; an `error` event while a commit is open (an empty buffer, a failed
  transcription) answers it empty too. The engine promotes the latest interim when an answer is
  empty.
- **Timeout.** No answer within `finalize_timeout` (default 2 s, the engine gives up at 3 s): the
  latest hypothesis is delivered as the answer, the item is remembered and its late transcript is
  dropped (no second final). vLLM: the connection is replaced instead (a generation may still run).
- **Errors.** `invalid_api_key` / `authentication_error` -> `Rejected 401`, `model_not_found` ->
  `Rejected 404`, `insufficient_quota` -> `Rejected 429`; any other `error` event is logged and the
  session goes on. Handshake status -> `Rejected`. Not JSON -> `Protocol`.
- **Liveness.** These servers do not answer every chunk, so the session core proves liveness with
  WebSocket pings (any frame counts) instead of expecting messages: see `wsstt`.
- **Auth transport.** A key is only sent over `wss://` or to a loopback host; a keyless local
  server may be reached over plain `ws://` anywhere (a GPU box on the LAN).

## Configuration

`docs/CONFIG.md` (`[stt.openai_realtime_stt]`). Minimal local stacks:

```toml
# NVIDIA Speech NIM (Nemotron ASR Streaming, Turkish and English), 16 kHz
[stt]
provider = "openai_realtime_stt"
[stt.openai_realtime_stt]
dialect = "nim"
base_url = "ws://gpu-box.lan:9000"

# Speaches (Whisper; no interim text), 24 kHz
[stt]
provider = "openai_realtime_stt"
[stt.openai_realtime_stt]
dialect = "speaches"
model = "Systran/faster-whisper-small"
```

The session language (`agent.language` / `stt.language`) is mapped per dialect: OpenAI and Speaches
get the primary subtag (`tr`), NIM a locale (`tr` -> `tr-TR`, `en` -> `en-US`, others as written),
vLLM none (the model decides).

## Tests

`tests/openai_realtime_stt.rs`: OpenAI (URL, bearer, session message with `turn_detection: null`,
16 -> 24 kHz resampling, deltas, commit/completed, empty finalizes, error while a commit is open,
unanswered commit and its late transcript, reconnect with session message and replay, disconnect,
garbage, `invalid_api_key`, refusals, live-but-silent server, dead server, hang-up, config
validation), NIM (URL, nested session message, audio byte for byte), Speaches (URL query, no
session message, VAD turn as endpoint, no interims), vLLM (model, start commit, final commit,
held audio, restart, timeout replaces the connection), and the provider contract suite for the
OpenAI, NIM and vLLM dialects (Speaches sends no interims, which the suite requires). Live:
`BRISVOX_LIVE_TESTS=1 LOCAL_REALTIME_STT_URL=ws://127.0.0.1:9000 LOCAL_REALTIME_STT_DIALECT=nim
[LOCAL_REALTIME_STT_MODEL=..] [LOCAL_REALTIME_STT_LANGUAGE=tr] cargo test -p brisvox-providers
--features provider-openai-realtime-stt --test openai_realtime_stt live_ -- --nocapture` prints
partial counts, first-partial time and **finalize latency** for the two-utterance fixture.

## Open points (all for a live run)

- OpenAI: the transcription URL (`?model=` vs `?intent=transcription`), the `committed` event and
  the error codes on a too-short commit.
- NIM: whether a delta is an increment (`delta_replaces = true` if not), what `commit` does exactly
  (one `completed` per commit?), whether server endpointing interferes, WSL2 is unsupported by the
  Nemotron container (`docs/providers/local-speech.md`).
- Speaches: Blackwell compatibility of its CUDA 12.6 image, latency of a commit on CPU/GPU.
- vLLM: the first `commit` before `session.update`'s model check, behaviour at `realtime_max_tokens`
  (the adapter restarts a generation that ends by itself).
