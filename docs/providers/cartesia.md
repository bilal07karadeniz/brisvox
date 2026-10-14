# Cartesia — TTS (WebSocket, Sonic)

Scope: contexts/continuations, word timestamps that feed played-text truncation, cancel on interrupt.
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that
date. Anything not confirmed is marked UNVERIFIED.

- Docs: https://docs.cartesia.ai/api-reference/tts/websocket (fetched 2026-10-08); full source list at the bottom.
- Auth:
  - Server side: header `X-API-Key: <key>` (WS reference page) or `X-Api-Key` / `Authorization: Bearer <key>`;
    header names are case-insensitive (authenticate-your-client-applications page). Use a header on
    the WS handshake, never put the API key in the URL.
  - Browser/client: short-lived token from `POST https://api.cartesia.ai/access-token`
    (headers `Authorization: Bearer <API_KEY>`, `Content-Type: application/json`,
    `Cartesia-Version: 2026-08-14`; body `{"grants": {"tts": true, "stt": true}, "expires_in": 60}`;
    response field `token`), passed as WS query `?access_token=<token>`. Grant `tts` allows `/tts/websocket`
    (grants object also has optional `agent`).
  - There is NO documented `api_key` query parameter for WebSockets (api-conventions page).
  - Brisvox: server side, so use the `X-API-Key` header; never log it (Error variants must not
    contain credentials).
- Endpoint(s) / regions:
  - `wss://api.cartesia.ai/tts/websocket`. HTTP is not supported (api-conventions). No regions
    documented (UNVERIFIED whether regional endpoints exist; none found).
  - Version: header `Cartesia-Version: YYYY-MM-DD` on the handshake, or query `?cartesia_version=YYYY-MM-DD`
    (query takes precedence; api-conventions). The WS reference lists `cartesia_version` as a
    required parameter ("API version, e.g. `2026-08-14`") but does NOT state whether it is a query
    parameter or header (the query form comes from api-conventions). Latest API version seen: `2026-08-14`
    (cleans up multilingual voice support, removes already-deprecated fields; pinned older versions
    keep working). `2026-03-01` introduced structured JSON errors (older versions use a legacy
    plain-string error envelope). The auth page's WS example uses `cartesia_version=2026-03-01`.
    Official Python SDK (cartesia-python, Apache-2.0; PyPI latest 4.2.0 as of 2026-10-08) sends the
    header `2026-08-14` by default.
    Recommendation: pin `cartesia_version=2026-08-14` in the query string; brisvox parses the
    structured error shape (requires >= 2026-03-01).
  - Example URL: `wss://api.cartesia.ai/tts/websocket?cartesia_version=2026-08-14`
- Models (current, from tts-models pages and changelog):
  - `sonic-3.6` = alias to latest stable snapshot (GA Aug 27, 2026); dated snapshot
    `sonic-3.6-2026-08-27` (never changes). `sonic-preview` = beta, not for production.
  - The WS schema enum for `model_id` lists `sonic-3.6`, `sonic-3.5`, `sonic-3`, `sonic-latest`
    (the reference example uses `"model_id": "sonic-latest"`).
  - `sonic-3.5` alias (GA May 2026, no day given; snapshot `sonic-3.5-2026-05-04`). `sonic-3` snapshots:
    `sonic-3-2026-01-12` (stable), `sonic-3-2025-10-27` (sunsets Oct 20, 2026).
  - Active sonic-2 and sonic-turbo snapshots sunset Oct 20, 2026 (changelog says "after October 20, 2026";
    older-models page says "sunsets October 20, 2026") and have no Turkish; do not use. (Changelog
    Feb 2026 also lists earlier discontinuations effective Jun 1, 2026.)
  - Sonic 3.6 is "fully backwards compatible with Sonic 3.5". Sonic 3 -> 3.5 migration: changing
    the model id is the only required change "for most customers"; also review punctuation-based
    alphanumeric pacing guidance and test beginning-of-word timestamps.
  - Recommended for brisvox: `model_id = "sonic-3.6"` (configurable).
- Protocol (messages in/out, with field names):
  - One WS connection multiplexes many contexts, each identified by a client-chosen `context_id`
    (string, e.g. UUID or readable id).
  - Client -> server, generation request (required: `model_id`, `transcript`, `voice`,
    `output_format`, `context_id`). Example (shape per the WS reference; values illustrative):
    ```json
    {
      "model_id": "sonic-3.6",
      "transcript": "Example text to convert to speech.",
      "voice": "your-voice-id",
      "language": "en",
      "context_id": "my-context-001",
      "output_format": {
        "container": "raw",
        "encoding": "pcm_s16le",
        "sample_rate": 24000
      },
      "continue": true,
      "max_buffer_delay_ms": 3000,
      "add_timestamps": true,
      "add_phoneme_timestamps": false,
      "use_normalized_timestamps": true,
      "generation_config": { "volume": 1, "speed": 1, "emotion": "neutral" }
    }
    ```
  - Fields:
    - `model_id` string; `transcript` string (text chunk appended to the context's audio);
      `voice` string voice id, or object with required `id` (embeddings no longer accepted since
      Jun 1, 2026); `context_id` string.
    - `output_format` object, all required: `container` = `raw` (only value); `encoding` one of
      `pcm_f32le`, `pcm_s16le`, `pcm_mulaw`, `pcm_alaw`; `sample_rate` integer one of
      `8000, 16000, 22050, 24000, 44100, 48000`. Keep voice and output_format identical across a context.
      Brisvox TtsConfig = PCM16 LE mono -> `pcm_s16le`; for telephony `pcm_mulaw`/`pcm_alaw` @ 8000
      are available directly (UNVERIFIED whether any extra latency/quality difference vs resampling).
    - `language` (e.g. `tr`) OR `locale` (e.g. `en-GB`, `tr-TR`; locale needs sonic-3.6 or newer): never both
      (400 error). `accent` optional (must be in the voice's `accents`). `normalization`:
      `auto` (default) | `off` | language/locale code; reused on later requests in the same
      context if omitted.
    - `continue` boolean, default `false`. `true` = more transcript will follow in this context.
    - `max_buffer_delay_ms` integer 0..5000, default 3000.
    - `flush` boolean. `add_timestamps` bool default false (word level). `add_phoneme_timestamps`
      bool default false. `use_normalized_timestamps` bool (true normalized, false original text).
      `pronunciation_dict_id` string. `generation_config`: `volume` 0.5-2.0 (default 1),
      `speed` 0.6-1.5 (default 1), `emotion` in `neutral|calm|angry|content|sad` (omitted = model infers: UNVERIFIED).
  - Across inputs on the same context every field except `transcript`, `continue` (and `duration`)
    must stay the same (contexts page).
  - Client -> server, cancel (exact message shape, from the WS reference and contexts page):
    ```json
    { "context_id": "happy-monkeys-fly", "cancel": true }
    ```
  - Server -> client messages, all carry `context_id`; `type` discriminates. Examples (shapes per the
    WS reference; ids and some values illustrative):
    ```json
    {"type":"chunk","data":"<base64-encoded audio>","done":false,"status_code":206,"step_time":123,"context_id":"my-context-001"}
    {"type":"flush_done","done":false,"flush_done":true,"flush_id":1,"status_code":206,"context_id":"my-context-001"}
    {"type":"done","done":true,"status_code":206,"context_id":"my-context-001"}
    {"type":"timestamps","done":false,"status_code":206,"context_id":"my-context-001",
     "word_timestamps":{"words":["Example","text"],"start":[0,0.5],"end":[0.4,0.9]}}
    {"type":"phoneme_timestamps","done":false,"status_code":206,"context_id":"my-context-001",
     "phoneme_timestamps":{"phonemes":["h","ə","l","oʊ"],"start":[0.09,0.17,0.26,0.34],"end":[0.17,0.26,0.34,0.42]}}
    {"type":"error","done":true,"error_code":"model_not_found","status_code":400,"title":"Invalid model",
     "message":"The model is not valid, make sure it is a valid model ID.",
     "doc_url":"https://docs.cartesia.ai/build-with-cartesia/tts-models/latest",
     "request_id":"<unique-connection-id>","context_id":"my-context-001"}
    ```
    (JSON reformatted onto fewer lines; the phoneme example values were not re-verified: UNVERIFIED.)
  - Field notes: chunk `data` = base64 audio in the requested `output_format`; `step_time` = server
    processing time for the chunk in ms (number); `status_code` 206 on streaming messages (the docs call it
    "HTTP-like"); `done` true on final message of a context; `flush_id` integer counting flushes
    in the context, starting at 1 (WS reference: "number of flush commands that have been sent for
    this context. Starts at 1"). The flushing page says each audio chunk payload includes a
    `flush_id`, but the WS reference chunk schema has NO `flush_id` field: UNVERIFIED on the wire
    (read it as optional, default 0/absent). `error`: required `type,title,message,status_code`;
    optional `done,error_code,doc_url,request_id,context_id`. `request_id` identifies the connection
    (api-errors: may be a UUID or a derived per-message string). The SDK exposes base64 `data` as
    `audio` bytes - the wire field is `data`.
  - Ordering: outputs for a context arrive in input order (audio for input A before B). `done: true`
    only after all outputs for the context were returned. Interleaving of `timestamps` vs `chunk`
    messages is not specified (UNVERIFIED). The adapter handles `timestamps` before or after the
    chunk's `flush_done`, within the limits written down in the implementation gotchas. Messages of different contexts
    interleave on the wire; route by `context_id` (UNVERIFIED wording; docs describe multiplexing).
- Keepalive / timeouts:
  - TTS WebSocket closed after 5 minutes idle (concurrency-limits-and-timeouts page). Docs advise
    closing idle sockets and opening a new one when needed (UNVERIFIED: that advice was not
    re-found on the fetched pages).
  - A "180 s inactivity timeout, ping every 60-90 s" statement exists only in the Cartesia Line
    Agents WebSocket API reference (and another Line page states a 120 s application-level timeout
    that pings do not reset), not the TTS WS docs; do not rely on it. A documented TTS keepalive
    message or ping behavior was NOT found: UNVERIFIED (use WebSocket protocol ping frames as a
    harmless probe; verify in the live test). Brisvox rule: reconnect if the socket is closed;
    `warm()` can pre-open a socket.
  - Context expiry: a context expires 1 second after its last audio output is streamed; sending
    another input on an expired context id is "not supported". If `continue` stays true for too
    long the server eventually expires the context ("contexts eventually expire on their own",
    compare-tts-endpoints page; duration not documented: UNVERIFIED). Brisvox: use a fresh
    `context_id` per utterance (ContextId -> string); never reuse one after `done`.
  - No documented per-message connect/handshake timeout; brisvox applies its own timeouts.
- Finalize / flush / cancel semantics:
  - End an utterance: send the last text with `continue:false`, or if the last text is unknown send
    `{"transcript":"", "continue":false, ...same fields..., "context_id":...}`. Server then
    emits remaining chunks/timestamps and a final `done`. (Maps to `TtsInput::Flush`, i.e. no more
    text: synthesize remaining, then `TtsEvent::Flushed` on `done`.)
    The contexts page explicitly documents an empty transcript with `continue:false` to close a
    context. A claim that an empty transcript causes a 400 on the plain generate path could not be
    re-found on the pages checked on the second pass: UNVERIFIED. Verify live.
  - Manual flush (boundary inside a context): send `continue:true`, `flush:true`, `transcript:""`,
    same `context_id`. Server replies `flush_done` with an incrementing `flush_id` (first = 1);
    audio chunks carry the `flush_id` of the transcript that produced them (per the flushing page;
    see field-note caveat above). The context staying alive is implied, not stated. The flushing
    page does not mention `flush_done` and does not say whether `flush_done` is sent after all
    chunks of the previous transcript (assumed, UNVERIFIED).
  - Cancel: `{"context_id": "...", "cancel": true}`. Docs say: "It will only halt requests that
    have not begun generating a response yet" and "Any currently generating request will continue
    sending responses until completion." CRITICAL GOTCHA for barge-in: cancel may NOT stop audio
    already being generated. Brisvox must therefore (a) send cancel, (b) immediately drop every
    later message for the cancelled `context_id` client-side (epoch/ContextId filter), (c) emit
    `TtsEvent::Cancelled` itself without waiting for the server, and (d) not reuse the id. Whether
    the server sends a `done`/error after cancel is not documented: UNVERIFIED. Whether late
    cancelled-context audio still counts toward billing/concurrency is UNVERIFIED.
  - Cancel is a normal text frame on the same socket; the brisvox `cancel` channel must be polled
    with priority over `input` (as per the trait docs).
- Timestamps / interim results / tool-call deltas:
  - Enable with `add_timestamps: true` (words) / `add_phoneme_timestamps: true`. Message type
    `timestamps` with `word_timestamps: { words: [string], start: [number], end: [number] }`
    (parallel arrays); phoneme variant `phoneme_timestamps: { phonemes, start, end }`.
  - Units: the reference says "Start times in seconds for each word" / "End times in seconds for each
    word" (floats, e.g. 0.4).
  - Reference point (relative to what: context start, chunk start, or cumulative across
    continuations): NOT documented in any page fetched (the contexts page says nothing about
    timestamps; the SSE timestamps page was not re-checked on the second pass). UNVERIFIED. Played-text truncation must
    treat it as unknown and be validated
    against a live recording: check whether `start[0]` of the second `timestamps` message of a
    context restarts at ~0 (per-message relative; brisvox must add the audio duration already
    received/sample offset) or continues (cumulative per context). Until verified, a safe
    implementation computes offsets from its own byte count of received audio only if per-message
    relative is observed.
  - Changelog / migration page: for the Sonic 3 -> 3.5 move, end-of-word timestamps (used for
    interruption handling) are "unchanged"; beginning-of-word timestamps may regress, test
    carefully. Whether the same holds for Sonic 3.6: UNVERIFIED.
  - `use_normalized_timestamps` chooses whether words are the normalized form (e.g. spoken-out
    numbers) or the original text; for played-text truncation (matching spoken text to LLM text) the original
    (`false`) is likely wanted. Which default applies if omitted: UNVERIFIED (schema lists no default).
  - "Subject to model and language limits": timestamps support for Turkish specifically is
    UNVERIFIED (compare-tts-endpoints says WS supports word/segment timestamps subject to model and
    language limits; no list found).
- Concurrency / rate limits:
  - Concurrency is counted per unique active `context_id` (not per connection); extra inputs on the
    same context add nothing (UNVERIFIED on the second pass: this exact statement was not re-found
    on the concurrency page). Plans (cartesia.ai/pricing and concurrency page agree): TTS concurrent
    requests Free 2, Pro 3, Startup 5, Scale 15, Enterprise custom. TTS and STT pools separate
    (UNVERIFIED on second pass).
  - Max parallel TTS WebSocket connections = 10x the TTS concurrency limit (Scale: 150);
    exceeding it returns 429 on opening a new connection. Exceeding concurrency returns 429
    ("429 Too Many Requests"). The error code `concurrency_limited` is defined as "over its plan's
    concurrency limit" (api-errors), and api-errors shows a 429 WebSocket error example; the concurrency
    page itself does not mention an `error_code` for WS errors, so the exact WS pairing
    `error_code: "concurrency_limited"` + `status_code: 429` is inferred: UNVERIFIED.
  - Rule of thumb in docs: for conversational use cases the number of parallel conversations is
    "effectively 4X your concurrency limit" ("just a rule of thumb"; limit 15 ~ 60 calls), because
    most of a call is idle.
  - Maximum number of contexts per connection: NOT documented (UNVERIFIED). Brisvox design: one
    connection per call/session, one active context at a time (plus a cancelled one draining).
  - No requests-per-minute limit documented. Quota: `quota_exceeded` error when credits exhausted
    (unless overage is enabled on the plan).
  - Pricing: ~1 credit per character for TTS (all endpoints incl. WS; "can vary slightly due to
    transcript pre-processing"); no extra charge for the WS endpoint is mentioned (whether multiple
    concurrent contexts per connection cost extra: not stated, UNVERIFIED). Plans: Free $0 / 20K
    credits; Pro $5 / 100K; Startup $49 / 1.25M; Scale $299 / 8M; overage per 1M credits Pro $65,
    Startup $45, Scale $38 (Free: overage not available). Pricing page: https://cartesia.ai/pricing
    (docs mirror https://docs.cartesia.ai/pricing lists only ~1 credit/char and points to the
    pricing page). Prices as read 2026-10-08.
  - Error codes (api-errors page; mapping to status not listed there): `quota_exceeded`,
    `concurrency_limited`, `voice_model_mismatch`, `voice_not_found`, `model_not_found`,
    `language_not_supported`, `file_too_large`, `unsupported_audio_format`, `plan_upgrade_required`;
    `error_code` may be null and new values may appear: handle unknown ones.
    `status_code` is "HTTP-like"; values seen in docs examples: 400 (model_not_found, WS reference),
    429 (WebSocket example in api-errors), 500 (SSE example). 401/403 for bad auth not documented on
    the fetched pages (SDK has `AuthenticationError`): UNVERIFIED on the WS handshake (likely HTTP
    401 before upgrade).
- Turkish support: YES. Sonic 3.6 supports 44 languages (changelog: "44 languages / 61 locales"),
  includes Turkish (`tr`, locale `tr-TR`); Sonic 3.5 and Sonic 3 list Turkish (42 languages).
  Active sonic-2 / sonic-turbo snapshots do NOT include Turkish (only already-sunsetted
  snapshots did).
  Use `"language": "tr"` (or `locale`, with 3.6+). Voice must support the language: a voice has an
  `accents` list; if it has no accent for the language it falls back to its original accent
  (documented with a Japanese example; for Turkish assumed the same: UNVERIFIED; the multilingual
  voices page does not mention Turkish). Pinning an unsupported accent -> 400 ("Unsupported accent
  for voice"). Pick/verify a voice id that is good for Turkish (the voice library
  filters by language; voice ids not researched: UNVERIFIED). Error `language_not_supported` if
  model/voice cannot do it.
- Chunking / buffering guidance for continuations (buffering page, stream-inputs guide):
  - Joined transcript chunks must form valid text (stream-inputs guide): keep spacing and
    punctuation (`" How are you?"` may follow `"Hello, world!"`, `"How are you?"` may not).
  - Two modes (buffering page). Managed buffering: stream LLM tokens straight in with
    `max_buffer_delay_ms > 0` (default 3000); the API buffers until it has enough context or the
    delay elapses, whichever comes first. Custom buffering: you aggregate sentences/phrases
    yourself and set `max_buffer_delay_ms: 0` so generation starts immediately. Do not combine
    client-side sentence aggregation with the default 3000 ms (adds unnecessary latency).
  - Brisvox recommendation (derived from the above, a design choice not a doc claim): aggregate in
    the engine (the LLM->TTS segmenter) and send phrase/sentence chunks with `continue:true`,
    `max_buffer_delay_ms:0`; finish with `continue:false` (empty transcript if needed).
  - End sentences with `.`, `?`, `!`; without terminal punctuation the model may hold text until the
    delay expires and the audio may show artifacts. Final segment should have terminal
    punctuation and `continue:false`.
  - Do not split decimal values (`1.0` into `1`,`.`,`0`) when using speed/volume SSML tags.
  - Minimum characters per chunk: not documented.
  - Contexts page tip: "Without this option set [max_buffer_delay_ms], the model will start
    generating immediately on the first request". Interaction with the 3000 default is not fully
    spelled out; treat time-to-first-audio as needing a live measurement (no numeric latency
    figures in the docs: UNVERIFIED).
  - Reuse one WS across turns (saves TCP+TLS, "often tens to low hundreds of ms per turn";
    compare-tts-endpoints page).
- Mapping to the brisvox trait (crates/brisvox-providers/src/tts.rs) - proposal:
  - `ContextId(u64)` -> `context_id` string (e.g. `format!("{session}-{n}")`), maintain a
    reverse map; drop messages whose context id is unknown/cancelled.
  - `TtsInput::Text` -> generation request with `continue:true`; first request for a context carries
    all config fields, later ones repeat them unchanged. `TtsInput::Flush` -> `continue:false`
    (empty transcript) and `TtsEvent::Flushed` when `done:true` arrives for that context.
    (Do not use `flush:true` for this; that is the mid-context boundary.)
  - `chunk.data` base64-decode -> `TtsEvent::Audio` (decode off the hot loop if large; chunks are
    small, but note the hot-path allocation rule: reuse buffers).
  - `timestamps.word_timestamps` -> one `TtsEvent::Word` per word with `start` seconds ->
    `Duration`; the offset basis question above must be resolved first.
  - `cancel` channel -> send `{"context_id":..,"cancel":true}`, mark context cancelled locally,
    emit `TtsEvent::Cancelled`.
  - Server `error` with `context_id` may be per-context (e.g. bad voice) vs. connection-level;
    whether the socket remains usable after an error is UNVERIFIED.
- Gotchas found during implementation (`brisvox_providers::cartesia`, feature
  `provider-cartesia`; tested against a local mock of this protocol, not against the live service):
  - Doc inconsistency noted: the WS reference lists the header `X-API-Key`; api-conventions only
    mentions `Authorization: Bearer`; authenticate-your-client-applications lists both. Both accepted.
  - Docs page `docs.cartesia.ai/<ver>/asyncapi.yml` redirects to a login page (not fetchable); the
    rendered API-reference page was used instead.
  - Implementation choices where the doc is silent or UNVERIFIED (all configurable or handled both ways):
    - Version: sent as the query `cartesia_version=2026-08-14` (api-conventions, query form). Key:
      `X-API-Key` header marked sensitive, never in the URL. `voice` is sent as the plain string id
      (the WS reference example), not as an object.
    - `language` is optional in the adapter (omitted when unset: server default); `max_buffer_delay_ms`
      defaults to 0, `add_timestamps` is always true, `use_normalized_timestamps` defaults to false
      (words as written, for played-text matching), model `sonic-3.6`. All fields except `transcript`,
      `continue` and `flush` are repeated unchanged on every request of a context.
    - One empty-transcript `flush: true` request follows every text chunk ("flush marker",
      `CartesiaConfig::flush_after_text`, default on). Reason: with `continue: true` the server sends
      nothing that says "this chunk is finished", so a silent server cannot be told from one waiting
      for more text. The marker's `flush_done` lets the session arm the stall deadline only while
      output is owed (a long LLM or tool-call pause between sentences must not kill the session) and
      gives the audio duration at each chunk boundary (word time base, next point). ASSUMED, not
      verified: `flush_done` arrives after all chunks and timestamps of the preceding transcript; a
      flush with nothing buffered (delay 0) does not change prosody. If a live test shows otherwise,
      set `flush_after_text = false`: then only the first response to a text chunk and the final
      `done` are timed and word times are taken as context-cumulative.
    - Word time base (the open question above): handled both ways, per chunk. A chunk is the
      text between two `flush_done` acknowledgements; the audio duration received at a boundary
      (counted from the bytes, so exact) is what separates the two cases. If the first word of a
      chunk starts more than 20 ms before that audio duration, the server restarted its clock
      and the boundary audio is added; otherwise the times are used as sent (a cumulative server
      cannot start a word before the earlier chunks' audio has ended). This replaces an earlier
      rule that compared with the previous word's end, which misread a short first chunk
      followed by a chunk whose first word comes early. Offsets never go backwards.
      Ordering of `timestamps` vs `flush_done` (UNVERIFIED upstream, so both are handled):
      if a chunk's `timestamps` arrive after its own `flush_done`, a cumulative server is still
      classified correctly (not shifted either way) and a restarting server is shifted by the
      start of the chunk before the last boundary. What is NOT resolvable: a chunk that gets no
      `timestamps` at all (no words) followed by a restarting server is shifted by the earlier
      chunk's start; offsets are monotonic, so the damage is bounded. `timestamps` before the
      audio need no special handling (only byte counts at `flush_done` are used). A word time
      that is negative (clamped to 0), missing, NaN or absurd is clamped to the previous offset;
      `words`/`start` arrays of different length never fail the call (word times only feed
      played-text truncation).
    - Context expiry (the docs: a context expires 1 s after its last audio, more input on it is
      "not supported"; what the server does is UNVERIFIED, so three outcomes are handled):
      (1) the adapter's estimate (`context_expiry`, default 1 s, only with flush markers, only
      when the server owes nothing): the next `Text` opens a new server context (new id) instead
      of sending to the old one; (2) the server answers input on an expired context with an
      `error` (not a refusal like 401/429/5xx, and the context had produced output): a new
      server context is opened and the text that was not answered yet (kept up to 8 KiB, cleared
      at each `flush_done`) is sent again, at most twice in a row; (3) the server sends an
      unsolicited `done`: if nothing is unanswered the server context is marked closed, the
      engine context lives on (no `Flushed`), the next `Text` opens a new one. If the `done`
      finds a flush marker unanswered or text not yet confirmed (a solicited `done` always
      follows the `flush_done` of every marker, so this means the server expired the context
      just as that text arrived), it is handled like (2): the unanswered text, and the closing
      request if one was sent or wanted, go to a new server context; it is never a completion
      and never a false `Flushed`. A server that keeps answering with such a `done` and no
      `flush_done` in between fails the session (`Protocol`) after two re-sends. Limit (also of
      (2)): the re-sent text is everything not yet confirmed by an all-clear, so if the loss
      happens while an earlier chunk's `flush_done` is still in flight, that earlier chunk is
      spoken a second time. In all three the engine context keeps its
      audio count, and the new server context's word times are offset by the audio already
      delivered, so `TtsEvent::Word` offsets stay continuous across the pause (the trait
      requires this). A `Flush` for a context whose server context is closed or expired reports
      `Flushed` at once without sending anything. Messages from the old server context are
      dropped (routing is by the current server id).
    - Input closed: dropping `input` closes every open server context (`continue:false`, empty
      transcript), waits for the `done`s (stall-timed like any owed output: the timer is armed
      when the closing requests go out, also if nothing was owed before; a server that never
      sends the `done` ends the session with `Timeout` after `io_timeout`), delivers what is
      left and ends `events`; `Flushed` is only emitted for contexts the engine flushed. Contexts
      the engine never flushes or cancels are evicted (oldest first, server context closed) when
      more than 64 are open.
    - Cancel during I/O: the cancel channel is also polled while a send or a (re)connect is in
      progress. `Cancelled` is handed to the consumer before any socket work; the wire cancel is
      queued behind the write in flight. A cancel that arrives while a new context is still
      connecting suppresses its text. Not covered by a test: a TCP send buffer that is full (a
      stalled connect is tested instead; both go through the same select).
    - Close: `TtsInput::Flush` sends `continue:false` with an empty transcript; `Flushed` is emitted
      on the server's `done` (or any message with `done: true`) for a context the engine closed. A
      `done` the engine did not ask for only closes the server context (no `Flushed`, see context
      expiry). `Flush` for a context with nothing sent yields `Flushed` immediately; `Text` after
      the `Flush` of the same context is ignored (the server context is closed); a `ContextId`
      used again after `Flushed` starts a new engine context (word offsets restart at 0).
    - Cancel: the cancel message is sent AND the context is forgotten locally at once: later server
      messages for it are dropped, events for it that the consumer has not received yet are purged
      from the adapter's queue, `Cancelled` is queued first. Barge-in latency does not depend on the
      server. Queued `Text`/`Flush` for a cancelled id are ignored (last 64 ids remembered); a cancel
      for an unknown context still yields `Cancelled`. Messages are routed in two steps: first only
      `type` and `context_id` are read, leniently; a message of a context that is not live (cancelled,
      finished, replaced by a re-open, unknown) is dropped without further parsing, so a malformed
      late message of a cancelled context cannot fail the call. Only messages of live contexts are
      parsed strictly.
    - Errors: handshake refusals map to `Rejected { status }` (401, 403, 429, 5xx, ...). A server
      `error` message for a live context ends the session (the trait's `Error` event is final),
      except the two recoverable cases (expiry above, concurrency below): `concurrency_limited` ->
      `Rejected 429`, `quota_exceeded` -> `Rejected` (status as sent, else 402),
      `plan_upgrade_required` -> `Rejected` (else 403), `status_code` 401/403/408/429/5xx ->
      `Rejected`, anything else (model/voice/language problems) -> `Protocol` with `error_code`,
      title and message. An error for a cancelled or unknown context is ignored; an error without a
      `context_id` is treated as about the connection. Unparsable text, bad base64 of a live context
      and binary frames are `Protocol`. A close or reset while output is owed is `Disconnected`.
      Concurrency interaction with barge-in (docs: concurrency is counted per active context and a
      cancelled request that is already generating keeps running; whether it still counts is
      UNVERIFIED): the follow-up utterance after a barge-in can be refused with
      `concurrency_limited` on plans with a limit of 2-3. A refusal of a context that has produced
      nothing is retried for that engine context only, on a new server context id, after
      `concurrency_retry_delay` x attempt (default 300 ms, 600, 900, 1200 ms); the other contexts
      and the socket are untouched, text and a flush that arrive meanwhile are kept. After the
      fourth retry the refusal ends the session as `Rejected 429`. Anything beyond that (a plan
      limit) is the caller's: do not let cancelled contexts pile up.
    - Idle: no keepalive is sent (none is documented). If the server closes the socket while it owes
      nothing (5-minute idle rule) the session survives, every server context is marked closed and
      the next `Text` reconnects lazily with its own connection (one retry if the socket turns out
      dead on that first write); it does NOT take the socket that `warm()` parked for the next
      `connect`. `warm()` parks one socket (no task, no keepalive); `connect` takes it if it is
      younger than `warm_max_age` (60 s), still open and the provider's `shutdown` token is not
      cancelled, otherwise connects. A cancelled token releases the parked socket the next time
      `warm` or `connect` runs (no task watches it while it waits).
    - Transport: `ws://` is accepted only for loopback hosts (tests, a local proxy) unless
      `allow_insecure_ws` is set, because the API key travels in the handshake header; anything
      other than `wss://`/`ws://` is refused at `CartesiaTts::new`.
    - Odd-sized PCM16 chunks are re-aligned across chunk boundaries so every `Audio` event holds
      whole samples. `pcm_mulaw` / `pcm_alaw` are available for 8 kHz sessions only; the `Audio`
      bytes are then G.711, not PCM16 (the caller must know).
    - Timeouts: one `io_timeout` (default 10 s) covers connect, every send, the close and every
      wait for output the server owes; it restarts on each message for a live context and does not
      run while the adapter has stopped reading because the consumer is behind.
    - Backpressure: the socket is not read while 64 or more events wait for the consumer; nothing
      is dropped except by cancel.
  - Still UNVERIFIED on the live service (see the open items below): everything marked ASSUMED
    above, the timestamp time base and the order of `timestamps` vs `flush_done`, `voice` as plain
    string with `sonic-3.6`, the empty-transcript close, whether `flush_done` carries `flush_id`
    semantics as documented, the concurrency error shape on the socket, what the server does with
    input on an expired context (error vs ignore vs `done`; a silent ignore ends in the stall
    timeout of `io_timeout`, because the adapter then waits for a marker answer), and whether a
    cancelled but still generating context counts against concurrency.

## Live run (2026-10-13)
Docs re-fetched 2026-10-13 (https://docs.cartesia.ai/api-reference/tts/websocket): endpoint
`wss://api.cartesia.ai/tts/websocket`; auth `X-API-Key` header (server) or `access_token` query
(browser), no `api_key` query parameter; `cartesia_version` required, example `2026-08-14`;
`generationRequest` and the `chunk`/`flush_done`/`timestamps`/`phoneme_timestamps`/`done`/`error`
messages as described above. No protocol fact changed, so the mock server is unchanged.

**Root cause of the first live failure (`Core(Timeout { what: "cartesia connect" })`): not the
protocol, but TCP dialing.** `api.cartesia.ai` (CloudFront) resolves to 8 IPv6 and 4 IPv4 addresses
(the other adapters' hosts resolve to IPv4 only). On a machine whose IPv6 path silently drops
packets, `tokio_tungstenite::connect_async` tries the addresses one after the other and waits for
the OS to give up on each one (a Python probe with an 8 s timeout per address needed about 64 s in total) before it
reaches the first IPv4 address, so the 10 s handshake deadline fires first. Evidence: a Python
`websockets` client hung the same way; `curl` (which races address families) got
`101 Switching Protocols` in about 0.9 s with the identical headers and query; a raw
`socket.connect` per address showed 8 IPv6 timeouts and 4 IPv4 successes in 10-30 ms.
Fix: the shared dialer `brisvox_providers::net::connect_ws` (`net/dial.rs` + `net/ws.rs`, moved
there from `cartesia/dial.rs`; see `docs/PROVIDERS.md`, "Dialing (happy eyeballs)")
resolves the host and races the addresses (RFC 8305: alternate families, next attempt after
250 ms or as soon as the previous one failed, at most 8 addresses, first connected socket wins),
then runs TLS and the WebSocket handshake over that socket (`client_async_tls_with_config`). The
whole dial stays inside the existing `io_timeout` deadline. Every WebSocket adapter now
uses it.

Measured (one run, Istanbul, residential link, `sonic-3.6`, `pcm_s16le` 24000 Hz, language `en`,
`max_buffer_delay_ms: 0`, `use_normalized_timestamps` default):
- `warm()` (DNS + raced TCP + TLS + WebSocket upgrade): 921 ms.
- Time from the first text send (socket already warm) to the first audio chunk: 207 ms.
- Word timestamps: 11 words for "Hello there, this is a short test." + " It has two chunks."; first word
  at 120 ms, last word at 2.68 s, 3.36 s of audio in total. Offsets were non-decreasing across the
  two chunks and ended inside the audio duration (as emitted by the adapter, which handles both
  time bases).
- `Flushed` arrived after all audio and all 11 words of the utterance.
- Barge-in: `TtsEvent::Cancelled` is emitted by the adapter itself (120 us after the cancel
  was sent); no further event for the cancelled context in the following 1.5 s; the next
  utterance (new context) on the same socket synthesised and flushed normally, so the socket
  stays usable after a cancel.
- Confirmed accepted on the wire: `X-API-Key` header with `cartesia_version=2026-08-14` in the
  query, `voice` as a plain string id, model `sonic-3.6`, `add_timestamps`, manual `flush`,
  `cancel`.

Resolved by this run: item 7 below (flush_done follows the chunk's audio, as far as the adapter
reports `Flushed`), the plain-string `voice` with `sonic-3.6`, and the version placement (query
parameter accepted, also with the older value 2025-04-16 in a curl probe; the `Cartesia-Version` header form was not exercised). Partly
settled: item 2 (the adapter's view only) and item 5 (one data point: `pcm_s16le` 24 kHz, delay 0:
207 ms). Everything else in the list below, in particular the raw timestamp time base, stays
UNVERIFIED because the live test observes only adapter-normalised output.

## Open items to settle with a live test
Run `BRISVOX_LIVE_TESTS=1 CARTESIA_API_KEY=... CARTESIA_VOICE_ID=... cargo nextest run -p brisvox-providers
--all-features --no-capture -E 'test(=live_cartesia_session)'`; it prints time to first
audio, audio length, first and last word offsets and the cancel acknowledgement time (never the key).

1. Timestamp time base (per message vs cumulative per context) and whether it spans `continue`
   chunks; relation to total audio duration.
2. What the server sends after a `cancel` (nothing / `done` / audio tail) and how much audio still
   arrives for an already-generating request.
3. Keepalive: does a WS ping frame reset the 5 min idle timer; what close code/reason is used.
4. Max contexts per connection; behavior when a second context starts while the first still
   generates (ordering across contexts).
5. Time to first audio with `max_buffer_delay_ms:0` vs default, with `pcm_s16le` 24000/16000/8000.
6. Turkish voice ids and whether timestamps are produced for `tr`.
7. Whether `flush_done` arrives after all chunks of the preceding transcript.
8. Whether chunk messages really carry `flush_id` on the wire; empty-transcript behavior.
9. Expiry: what a request on a context idle for more than 1 s produces (error code/status, silence,
   or an unsolicited `done`), and how long after the last audio the expiry really is; then tune
   `context_expiry` and the error classification in `session.rs::on_error`.
10. Whether a cancelled context that keeps generating still counts against concurrency, and how
    long it takes to stop (sizes `concurrency_retry_delay`).

## Sources (all fetched 2026-10-08)
- https://docs.cartesia.ai/api-reference/tts/websocket (and .md)
- https://docs.cartesia.ai/use-the-api/tts-websocket/contexts.md
- https://docs.cartesia.ai/api-reference/tts/working-with-web-sockets/contexts
- https://docs.cartesia.ai/use-the-api/tts-websocket/context-flushing-and-flush-i-ds.md
- https://docs.cartesia.ai/use-the-api/tts-websocket/buffering.md
- https://docs.cartesia.ai/build-with-cartesia/capability-guides/stream-inputs-using-continuations.md
- https://docs.cartesia.ai/use-the-api/concurrency-limits-and-timeouts.md
- https://docs.cartesia.ai/use-the-api/api-conventions.md
- https://docs.cartesia.ai/use-the-api/api-errors.md
- https://docs.cartesia.ai/get-started/authenticate-your-client-applications.md
- https://docs.cartesia.ai/use-the-api/compare-tts-endpoints.md
- https://docs.cartesia.ai/build-with-cartesia/tts-models/latest.md (Sonic 3.6)
- https://docs.cartesia.ai/build-with-cartesia/tts-models/sonic-3-5.md
- https://docs.cartesia.ai/build-with-cartesia/tts-models/sonic-3-to-sonic-3-5.md
- https://docs.cartesia.ai/build-with-cartesia/tts-models/older-models.md
- https://docs.cartesia.ai/build-with-cartesia/capability-guides/multilingual-voices.md
- https://docs.cartesia.ai/changelog/2026.md
- https://docs.cartesia.ai/pricing.md and https://cartesia.ai/pricing
- https://docs.cartesia.ai/examples/ (tts-websocket-flushing, -continuations, -concurrent-contexts,
  -response-handling, tts-sse-with-timestamps, error-handling) - not re-checked on the second pass
- https://github.com/cartesia-ai/cartesia-python (Apache-2.0; no Rust SDK is listed on its page, but
  no official Rust SDK was positively ruled out: UNVERIFIED; brisvox uses tokio-tungstenite directly)
- https://pypi.org/pypi/cartesia/json (latest 4.2.0)
- https://docs.cartesia.ai/api-reference/agents/websocket (Line 180 s / 60-90 s ping statement, via search)

Fact-checked 2026-10-08: 62 claims checked, 14 corrected. Corrections: (1) SDK version "v4.0.1 examples" -> PyPI latest 4.2.0 (default header 2026-08-14 confirmed); (2) `cartesia_version` placement: WS reference does not say query vs header; (3) timestamps units wording is "Start/End times in seconds for each word"; (4) `step_time` example 120.5 -> 123, error message uses a comma ("is not valid, make sure"), timestamps start is 0 not 0.0, examples are not verbatim copies; (5) chunk `flush_id` not in WS reference chunk schema (only claimed by the flushing page) -> flagged UNVERIFIED; (6) "Without it, generation starts on the first request" was a mis-quote -> actual: "Without this option set, the model will start generating immediately on the first request"; (7) "tens to low hundreds of ms" is from compare-tts-endpoints, not stream-inputs; "keep leading spaces" is from stream-inputs, not buffering; (8) "Sonic 3.6 same" timestamps note was unsupported -> UNVERIFIED (note is from the Sonic 3 -> 3.5 migration); (9) Sonic 3 -> 3.5 "only the model id changes" -> only for most customers, plus punctuation and timestamp checks; (10) `concurrency_limited` + 429 on WS is inferred, not stated; "concurrency per context_id", "separate TTS/STT pools" and "contexts cost nothing extra" could not be re-found -> UNVERIFIED; (11) 4x rule wording and "180 s idle" source clarified (Line Agents API, with a conflicting 120 s rule on another Line page); (12) empty-transcript-400 claim not re-found -> UNVERIFIED; (13) `normalization` "initial default" -> `auto` default, emotion "omitted = model infers" unconfirmed, sonic-3.5 GA has no day, sunset wording differs between changelog and older-models page; (14) pricing docs mirror lists no prices; Free plan has no overage; Python SDK page lists no Rust SDK (not positively ruled out). Remaining UNVERIFIED: see items marked above (timestamp time base, cancel aftermath, keepalive, contexts per connection, Turkish voices/accent fallback/timestamps, flush_done ordering, auth failure status, regions, phoneme example values, sources under /examples).

## Live check, Turkish (2026-10-13)
Docs re-fetched 2026-10-13 (https://docs.cartesia.ai/build-with-cartesia/tts-models/latest): `sonic-3.6` supports 44
languages including Turkish (`tr`); the page does not say whether voices are cross-lingual (the voice of
`CARTESIA_VOICE_ID` was used with `language=tr` and works).

Measured (`live_checks.rs`, `model sonic-3.6`, `language=tr`, `pcm_s16le` 24 kHz, `add_timestamps`, defaults of
`CartesiaConfig`): item 6 of the open list is settled:
- Word timestamps are produced for Turkish; ç ğ ı İ ö ş ü are intact in the word texts (as written, not normalized);
  start times monotonic and the last start is inside the audio (e.g. 3.48 s of 4.00 s).
- First audio 204-238 ms (Turkish and English alike). Round trip through Deepgram: WER 0.000 on a sentence with
  ç ğ İ ş ü, 0.167 on a short question (an STT error).
- Number reading: raw `14.30'da` is read digit by digit (`bir dört üç sıfır da`), `250 lira` is read correctly
  (`iki yüz elli`). With the repo normaliser (`on dört otuzda`) the reading is right (WER difference is only the
  spacing `otuz da`). The engine's TTS stage always applies the normaliser before Cartesia (docs/TURKISH.md).
- One-word first chunk `Okay,` / `Tamam,` + rest 150 ms later in one context: no silence beyond the single chunk's
  own comma pause (0-90 ms vs 0-160 ms), no pacer underrun (`bench/results/live-checks-2026-10-13.md`).
