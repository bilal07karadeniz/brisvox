# Deepgram Aura — TTS (WebSocket `/v1/speak`)

Scope: WebSocket streaming TTS ( flush/clear, word timing if offered, cancel, Turkish recorded).
Researched: 2026-10-08. Every fact below was read from the pages listed under Sources on that date
(the fetch tool summarizes pages, so quotes are short; JSON blocks are as shown on the pages unless
marked "illustrative"). Anything not confirmed is marked UNVERIFIED.

Headline findings for the implementer:
- Aura (`/v1/speak`) has NO contexts, NO word timestamps and NO Turkish voice. It is one sequential
  text buffer per connection. Cancel = `Clear`, whose acknowledgement (`Cleared`) is delayed behind
  already-generated audio, so brisvox must drop audio client-side from the moment it sends `Clear`.
- Deepgram's newer model family "Flux TTS" (`/v2/speak`) is described as "Recommended for all new builds",
  has real interrupt semantics (`text_spoken` / `text_remaining`), but is English-only today. See the
  Flux section; it does not change the Turkish answer.
- `Flush` is limited to 20 per 60 s and the Flush page warns "Very frequent flushes can affect audio output quality":
  flush once per bot turn, never per sentence.

- Docs: https://developers.deepgram.com/reference/text-to-speech/speak-streaming (fetched 2026-10-08);
  full source list at the bottom.
- Auth:
  - HTTP handshake header `Authorization: Token <DEEPGRAM_API_KEY>`. The authentication guide documents
    exactly `Authorization: Token YOUR_DEEPGRAM_API_KEY`; the "Sending LLM outputs to a WebSocket" page shows a raw
    Python WebSocket client against `/v1/speak` passing `additional_headers={"Authorization": f"Token {DEFAULT_TOKEN}"}`.
    The speak-streaming reference only lists "an `Authorization` header" (type string) with no prefix.
  - Temporary/short-lived tokens exist (Auth API, `/reference/auth/tokens/grant`); whether they are accepted on
    the `/v1/speak` WebSocket, and with which prefix (`Bearer`), is not stated on any fetched page (the authenticating guide
    gives no Bearer format and does not mention WebSocket auth): UNVERIFIED.
  - Browser subprotocol/query-string auth: not documented on the fetched pages: UNVERIFIED. Brisvox is server side:
    use the header, never put the key in the URL, never log it (Error variants must not contain credentials).
  - Failed upgrade: HTTP 4xx/5xx with response headers `dg-request-id` (included on every request, success or failure) and
    `dg-error` (only on a failed upgrade; detailed reason, also in the response body) (TTS troubleshooting page). HTTP error table
    (errors page): 401 `INVALID_AUTH` ("Invalid credentials."); 401 `INSUFFICIENT_PERMISSIONS` ("User does not have sufficient
    permissions."); 403 `INSUFFICIENT_PERMISSIONS` ("Project does not have access to the requested model."). Map these to
    `Rejected { status }`. That these exact bodies occur on the WS upgrade (rather than only on REST) is UNVERIFIED.
- Endpoint(s) / regions:
  - `wss://api.deepgram.com/v1/speak` (AsyncAPI server `wss://api.deepgram.com/`, channel `/v1/speak`).
    Example URL from the docs: `wss://api.deepgram.com/v1/speak?model=aura-2-thalia-en&encoding=linear16&sample_rate=48000`.
  - Regions: the latency page says "Deepgram's servers are only in the United States" (non-US clients see more
    network latency); self-hosted is the documented alternative. No regional hostnames found: UNVERIFIED that
    none exist (the concurrency table does distinguish "North America" vs "Europe/Australia/India" for Growth; see Limits).
  - Query parameters (speak-streaming reference):

    | Name | Type | Allowed values | Default |
    |---|---|---|---|
    | `model` | string | enum of Aura-1, Aura-2 English and Aura-2 multilingual voices (see Models) | `aura-asteria-en` |
    | `encoding` | string | `linear16`, `mulaw`, `alaw` (streaming only; compressed formats are REST-only) | `linear16` |
    | `sample_rate` | string | `8000`, `16000`, `24000`, `32000`, `48000` (per encoding, see below) | `24000` |
    | `speed` | number | 0.7 .. 1.5 ("Not yet supported in all languages") | `1` |
    | `mip_opt_out` | any | schema type "Any type" | `false` |

  - Encoding/sample-rate combinations (media-output-settings page, "Audio Format Combinations"): `linear16`
    8000, 16000, 24000 (default), 32000, 48000; `mulaw` 8000 (default), 16000; `alaw` 8000 (default), 16000.
    Streaming default (page note): linear16, no container, 24000 Hz. "Currently we only support linear16, mulaw, and alaw
    for our Streaming TTS Websocket." Sample rate does not affect latency (latency page).
  - Containers: the TTS WebSocket "does not support containerized audio formats" (handling-audio-issues page): the stream is raw
    samples (no WAV header; clients who need playback must prepend their own header). The page also recommends
    `container=none` for streaming to an output device (clicks/static), but `container` is not a documented query parameter of the
    streaming reference: whether it is accepted/ignored/rejected on `/v1/speak` is UNVERIFIED; do not send it.
  - Brisvox `TtsConfig` = PCM16 LE mono at a `SampleRate`: `encoding=linear16&sample_rate=<rate>` (8000/16000/24000/32000/48000
    available). For G.711 telephony `mulaw`/`alaw` at 8000 are available directly; the events then carry G.711 bytes,
    not PCM16 (caller must know). Whether a rate the brisvox `SampleRate` type offers is outside this list: check at construction.
- Models (current):
  - Docs recommend Flux TTS for all new builds (English only) and Aura-2 where Flux does not cover the language;
    Aura (Aura-1) is first generation, English only (models-languages-overview). Model string formats:
    Flux `flux-{voice}-en` (`/v2/speak`; the overview page writes `flux-{voice}-{language}`), Aura-2 `aura-2-{voice}-{language}` (`/v1/speak`),
    Aura `aura-{voice}-en` (`/v1/speak`). Aura voices run only on `/v1/speak`, Flux voices only on `/v2/speak` (migration page).
  - Aura-2 voices (reference enum, matches tts-models page; 90 voices): English 41 (amalthea, andromeda, apollo, arcas,
    aries, asteria, athena, atlas, aurora, callista, cora, cordelia, delia, draco, electra, harmonia, helena, hera, hermes,
    hyperion, iris, janus, juno, jupiter, luna, mars, minerva, neptune, odysseus, ophelia, orion, orpheus, pandora, phoebe,
    pluto, saturn, selene, thalia, theia, vesta, zeus; e.g. `aura-2-thalia-en`); Spanish 17 (agustina, alvaro, antonia,
    aquila, carina, celeste, diana, estrella, gloria, javier, luciano, nestor, olivia, selena, silvia, sirio, valerio);
    German 7 (aurelia, elara, fabian, julius, kara, lara, viktoria); French 2 (agathe, hector); Dutch 9 (beatrix, cornelia,
    daphne, hestia, lars, leda, rhea, roman, sander); Italian 9 (cesare, cinzia, demetra, dionisio, elio, flavio, livia, maia,
    melia); Japanese 5 (ama, ebisu, fujin, izanami, uzume). Spanish voices Aquila, Carina, Diana, Javier, Selena support
    English-Spanish codeswitching. A remark that Deepgram is "constantly adding additional language support" was not re-found on
    the fetched pages: UNVERIFIED.
  - Aura-1 voices (English, `aura-<name>-en`): angus, arcas, asteria, athena, helios, hera, luna, orion, orpheus, perseus,
    stella, zeus. No deprecation notice found on the tts-models page.
  - Recommended default for brisvox: `aura-2-thalia-en` (the model used throughout the docs' examples), configurable.
    The voice id in `TtsConfig.voice` is the model string (it carries voice and language together); voice and output settings
    cannot change after connecting ("one WebSocket per conversation", getting-started page).
- Protocol (messages in/out, with field names):
  - All control messages are JSON text frames; audio is binary frames of raw samples in the requested encoding.
    There are NO context ids: a connection has one text buffer and one audio stream.
  - Client -> server (speak-streaming reference):
    ```json
    {"type": "Speak", "text": "<input text>"}
    {"type": "Flush"}
    {"type": "Clear"}
    {"type": "Close"}
    ```
    `Speak`: required `type`, `text`. Behavior of `Speak` without a `Flush` (when does the server start generating?):
    NOT documented (the Flush page only says that in some real-time cases you may need to "force the server to process all (or flush)
    unprocessed speech-to-text data immediately" [sic, the page says speech-to-text], implying some automatic processing; trigger
    unknown): UNVERIFIED. Whether the server inserts whitespace between consecutive `Speak`
    texts is documented only for Flux ("The server doesn't add whitespace between messages"; the migration page says v1 and v2 differ here,
    without saying what v1 does): UNVERIFIED for Aura; keep leading/trailing spaces in the text you send so concatenation is valid.
  - Server -> client (speak-streaming reference):
    ```json
    {"type": "Metadata", "request_id": "<uuid>", "model_name": "<string>", "model_version": "<string>",
     "model_uuid": "<uuid>", "additional_model_uuids": ["<uuid>"]}
    {"type": "Flushed", "sequence_id": 0}
    {"type": "Cleared", "sequence_id": 0}
    {"type": "Warning", "description": "<string>", "code": "<string>"}
    ```
    (JSON reformatted onto fewer lines; the angle-bracket values are the schema's placeholders. `Metadata` requires `type`,
    `request_id`, `model_name`, `model_version`, `model_uuid`; `additional_model_uuids` optional. `Flushed`/`Cleared` require
    `type`, `sequence_id` (integer). `Warning` requires `type`, `description`, `code`.) Binary audio: "Receive audio chunks as they are
    generated", no JSON wrapper. Schema quirk: the spec's `type` enum for `Flushed` and `Cleared` lists both values; use the per-message values.
  - `sequence_id` semantics (a counter of flushes/clears? per connection? starts at 0?): only `0` appears in examples; meaning
    is not documented: UNVERIFIED. Do not rely on it for routing; treat as opaque and log it.
  - When `Metadata` is sent: the migration page says v1 "sends `Metadata`" on open (v2 sends `Connected`); exact timing relative to
    the first audio is UNVERIFIED. Handle it at any time and ignore unknown message types.
  - `Warning` `code` values: no list found on any fetched Aura page (the only listed codes are Flux's, below). Treat unknown
    codes as non-fatal (the Flush page says that past the flush limit "you will receive a warning message"). Warning is not an error: do not end the session.
  - Ordering/termination:
    - After `Flush` the server processes the remaining text, returns the final audio, and confirms with a `Flushed` message
      (Flush page; the exact "It confirms with" wording was not re-found verbatim). Read as: all audio for text sent before the flush
      arrives before `Flushed`. A strict "all binary frames of that text precede `Flushed`" guarantee is not worded as a guarantee:
      UNVERIFIED (verify live; the adapter should fail safe by treating `Flushed` as the end marker and discarding audio that arrives
      after it only if a later `Flush`/`Clear` demands it).
    - After `Clear` the server "will stop sending new audio chunks as soon as possible"; it "will process all remaining audio data"
      before it sends `Cleared` (Clear page). So audio frames CAN still arrive between sending `Clear` and receiving `Cleared`.
    - Binary frames carry no marker saying which flush/utterance they belong to; attribution is purely by order. Therefore brisvox
      can run only ONE active utterance per connection at a time (see mapping).
    - Alignment of binary frames to whole PCM16 samples is not documented (chunk alignment/odd bytes "not covered"):
      UNVERIFIED; the adapter should re-align odd-sized frames across frame boundaries (as the Cartesia adapter does).
  - Close: a client may send a WebSocket Close frame or the optional `{"type":"Close"}` message; the server then closes the
    connection and reports status code 1000 (Normal Closure, "Connection was closed successfully.") (Close page). The reference describes `Close` as
    "Flush the buffer and close the connection gracefully after all audio is generated", while the Close page's conclusion says it
    "ensures the server halts processing and closes the connection promptly" and the page does not address buffered text. The two descriptions
    conflict on whether buffered text is synthesized: UNVERIFIED. For end-of-turn use `Flush` and wait for `Flushed`; use `Close` only at session end.
- Keepalive / timeouts:
  - Session limit: an active WebSocket has "a 60-minute timeout period from the initial connection" (even when active); open a new connection for longer
    sessions (getting-started page). Brisvox: calls longer than 1 h need a transparent reconnect between utterances.
  - No `KeepAlive` message exists in the v1 speak schema and none is mentioned (reference: KeepAlive "not mentioned").
    Idle timeout for `/v1/speak`: not documented. A close code `1011 NET-0001` ("The server failed to receive a message", troubleshooting page;
    the Flux page words the same code "Failed to receive message") exists but whether it is an idle timeout, its duration, and whether a WebSocket
    ping frame resets it, are UNVERIFIED. Flux (v2) documents a 60 s inactivity timeout (`NET-0004`) reset by a WebSocket Ping or Pong; do not
    assume the same for v1. Brisvox rule: if the socket is closed when the next text arrives, reconnect lazily; `warm()` can pre-open a socket.
  - No documented connect timeout; brisvox applies its own (explicit timeout on every await).
- Finalize / flush / cancel semantics:
  - End of an utterance (maps to `TtsInput::Flush`): send `{"type":"Flush"}` after the last `Speak`. The docs state: "It is required
    that the last fragment of speech be `Flush`ed when the LLM is at the end of the LLM response" (send-LLM-outputs page).
    Server returns remaining audio, then `Flushed`. Emit `TtsEvent::Flushed` when `Flushed` arrives.
  - Flush limits: at most 20 `Flush` messages every 60 s; beyond that "you will receive a warning message stating that we cannot process any more
    flush messages" until the 60-second time window has passed (Flush page, getting-started page). So a flush swallowed by the limit never yields `Flushed`:
    the adapter must not wait forever (stall timeout) and should count its own flushes (rate guard). "Very frequent flushes can affect audio output
    quality." Whether `Clear` counts toward the 20: UNVERIFIED.
  - Cancel (barge-in; maps to `TtsSession::cancel`): send `{"type":"Clear"}`. The server resets its text buffer, stops sending new
    audio as soon as possible, and replies `Cleared` after processing remaining audio data. Pending unspoken text in the buffer is discarded
    (the reference calls Clear a "Potentially destructive operation for any text in the buffer"). Brisvox must therefore: (a) send `Clear`, (b) from that moment drop every
    binary frame and `Flushed` that arrives for the cancelled utterance (the consumer already treats the epoch as dead), (c) emit
    `TtsEvent::Cancelled` itself immediately without waiting for `Cleared`, (d) resume mapping new audio to the next utterance only after
    `Cleared` has arrived (frames before `Cleared` are the cancelled utterance's tail), or alternatively close the socket and open a new
    one (v1 migration page: "v1 requires reconnecting the WebSocket" to reset context; a reconnect costs TCP+TLS+handshake, so prefer
    `Clear`). Text sent for the NEXT utterance after `Clear` but before `Cleared`: whether it is kept or cleared is UNVERIFIED; hold the
    next utterance's text until `Cleared` arrives (costs one round trip, avoids the ambiguity).
  - `Clear` when nothing is queued: behavior not documented (presumably still `Cleared`): UNVERIFIED.
  - The reference says Clear "Clear the buffer and start a new audio generation"; the Clear page does not say anything about it. No context state is
    preserved (v1 has no cross-turn prosody state; Flux does).
- Timestamps / interim results / tool-call deltas:
  - Word, character or any timestamps are NOT offered on `/v1/speak`: the server messages are binary audio, `Metadata`, `Flushed`,
    `Cleared`, `Warning` only, and `Metadata` has no timing data (reference page, checked). `TtsEvent::Word` therefore cannot come from the provider.
    Played-text options (design choices, not doc claims): (1) emit no `Word` events and let the consumer fall back to an estimate; (2) approximate
    from text length and the exact audio duration derived from the received bytes (bytes / (rate * 2) for linear16) at `Flushed`, with the
    estimate clearly marked as such. Per-word precision is impossible; interruption truncation accuracy will be coarse (per utterance/segment).
    The adapter documents this ("word timing if offered, else documented").
  - Flux TTS (v2) does not give word timestamps either (checked: only `audio_duration_ms` per turn, `audio_played_ms` on interrupt, and
    `total_audio_duration_ms`), but it reconciles interruptions server-side (`text_spoken`).
- Concurrency / rate limits (api-rate-limits page, "Rate limits apply per project, not per account or API key"):

  | Plan | REST (Aura / Aura-2) | Streaming (Aura / Aura-2) |
  |---|---|---|
  | Pay as You Go | 15 | 45 |
  | Growth | 15 | 60 in North America, 45 in Europe/Australia/India |
  | Enterprise | starting at 25 | Aura: starting at 150; Aura-2: starting at 100 |

  - Secondary projects of a self-serve account are limited to one concurrent stream. Exceeding concurrency returns HTTP `429`
    ("This error indicates that your project has more concurrent requests than allowed", concurrency guide; the guide does not single out TTS or
    streaming); docs recommend exponential backoff retry; Pay As You Go and Growth limits cannot be
    raised, Enterprise can (concurrency guide). The getting-started page also says that new requests get a 429 when in-progress requests reach the project limit.
    Whether a streaming "concurrent request" is counted per WebSocket connection or per active generation: not
    stated: UNVERIFIED (assume per connection). For the 429 on the WS upgrade, the handshake fails with an HTTP status: map to `Rejected { status: 429 }`
    (that a 429 is delivered on the WS upgrade specifically is UNVERIFIED).
  - Brisvox: one WebSocket per call/session (docs: "a single websocket per conversation is required"); at 45 streams on Pay As You Go, a container
    with hundreds of calls needs a Growth/Enterprise plan or multiple projects (note: spreading traffic across multiple projects "violates
    our Terms of Service" per the rate-limits page; secondary projects get 1 concurrent stream anyway).
  - Text limits (getting-started page): up to 2000 characters per request for Aura-2 and Aura-1 (longer input errors with 413 and "no audio file is
    created"); 422 "Unprocessable Content" can be returned "if the client fails to send the request successfully"; character throughput "2400 characters per minute, measured by characters sent over the
    WebSocket"; flush limit 20/60 s; connection limit 60 min. Whether 2000 applies per `Speak` message or per flush/turn or per
    connection request is not stated ("per request"): UNVERIFIED (the latency page also says "2000 characters per request"; the troubleshooting table has
    `1009 BIG-0001` "The input text has too many characters" and `1009 BIG-0000` "The input message is too large", with a 2MB request body limit for
    REST per the errors page). Safe rule for the adapter: split any single `Speak` text to <= ~1000 characters at sentence/space boundaries (a design choice).
    The 2400 chars/min throttle is believed to be the cause of `1008 DATA-0001` ("Too many characters were submitted within a recent time window"; the link between the two
    is an inference: UNVERIFIED), so the adapter should rate-account characters sent (a single
    1000-character reply per 25 s average is far below typical speech, but bursty LLM output can exceed it: measure live).
  - Chunking guidance (tts-text-chunking page, applies to `/v1/speak`): split at sentence ends; suggested sizes 50-100 characters for voice assistants,
    complete sentences for call-center bots, 200-400 characters for long-form. For Flux the server places flush boundaries, no client chunking.
  - Pricing: https://deepgram.com/pricing (read 2026-10-08): per 1K characters, Pay As You Go / Growth: Aura-1 $0.0150 / $0.0135;
    Aura-2 $0.030 / $0.027; Flux TTS $0.0450 / $0.0405 (Flux: promotion through 2026-12-31, each $1 spent on Flux TTS adds $1 credit, up to $500,
    Pay As You Go and Growth). Characters are counted by characters sent (Flux reports `billable_character_count`; for Aura no usage message exists).
  - Error close codes (TTS troubleshooting page; the close "payload"/reason is the code string; message text paraphrased):

    | Close code | Payload | Meaning |
    |---|---|---|
    | 1003 | MESSAGE-0000 | message type not supported (send only text messages) |
    | 1008 | DATA-0000 | message is not a recognized command |
    | 1008 | DATA-0001 | too many characters submitted within a recent time window |
    | 1009 | BIG-0000 | message too large |
    | 1009 | BIG-0001 | text exceeds the character limit |
    | 1011 | NET-0000 | internal server error (retry) |
    | 1011 | NET-0001 | the server failed to receive a message |
    | 1011 | NET-0002 | server failed to send a message to the client |
    | 1011 | NET-0003 | time limit exceeded |

    Map: 1008/1009/1003 -> `Protocol` (with code and payload; never credentials); 1011 NET-0000/0002 -> retryable `Disconnected`;
    1011 NET-0003 (also the 60-min limit? UNVERIFIED which code the 60 min limit uses on v1; Flux uses NET-0003 for its 1 h session limit) -> reconnect; a close while output is owed -> `Disconnected`.
    Whether an error JSON frame precedes the close on v1: not documented (the page shows no error JSON bodies): UNVERIFIED. The reference's
    `Warning` is the only documented JSON error-like frame. ("retry" on NET-0000 and "send only text messages" are paraphrases not confirmed verbatim.)
- Turkish support: NO. Aura-2 languages are English, Spanish, German, French, Dutch, Italian, Japanese (tts-models page and the
  models-languages-overview table: Turkish is not listed for Flux TTS, Aura-2 or Aura; not mentioned as planned). The reference model enum has
  no Turkish voice (suffixes only `-en -es -de -nl -fr -it -ja`). Flux TTS voices are English only. A web search on 2026-10-08 (repeated during the
  fact-check) found no evidence of Turkish either (Deepgram supports Turkish for speech-to-text only). Consequence for brisvox: this adapter is NOT usable for Turkish
  calls; the engine's Turkish TTS must be Cartesia (Sonic 3.x includes `tr`; not re-verified in this fact-check, see cartesia.md) or another provider. Documented as: "Turkish: not supported by
  Deepgram TTS as of 2026-10-08; adapter ships for the supported languages, tests use English voices". Using an English voice for Turkish text
  produces unintelligible/accented output; not tested: UNVERIFIED but do not do it.
- Flux TTS (`/v2/speak`) — noted, NOT the scope of this adapter but the likely better v2 adapter for English (docs: "Recommended for all new builds"):
  - `wss://api.deepgram.com/v2/speak?model=flux-haley-en`, header `Authorization: Token ...` (Flux quickstart). Params: `model` (required, `flux-*`),
    `encoding` (`linear16` default, `mulaw`, `alaw`), `sample_rate` (linear16 8000, 16000, 24000, 32000, 44100, 48000; mulaw/alaw 8000 or 16000), `speed` 0.5-1.5 step 0.05,
    `expressivity` (beta, integer -2..2, default 0), `mip_opt_out`, `tag` (repeatable); unknown and batch-only params (`container`, `bit_rate`, `callback`, `callback_method`, `priority`) are rejected.
    Voices: 13 featured + 23 additional (36), all `flux-{voice}-en`.
  - Client messages: `{"type":"Speak","text":...}`, `{"type":"Flush"}` (ends the turn), `{"type":"Interrupt","playback_offset":{"type":"time_ms","value":2340}}`
    (offset measured from the start of the session's audio, must advance past the previous interrupt's offset; without it the text_spoken/text_remaining split is omitted;
    the example value 2340 is illustrative), `{"type":"Configure","speed":1.15}`, `{"type":"Close"}`. Server: `Connected {request_id, model_name, model_version, model_uuids}`,
    `SpeechStarted {speech_id}`, `SpeechMetadata {audio_duration_ms, input_character_count,
    billable_character_count, controls_applied}`, `SpeechInterrupted {audio_played_ms, text_spoken, text_remaining, metadata}`, `Flushed {speech_id}`,
    `SessionMetadata {total_audio_duration_ms, total_input_character_count, total_billable_character_count}`, `ConfigureSuccess`, `ConfigureFailure`, `Warning {code, description}` (non-fatal; e.g. `NO_ACTIVE_SPEECH`, `NO_SYNTHESIZABLE_TEXT`,
    `SYNTHESIS_RETRYING`, `PRONUNCIATION_WARNINGS`, `INTERRUPT_IN_PROGRESS`, `NO_AUDIO_GENERATED`, `INVALID_INTERRUPT_OFFSET`, plus several markup/pronunciation/break codes), `Error {code, description}` (fatal; the
    socket then closes; codes `MESSAGE-0000`, `DATA-0000`, `DATA-0002`, `BIG-0000`, `NET-0000..NET-0004`). 60 s inactivity timeout (`NET-0004`; WebSocket Ping or Pong resets it), 1 h session limit (`NET-0003`). Interrupt does not
    halt audio already on the wire: the interrupt-handling page says to stop playback locally, now (the `Interrupt` round trip is for context reconciliation), and frames
    arriving between sending `Interrupt` and `SpeechInterrupted` "were already on the wire. Drop them."
  - GA/beta status of Flux TTS: not stated on the fetched pages (only `expressivity` and inline pronunciation are labelled beta/early access): UNVERIFIED.
    A follow-up adapter is the better route than extending this one.
- Mapping to the brisvox trait (crates/brisvox-providers/src/tts.rs) - proposal:
  - One connection per `TtsSession`; query `model=<TtsConfig.voice>&encoding=linear16&sample_rate=<TtsConfig.sample_rate>`; header `Authorization: Token <key>`
    (mark sensitive). `ws://` only for loopback tests (as the Cartesia adapter does).
  - Single-flight utterances: the provider has no ids, so the adapter keeps `current: Option<ContextId>` and a FIFO of utterances awaiting `Flushed`. `Text{ctx}` ->
    `Speak` (if a different context's text arrives while another is unflushed, queue it until that context's `Flushed`; the engine normally flushes a turn before
    starting the next). `Flush{ctx}` -> `Flush`; `Flushed` -> `TtsEvent::Flushed{ctx}` for the oldest flushed context. Binary frames -> `TtsEvent::Audio{ctx, ..}` for the
    context at the head of the FIFO.
  - Pipelining the next utterance's text before the previous `Flushed`: Aura could generate it contiguously, but the attribution of frames to contexts would then rest
    only on counting `Flushed`s; this is allowed by the protocol description but UNVERIFIED (ordering). Safer first version: one in-flight flush at a time.
  - `cancel` -> `Clear`; local forget + `TtsEvent::Cancelled{ctx}` at once (before any socket I/O completes, cancel polled with priority over input); drop frames until `Cleared`;
    a cancel for an already-flushed (finished) or unknown context still yields `Cancelled`; a Clear for a context with nothing sent is not sent. Text for cancelled ids is ignored (remember last 64 ids).
  - Words: none from the provider (see Timestamps). Do not emit invented `Word` events as if provider-supplied unless explicitly decided (and record the decision in the design docs).
  - Flush rate guard: refuse to send more than ~20 `Flush`/60 s (error `Protocol` or merge flushes); stall deadline only while a `Flushed` is owed.
  - Reconnect: socket closed while idle -> lazy reconnect on next `Text`; 60 min limit -> same.
- Gotchas found during implementation (adapter `brisvox_providers::deepgram_aura`; all verified
  against a local mock only, nothing live yet):
  - The mock-free reading of the docs is implemented as: one FIFO of engine contexts, only the head on the wire;
    `Clear` on cancel with local-first `Cancelled`, frames and `Flushed` dropped until `Cleared`, the next
    context's text held until then; a `Cleared` that does not arrive within `io_timeout` of the last frame is
    given up on with a warning (UNVERIFIED #2 above: if the server does not answer a `Clear` on an empty buffer
    this costs one deadline once; if it answers late, a stale `Cleared` can be matched to a later `Clear`).
    A `Clear` is only sent when the server may hold text (written on the current connection, unflushed or a
    `Flushed` owed), so the empty-buffer case is not provoked by a cancel during a reconnect or of an answered
    context. A `Flushed` that follows `Cleared` directly (ordering UNVERIFIED, open item 1) would be taken
    for the answer to the next utterance's flush if that was already sent.
  - `Metadata` may arrive on a parked (warmed) socket before anything is sent: the "is it still open?" probe consumes
    benign text frames instead of treating them as a dead socket.
  - Fault injection counts `TtsEvent`s, not wire frames: the mock does not count `Metadata`.
  - Not implemented / left out: `mip_opt_out`; Flux TTS (`/v2/speak`); `Close` is sent only at the end of a session after the
    input was dropped (the Flush-vs-halt ambiguity of `Close` does not matter there because everything is flushed first);
    no keepalive message (no documented idle timeout); no JWT/Bearer auth. A WebSocket `Ping` is sent only as a liveness probe
    while unflushed text is on the server and nothing is owed (half an `io_timeout` after the last sign of life); that Aura
    answers pings (RFC 6455 says it must) is UNVERIFIED, the live test shows it. If it does not, a turn whose LLM is
    slower than `io_timeout` between the first text and the flush ends the session with a `Timeout`: switch to `PerText`.
  - Guessed around (UNVERIFIED in the docs): whether `Speak` without `Flush` starts generation (both are handled: the mock has an `auto`
    switch and `FlushPolicy` picks the strategy), whether `Flushed` always follows all frames of its flush (assumed; late frames
    would be attributed to the next utterance), whether the frames are whole samples (odd ones are re-aligned), whether
    `Clear` counts toward the 20 flushes (assumed not), what the 2000-character limit applies to (pieces are cut at 1000 and no flush interval carries more than 1800 characters), which close code
    ends a 60-minute connection (any close while idle reconnects; the connection is replaced before it at 55 minutes).
  - A 1008 `DATA-0001`, 1009 or 1003 close is `Error::Protocol` even while idle (it means we sent something wrong); 1011 and every
    other close are `Disconnected`, benign when no text of an unfinished utterance is on the server.
  - Voice ids are checked by their language suffix (the last `-` or `_` separated part, e.g. `aura-2-ozge-tr`, `tr_TR`), compared
    case-insensitively: `aura-2-ozge-TR` is refused like `-tr` (review fix). Only the seven languages above pass.
  - Test coverage that pins the behaviour above (review fixes; `tests/deepgram_aura.rs`, mock in `tests/deepgram_aura/mock.rs`):
    - Bounded queue: with a consumer that reads nothing, the mock (32 MB of audio, no pacing) is held back by TCP flow control after a
      bounded amount (at most a quarter of the frames are sent); the adapter stops reading the socket at 64 queued events.
    - `Clear`: the mock can discard `Speak` text that arrives between `Clear` and `Cleared` (the docs allow it); the barge-in tests send
      the next utterance immediately and still get complete audio, so the hold until `Cleared` is proven, not assumed. A connection
      lost while a `Clear` is pending resets that state (the next utterance does not wait for the stall deadline).
    - Dead socket: the first `Speak` of a context written to a socket that was reset (the session was not reading, the consumer was
      behind) is retried once on a new connection; a server that drops every connection gets exactly that one retry. The mock resets
      with SO_LINGER 0. The `exactly one retry` check races the reset of the second connection against the write, so it detects an
      endless reconnect loop reliably but not on every run of a broken build.
    - Alignment: mu-law/A-law frames pass through byte for byte (each event is one server frame, an odd total keeps its last byte);
      only PCM16 is re-aligned to whole samples.
    - Limits: the defaults of `DeepgramAuraConfig::new` (20 flushes, 2400 characters, 60 s, 1000 per `Speak`, 55 min, 10 s), the
      flush reserve (4) and the mid-turn threshold (1800) are asserted. Flush intervals are measured in characters from what the
      server received: exactly 1800 unflushed characters are allowed, 1801 force a flush first. The character budget, the 1000
      character split and the 1800 threshold count characters, not bytes (tested with `ñ` and Japanese).
    - `PerText` after `Flushed`: a pause far above `io_timeout` is neither a timeout nor a ping (nothing is owed); the `PerTurn` pause
      test covers the probe path (unflushed text on the server).
    - Idle close codes: 1008/1009/1003 while idle is an error; 1011 and 1000 reconnect lazily.

## Open items to settle with a live test
Run with `BRISVOX_LIVE_TESTS=1 DEEPGRAM_API_KEY=...` (never print the key).
1. Whether all binary frames of a flush precede `Flushed`, and whether `sequence_id` counts flushes/clears (per connection or per kind).
2. What `Clear` does to text sent between `Clear` and `Cleared`; how much audio arrives after `Clear`; whether `Clear` with an empty buffer still answers `Cleared`.
3. When the server starts generating without `Flush` (sentence-boundary auto-trigger?), and whether `Speak` text needs leading spaces.
4. Idle timeout of `/v1/speak` and whether a ping frame resets it; which close code/payload the 60 min limit uses.
5. The warning `code` the server sends when the 20 flushes/60 s limit is hit, and whether the connection survives; whether `Clear` counts.
6. What the 2000-character limit applies to (per `Speak`, per flush, per connection) and the exact effect of the 2400 chars/min throttle (`1008 DATA-0001`).
7. Time to first audio with `linear16` at 16000/24000 and `mulaw` 8000; whether frames are whole-sample aligned.
8. Whether `container=none` or unknown query params are accepted/ignored/rejected; whether a JWT (`Bearer`) is accepted on `/v1/speak`.
9. Whether concurrency (429) is counted per connection or per active generation.
10. Whether a Turkish text sent to an Aura-2 voice is rejected, spoken badly, or rejected by language (expect: not supported).

## Sources (all fetched 2026-10-08)
- https://developers.deepgram.com/reference/text-to-speech/speak-streaming (and .md) - endpoint, query parameters, messages, model enum
- https://developers.deepgram.com/docs/tts-websocket.md - intro only
- https://developers.deepgram.com/docs/streaming-text-to-speech.md - getting started, limits (2000 chars, 2400 chars/min, 20 flush/60 s, 60 min)
- https://developers.deepgram.com/docs/tts-streaming-feature-overview.md - only links to Flush/Clear/Close pages, no descriptions
- https://developers.deepgram.com/docs/tts-ws-flush.md, https://developers.deepgram.com/docs/tts-ws-clear.md, https://developers.deepgram.com/docs/tts-ws-close.md
- https://developers.deepgram.com/docs/tts-websocket-streaming.md - SDK examples (WebSocket)
- https://developers.deepgram.com/docs/send-llm-outputs-to-the-tts-web-socket.md - raw WebSocket URL and `Authorization: Token` header example
- https://developers.deepgram.com/docs/tts-troubleshooting-websocket-net-and-data-errors.md - close codes, `dg-request-id`/`dg-error`
- https://developers.deepgram.com/docs/errors.md - HTTP errors (401/403/413/422/429)
- https://developers.deepgram.com/reference/api-rate-limits.md and https://developers.deepgram.com/docs/working-with-concurrency-rate-limits.md - concurrency
- https://developers.deepgram.com/docs/tts-models.md and https://developers.deepgram.com/docs/tts-models-languages-overview.md - voices, languages (Turkish not listed)
- https://developers.deepgram.com/docs/tts-encoding.md, https://developers.deepgram.com/docs/tts-sample-rate.md, https://developers.deepgram.com/docs/tts-media-output-settings.md
- https://developers.deepgram.com/docs/handling-audio-issues-in-text-to-speech.md, https://developers.deepgram.com/docs/text-to-speech-latency.md, https://developers.deepgram.com/docs/tts-text-chunking.md
- https://developers.deepgram.com/guides/fundamentals/authenticating - `Authorization: Token` scheme (general API)
- https://developers.deepgram.com/docs/flux-tts/overview.md, /quickstart.md, /client-messages.md, /server-messages.md, /interrupt-handling.md, /migrating.md, /voices.md
- https://deepgram.com/pricing - TTS prices
- https://crates.io/api/v1/crates/deepgram - Rust crate version 0.12.0, MIT
- https://developers.deepgram.com/llms.txt - doc index
- Web search "Deepgram Aura-2 Turkish language support text-to-speech" (no Turkish evidence)

Fact-checked 2026-10-08: 85 claims checked, 12 corrected. Corrections: (1) Flush-page wording: the "forces the server to process all unprocessed text immediately" quote is actually "force the server to process all (or flush) unprocessed speech-to-text data immediately"; (2) the quality warning is "Very frequent flushes can affect audio output quality" (not "degrade"); (3) close code 1011 NET-0001 means "server failed to receive a message", not "didn't receive a client message in time" (the idle-timeout reading is now UNVERIFIED); (4) Growth non-NA streaming limit applies to Europe/Australia/India, not "elsewhere"; (5) Clear quote: reference says "Potentially destructive operation for any text in the buffer" (not "This can discard buffered text"), and Clear wording "Clear the buffer and start a new audio generation"; (6) Clear page says server "will process all remaining audio data"; (7) Close reference/page quotes corrected ("Flush the buffer and close the connection gracefully after all audio is generated"; page conclusion "ensures the server halts processing and closes the connection promptly"); (8) 429 text comes from the concurrency guide (rate-limits page has none) and is not TTS-specific; (9) the 2000-character limit is worded "per request" on both pages; (10) Flux `sample_rate` also allows 44100 for linear16, `expressivity` default 0, and the interrupt-handling quotes were fixed ("Stop local audio playback immediately" is not verbatim); (11) removed unconfirmable quote "constantly adding additional language support" (now UNVERIFIED) and the unconfirmed claim that 1008 DATA-0001 is a hard disconnect caused by the 2400 chars/min throttle (now an inference, UNVERIFIED); (12) pricing promotion details clarified, 413/422/Node-SDK/dg-error details sharpened (dg-error only on failed upgrade). Remaining UNVERIFIED are marked inline (Bearer/JWT on WS, container param, Speak auto-trigger, whitespace on v1, sequence_id, v1 idle timeout, ordering guarantee, per-Speak 2000 scope, Flux GA status, Cartesia `tr` not re-checked here).

## Live check, prosody (2026-10-13)
Per the 2026-10-08 model list (not re-fetched today) Aura-2 has no Turkish voice, so Aura is checked in
English only (`aura-2-thalia-en`, `linear16` 24 kHz, `FlushPolicy::PerTurn`). Open item 10 (Turkish text) not run.
- Open item 3 (does generation start without a Flush): for a complete sentence yes (earlier live test); for a one-word
  chunk `Okay,` NO: the first audio arrived 413-418 ms after the first text, i.e. only after the second chunk (150 ms
  later) and the flush, against 257-271 ms for the same answer as one chunk. A one-word first chunk gives no first-audio
  gain on Aura and delays it by about the inter-chunk time.
- Silence between `Okay,` and the rest: 290 ms in one of two runs (three 80-100 ms pauses in the other); the single
  chunk had 90 ms and a 260 ms lead-in. No word timestamps exist, so the position is measured from the PCM.
- No pacer underrun (the one-word burst is longer than the wait).
