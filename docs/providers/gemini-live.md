# Gemini Live — speech-to-speech (S2S)

- Docs (all fetched 2026-10-13):
  - WebSocket reference (client/server messages): https://ai.google.dev/api/live
  - Live API guide (models, limits, VAD, interruptions, tools, transcription, formats, languages):
    https://ai.google.dev/gemini-api/docs/live-guide
  - Session management (lifetime, GoAway, compression, resumption):
    https://ai.google.dev/gemini-api/docs/live-session
  - Tool use: https://ai.google.dev/gemini-api/docs/live-tools
  - Capabilities (voice, transcription): https://ai.google.dev/gemini-api/docs/live-api/capabilities
  - Raw WebSocket quick start (endpoint, key, setup, audio, tool response JSON):
    https://ai.google.dev/gemini-api/docs/live-api/get-started-websocket
  - `FunctionDeclaration` / `FunctionCall` / `FunctionResponse` / `Blob` are defined on the
    GenerateContent reference, which was NOT read (see UNVERIFIED below).
- Auth: API key as the `key` query parameter of the WebSocket URL ("Authentication is handled by
  including your API key as a query parameter"). The pages do not mention a header option;
  `x-goog-api-key` as a header on the WebSocket upgrade is UNVERIFIED (the adapter offers it as
  opt-in `key_in_header`). Browsers/clients use ephemeral tokens (`BidiGenerateContentConstrained`,
  `access_token`); Brisvox is a server and uses the key. The key is URL material: the adapter
  never logs the URL and error texts never contain it; keys are restricted to RFC 3986 unreserved characters `[A-Za-z0-9._-]` (Google's newer keys
  start with `AQ.`; live 2026-10-13).
- Endpoint(s) / regions:
  `wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key=<KEY>`.
  Regions/data residency: UNVERIFIED (not on the pages read). Vertex AI has a separate Live
  endpoint: out of scope.
- Models (live-guide): `gemini-3.8-live` (default for low-latency voice agents; `thinkingLevel`
  must be omitted), `gemini-3.8-live-extended-thinking` (supports `thinkingLevel`),
  `gemini-3.1-flash-live-preview` (legacy, "migrate to 3.8"). The `model` field is `models/<name>`.
  The adapter default is `gemini-3.8-live`.
- Protocol: JSON messages; each client message has exactly one of `setup`, `clientContent`,
  `realtimeInput`, `toolResponse`. The first message is `setup` and the configuration cannot be
  changed afterwards; wait for `setupComplete` before anything else.
  - `setup`: `model`, `generationConfig` {`responseModalities` ["AUDIO"], `speechConfig`
    {`voiceConfig` {`prebuiltVoiceConfig` {`voiceName`}}}}, `systemInstruction` {`parts` [{`text`}]},
    `tools` [{`functionDeclarations` [{`name`, `description`, `parameters`, `behavior`}]}],
    `realtimeInputConfig` {`automaticActivityDetection` {`disabled`, `startOfSpeechSensitivity`
    (`START_SENSITIVITY_HIGH|LOW`), `endOfSpeechSensitivity` (`END_SENSITIVITY_HIGH|LOW`),
    `prefixPaddingMs`, `silenceDurationMs`}, `activityHandling` (`START_OF_ACTIVITY_INTERRUPTS` |
    `NO_INTERRUPTION`), `turnCoverage`}, `inputAudioTranscription` {}, `outputAudioTranscription`
    {}, `sessionResumption` {`handle`}, `contextWindowCompression` {`slidingWindow`
    {`targetTokens`}, `triggerTokens`}, `historyConfig` {`initialHistoryInClientContent`},
    `proactivity`. Unsupported in `generationConfig`: `responseMimeType`, `responseSchema`,
    `stopSequence` and others (see the reference).
  - `realtimeInput`: `audio` {`data` (base64), `mimeType`}, `video`, `text`, `activityStart` {},
    `activityEnd` {} (only valid with automatic detection disabled), `audioStreamEnd` (bool, with
    automatic detection on).
  - `clientContent`: `turns` [Content {`role` `user`|`model`, `parts` [{`text`}]}], `turnComplete`
    (bool; true starts generation from the accumulated prompt).
  - `toolResponse`: `functionResponses` [{`id`, `name`, `response` {object}}]. Quick start:
    `{"toolResponse":{"functionResponses":[{"name":"...","id":"...","response":{}}]}}`. The tool
    guide's SDK samples put `{"result": "ok"}` in `response` and (non-blocking functions only)
    `scheduling` inside it (`INTERRUPT`, `WHEN_IDLE`, `SILENT`).
  - Server messages (each may also carry `usageMetadata` and `voiceActivity`): `setupComplete`;
    `serverContent` {`modelTurn` {`parts` [{`inlineData` {`mimeType`, `data`}}]}, `generationComplete`,
    `turnComplete`, `interrupted`, `inputTranscription` {`text`}, `outputTranscription` {`text`},
    `waitingForInput`, ...}; `toolCall` {`functionCalls` [{`id`, `name`, ...}]};
    `toolCallCancellation` {`ids`}; `goAway` {`timeLeft`}; `sessionResumptionUpdate`
    {`newHandle`, `resumable`}; `voiceActivity` {`type` `ACTIVITY_START|ACTIVITY_END`,
    `audioOffset`}.
  - Frames: the service is known to send its JSON in WebSocket BINARY frames; UNVERIFIED on the
    pages read. The adapter accepts text and binary frames.
- Audio: input raw 16-bit little-endian PCM, mime `audio/pcm;rate=16000`; "the API resamples other
  rates if you set the matching rate in the MIME type". Output 24 kHz PCM. The adapter labels the
  engine's audio with its real rate (the engine's provider rate is 24 kHz, so
  `audio/pcm;rate=24000`) and requires 24 kHz output.
- Keepalive / timeouts: connection lifetime "around 10 minutes"; the server sends `goAway`
  (`timeLeft`, a protobuf Duration such as `"50s"`) before the connection is terminated as
  ABORTED. Without compression audio-only sessions are limited to 15 minutes (audio+video: 2).
  Idle/ping behaviour of the server: UNVERIFIED. The adapter pings after `keepalive_interval` of
  inbound silence and fails the session when nothing arrives within `io_timeout`.
- Finalize / flush / cancel semantics:
  - Turn detection: automatic activity detection by default (`silenceDurationMs` default about
    800 ms, 500 to 800 recommended). With it disabled the client sends `activityStart` /
    `activityEnd` (silence threshold at least 500 ms recommended). Hybrid: client VAD plus
    `audioStreamEnd`.
  - Interruption: `serverContent.interrupted` = true; "only content already sent to the client
    stays in session history"; pending function calls are discarded and the server reports the
    cancelled ids (`toolCallCancellation`). `generationComplete` is not sent for an interrupted
    turn. There is no client message that cancels a response and none that truncates the history
    to what the caller heard. Whether a client `activityStart` interrupts: UNVERIFIED but implied
    by `activityHandling: START_OF_ACTIVITY_INTERRUPTS`.
  - Tools: "the Live API doesn't support automatic tool response handling": the client answers
    every call. `gemini-3.8-live` is asynchronous by default (`behavior` `NON_BLOCKING` or
    `BLOCKING`); `3.8-live-extended-thinking` non-blocking only; the legacy 3.1 model is
    sequential ("the model will not start responding until you've sent the tool response").
- Timestamps / interim results / tool-call deltas: transcripts arrive as chunks in
  `inputTranscription` / `outputTranscription`; `interimInputTranscription` also exists. Tool calls
  arrive whole in one `toolCall` message (no deltas).
- Concurrency / rate limits: the pages link to a separate rate-limit page that was NOT read:
  UNVERIFIED. Context window: 128k tokens for native audio models, 32k for others. Session
  resumption tokens are valid "2 hr after the last session's termination"; resumption and
  compression "allow unlimited extension of session duration".
- Turkish support: Turkish (`tr`) is in the list of 99 supported languages; native audio models pick
  the language automatically and do NOT accept an explicit language code ("restrict via system
  instructions"). The adapter therefore ignores `S2sConfig::language`. Quality: UNVERIFIED
.
- Gotchas found during implementation (design decisions, see `docs/S2S.md`):
  - No response or item ids exist; the adapter mints `gr<n>` and `gr<n>-audio`. A response ends at
    `turnComplete`, `interrupted`, or when a `toolCall` arrives (the model then waits, the answer
    begins a new response). UNVERIFIED whether the server also sends a `turnComplete` after a
    `toolCall`; a `turnComplete` with no open response is ignored.
  - `Truncate` has no wire form: nothing is sent. The server's history ends where the server
    stopped generating, not where the caller stopped listening (generation runs ahead of
    playback), so after a barge-in Gemini's own history can contain words the caller did not hear.
    The engine's local mirror is exact and is what seeds the next session (rotation every ~10
    minutes or any reconnect), which bounds the divergence.
  - `CancelResponse` has no wire form either: the adapter ends the response locally
    (`ResponseDone(Cancelled)`) and drops the rest of it until the turn ends; the caller's next
    `activityStart` interrupts the server.
  - After `activityEnd` and after a `toolResponse` the server answers by itself, so the adapter
    absorbs the engine's following `CreateResponse`. A `CreateResponse` after only a note is sent
    as `{"clientContent":{"turns":[],"turnComplete":true}}` (UNVERIFIED that this starts
    generation).
  - Tool answers are batched: one `toolResponse` carries the answers of all calls of one `toolCall`
    message (sent when the last one is answered or at `CreateResponse`). Results of calls the
    server cancelled or interrupted are not sent.
  - Notes (`Note`) go as a `clientContent` user turn `"[note] ..."` with `turnComplete: false`.
    Whether `clientContent` is accepted in the middle of a conversation by 3.8 is UNVERIFIED (the
    guide describes it for seeding the context).
  - History seeding: after `setupComplete`, one `clientContent` with the turns (user/model),
    `turnComplete: false`, and `historyConfig.initialHistoryInClientContent: true` in the setup.
    Tool calls and results of the history are written as TEXT turns (`[tool call] name(args)`,
    `[tool result] name: output`) because the structured shape of past calls is UNVERIFIED. The
    `historyConfig` semantics themselves are only listed in the reference, not explained.
  - Tool declarations use `parameters` (the guide's hint). Whether the schema must be the OpenAPI
    subset or may be full JSON Schema (`parametersJsonSchema`) is UNVERIFIED; `additionalProperties`
    and similar may be rejected. A live run must test with the engine's real schemas.
  - Session limits are handled by the engine, not the adapter: `session_limit` reports
    `connection_lifetime` (10 minutes), the engine opens a replacement session seeded from its
    mirror a margin before it (2 minutes) or at `goAway`, and switches over while the bot is idle.
    The adapter does not use Gemini's own session resumption (handles); that would keep the
    audio-native context but is a second recovery path for the same problem.
  - `voiceActivity` messages (ACTIVITY_START/END) become `SpeechStarted`/`SpeechStopped`. Whether
    the server sends them by default is UNVERIFIED; when it does not, `interrupted` yields a
    `SpeechStarted` and the start of the next response yields the `SpeechStopped`.

## Live verification (2026-10-13)

Real key, `gemini-3.8-live`, FLEURS speech at real time (16 kHz in, 24 kHz out), provider VAD
(`crates/brisvox-providers/tests/gemini_live/live.rs`, `BRISVOX_LIVE_TESTS=1`). Confirmed:
- Auth: the `key` query parameter works, and so does `x-goog-api-key` on the upgrade
  (`key_in_header = true`). Keys may contain '.' (the adapter rejected `AQ.` keys before this).
- `voiceActivity` is sent by default: order `SpeechStarted, SpeechStopped, UserTranscript
  (input transcription), ResponseStarted, Audio/AssistantTranscript deltas, AudioDone, final
  AssistantTranscript, ResponseDone(Completed)`.
- Tools: the declaration the adapter sends is accepted; `toolCall` arrives BEFORE the turn's
  `turnComplete` (ToolCall then ResponseDone(Completed)); after `toolResponse` + the adapter's
  CreateResponse a new response starts with audio (which of the two triggers it is not isolated).
- Mid-session `clientContent` (S2sControl::Note) is accepted without warning and is used by the
  model (it repeated a name given only in the note).
- Barge-in: caller speech during the answer -> `interrupted` -> ResponseDone(Cancelled) 216 ms
  after the barge-in audio started.
- Latency (this machine, Istanbul): session setup 0.7-1.05 s; end of caller speech -> first audio
  1.2-1.9 s (includes the provider VAD's end-of-speech wait); tool result -> first answer audio
  0.76 s.
- Turkish: input transcript exact ("%80'lik" normalised), spoken answer in natural Turkish.
Still UNVERIFIED: regions/data residency; server idle/ping behaviour; the 10-minute connection
lifetime and goAway (not reached in a short test).
- 2026-10-14 (live tool-calling run): tool declarations must use `parametersJsonSchema` for full JSON Schema;
  `parameters` (OpenAPI subset) rejected a schema with `additionalProperties`/`minimum`, and the
  server refused the whole setup (every connect failed). Fixed in `gemini_live/wire.rs`. Parallel
  calls (3 in one turn) and a 3-round chain then passed live.

