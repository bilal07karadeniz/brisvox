# Local speech servers — STT + TTS (self-hosted survey)

Researched 2026-10-14. Every fact below came from a page fetched that day (URL in "Sources" and
inline). Pages were read through a summarising fetch tool: quoted strings are copied from its
output, JSON/message shapes are field lists, NOT verbatim wire captures. Source-code reads are
labelled "(source)". Anything not confirmed is marked UNVERIFIED. This is a survey written from
documentation. Afterwards three of the servers were run and measured (2026-10-14): sherpa-onnx's
Python server (Turkish `seda-v0.1` and the English zipformer: `docs/providers/sherpa-onnx.md`,
"Live"), Kokoro-FastAPI and Chatterbox-TTS-Server (`docs/providers/openai-speech.md`, "Live").
Every other server in this survey was not run. Latency numbers below are the projects' own claims
unless stated.

**Recommended (read this, skip the survey).**

| Need | Start with | Verified live (2026-10-14) |
|---|---|---|
| English speech recognition, no GPU | sherpa-onnx `streaming_server.py` with the streaming zipformer `sherpa-onnx-streaming-zipformer-en-2023-06-26`, port 6007 (`provider = "sherpa_onnx"`, `docs/providers/sherpa-onnx.md`) | Yes, in a fully local browser call. Output is UPPERCASE without punctuation, and a very short first utterance can come back nearly empty (`docs/providers/sherpa-onnx.md`, "First-turn check"). The script has no bind flag and listens on all interfaces: firewall the port. |
| Turkish speech recognition, no GPU | sherpa-onnx with `atasoglu/seda-v0.1`, port 6006 | Yes (WER 0.00 on the two fixture utterances, finalize in 1 to 21 ms); read speech only, 8 kHz phone audio unmeasured. |
| English voice | Kokoro-FastAPI through `openai_speech`, `preset = "kokoro"`, port 8880 (`latest-cu128` image on an RTX 50 series card, the `cpu` image otherwise) | Yes: first audio 50 to 164 ms on a GPU. The first request after a long idle is slower; the preset warms it in the background. |
| Turkish voice | Nothing live-ready. Chatterbox-TTS-Server works (`preset = "generic"`) but takes about 3 s to the first audio | Measured, too slow for a live turn; usable for offline speech. |
| A local language model (with tools) | Ollama through `openai_compatible`: `preset = "ollama"`, `model = "gemma4:12b-it-qat"`, `reasoning_effort = "none"` (7.7 GB of GPU memory) | Yes: all six tool scenarios 3/3 times, first sentence 267 ms on an RTX 5070; four smaller local models failed some or all tool scenarios (`docs/TOOLS.md`, "Local models through Ollama"). Without tools `gemma4:e4b-it-qat` (3.1 GB, 157 ms) is enough. |
| Everything else in this survey (Speaches, vLLM-Omni, Kyutai, NVIDIA NIM, Piper, Pocket TTS ...) | Do not start here | Not run; paper facts only. |

The whole local stack, step by step: `book/src/quickstart/local.md`; its agent files are
`examples/agents/local/local-en.toml` and `local-tr.toml`.

- Implemented: the sherpa-onnx client (`docs/providers/sherpa-onnx.md`) and the OpenAI
  Realtime transcription client with a dialect table (`docs/providers/openai-realtime-stt.md`).
  Two corrections from reading the server sources: Speaches' server VAD cannot be disabled
  (`session.update` does not accept `turn_detection: null`), and vLLM's realtime route starts a
  generation with a plain `commit` and ends it with `commit {final: true}` (`connection.py`).
- Docs: see "Sources" at the end (fetched 2026-10-14).
- Auth: local servers mostly none (Kyutai `kyutai-api-key` header default `public_token`;
  Speaches optional Bearer / `X-API-Key` / `api_key` query; NVIDIA NIM needs an NGC key to PULL the
  container).
- Endpoint(s) / regions: self-hosted; ports per server below.
- Protocol: no single standard. Two de-facto shapes exist (see "Protocol convergence").
- Keepalive / timeouts: not documented for any server unless stated; Brisvox must apply its own.
- Finalize / flush / cancel semantics: per server in the matrix below.
- Timestamps / interim results: per server.
- Concurrency / rate limits: per server.
- Turkish support: see the Turkish summary table.
- Gotchas: see "Gotchas".

Test hardware: RTX 5070 (NVIDIA page: 12 GB GDDR7, 6,144 CUDA cores), Windows 11 +
WSL2 Ubuntu + Docker. Production may be CPU-only.

## 1. Protocol convergence (answer to "is there a standard?")

**TTS: yes, OpenAI `POST /v1/audio/speech`.** Body `{model, input, voice, response_format, speed,
...}`; with `response_format: "pcm"` the OpenAI docs define "raw 24kHz, 16-bit signed samples,
little-endian ... without the header" and chunked transfer, so playback starts before generation
ends (developers.openai.com text-to-speech guide). Servers that expose it: Kokoro-FastAPI
(pcm, 24 kHz, `stream`), Speaches (pcm/wav/mp3/flac, `stream_format` audio|sse, optional
`sample_rate`), vLLM-Omni serving Voxtral TTS (`{BASE_URL}/audio/speech`), Chatterbox-TTS-Server
(`/v1/audio/speech`; whether it streams: UNVERIFIED). NOT compatible: Piper (`POST /synthesize`
returns a whole WAV), Kyutai (msgpack WebSocket), Orpheus/others (library only).
Limitation of the standard: the whole text is one request. Incremental text-in is only possible by
sending one request per sentence/clause (or a WebSocket protocol like Kyutai's).

**STT: OpenAI Realtime *transcription* WebSocket is the closest thing**, in three dialects:

| Dialect | URL | Audio in | Events out | Finalize |
|---|---|---|---|---|
| OpenAI (cloud, from `developers.openai.com/api/docs/guides/realtime-transcription`) | see `docs/providers/openai-realtime.md` | `input_audio_buffer.append {audio: base64 PCM16}`; guide example `audio/pcm` rate 24000 | `conversation.item.input_audio_transcription.delta {item_id, content_index, delta}`, `...completed {item_id, content_index, transcript}` | with `turn_detection` null/omitted the client sends `input_audio_buffer.commit`; "final transcript arrives in the completed event for that committed item" |
| NVIDIA Speech NIM (docs.nvidia.com/nim/speech/latest/reference/api-references/asr/realtime-asr.html) | `ws://<host>:9000/v1/realtime?intent=transcription` (missing/invalid intent -> close 1008) | base64 PCM16, default `sample_rate_hz` 16000, 1 channel; max 15 MB per append | `conversation.item.input_audio_transcription.delta {item_id, content_index, delta}`, `...completed {item_id, content_index, transcript, words_info, vad_states, is_last_result, usage}`, `...failed`, `input_audio_buffer.committed {previous_item_id, item_id}`, `error` | `input_audio_buffer.commit` per utterance; `input_audio_buffer.done` = no more audio (process remaining, stop); `input_audio_buffer.clear`; config via `transcription_session.update` |
| Speaches (speaches.ai/usage/realtime-api, source `input_audio_buffer_event_router.py`, `session.py`) | `wss://host/v1/realtime?intent=transcription&model=<stt model>&language=<iso639-1>` (auth Bearer / `X-API-Key` / `api_key`) | base64 audio treated as **24 kHz** and resampled to 16 kHz server-side (source); session default format `pcm16` | `input_audio_buffer.speech_started/stopped/committed`, `conversation.item.created`, `conversation.item.input_audio_transcription.completed {item_id, transcript, usage{type:"duration", seconds}}`. **No delta events** (source: `InputAudioBufferTranscriber` publishes only `completed`) | `input_audio_buffer.commit` (< 100 ms of audio -> `invalid_request_error`); server VAD default `server_vad` threshold 0.9, `silence_duration_ms` 550, `prefix_padding_ms` 0 (source); `response.cancel`, `conversation.item.truncate` unsupported |
| vLLM `/v1/realtime` (docs.vllm.ai serving/online_serving/speech_to_text; ASR models only) | `ws://host:8000/v1/realtime` | base64 PCM16 **16 kHz** mono in `input_audio_buffer.append {audio}` | `session.created {id}`, `transcription.delta {delta}`, `transcription.done {text, usage?}`, `error {error}` (different names from OpenAI) | client sends `session.update {model}`, then `input_audio_buffer.commit` (ready signal), appends, and `input_audio_buffer.commit {final:true}` when all audio is sent; the docs flow also says "Repeat from step 5 for the next utterance" but how a mid-session turn flush works: UNVERIFIED |

Consequence: one `OpenAiRealtimeTranscription`-style adapter with a small dialect config (URL/query,
input sample rate, base64 PCM16, event-name table, whether deltas exist) covers OpenAI, NVIDIA NIM
and Speaches, and with a second event-name table vLLM. Servers outside this family (own adapters
needed): sherpa-onnx, Kyutai, WhisperLive, Vosk.

## 2. STT matrix

Legend: S = true streaming (decodes while audio arrives, emits partials). "Fin" = explicit finalize.

| Server / model | S? | Protocol and finalize | Latency claim | Turkish | License (code / weights) | Hardware | Status |
|---|---|---|---|---|---|---|---|
| **Speaches** (faster-whisper; also Parakeet since v0.9.0-rc.1) | No interim. REST `/v1/audio/transcriptions` (SSE of output as the file is transcribed, i.e. per file); Realtime WS gives `completed` only | see dialect table; Fin = `commit` | none published (docs say real-time needs CUDA) | Whisper multilingual (Turkish in the Whisper language set; speaches default STT model is English-only `Systran/faster-distil-whisper-small.en`) | MIT / Whisper MIT | CPU or GPU; images `ghcr.io/speaches-ai/speaches:latest-cuda`, `:latest-cpu`; Dockerfile base `nvidia/cuda:12.6.3-base-ubuntu24.04` (RTX 50 / Blackwell compatibility: UNVERIFIED) | repo pushed 2026-10-13, 3.7k stars, 147 open issues, last release v0.9.0-rc.3 2025-12-27 (master far ahead) |
| **whisper.cpp server** | No (HTTP `POST /inference` multipart WAV `file`, `temperature`, `response_format`...; `POST /load`) | request/response only; streaming over HTTP not documented (UNVERIFIED). `whisper-stream` CLI example exists but was not documented on the fetched page | n/a | Whisper multilingual | code license UNVERIFIED (not fetched) | CPU/GPU | latest release v1.9.5 (2026-10-06) |
| **WhisperLive** (Collabora) | Near-live (Whisper re-decoding with VAD), "nearly-live" per README | WS: first message JSON options (`uid`, `language`, `task`, `model`, `use_vad`, `audio_format` float32/int16/uint8, `initial_prompt`, `hotwords`, `word_timestamps`, `send_last_n_segments`...); binary 16 kHz audio frames; **`b"END_OF_AUDIO"` binary frame ends the stream** (server finalizes then closes); refusal `{"uid","status":"WAIT","message":minutes}`; segments carry `start`,`end`,`text`,`completed` (segment shape from README, not server source); max clients default 4, max connection 600 s (source `server.py`); optional OpenAI-style REST with `--enable_rest` | none stated | Whisper multilingual | MIT / Whisper MIT | faster_whisper (CPU/GPU), TensorRT, OpenVINO; images `ghcr.io/collabora/whisperlive-gpu:latest`, `-cpu:latest`, `-openvino` | 4.3k stars, pushed 2026-10-07, no release info found |
| **sherpa-onnx streaming WS server** (Zipformer, Paraformer, NeMo transducers...) | **Yes** (true streaming transducers) | WS (python `streaming_server.py`, source): binary msgs = raw little-endian **float32** samples at `--sample-rate` (default 16000; no resampling); text msg `"Done"` ends the utterance (server pads 0.3 s zeros, decodes, sends final message, then closes); server -> client JSON `{"text": ..., "segment": n}` after each decode step (no `is_final`, `tokens`, `timestamps` in the python example); endpointing built in: `--use-endpoint` (default 1), `--rule1-min-trailing-silence` 2.4 s, `--rule2-min-trailing-silence` 1.2 s, `--rule3-min-utterance-length` 20 s; on endpoint the stream is reset and `segment` increments; `--max-active-connections` 200 (extra get HTTP 503), `--max-batch-size` 3, `--max-wait-ms` 10. A C++ server (websocketpp+asio) also exists; its message format was not fetched (UNVERIFIED) | community Turkish model: median first-word latency 0.70 s, final ~40 ms after audio end, RTF 0.027-0.034 on 1 CPU thread (model card, below) | no official k2-fsa Turkish streaming model in the listing fetched; community models below | Apache-2.0 / per model | CPU first-class, also GPU | v1.13.8 (2026-09-10, latest seen), very active |
| ... sherpa-onnx + `atasoglu/seda-v0.1` (community, Turkish only) | Yes (Zipformer2 transducer, 66.1M params, int8 ONNX, chunk 32 frames = 0.64 s) | as above (card says it "can also be used directly with sherpa-onnx"; the exact server command was NOT on the card: UNVERIFIED that `streaming_server.py` loads it unchanged). Output is lowercase, no punctuation, numbers as words, no ITN | see above | **Yes**: streaming WER 11.52 FLEURS-TR test, 13.61 Common Voice 27.0 TR test, 15.23 ISSAI TSC test (beam 4 + LM); card compares Whisper small at 11.90 on FLEURS-TR under the same normalisation | acoustic model Apache-2.0; optional LM files CC BY-SA 4.0 (Turkish Wikipedia text); training data CC0 (Common Voice) + MIT/CC BY 4.0 (ISSAI TSC) | ~170 MB RAM + ~1.4 MB per extra stream; encoder int8 70 MB | trained mostly on read speech; telephone / noisy / spontaneous speech UNMEASURED (card) |
| ... sherpa-onnx + `duxx/turkish-stt-zipformer` | Yes (23.6M, ~27 MB int8) | endpoint detection + hotwords per card | not stated | Turkish; dev-set WER 13.09 (CV17), 17.39 (daily-use), 20.16 (FLEURS valid) | **CC BY-NC 4.0 (non-commercial)** | CPU | not usable commercially |
| **Kyutai STT** (`kyutai/stt-1b-en_fr`, `stt-2.6b-en`) via `moshi-server` | **Yes** (delayed streams) | WS `ws://host:8080/api/asr-streaming`, header `kyutai-api-key` (default `public_token`; or `?auth_id=`), **MessagePack** both ways (source `stt_from_file_rust_server.py`). Client: `{"type":"Audio","pcm":[float32 ...]}` mono **24 kHz**, 1,920-sample (80 ms) frames; `{"type":"Marker","id":0}`. Server: `Step` (semantic VAD / progress), `Word {text, start_time}`, `EndWord {stop_time}`, `Marker` echoed once audio before it is transcribed. The script sends 1 s leading silence (needed by 2.6B), 5 s trailing silence, the Marker, then 35 more 1 s silences because "the model returns the marker late" -> finalize = Marker + enough silence to flush the 0.5 s delay (how little silence suffices: UNVERIFIED) | 0.5 s delay (1B en/fr, includes semantic VAD), 2.5 s (2.6B en); H100 400 real-time streams, L40S 64 streams at RTF 3x | **No** (English, French only) | Python MIT, Rust Apache-2.0 / weights CC-BY 4.0 | GPU (benchmarks on L40S/H100; minimums not stated; fit in 12 GB: UNVERIFIED) | repo pushed 2026-01-26 (slower since); `moshi-server` crate 0.6.4 (2025-10-01), MIT/Apache-2.0 |
| **Vosk server** (Kaldi) | Yes | WS (source `asr_server.py`, port 2700): text `{"config": {"sample_rate", "words", "max_alternatives", "phrase_list", "model"}}` (no reply; default rate 8000); binary audio (16-bit mono PCM per docs); reply per chunk: `Result()` if utterance finished else `PartialResult()` (Vosk JSON, schema not in the code); `{"eof":1}` -> `FinalResult()` then ends; **`{"reset":1}` -> `FinalResult()` and keeps connection open** (best finalize primitive of the group) | n/a | `vosk-model-small-tr-0.3` (35 MB, Apache-2.0, **WER "TBD"**) - weak small model | Apache-2.0 / Apache-2.0 per model | CPU; image `alphacep/kaldi-en:latest` port 2700 | 1.3k stars, 81 open issues; last commit date not seen |
| **NVIDIA Nemotron ASR Streaming (3.5 multilingual)** `nvidia/nemotron-3.5-asr-streaming-0.6b` | **Yes** (cache-aware FastConformer-RNNT) | via **Speech NIM** `nvcr.io/nim/nvidia/nemotron-asr-streaming:latest`, `NIM_TAGS_SELECTOR=name=nemotron-asr-streaming,type=multi` (or `type=en-US`), ports 9000 (HTTP + realtime WS) and 50051 (gRPC); needs `NGC_API_KEY`; realtime WS = OpenAI-transcription dialect in section 1; endpointing: silence-based on by default; `endpointing_config.*` fields exist; a `force_eou` chunk flag exists in the gRPC example (NOT confirmed over WS). Also runnable without NIM via NeMo, HF Transformers >= 5.13.0, NeMo-Speech.cpp (GGUF) - none of these ships a WebSocket server (per the card) | chunk sizes 80/160/320/560/1120 ms (`att_context_size`); NVIDIA "Voice Agent Blueprint" claim ASR 0.04 s single stream, 0.067 s at 64 streams (AI-generated FAQ page, verify) | **Yes, "transcription-ready" tier** tr-TR; FLEURS WER 12.34 (80 ms), 12.33 (160), 12.05 (320), 11.34 (560), 11.17 (1120) with LangID | NeMo code Apache-2.0 (UNVERIFIED, not fetched) / weights **OpenMDW-1.1** (HF card, NGC page; Together AI page says NVIDIA Open Model License: conflicting secondary source); NIM container: NVIDIA Software License Agreement + product terms; all "ready for commercial and non-commercial use" | NIM: Compute Capability >= 8.0, "at least 16 GB VRAM"; per-profile GPU memory ~4 GB (batch 32), ~12-13 GB (batch 128), ~21-23 GB (batch 512); Linux driver >= 535; **WSL2 not supported for Nemotron ASR Streaming** (support matrix); table lists GeForce RTX 50xx but the Nemotron section names only "Blackwell and DGX Spark"; the maintainers' 12 GB RTX 5070 fits only the small-batch profile at best and WSL2 rules out the maintainers' Docker setup: UNVERIFIED in practice | HF card released 2026-06-04; NIM tag 1.4.0 (NGC, updated 2026-10-03) |
| ... `nvidia/nemotron-speech-streaming-en-0.6b` | Yes | same family; English only; chunk 80/160/560/1120 ms; WER (OpenASR avg, no PnC) 6.93 / 7.07 / 7.67 / 8.43 % at 1.12 / 0.56 / 0.16 / 0.08 s; built-in punctuation/capitalisation; 16 kHz mono | see above | No | NVIDIA Open Model License Agreement | NeMo, Transformers, NeMo-Speech.cpp, NIM | |
| `nvidia/parakeet_realtime_eou_120m-v1` | Yes (EOU token) | 120M, ~80-160 ms, English, `<EOU>` token, no punctuation (search-result summary, primary card NOT fetched) | | No | NVIDIA Open Model License (secondary) | | UNVERIFIED |
| NVIDIA Parakeet-TDT-0.6B-v3 / Canary-1b-v2 | Parakeet: not natively streaming (NeMo chunked-inference script) ; Canary: not documented | NeMo | | **No** (25 European languages, Turkey's language not among them) | CC BY 4.0 | | Parakeet card dated 2025-08-14 |
| **vLLM Realtime + Voxtral-Mini-4B-Realtime-2602** | Yes | `vllm serve mistralai/Voxtral-Mini-4B-Realtime-2602 --compilation_config '{"cudagraph_mode":"PIECEWISE"}'` (card) / `--enforce-eager` (vLLM docs example); route `/v1/realtime`; vLLM dialect in section 1 | configurable delay 80 ms-2.4 s (card; intro says 240 ms), recommended 480 ms | **No** (13 languages: ar, de, en, es, fr, hi, it, nl, pt, zh, ja, ko, ru) | vLLM Apache-2.0 (UNVERIFIED) / weights Apache-2.0 | >= 16 GB GPU (card) | Feb 2026 |
| **vLLM Realtime + Qwen3-ASR-0.6B / 1.7B** | Streaming via vLLM only, no batch, no timestamps (README); vLLM lists `Qwen3ASRRealtimeGeneration` (`Qwen/Qwen3-ASR-0.6B`) under "Realtime Transcription", with a note it "is not auto-detected from `config.json`" (exact serve flag/override: UNVERIFIED); the realtime module buffers audio in fixed segments, default `segment_duration_s` 5.0 s (vLLM API docs) -> likely much coarser than Nemotron/Kyutai; interim cadence UNVERIFIED | no latency figures (README) | **Yes** (`tr` in the 30-language list); no Turkish WER published on the card | Apache-2.0 / Apache-2.0 | GPU (`qwenllm/qwen3-asr` image; `qwen-asr-serve`) | experimental candidate |
| **Moonshine Voice** | Yes (streaming models Tiny/Small/Medium, English only) | in-process library (Python, C++, mobile...); no server/WebSocket documented | "very low latency", no numbers | **No Turkish STT** (en, es, zh, ja, ko, vi, uk, ar) | MIT; non-English legacy non-streaming models under the non-commercial Moonshine Community License | CPU, edge | |
| OpenAI `whisper` itself | No | library | | yes (multilingual) | MIT | | |

Whisper Turkish WER: the Whisper paper table could not be read (PDF text not extractable) and no
published `large-v3` Turkish WER was found. Only data points: seda card, Whisper small FLEURS-TR
11.90 WER (their normalisation); a third-party table quoting 8.4 % for large-v2 on FLEURS (not
verified at the source).

## 3. TTS matrix

| Server / model | Streaming in / out | Protocol | Time to first audio (claim) | Turkish | License (code / weights) | Hardware | Status |
|---|---|---|---|---|---|---|---|
| **Kokoro-FastAPI** (Kokoro-82M v1.0, 54 voices) | in: whole text per request; out: chunked audio | `POST /v1/audio/speech` `{model (tts-1, tts-1-hd, kokoro, gpt-4o-mini-tts accepted), input, voice (combos like `af_bella(2)+af_sky(1)`), response_format mp3/opus/aac/flac/wav/pcm, stream, speed, lang_code, normalization_options}`; `pcm` = 16-bit mono **24 kHz**; `Transfer-Encoding: chunked`; extras `/v1/audio/voices`, `/dev/captioned_speech` (word timestamps), `/dev/phonemize`; validation errors are normal 400 before streaming starts; **cancel = client disconnect** (server checks `is_disconnected` before each chunk and stops generating) | ~300 ms GPU @ chunk size 400; ~3500 ms older i7 CPU @ 200; < 1 s M3 Pro CPU @ 200 (README, "first-token latency (streaming)") | **No** (en-US/GB, es, fr, hi, it, ja, pt-BR, zh) | wrapper Apache-2.0, weights Apache-2.0, StyleTTS2-derived code MIT | CPU image `ghcr.io/remsky/kokoro-fastapi-cpu`, GPU `ghcr.io/remsky/kokoro-fastapi-gpu` (cu126), **RTX 50 / Blackwell: `ghcr.io/remsky/kokoro-fastapi-gpu:latest-cu128`**; port 8880 | v0.9.0 (2026-09-10), pushed 2026-10-14, 5.5k stars, README: "development focused", `release` branch for production |
| **Speaches TTS** (Kokoro ONNX `speaches-ai/Kokoro-82M-v1.0-ONNX`, Piper) | out: StreamingResponse | `POST /v1/audio/speech` `{model, input, voice, response_format pcm/mp3/wav/flac/opus/aac, speed, stream_format "audio"|"sse", sample_rate}`; SSE mode = `SpeechAudioDeltaEvent` (base64) then `SpeechAudioDoneEvent`; PCM response carries no rate/format header (client must know); docs say opus/aac unsupported (code lists them: conflicting) (source `routers/speech.py`, docs) | none | only via Piper voices (below) | MIT | CPU/GPU | |
| **Piper** (`OHF-Voice/piper1-gpl`) | out: whole WAV | `python3 -m piper.http_server -m <voice>`, port 5000: `POST /synthesize {text, voice, speaker, speaker_id, length_scale, noise_scale, noise_w_scale}` -> WAV file; `GET /voices`, `GET /info`; sample rate / chunked streaming not documented | n/a (fast ONNX VITS, no number found) | **Yes but one voice**: `tr_TR-dfki` (`rhasspy/piper-voices` tree has only `tr/tr_TR/dfki`; `fahrettin` and `fettah` were removed "at request of contributors"); 22,050 Hz, medium, 1 speaker | engine **GPL-3.0** (use as a separate process, do not link); voice **CC BY-NC-SA 4.0 = non-commercial** (MODEL_CARD); other voices have their own licenses | CPU | repo pushed 2026-10-11; README: Open Home Foundation "is looking for maintainers" |
| **Chatterbox** (Resemble AI) Multilingual V3 (500M), Turbo (350M, EN), Nano (110M, EN, CPU) | README does not mention streaming for the library | library; wrapper below | open-source models: no figure (Nano "about 3x faster than realtime on 8 CPU cores"); "sub-200 ms" is the paid hosted service only | **Yes** (23 languages incl. Turkish; V3 list not itemised separately, V2 list includes Turkish) | MIT / MIT; all output carries a Perth watermark | Multilingual: CUDA/CPU/MPS examples; VRAM not stated | |
| ... **Chatterbox-TTS-Server** (`devnen`) v2.0.0 | out: `stream: true` on `/tts` returns WAV as each chunk finishes ("chunk-level, not token-level"; single-chunk text gets no TTFB benefit) | `POST /tts` (WAV/MP3/Opus, `language` param), `POST /v1/audio/speech` (OpenAI-compatible; streaming there UNVERIFIED), `GET /v1/audio/voices`, `/docs`; hot-swap Original/Multilingual/Turbo | no figure | Turkish via multilingual model; exact language code not on page (check `/docs`) | MIT | **Blackwell/RTX 50 (incl. 5070): `docker-compose-cu128.yml`, CUDA 12.8, driver >= 570**; CPU image exists | no TTFA data; needs a live measurement |
| **Kyutai TTS** (`kyutai/tts-1.6b-en_fr`, ~1.8B total) via `moshi-server` | **in: incremental text (one word per message)**; out: streamed audio | WS `ws://host:8080/api/tts_streaming?voice=<path in kyutai/tts-voices>&format=PcmMessagePack`, header `kyutai-api-key`, MessagePack: client `{"type":"Text","text":word}` per word then `{"type":"Eos"}`; server `{"type":"Audio","pcm":[float32...]}` mono **24 kHz**, 1,920-sample blocks; no explicit end message (receive loop ends when the socket closes); no cancel message documented (close the socket: UNVERIFIED semantics) (source `tts_rust_server.py`) | audio trails text by 16 steps ~1.28 s (card); throughput ~75x batched; no TTFA figure | **No** (en, fr) | Python MIT, Rust Apache-2.0 / weights CC-BY 4.0 (card); no watermark; voice cloning only from the provided embeddings | GPU (pretrained on H100; inference minimum not stated) | Rust-server install "a bit tricky" (README) |
| **Kyutai Pocket TTS** | streaming out | `serve` command: local HTTP server on 8000, endpoints not documented on README | ~200 ms first chunk on CPU, ~6x realtime on a MacBook Air M4 with 2 cores | **No** (en, fr, de, pt, it, es, nl; community adds cs, hi, ko, el) | MIT code; each voice has its own license | CPU, ~100M params | |
| **Voxtral-4B-TTS-2603** (Mistral) | streaming | vLLM-Omni `vllm serve mistralai/Voxtral-4B-TTS-2603 --omni`; `POST {BASE}/audio/speech {input, model, response_format, voice}`; 24 kHz; wav/pcm/flac/mp3/aac/opus | 70 ms at concurrency 1, 331 ms at 16, 552 ms at 32 (500-char input, single H200, card) | **No** (9: en, fr, es, de, it, pt, nl, ar, hi) | **weights CC BY-NC 4.0 (non-commercial)** | >= 16 GB GPU | |
| **XTTS-v2** | `inference_stream` (docs claim < 200 ms latency; "usually slower overall but first chunk sooner") | library; `xtts-streaming-server` demo (port 80 in container; README: "doesn't support concurrent streaming requests, it's a demo server, not meant for production"; needs `COQUI_TOS_AGREED=1`) | < 200 ms (claim) | **Yes** (17 languages incl. `tr`) | maintained fork `idiap/coqui-ai-TTS` code MPL-2.0 (v0.27.5, 2026-01-26); **weights Coqui Public Model License = non-commercial only** ("allows only non-commercial use of a machine learning model and its outputs") | GPU | |
| **F5-TTS** | benchmark only | Triton/TensorRT-LLM, Gradio, Docker | 253 ms avg at concurrency 2 on one L20 (README, client-server) | not listed on README | code MIT; **pretrained models CC-BY-NC** (Emilia data) | GPU | |
| **Fish Audio S2 Pro** | streaming engine (SGLang-based) | no server API described on card | ~100 ms TTFA on an H200, RTF 0.195 (vendor) | "tr" in the "other supported languages" list (card claims 80+) | **Fish Audio Research License: research/non-commercial free, commercial needs a license** | 5B params | |
| **Orpheus-TTS** | streaming (vLLM) | library | "~200 ms streaming latency ... reducible to ~100 ms with input streaming" (README) | **No** (multilingual research release: zh, hi, ko, es+it, fr, de) | repo Apache-2.0; weights license not stated on the pages read (Llama-3b backbone) | GPU | |
| **Qwen3-TTS** (12Hz 1.7B/0.6B) | streaming | `qwen-tts` package, DashScope API; vLLM-Omni offline only (online serving "will be supported later") | "as low as 97 ms" (vendor) | **No** (zh, en, ja, ko, de, fr, ru, pt, es, it) | Apache-2.0 | GPU | |
| **OmniVoice** (k2-fsa) | none documented | `model.generate()` returns 24 kHz arrays | RTF 0.025-0.09 on H100 | "600+ languages" claimed; Turkish not confirmed; voice design trained on zh/en only | Apache-2.0 | GPU | |
| **sherpa-onnx TTS** | non-streaming API shown | library | | **no Turkish model on the pages fetched** (families listed: Matcha zh/en, Kokoro zh/en, KittenTTS en, VITS incl. MeloTTS zh+en, Piper en) | Apache-2.0 | CPU | the full >100 models / 40 languages list was NOT enumerable |
| MeloTTS | | | | zh+en only in the sherpa-onnx listing; repo not fetched | | | UNVERIFIED |
| Meta MMS-TTS `facebook/mms-tts-tur` | VITS 36.3M | transformers | | yes | **CC-BY-NC 4.0 (non-commercial)** | | |
| FreyaTTS (arXiv 2607.09530) | not stated | paper only | RTF 0.11 on RTX 4090 | Turkish-first, 183.2M, WER 8.0 % on their own benchmark | authors state Apache-2.0 weights+code will be released; **no repository link on the abstract page and none found** | | UNVERIFIED availability |
| Moonshine TTS (library) | `stream()`/`push_text()`/`flush()`/`end_input()`/`cancel_stream()` | in-process library using Kokoro, Piper, ZipVoice voices | "well under a second" first audio streaming (docs example) | `tr-tr` listed (20 languages) | MIT code; TTS/G2P models and data follow their own readmes (not MIT by default); the Turkish voice's source/license is UNVERIFIED (likely the Piper dfki voice = non-commercial) | | |

## 4. Turkish summary

| Need | Commercial-safe, verified on paper | Notes |
|---|---|---|
| STT streaming | `atasoglu/seda-v0.1` via sherpa-onnx (Apache-2.0 acoustic model; FLEURS-TR 11.52 WER), NVIDIA Nemotron 3.5 ASR Streaming (OpenMDW-1.1; FLEURS 11.2-12.3 WER) | seda: CPU, lowercase unpunctuated output, read-speech training, phone audio unmeasured. Nemotron: needs NIM (Linux, >= 16 GB nominal, no WSL2) or custom serving. Qwen3-ASR (Apache-2.0) via vLLM realtime: Turkish listed, quality/latency unpublished |
| STT non-streaming (commit-based) | Whisper via Speaches / WhisperLive (MIT) | no interim; latency not published |
| TTS | Chatterbox Multilingual (MIT) via Chatterbox-TTS-Server | quality and TTFA on Turkish unmeasured |
| TTS non-commercial only | Piper `tr_TR-dfki` (CC BY-NC-SA), XTTS-v2 (CPML), MMS-TTS-tur (CC-BY-NC) | fine for maintainer-only dev, not for a product |
| No Turkish | Kokoro, Kyutai STT/TTS, Parakeet v3, Canary v2, Voxtral STT/TTS, Moonshine STT, Qwen3-TTS, Orpheus | |

## 5. Gotchas found

1. **Sample rates differ per server**: OpenAI-realtime 24 kHz, NIM and vLLM 16 kHz, Speaches treats
   incoming base64 as 24 kHz regardless of what you intended (source), sherpa-onnx float32 at the
   model rate (usually 16 kHz) and does not resample, Kyutai 24 kHz float32, Vosk configured per
   connection (default 8000), WhisperLive 16 kHz. TTS: Kokoro/OpenAI pcm 24 kHz, Kyutai 24 kHz,
   Piper dfki 22,050 Hz. Brisvox needs per-adapter resamplers (see `docs/providers/rubato.md`).
2. **Client-driven finalize is not uniform**: OpenAI/NIM/Speaches `commit`; Vosk `{"reset":1}`;
   sherpa-onnx `"Done"` ends the connection (use one connection per utterance, or rely on its
   endpoint rules); Kyutai needs a Marker plus trailing silence; WhisperLive `END_OF_AUDIO` ends the
   session; vLLM mid-session flush unclear.
3. **Speaches Realtime is not a streaming-transcript server**: no deltas, its own server VAD
   (threshold 0.9 default) unless `turn_detection` is disabled (how to disable: UNVERIFIED), and it
   transcribes the committed buffer in one call (source). Its docs say real-time use needs CUDA.
4. **Kokoro-FastAPI is request-scoped**: Brisvox must send one request per LLM sentence/clause and
   cancel by closing the HTTP connection. Voices, not text, decide language: no Turkish.
5. **Nemotron NIM**: WSL2 unsupported, 16 GB nominal minimum, the container needs an NGC key to
   pull; the realtime page lists both 100 and 10,000 as max concurrent connections (page
   inconsistency).
6. **Licenses that block commercial use**: Piper Turkish voice, XTTS-v2, F5-TTS weights, Fish S2,
   Voxtral TTS, MMS-TTS, duxx Turkish Zipformer, Moonshine non-English legacy STT. GPL-3.0: Piper
   engine (run as a separate process only).
7. **Model-card WERs are not telephony WERs**: all Turkish figures are read speech (FLEURS, Common
   Voice); 8 kHz G.711 calls are untested by every source above.
8. **Maintenance**: Kyutai repo quiet since 2026-01; Piper seeking maintainers; Speaches releases
   lag master by months (last tag 2025-12-27 vs master pushed 2026-10-13); Kokoro-FastAPI README
   labels master "development focused".

## 6. Recommendation (Brisvox judgement, built on the facts above)

**Protocols to implement, in order**
1. `OpenAI /v1/audio/speech` streaming-PCM TTS client (24 kHz s16le, chunked; per-sentence
   requests; cancel = drop connection). One adapter covers Kokoro-FastAPI, Speaches, vLLM-Omni
   (Voxtral TTS if licensing allows), Chatterbox-TTS-Server.
2. `OpenAI Realtime transcription` STT client with a dialect table (OpenAI 24 kHz, NVIDIA NIM 16 kHz
   with `?intent=transcription`, Speaches 24 kHz completed-only, vLLM event names). Client VAD ->
   `input_audio_buffer.commit`.
3. A native sherpa-onnx streaming WebSocket STT client (tiny protocol: float32 binary, `"Done"`,
   JSON `{text, segment}`) for CPU-only / Turkish.
4. Later: Kyutai msgpack STT + TTS clients (best incremental text-in TTS, English/French only).

**STT, first two**
- **#1 sherpa-onnx streaming server** (Apache-2.0, true streaming, CPU-friendly, 200 connections,
  built-in endpointing, covers English zipformers and the Turkish seda model). Trade-offs:
  proprietary protocol, accuracy below large GPU models, Turkish model is community-made, read
  speech only, lowercase/no punctuation, python server example only (production hardening and the
  C++ server's protocol UNVERIFIED).
- **#2 OpenAI-realtime-dialect server**: NVIDIA Speech NIM with Nemotron 3.5 for the GPU/production
  Turkish+English path (best published Turkish streaming numbers, 80-1120 ms chunks), and Speaches
  (Whisper) as the easy local/dev path on the maintainers' box. Trade-offs: NIM = NGC key, container
  terms, >= 16 GB VRAM nominal, no WSL2 (so not on the maintainers' Docker-in-WSL2 setup); Speaches = no
  interim transcripts, Blackwell compatibility of its CUDA 12.6 image UNVERIFIED.

**TTS, first two**
- **#1 Kokoro-FastAPI** via `/v1/audio/speech` (English; Apache-2.0; documented cu128 image for the
  RTX 5070; ~300 ms GPU first-chunk claim; CPU workable on fast CPUs only).
- **#2 Chatterbox-TTS-Server** (MIT) for Turkish, multilingual quality unknown, streaming is
  chunk-level, TTFA unmeasured, GPU needed. Use Piper `tr_TR-dfki` only as a non-commercial CPU
  fallback for the maintainers' dev. Kyutai TTS is the best text-incremental fit but English/French
  only.

**Fully local stacks**
- **English**: STT sherpa-onnx streaming zipformer en (CPU) or Kyutai STT 1B en/fr on the GPU
  (0.5 s delay + semantic VAD; VRAM fit UNVERIFIED); TTS Kokoro-FastAPI (`latest-cu128` on the
  5070, CPU image for servers); CPU-only alternative TTS: Pocket TTS (MIT, ~200 ms claim, EN).
- **Turkish**: STT sherpa-onnx + seda-v0.1 (CPU, Apache-2.0; median first word 0.70 s); GPU
  upgrade Nemotron 3.5 ASR via NIM on a Linux server (not WSL2); TTS Chatterbox Multilingual
  (MIT, GPU) with Piper dfki for non-commercial dev. No verified fully-local, commercial-safe,
  CPU-only Turkish TTS exists in this survey.

**Follow-up measurements.** (a) First-audio latency of Kokoro-FastAPI and Chatterbox (Turkish)
on the RTX 5070: done, see `docs/providers/openai-speech.md` ("Live"). (b) seda-v0.1 on
sherpa-onnx's own server: done for 16 kHz speech (`docs/providers/sherpa-onnx.md`, "Live"); 8 kHz
phone audio is still unmeasured. (c) Speaches on Blackwell and (d) a Turkish MOS/WER listening
test: not done.

## Sources (all fetched 2026-10-14)

Speaches: github.com/speaches-ai/speaches; speaches.ai; speaches.ai/usage/realtime-api/;
api.github.com/repos/speaches-ai/speaches (+/releases); raw.githubusercontent.com/speaches-ai/speaches/master/src/speaches/{realtime/input_audio_buffer_event_router.py,realtime/input_audio_buffer.py,realtime/session.py,routers/speech.py,Dockerfile};
github.com/speaches-ai/speaches/blob/master/docs/{installation,usage/speech-to-text,usage/text-to-speech}.md.
whisper.cpp: github.com/ggml-org/whisper.cpp/blob/master/examples/server/README.md; api.github.com/repos/ggml-org/whisper.cpp/releases.
WhisperLive: github.com/collabora/WhisperLive; raw.githubusercontent.com/collabora/WhisperLive/main/whisper_live/server.py; api.github.com/repos/collabora/WhisperLive.
sherpa-onnx: github.com/k2-fsa/sherpa-onnx; api.github.com/repos/k2-fsa/sherpa-onnx/releases;
raw.githubusercontent.com/k2-fsa/sherpa-onnx/master/python-api-examples/streaming_server.py;
k2-fsa.github.io/sherpa/onnx/websocket/index.html; .../pretrained_models/online-transducer/index.html; .../tts/pretrained_models/{index,vits}.html.
Turkish ASR: huggingface.co/atasoglu/seda-v0.1; huggingface.co/duxx/turkish-stt-zipformer; huggingface.co/datasets/issai/Turkish_Speech_Corpus.
Kyutai: github.com/kyutai-labs/delayed-streams-modeling (+ raw README, scripts/stt_from_file_rust_server.py, scripts/tts_rust_server.py); huggingface.co/kyutai/{stt-1b-en_fr,tts-1.6b-en_fr}; api.github.com/repos/kyutai-labs/delayed-streams-modeling; crates.io/api/v1/crates/moshi-server; github.com/kyutai-labs/pocket-tts.
Vosk: github.com/alphacep/vosk-server; raw.githubusercontent.com/alphacep/vosk-server/master/websocket/asr_server.py; alphacephei.com/vosk/server; alphacephei.com/vosk/models.
NVIDIA: huggingface.co/nvidia/{parakeet-tdt-0.6b-v3,canary-1b-v2,nemotron-speech-streaming-en-0.6b,nemotron-3.5-asr-streaming-0.6b};
docs.nvidia.com/nim/speech/latest/reference/api-references/asr/realtime-asr.html; docs.nvidia.com/nim/speech/latest/asr/deploy-asr-models/nemotron-asr-streaming.html; docs.nvidia.com/nim/speech/latest/reference/support-matrix/asr.html; catalog.ngc.nvidia.com/orgs/nim/teams/nvidia/containers/nemotron-asr-streaming; perspectives.nvidia.com/nemotron-speech/task/faq/asr-models-streaming-transcription-real-time-agent-response/; www.together.ai/models/nemotron-3-asr-streaming-0-6b (secondary).
vLLM / Mistral / Qwen: docs.vllm.ai/en/latest/serving/online_serving/speech_to_text/; docs.vllm.ai/en/latest/examples/speech_to_text/realtime/; docs.vllm.ai/en/latest/models/supported_models/; docs.vllm.ai/en/latest/api/vllm/model_executor/models/qwen3_asr_realtime/; vllm.ai/blog/streaming-realtime; huggingface.co/mistralai/{Voxtral-Mini-4B-Realtime-2602,voxtral-4b-tts-2603}; huggingface.co/Qwen/{Qwen3-ASR-1.7B,Qwen3-TTS-12Hz-1.7B-CustomVoice}; github.com/QwenLM/Qwen3-ASR.
Moonshine: github.com/moonshine-ai/moonshine; raw LICENSE; moonshine-voice.readthedocs.io/en/latest/using/text-to-speech/; core/moonshine-tts/data/README.md (raw).
OpenAI: developers.openai.com/api/docs/guides/realtime-transcription; developers.openai.com/api/docs/guides/text-to-speech; github.com/openai/whisper.
TTS: github.com/remsky/Kokoro-FastAPI (+ raw README, api/src/routers/openai_compatible.py, API releases); huggingface.co/hexgrad/Kokoro-82M (+ VOICES.md); github.com/OHF-Voice/piper1-gpl (+ raw docs/API_HTTP.md, docs/VOICES.md, API repo metadata); huggingface.co/rhasspy/piper-voices (tr/tr_TR tree via API; dfki MODEL_CARD); github.com/resemble-ai/chatterbox; huggingface.co/ResembleAI/chatterbox; github.com/devnen/Chatterbox-TTS-Server; github.com/idiap/coqui-ai-TTS (+ docs/source/models/xtts.md, releases API); huggingface.co/coqui/XTTS-v2; tts-hub.github.io/cpml; github.com/coqui-ai/xtts-streaming-server; github.com/SWivid/F5-TTS; huggingface.co/fishaudio (and /s2-pro); github.com/canopyai/Orpheus-TTS; huggingface.co/collections/canopylabs/orpheus-multilingual-research-release-67f5894cd16794db163786ba; github.com/k2-fsa/OmniVoice; huggingface.co/facebook/mms-tts-tur; arxiv.org/abs/2607.09530; nvidia.com RTX 5070 family page.
