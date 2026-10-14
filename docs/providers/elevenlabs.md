# ElevenLabs — TTS (WebSocket: stream-input and multi-stream-input)

Scope: WebSocket streaming, alignment/timestamps for played-text truncation, cancel; contract tests pass.
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that
date (the API-reference pages are rendered AsyncAPI specs and mostly carry field names and
defaults, little prose). Anything not confirmed is marked UNVERIFIED. No live call was made.

- Docs: https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-stream-input and
  https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-multi-stream-input
  (fetched 2026-10-08); full source list at the bottom.
- Auth:
  - Handshake header `xi-api-key: <key>` (both endpoints). Use the header; never put the key in the URL.
  - Alternatives in the spec: query `authorization` (bearer token) and query `single_use_token`
    (no further detail on the page: UNVERIFIED how a single-use token is obtained). Also the first
    message may carry the key instead of the header: single-stream `xi-api-key` and `authorization`
    fields ("only needed if not already provided in the header"); multi-context `xi_api_key` and
    `authorization` fields (apply to that context's (re)initialisation). The realtime-tts guide
    Python example sends `xi_api_key` in the first message (underscore), the API reference for the
    single-stream first message names the field `xi-api-key` (hyphen). Name mismatch in the docs;
    brisvox uses the header only, so it does not matter.
  - Brisvox: server side, so use the header (marked sensitive); never log it (Error variants must
    not contain credentials).
  - Failure shape on bad key over WebSocket: NOT documented (UNVERIFIED whether HTTP 401 before the
    upgrade or a close frame after it). HTTP errors use `authentication_error` /
    `invalid_api_key` / `missing_api_key` (errors page), but the errors page has no WebSocket
    section.
- Endpoint(s) / regions:
  - Single context: `wss://api.elevenlabs.io/v1/text-to-speech/{voice_id}/stream-input?model_id={model_id}`
  - Multi context: `wss://api.elevenlabs.io/v1/text-to-speech/{voice_id}/multi-stream-input?model_id={model_id}`
  - The voice is fixed in the URL path (one voice per connection; `voice_id` path param).
  - Servers listed in both specs: `wss://api.elevenlabs.io/` (default), `wss://api.us.elevenlabs.io/`,
    `wss://api.eu.residency.elevenlabs.io/`, `wss://api.in.residency.elevenlabs.io/`,
    `wss://api.sg.residency.elevenlabs.io/`. Latency guide: models are served from several regions
    (USA, Netherlands, Singapore), global routing is the default on `api.elevenlabs.io`, check the
    `x-region` response header, and `api.us.elevenlabs.io` pins US. Latency guide also says "Enterprise
    customers can use dedicated EU and India data residency environments", so the `*.residency.*` hosts
    presumably need an Enterprise plan (the specs do not say; the Singapore residency host is not
    mentioned in the guide: UNVERIFIED).
  - Query parameters (identical set on both endpoints; defaults from the spec). The specs give NO
    description text for most of them:

    | Param | Type / default | Notes |
    |---|---|---|
    | `model_id` | string, default `eleven_multilingual_v2` | Set it explicitly (see Models) |
    | `output_format` | spec type "Any type", no default, no enum on the WS pages | Values below come from the HTTP convert endpoint page; assumed identical for WS: UNVERIFIED. Per the stream-input API reference the default audio is MP3 (`AudioOutput.audio` description: "encoded using the selected output_format, by default this" is MP3; the realtime-tts guide itself states no default, its examples just save `.mp3`), so ALWAYS set it |
    | `auto_mode` | boolean, default `false` | No description in the spec. Latency guide: "Setting `auto_mode` to true automatically handles generation triggers, removing the need to manually manage chunk strategies" so you do not manage chunk strategies yourself. Audio-streaming concept page: "`auto_mode` attempts to find a good balance automatically by analysing the incoming text". Exact algorithm and whether it overrides `chunk_length_schedule`: UNVERIFIED |
    | `inactivity_timeout` | integer, default `20`, max `180` (seconds) | Blog 2024-09-24: "The default remains at 20 seconds", "a new maximum of 180 seconds" |
    | `sync_alignment` | boolean, default `false` | No description. TTS-vs-TTD page: "Optional `sync_alignment` (TTS field naming in API reference)". Effect MEASURED live 2026-10-13: it changes the alignment time base. Off: every message's times start at 0 for that message's own audio. On: the times run from the start of the server context (message 2 starts where message 1's alignment ended, about 836 ms, not at 0) |
    | `apply_text_normalization` | spec type "Any type", no default on WS pages | HTTP convert page: `auto` (default) / `on` / `off`; "when set to 'auto', the system will automatically decide whether to apply text normalization (e.g., spelling out numbers)". Same values on WS assumed: UNVERIFIED. Latency/plan implications of `on`: not found, UNVERIFIED |
    | `language_code` | string | No description on WS pages. HTTP convert page: optional ISO 639-1 code; ignored if the model does not support the code; "This parameter is not supported for multilingual_v2 models". Behaviour on WS: UNVERIFIED |
    | `enable_logging` | boolean, default `true` | No description (UNVERIFIED that `false` = zero retention; plan restrictions unknown) |
    | `enable_ssml_parsing` | boolean, default `false` | No description in the WS spec. Guide: needed for phoneme-based pronunciation dictionaries (UNVERIFIED, not re-confirmed on 2026-10-08) |
    | `seed` | integer 0..4294967295 | Determinism not described |
    | `authorization`, `single_use_token` | string | See Auth |

  - `output_format` allowed values (HTTP convert page, default there `mp3_44100_128`): `alaw_8000`,
    `mp3_22050_32`, `mp3_24000_48`, `mp3_44100_128`, `mp3_44100_192`, `mp3_44100_32`, `mp3_44100_64`,
    `mp3_44100_96`, `opus_48000_128`, `opus_48000_192`, `opus_48000_32`, `opus_48000_64`, `opus_48000_96`,
    `pcm_16000`, `pcm_22050`, `pcm_24000`, `pcm_32000`, `pcm_44100`, `pcm_48000`, `pcm_8000`, `ulaw_8000`,
    `wav_*` (8000/16000/22050/24000/32000/44100/48000). Tiers: `mp3_44100_192` needs Creator or above;
    `pcm_44100` and `wav_44100` need Pro or above. The page calls `ulaw_8000` "commonly used for
    Twilio audio inputs". Brisvox TtsConfig = PCM16 LE mono at 8k/16k/24k -> `pcm_16000`, `pcm_24000`
    (and `pcm_8000`, `ulaw_8000` for telephony). The docs say only "PCM" (not "s16le", not
    endianness): that `pcm_*` is signed 16-bit little-endian mono raw samples, with no WAV header,
    is UNVERIFIED and must be checked in the live test. `ulaw_8000` bytes are G.711, not PCM16.
  - Example URL (brisvox target):
    `wss://api.elevenlabs.io/v1/text-to-speech/{voice_id}/multi-stream-input?model_id=eleven_flash_v2_5&output_format=pcm_24000&inactivity_timeout=180&sync_alignment=false`
    (parameter combination assembled from the documented names; not run).
- Models (models overview page, fetched 2026-10-08):
  - Low-latency, usable on the TTS WebSocket: `eleven_flash_v2_5` (~75 ms inference latency, 32
    languages, 40,000 characters/request, recommended by the realtime-tts guide as the low-latency model)
    and `eleven_flash_v2` (~75 ms, English only, 30,000 chars). `eleven_multilingual_v2` (29 languages,
    10,000 chars, higher latency and cost than Flash). `eleven_turbo_v2_5` / `eleven_turbo_v2` are
    marked deprecated ("functionally equivalent" to the Flash models, "We recommend using the Flash
    models over Turbo models in all use cases").
  - The TTS WebSocket does NOT support `eleven_v3` or `eleven_v4` (realtime-tts guide: "That endpoint
    does not support the `eleven_v3` or `eleven_v4` models"; TTS-vs-TTD page:
    "Flash, Multilingual v2, and other supported TTS models. No eleven_v3 or eleven_v4 on this
    endpoint"). The multi-context guide does not address `eleven_v3` at all (its examples use
    `eleven_flash_v2_5`); the exclusion for multi-stream-input is only implied by the two pages above
    (UNVERIFIED for multi-stream-input specifically).
  - Newer real-time models exist: `eleven_v4_turbo` (~100 ms median inference latency, 90+ languages
    incl. Turkish, ONE registered voice) and `eleven_v3_conversational` (~280 ms, 70+ languages), but
    they are served via the separate Text to Dialogue (TTD) WebSocket
    (https://elevenlabs.io/docs/eleven-api/guides/how-to/websockets/realtime-tdd.md). Per that page:
    endpoint `wss://api.elevenlabs.io/v1/text-to-dialogue/stream-input`, `model_id` must start with
    `eleven_v3` or `eleven_v4`, responses use snake_case (e.g. `is_final`), the first message carries a
    `voices` array, `eleven_v4_turbo` allows exactly one registered voice and `eleven_v4` up to 10. It has
    its own multi-context API-reference page `.../api-reference/text-to-dialogue/ttd-multi-websocket`
    (page exists). The rest of the TTD protocol was NOT researched here: UNVERIFIED. Whether
    `eleven_v3_conversational` is accepted there: the TTD guide does not name it (models page says it comes
    with the TTD WebSocket): UNVERIFIED. Possible follow-up if Flash quality is insufficient. Whether
    `eleven_v4_turbo` also works on the plain TTS stream-input endpoint: not stated, treat as NO.
  - Recommended for brisvox: `model_id = eleven_flash_v2_5` (configurable). Latency guide (TTFB with
    Flash + WebSockets, excluding the ~75 ms nuance noted there that it is inference only): North
    America 100-150 ms, Europe 100-150 ms, South East Asia 100-150 ms, South Asia 150-200 ms,
    North East Asia 150-200 ms. Turkey is not listed as a region: UNVERIFIED (expect the Europe figure).
- Protocol (messages in/out, with field names):
  - Naming warning: client messages use snake_case (`context_id`, `close_context`, `close_socket`,
    `voice_settings`, `generation_config`), server messages use camelCase (`contextId`, `isFinal`,
    `normalizedAlignment`, `charStartTimesMs`, `charDurationsMs`). The multi-context guide example code
    reads a key it calls `is_final` (Python `data.get("is_final")`, JS `data.is_final`); the API reference
    schema says `isFinal`, and the single-stream guide's Python example uses `data.get('isFinal')`. Parse both
    (UNVERIFIED which one is on the wire for multi-context; the AsyncAPI schema `isFinal` is treated as
    authoritative).
  - Single-context endpoint (`stream-input`), client -> server (spec `V1TextToSpeechVoiceIdStreamInputPublish`):
    1. InitializeConnection (first message): required `text` = `" "` (single space). Optional
       `voice_settings`, `generation_config`, `pronunciation_dictionary_locators`, and (first message
       only) `xi-api-key`, `authorization`. Guide example (verbatim):
       ```json
       {
         "text": " ",
         "voice_settings": {"stability": 0.5, "similarity_boost": 0.8, "use_speaker_boost": false},
         "generation_config": {"chunk_length_schedule": [120, 160, 250, 290]},
         "xi_api_key": "your_elevenlabs_api_key_here"
       }
       ```
    2. SendText: required `text` ("Should always end with a single space string"); optional
       `try_trigger_generation` (bool, default false: "attempt to immediately trigger the generation of
       audio, overriding the `chunk_length_schedule`"; only generates above a minimum buffer threshold;
       docs recommend the default), `voice_settings`, `generator_config` (note the different name;
       "must be omitted or unchanged" after initialisation), `flush` (bool, default false: "Flush
       forces the generation of audio. Set this value to true when you have finished sending text,
       but want to keep the websocket connection open.").
       Guide examples: `{"text": "Your text to synthesize."}` (guide code: `JSON.stringify({ text: text })`) and
       `{"text": "Generate this audio immediately.", "flush": true}`.
    3. CloseConnection: `{"text": ""}` ("End the stream with an empty string"; guide: "Send an empty string
       to close the WebSocket connection after sending the last text message"). Buffered text is
       generated before the stream ends (guide: closing the socket also forces generation of buffered text).
  - `voice_settings` (RealtimeVoiceSettings): `stability` 0.5, `similarity_boost` 0.75, `style` 0
    (V2+ models only), `use_speaker_boost` true (V2+ only), `speed` 1 (range 0.7..1.2). The docs
    list defaults 0.5/0.75; the guide example uses 0.8 for similarity_boost.
  - `generation_config.chunk_length_schedule`: array of numbers, default `[120, 160, 250, 290]`, "Each
    item should be in the range 50-500". Text is buffered; generation starts when the buffer reaches
    the first threshold (120 characters), later chunks follow the schedule; lower values reduce
    latency but can reduce quality. Latency guide: without `auto_mode`, the model waits for enough
    text to match the chunk schedule before starting to generate audio and "stalls" if text arrives
    slowly (guide example: "if you set a chunk schedule of 125 characters but only 50 arrive, the model
    stalls until additional characters come in").
  - Multi-context endpoint (`multi-stream-input`), client -> server (all identify the context by
    `context_id`; fields from the spec, example JSON from the multi-context guide):
    | Message | Fields | Example (guide) |
    |---|---|---|
    | Initialize connection (`InitializeConnectionMulti`, first message) | `text` = `" "` (required), `context_id` (optional; "unique identifier for the first context created in the websocket"; if omitted a default context is used), `voice_settings`, `generation_config`, `pronunciation_dictionary_locators`, `xi_api_key`, `authorization` | not shown |
    | Initialise context (`InitialiseContext`) | `text` (required, should end with a single space), `context_id` (spec: "If omitted, a default context ID may be assigned by the server"), `voice_settings`, `generation_config`, `pronunciation_dictionary_locators`, `xi_api_key`, `authorization`. "Creates a context with this ID, or re-initializes an existing one with new settings and text" (UNVERIFIED wording, not re-found in the 2026-10-08 re-fetch of the spec) | `{"text": "Hello! I'm your virtual assistant.", "context_id": "greeting", "voice_settings": {...}}` (guide code comment: "Only include voice_settings for the first message in a context") |
    | Send text (`SendTextMulti`) | `text` (required), `context_id`, `flush` (default false; true flushes that context's buffer, false appends to the buffer) | `{"text": "More text...", "context_id": "greeting"}` |
    | Flush context (`FlushContextClient`) | `context_id` (required), `flush` (required), optional `text` appended to the buffer before flushing | `{"context_id": "greeting", "flush": true}` (guide: "use the `flush: true` flag at the end of complete sentences") |
    | Close context (`CloseContextClient`) | `context_id` (required), `close_context` (required; "Must set the close_context to true, to close the specified context"; if false the context stays open and text is ignored; if the context was set to flush, the flush finishes first; the same ID can be reused but is not linked to the earlier context) | `{"context_id": "greeting", "close_context": true}` |
    | Close socket (`CloseSocketClient`) | `close_socket` (bool): closes all contexts and the entire WebSocket connection; "Any context that was previously set to flush will wait to flush before closing" | `{"close_socket": true}` |
    | Keep alive (`KeepContextAlive`) | `text` = `""` (required) and `context_id` (required): spec "This text is ignored by the server but its presence resets the inactivity timeout for the specified context" (guide: "send an empty text message to reset the timeout clock") | `{"context_id": "greeting", "text": ""}` |
    There is NO message literally named `initialiseContext` as a JSON field: `InitialiseContext` is
    the spec's schema name for "text with a context_id (new or re-init)". The first message with a
    new `context_id` creates the context (guide); no separate create message is needed.
  - Server -> client, single context (`stream-input`):
    - `AudioOutput`: `audio` (string, required, base64 partial audio in the selected `output_format`),
      `alignment` (optional), `normalizedAlignment` (optional).
    - `FinalOutput`: `isFinal: true` (always true; "Indicates if the generation is complete. If set to
      `True`, `audio` will be null.").
  - Server -> client, multi-context: `AudioOutputMulti`: `audio` (required, "Base64 encoded audio chunk"),
    `contextId`, `alignment` (nullable), `normalizedAlignment` (nullable); `FinalOutputMulti`: `isFinal: true` and
    `contextId` ("Indicates this is the final message for the context").
    No full wire examples of server messages exist in the docs. Shape per the schema (values illustrative,
    assembled from field names, NOT copied from the docs):
    ```json
    {"audio":"<base64>","alignment":{"chars":["H","i"," "],"charStartTimesMs":[0,60,120],"charDurationsMs":[60,60,40]},"normalizedAlignment":{...},"contextId":"greeting"}
    {"isFinal":true,"contextId":"greeting"}
    ```
    Whether `audio` may be the empty string or null on alignment-only messages, whether
    `alignment` is `null` or absent when not produced, and whether a message can carry both `audio`
    and `isFinal`: UNVERIFIED (spec says `audio` is null on final, single-stream only).
  - Ordering / termination: not specified beyond "isFinal = generation complete" (single) and "final
    message for the context" (multi). After a flush the docs do NOT say whether `isFinal` is sent
    (UNVERIFIED; the multi-context guide explicitly does not state whether a final message follows a flush):
    the single-stream guide treats `isFinal` as the end of the whole stream (Python
    example stops listening on it), and `flush: true` keeps the socket open. For multi-context the
    signal that all audio of a flushed segment has arrived is therefore UNVERIFIED; there is no
    `flush_done`-like message in the spec. Brisvox plan: end a bot turn with `close_context` (or flush
    + close_context) and take `isFinal` for that `contextId` as `TtsEvent::Flushed` (to be confirmed
    live; see open items).
  - Message size: the docs examples set a 16 MB max message size on the client; reason not explained
    (UNVERIFIED, not re-confirmed on 2026-10-08).
- Keepalive / timeouts:
  - Default inactivity timeout 20 s (single stream: "The default value is 20 seconds", max "180 seconds",
    help center; multi guide: "Contexts by default timeout after 20 seconds and are closed automatically"
    and "The inactivity timeout is a websocket level parameter that applies to all contexts and can be up
    to 180 seconds if needed"). Set `inactivity_timeout=180` in the query to maximise it (blog,
    help-center article, and both specs agree on default 20/max 180).
  - Single stream keepalive: send `{"text": " "}` (a single space) to reset the timer (help center: repeat
    every 20 s for longer sessions). A fully empty string `""` is an end-of-sequence message and closes the
    socket (help-center: "will send an End of Sequence (EOS) message").
  - Multi-context keepalive: `{"context_id": "...", "text": ""}` (empty text WITH a context_id is
    ignored and resets that context's timer). Note the opposite semantics of `""` between the two
    endpoints; do not reuse code across them. Whether a message for context A resets the timer of
    context B or the socket: UNVERIFIED. Whether a WebSocket protocol ping frame resets any timer:
    UNVERIFIED (the blog does not mention keepalive/ping at all).
  - What the server does on timeout (close code / error message / whether the socket closes with a
    final message): NOT documented (UNVERIFIED). A third-party (Vapi) log is reported as "ElevenLabs
    input timeout exceeded" (not re-checked), not an ElevenLabs doc: do not rely on the wording.
  - Brisvox: one connection per session; send a keepalive for the active context while text is
    owed but delayed (LLM/tool pause) or use `inactivity_timeout=180`; reconnect lazily if the
    socket was closed. No documented connect timeout; brisvox applies `io_timeout`.
- Finalize / flush / cancel semantics:
  - Flush: `flush: true` (with text, or with only `context_id` in multi-context) forces generation of
    whatever is buffered without closing the context/socket ("generation of any buffered audio in the
    context", multi guide). Closing the stream (single) also forces generation of the buffer.
  - Brisvox `TtsInput::Flush` ("no more text for this utterance, synthesize rest, report Flushed")
    maps to: send the final text with `flush: true`, then `close_context` for that context; emit
    `Flushed` on `isFinal` for that `contextId`. UNVERIFIED that `isFinal` is emitted after a flush
    or close_context and after ALL audio of the context; confirm live.
  - Cancel (barge-in): docs (multi-context guide): "When a user interrupts your agent, you should close
    the current context and create a new one" (example sends `{"context_id": old_context_id,
    "close_context": true}`, then starts a new context with its own `context_id`).
    That is the only documented cancel. The docs do NOT say whether (a) audio already generated and in
    flight stops, (b) buffered-but-not-generated text is discarded or generated (the spec says a context
    set to flush finishes flushing before closing, which implies a flushed context may keep producing audio
    after `close_context`; whether audio continues to arrive after `close_context` is sent is not
    covered), (c) an `isFinal` follows. CRITICAL for barge-in: do not rely on the
    server. Brisvox must (1) send `close_context` for the cancelled context, WITHOUT a preceding
    flush, (2) drop every later message for that `contextId` client-side immediately (turn-epoch /
    ContextId filter), (3) emit `TtsEvent::Cancelled` itself, (4) never reuse the id (docs allow
    reuse but "not linked", so a late message of the old context could be mistaken for the new one;
    use a fresh id per utterance, e.g. `format!("{session}-{n}")`).
  - Single-stream endpoint has no cancel: the only way to stop is to close the socket (then reconnect,
    which costs a handshake). This is why multi-stream-input is the right choice for barge-in.
  - Text normalization/continuity: within a context, prosody continuity is a stated purpose of
    contexts ("Maintain prosodic consistency for utterances within the same logical context", guide);
    how much previous-text conditioning exists across `flush` boundaries is not documented. UNVERIFIED.
- Timestamps / interim results / tool-call deltas:
  - Alignment objects on audio messages (both endpoints): `alignment` (input text) and
    `normalizedAlignment` (normalized text, e.g. numbers spelled out). Each has three parallel arrays of
    equal length: `chars` (array of string; "may include spaces and punctuation"), `charStartTimesMs`
    (array of integer, start of each char in ms), `charDurationsMs` (array of integer, duration in ms).
    There is NO word-level alignment in the schema: words must be derived by grouping `chars` between spaces
    (the realtime-tts guide says "You can use `alignment` to get the word-level timestamps for each word in
    the text", which contradicts the spec's char-level arrays; the spec is treated as authoritative:
    UNVERIFIED which is right).
  - Time base: the spec says "Times are relative to the returned chunk, not the full audio." So the
    offset of a message's chars = the audio already delivered for that context before that message.
    Brisvox should compute that offset from its own PCM byte count for the context (exact), not from the
    alignment itself. Live 2026-10-13 (eleven_flash_v2_5, `pcm_24000`, "Hello there, this is a short
    test. It has two chunks."): a message's alignment arrives in the SAME message as its audio (3 audio
    messages with alignment, then an empty `isFinal`), and the statement above holds only with
    `sync_alignment=false` (the adapter's default, the server default). With `sync_alignment=true` the
    times are NOT chunk-relative: they run from the start of the server context. Recorded message
    start times (ms) with audio of 997 / 685 / 1277 ms: [0 .. 789], [836 .. 1579], [1718 .. 2798];
    with `false`: [0 .. 824], [0 .. 731], [0 .. 1022] for audio of 997 / 748 / 1153 ms. Adding the
    audio delivered so far to the cumulative times counted it twice: the last word landed at 4.0-4.3 s
    in 2.9 s of audio (the failure seen in the live run). Audio length matched the format: 2.90-2.96 s from
    the `pcm_24000` byte count (2 bytes/sample at 24 kHz) against a last character start of 2.8 s, so
    the `pcm_*` bytes are 16-bit mono at the requested rate. The adapter now uses the audio before
    the message as the origin when off and the audio before the server context when on (
    `Live::alignment_origin`; fixtures `SYNC_RECORDED` and `CHUNK_RECORDED` in
    crates/brisvox-providers/tests/elevenlabs.rs). `normalizedAlignment` had one more character than
    `alignment` in the first and third message (22 vs 21, 20 vs 19, an extra entry right after the
    first character) and the same times otherwise. Whether the sync clock restarts for a new server
    context after an unsolicited `isFinal` is not measured (assumed, as the id is a new context).
  - Which text is spoken: use `normalizedAlignment` when present for timing of what was actually
    spoken, but played-text truncation matches against the LLM text, which corresponds to `alignment` (input text).
    Choose `alignment` for matching; handle `normalizedAlignment` being the only one present.
    Whether chars of `alignment` can map 1:1 onto the original text (leading spaces, SSML,
    trailing-space convention): UNVERIFIED.
  - No interim results or tool-call deltas apply (TTS).
- Concurrency / rate limits:
  - Plan concurrency (help-center TTS page and the rate-limiting blog agree), per model family:

    | Plan | Flash and Turbo models | All other models (e.g. Multilingual v2) |
    |---|---|---|
    | Free | 4 | 2 |
    | Starter | 6 | 3 |
    | Creator | 10 | 5 |
    | Pro | 20 | 10 |
    | Scale | 30 | 15 |
    | Business | 30 | 15 |
    | Enterprise | custom (help center: Enterprise department can offer a custom plan; blog: "Elevated") | |

    Flash and Multilingual draw from separate budgets (blog). The blog's queue weights are Free 3,
    Starter 4, Creator 5, Pro 5, Scale 5, Business 5, Enterprise 6.
  - WebSocket counting: "With the Text to Speech WebSocket, only the time where our model is generating
    audio counts towards your concurrency limit" (models page; blog: "Over a WebSocket, only the time
    the model is actively generating audio counts"; HTTP counts for the full request). Whether
    several contexts generating at once on one connection count as several in-flight requests, and
    whether a closed context that keeps generating still counts: UNVERIFIED.
  - Over the limit: requests are queued by plan priority (blog: queue "typically adds around 50ms of
    latency"), and "if the system is still over capacity after queueing, you receive an HTTP 429". Errors
    page: 429 with `code` `rate_limit_exceeded` or `concurrent_limit_exceeded`; `system_busy`
    (type `rate_limit_error`, 429): "The system is currently busy. Try again later." Response headers
    `current-concurrent-requests` and `maximum-concurrent-requests` (HTTP responses; whether present
    on the WS upgrade response or WS messages: UNVERIFIED). How a concurrency refusal is reported on
    an already-open WebSocket (handshake 429, error message, close frame): NOT documented (UNVERIFIED).
    Third-party summaries mention `too_many_concurrent_requests` (not re-checked); the official errors
    page uses `concurrent_limit_exceeded`: use the official name, handle unknown codes.
  - Contexts per connection: 5 concurrent contexts ("Each connection is limited to 5 concurrent
    contexts to reflect this", multi-context guide, which calls the API "optimized for voice applications";
    also "The server can maintain up to 5 concurrent contexts per connection, but you should close contexts
    when they are no longer needed"); the guide also says "Establish one WebSocket connection for each
    end-user session" (an earlier claim here that it forbids using the connection for unrelated parallel
    audio streams was not found on re-fetch: UNVERIFIED). The error text when exceeding 5 is not in the
    docs (a third-party community post quotes "Maximum simultaneous contexts": UNVERIFIED). Brisvox design:
    one connection per call/session, one active context at a time plus the just-closed one draining, so at
    most 2-3 live; always `close_context` finished contexts.
  - Request size limits (models page): Flash v2.5 40,000 chars, Multilingual v2 10,000.
    Per-WebSocket-message / total-per-connection limits: not found (UNVERIFIED).
  - Pricing / credits: not researched here (credits per character differ by model; Flash has "a lower API
    price per character", UNVERIFIED wording). `insufficient_credits` (HTTP 402, type `payment_required`)
    exists on the errors page; WS shape UNVERIFIED.
- Errors:
  - HTTP error body (errors page): JSON object with `detail` containing `type`, `code`, `message`,
    `status` (legacy, no longer used), `request_id`, `param`; types/codes seen: `authentication_error`
    type (codes `invalid_api_key`, `missing_api_key`, 401), `rate_limit_exceeded`, `concurrent_limit_exceeded`,
    `system_busy` (429), `payment_required`/`insufficient_credits` (402), `internal_error` (500, "An
    unexpected server error occurred").
  - WebSocket error messages and close codes: NOT documented anywhere fetched (the errors page does not
    mention WebSockets; the specs list no error messages or close codes). UNVERIFIED, including: whether the
    server sends a JSON error object (and with which fields) before closing, which close codes it uses for
    auth failure, invalid voice/model/format, timeouts, and context limit. Adapter plan: treat any message
    that is not `audio`/`isFinal` and contains an `error`/`message`/`detail` key leniently as an error (log
    shape), map handshake HTTP failures to `Rejected { status }`, unexpected close while output is owed to
    `Disconnected`, and record the real shapes from the live test back into this file.
- Turkish support: YES on `eleven_flash_v2_5` and `eleven_multilingual_v2` (models page: multilingual v2
  language list includes `tr`, and the models table marks Turkish for both; Flash v2.5 = "All
  `eleven_multilingual_v2` languages plus: `hu`, `no`, `vi`" = 32 languages). `eleven_flash_v2` is English
  only (`en`), so it is unusable for Turkish. `eleven_v4_turbo` and `eleven_v3_conversational` also list
  Turkish but are TTD-only here.
  Language is inferred from the text ("The language is determined by the text, while the accent and
  pronunciation are determined by the voice itself", help center); `language_code` can force it on models
  that support it (ignored otherwise; not supported for multilingual_v2 per the HTTP page). Whether
  `language_code=tr` is honoured on the WS with Flash v2.5, whether alignment is produced for
  Turkish, Turkish text-normalization quality (numbers, dates, currency) and a good Turkish voice id:
  UNVERIFIED (voices not researched; use `apply_text_normalization` experiments live).
- Chunking / buffering guidance for continuations (derived from the docs; the recommendation itself
  is a design choice, not a doc claim):
  - Default behaviour waits for 120 characters before first audio, which is bad for first-audio
    latency. Options: `auto_mode=true`; a small `chunk_length_schedule` (docs example
    `[50, 120, 160, 290]`, items 50..500); `try_trigger_generation`; or flush at sentence ends.
    Recommended design (to measure live): send LLM text aggregated by the engine's segmenter (phrase
    or sentence, each ending with a single space) with `flush: true` on each segment so audio is
    generated promptly (realtime-tts guide: use `flush: true` "along with the text at the end of
    conversation turn" and `flush: true` "to clear out the buffer and force generate any buffered text";
    the "so audio is generated promptly" rationale is the author's, not a quote; multi-context guide:
    flush at the end of complete sentences). Lower thresholds "may reduce audio quality".
  - Flushing mid-turn is allowed and keeps the context open (docs). Effect of many small flushes
    on prosody: not documented; UNVERIFIED.
  - The send-text rule "should always end with a single space" applies to every text chunk; the
    guide's examples omit it. Add it in the adapter (single space, don't double).
  - Reuse one WS across turns (`inactivity_timeout=180`) to save connect time.
- Mapping to the brisvox trait (crates/brisvox-providers/src/tts.rs) - proposal:
  - Use `multi-stream-input`, one socket per `connect` (one session = one call). `TtsConfig.voice`
    -> `{voice_id}` path segment (voice is fixed per socket); `TtsConfig.sample_rate` -> `output_format`
    (`pcm_8000`/`pcm_16000`/`pcm_24000`; other rates only through resampling or `pcm_22050`/
    `pcm_44100` etc., 44.1 kHz needs Pro).
  - `ContextId(u64)` -> `context_id` string; maintain a reverse map; drop messages whose `contextId`
    is unknown/cancelled/closed. First message of the socket: `{"text":" "}` (+ `voice_settings`,
    `generation_config`) per the spec; whether that initialises a default context that then counts
    toward the 5-context limit: UNVERIFIED. (A plain `InitialiseContext` message with a real
    `context_id` per utterance may avoid this.)
  - `TtsInput::Text` -> `{"text": "<text> ", "context_id": id}` (first one with `voice_settings`),
    optionally `flush: true` per segment. `TtsInput::Flush` -> `{"context_id": id, "flush": true}` then
    `{"context_id": id, "close_context": true}`; `Flushed` on `isFinal` for that id (UNVERIFIED).
  - `audio` base64-decode -> `TtsEvent::Audio` (hot path: reuse buffers; decode where allocation is
    acceptable; the format is raw PCM only if requested; verify no WAV header).
  - Alignment chars -> words -> `TtsEvent::Word` with start = chunk byte offset + `charStartTimesMs`
    of the word's first char (time base above; word grouping on spaces; handle `null` alignment and
    empty arrays).
  - `cancel` channel -> send `close_context`, forget the context locally, queue `Cancelled` first;
    must be polled with priority over `input` (as per the trait docs).
- Official SDKs / licenses: ElevenLabs Python SDK `elevenlabs/elevenlabs-python` is MIT licensed; its
  GitHub page shows HTTP-stream TTS (`text_to_speech.stream()`) and a "Speech Engine" WebSocket SERVER
  feature, but nothing on the TTS stream-input / multi-context client WebSocket (no release version on
  the page: UNVERIFIED). No official Rust SDK was found. Community crates (not ElevenLabs): `elevenlabs-sdk`
  0.1.0 (docs.rs; Apache-2.0; repo `longcipher/elevenlabs-sdk-rs`, crates.io owner Akagi201; describes
  "WebSocket streaming" incl. TTS input-streaming) and `elevenlabs_rs` 0.7.1 (released 2026-07-27, repo
  `rwxbytes/elevenlabs_rs`, "An unofficial Rust crate for ElevenLabs", optional `ws` feature whose
  functionality the docs.rs page does not explain, MSRV 1.85, license not shown on the docs.rs page:
  UNVERIFIED). Not evaluated further (quality not checked). Brisvox should use tokio-tungstenite
  directly, as for Cartesia.
- Gotchas found during implementation (`crates/brisvox-providers/src/elevenlabs`, tested
  against a local mock only; nothing here was seen on the real service):
  - There is no per-flush completion message, so the adapter infers what the server still owes
    by comparing the non-whitespace characters sent with those covered by `alignment` (the plain
    one once seen; `normalizedAlignment` only while no plain one was seen, because the spelled-out
    text can be longer than the input and would otherwise end the wait early and split words).
    If the real alignment turns out not to cover the characters sent (UNVERIFIED: normalization,
    SSML, trimmed spaces), a server that went quiet after responding is tolerated when at most
    4 characters (or a quarter of the text) are missing: coverage is then dropped for that
    context. A larger shortfall is a `Timeout` after `io_timeout`.
    `ElevenLabsConfig::completion_by_alignment = false` times only the first response after
    each text instead. Confirm live (open items 1 and 2).
  - `Flushed` is delivered on `isFinal` for the context after `flush` + `close_context`. If no
    `isFinal` follows (UNVERIFIED), `final_grace` (default 1 s) after the last message ends the
    context once all text is aligned (5 s when nothing was ever aligned: with no alignment the
    first audio chunk is all there is to go by, and a gap longer than the grace between chunks
    would otherwise yield `Flushed` early and drop the rest of the audio).
  - Every text frame carries `flush: true` (and ends in a space); no `try_trigger_generation`, no
    `auto_mode`. `chunk_length_schedule` is only sent if configured. Effect of many small flushes
    on prosody: open item 7.
  - A server context starts with `{"text":" ","context_id":..}` (carrying voice settings and
    `chunk_length_schedule`), not with a context-less first message, so no default context is
    created (open item 6 stays relevant for the first message of the socket).
  - The voice is in the URL, so `warm()` needs `ElevenLabsConfig::warm_target = (voice, rate)`;
    without it `warm()` does nothing.
  - Alignment times are taken relative to the audio delivered before the message. An alignment in
    its own message is anchored at the audio delivered so far (either order of audio and
    alignment is handled; the real service sends them in one message, measured 2026-10-13). With
    `sync_alignment=true` the origin is the audio delivered before the server context opened.
    Words may span messages.
  - Unsolicited `isFinal` while nothing is owed closes the server context (the adapter also sends
    `close_context` for it, so it cannot count against the 5-context limit; how a close for an
    already closed context is answered is unverified, an error carrying that old id is dropped)
    and the engine context gets a new server context on its next text. While text is still owed
    it is ignored (HIGH RISK, open item 2): if the server does send `isFinal` per flushed piece,
    ignoring is right; if it really means "context closed by the server", the unspoken text is
    lost and the stall deadline ends the session with a `Timeout` (no text re-send, unlike the
    Cartesia adapter).
  - Text with no letter or digit (blank, `...`, `-`, emoji only) is not sent and not counted as
    owed output: if the server answers such a chunk with no audio and no alignment (UNVERIFIED),
    counting it would arm the stall deadline and end the session with a `Timeout`. A mixed chunk
    (`Hi 😀`) is sent whole; the emoji is then an alignment shortfall, which is tolerated (see
    above).
  - After the `Flushed` of an engine context, later text and flushes for the same id are
    ignored (the last 64 such ids are remembered, as for cancels; do not reuse ids). Before this
    was remembered, the outcome depended on timing: a late text was dropped if the server's
    `isFinal` had not arrived yet, but spoken in a new server context with restarted word
    offsets if it had. A flush for an id nothing was sent for is answered with `Flushed` once.
  - Server message bounds: a message over 1 MiB (the WebSocket frame limit; real chunks are a few
    KiB to a few tens of KiB) or an `alignment` / `normalizedAlignment` with more than 16384
    entries ends the session with `Protocol`. Without the caps one message could queue
    millions of `Word` events (and reserve a 12 MiB decode buffer) for every call in the
    container. The alignment characters are parsed as borrowed strings (one `Vec` per array, no
    `String` per character) and a message is parsed once, not once for routing and once more in
    full.
  - When all 4 engine context slots are in use, the one evicted for a new context is, in this
    order: one without an open server context, the oldest the engine has not flushed, and only
    if all await their `isFinal` the oldest of those, which then gets its `Flushed` at once
    (its remaining audio is lost).
  - The provider contract suite's "idle gap after the first event" check needs a frame that is
    one event, so its harness mock sends the alignment of a word as its own message ahead of the
    audio (`AlignMode::AlignmentFirst`); with audio and alignment in one message a single frame
    is two events (Audio, Word). The adapter's own protocol tests use the real shape (audio and
    alignment in one message) for completion, cancel, shutdown and eviction; see docs/TESTING.md.

## Open items to settle with a live test
1. Time base and sync of `alignment`: SETTLED 2026-10-13 (per message when `sync_alignment=false`,
   from the server context start when `true`; alignment and audio arrive in the same message; see
   the Time base entry). Still open: whether alignment arrives for Turkish; `alignment` vs
   `normalizedAlignment` presence when `apply_text_normalization` is `auto`/`on`/`off`; the sync
   clock after a server context restart.
2. `isFinal` semantics (HIGH RISK: an unsolicited one while text is owed is ignored, see the
   gotchas): after `flush`, after `close_context`, after `close_socket`; is it per context;
   key spelling (`isFinal` vs `is_final`); is audio still sent after `close_context` (and how much);
   message order of final audio vs `isFinal`.
3. Behaviour of `try_trigger_generation`, `auto_mode`, and `chunk_length_schedule` in multi-context;
   time to first audio for `eleven_flash_v2_5` from Turkey/Europe with `pcm_16000`/`pcm_24000`/`ulaw_8000`
   and with `auto_mode` on/off.
4. Exact `output_format` support on the WS (`pcm_*` encoding, header, `ulaw_8000`), default when
   omitted.
5. WebSocket error payloads and close codes: bad key, bad voice, bad model, bad output_format,
   inactivity timeout, 6th context, concurrency refusal; do HTTP handshake failures occur.
6. Whether a keepalive message resets the socket-level or only the context timer; whether the
   initial `{"text":" "}` creates a counted default context.
7. Whether many `flush: true` calls in one context hurt prosody; effect on billing.
8. Turkish: `language_code=tr` effect, text normalization of numbers/dates, a good Turkish voice id.
9. Whether concurrency counts a cancelled (closed) context that keeps generating.

## Sources (all fetched 2026-10-08; the time-base fact was re-read on 2026-10-13)
- https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-stream-input (and `.md`)
- https://elevenlabs.io/docs/api-reference/text-to-speech/v-1-text-to-speech-voice-id-multi-stream-input (and `.md`; re-fetched 2026-10-13: "relative to the returned chunk", `sync_alignment` default false, no description; `output_format` untyped)
- https://elevenlabs.io/docs/api-reference/text-to-speech/convert (output_format and apply_text_normalization values, HTTP endpoint)
- https://elevenlabs.io/docs/eleven-api/guides/how-to/websockets/realtime-tts (TTS WebSocket guide)
- https://elevenlabs.io/docs/eleven-api/guides/how-to/websockets/multi-context-web-socket.md
- https://elevenlabs.io/docs/eleven-api/guides/how-to/websockets/tts-vs-ttd-websockets.md
- https://elevenlabs.io/docs/eleven-api/guides/how-to/websockets/realtime-tdd.md
- https://elevenlabs.io/docs/api-reference/text-to-dialogue/ttd-multi-websocket (existence only)
- https://elevenlabs.io/docs/overview/models
- https://elevenlabs.io/docs/eleven-api/resources/errors.md
- https://elevenlabs.io/docs/eleven-api/guides/how-to/best-practices/latency-optimization.md
- https://elevenlabs.io/docs/eleven-api/concepts/audio-streaming.md
- https://elevenlabs.io/docs/help-center/technical/how-many-text-to-speech-requests-can-i-make-and-can-i-increase-it (concurrency table)
- https://elevenlabs.io/blog/ai-rate-limiting-for-voice (queue, headers, WebSocket counting)
- https://elevenlabs.io/docs/help-center/technical/how-can-i-keep-the-websocket-open
- https://elevenlabs.io/blog/websocket-improvements-reliability-and-custom-timeout (published 2024-09-24, updated 2026-10-06)
- https://elevenlabs.io/docs/help-center/product/core-capabilities/text-to-speech/what-languages-can-the-ai-speak.md (only the "language is determined by the text" sentence; the language lists are on the models page)
- https://elevenlabs.io/docs/llms.txt (page index)
- https://github.com/elevenlabs/elevenlabs-python (MIT)
- https://docs.rs/crate/elevenlabs_rs and https://docs.rs/elevenlabs-sdk

Note: page contents were read through a summarising fetch tool, not raw page text. Quoted phrases are
as returned by that tool; the JSON examples in the guides were returned verbatim for the realtime-tts
guide and as reconstructed one-liners for the multi-context guide (the tool refused to reproduce the full
code). Verify exact examples against the live pages when implementing the contract fixtures.

Fact-checked 2026-10-08: about 85 claims checked, 15 corrected. Corrections: (1) default MP3 attribution
moved from the realtime-tts guide (states no default) to the stream-input API reference; (2) multi-context
"Each context times out after 20 seconds..." quote replaced by the real guide wording; (3) interruption
quote replaced by the real guide sentence; (4) "voice-focused design" quote replaced by "to reflect this" /
"optimized for voice applications", and the unconfirmed "no unrelated parallel streams" claim marked
UNVERIFIED; (5) prosody quote corrected; (6) keep-alive spec quote corrected; (7) auto_mode concept-page quote
corrected; (8) "at the end of each conversation turn" corrected to "at the end of conversation turn"; (9) alignment
"word-level" quote corrected; (10) claim that the multi-context guide says it is unavailable for eleven_v3 removed
(guide does not address it); (11) `*.residency.*` hosts: latency guide says Enterprise customers get EU and India
residency environments; (12) added `internal_error` (500) to the errors list; (13) Python SDK page does show HTTP
stream TTS and a Speech Engine WebSocket server (earlier text said no WebSocket mention); (14) Rust crates: added
versions, licenses, repos, ownership (`elevenlabs-sdk` is Apache-2.0 0.1.0 by Akagi201; `elevenlabs_rs` 0.7.1
unofficial); (15) TTD facts added (endpoint, snake_case, voice limits; multi-context TTD page exists) and `InitialiseContext`
"Creates a context with this ID, or re-initializes..." wording marked UNVERIFIED (not found in the re-fetch).

## Live check, Turkish (2026-10-13)
Docs re-fetched 2026-10-13 (https://elevenlabs.io/docs/overview/models): `eleven_flash_v2_5`,
`eleven_multilingual_v2` and `eleven_v3` all list Turkish (`tr`).

Measured (`live_checks.rs`, `eleven_flash_v2_5`, `language_code=tr`, `output_format=pcm_24000`, `sync_alignment=false`):
- Open item 1 (Turkish alignment) SETTLED: alignment arrives for Turkish and the words keep ç ğ ı İ ö ş ü; starts
  monotonic, inside the audio (3.45 s of 4.39 s). Open item 4 (`pcm_*` encoding): `pcm_24000` is signed 16-bit
  little-endian mono without a header (the audio is intelligible and Deepgram recovers the text).
- First audio 135-200 ms. Round trip through Deepgram: WER 0.000 on sentence 2, 0.000-0.154 on the number sentence
  (raw `250 lira ... 14.30'da` is read correctly: `iki yüz elli`, `on dört otuz`), 0.167 on the short question (an STT
  error, same as for Cartesia).
- One-word first chunk: English split pause 200 ms vs 170 ms for the single chunk; Turkish split file has a 210-230 ms
  pause after `Tamam,` that the single file lacks (to be judged by ear); no pacer underrun.
- Still open: items 2, 3, 5, 6, 7 of the list above.
