# RTVI (Real-Time Voice Inference) and the Pipecat WebSocket wire format — protocol

Researched 2026-10-08. Every fact below was fetched on that date from the sources
listed; anything not confirmed is marked UNVERIFIED. Nothing here is from memory.
Fact-checked 2026-10-08 (see the last section).

- Docs / sources (all fetched 2026-10-08):
  - RTVI standard (docs): https://docs.pipecat.ai/client/rtvi-standard (also `.md` variant)
  - Pipecat server RTVI docs: https://docs.pipecat.ai/server/frameworks/rtvi/rtvi-processor
  - JS WebSocket transport docs: https://docs.pipecat.ai/client/js/transports/websocket
  - Pipecat source, tag `v1.12.0` (published 2026-09-26, tag object `c8d5d28c...`, commit
    `1559a684b1ee9771b36454b72418d7364b518e7f` (commit hash not re-checked in the fact-check)).
    `main` HEAD on 2026-10-08 was `a3b149f834d1791249b8b258960b111318d119b4` (commit date
    2026-10-08T03:09:05Z). Files used:
    - https://github.com/pipecat-ai/pipecat/blob/v1.12.0/src/pipecat/processors/frameworks/rtvi/models.py
    - .../rtvi/processor.py, .../rtvi/observer.py (same directory)
    - https://github.com/pipecat-ai/pipecat/blob/v1.12.0/src/pipecat/frames/frames.proto
    - https://github.com/pipecat-ai/pipecat/blob/v1.12.0/src/pipecat/serializers/protobuf.py
    - https://github.com/pipecat-ai/pipecat/blob/v1.12.0/src/pipecat/serializers/rtvi_client.py
    - https://github.com/pipecat-ai/pipecat/blob/v1.12.0/src/pipecat/transports/websocket/fastapi.py
  - JS client, tag `client-js-v1.13.1` (npm 2026-09-04, commit
    `936217a73c17db46f64154e4879c8218cc7bd55d`):
    https://github.com/pipecat-ai/pipecat-client-web/tree/client-js-v1.13.1/client-js
    (`rtvi/messages.ts`, `client/client.ts`, `client/transport.ts`)
  - JS WebSocket transport, tag `websocket-transport-v1.7.2` (npm 2026-09-15, commit
    `b1cd6483a555f7d56144f0f5bdfb64d565394442`):
    https://github.com/pipecat-ai/pipecat-client-web-transports/tree/websocket-transport-v1.7.2/transports/websocket-transport
    (`src/webSocketTransport.ts`, `src/serializers/protobufSerializer.ts`, `proto/frames.proto`)
  - Reference server example: https://github.com/pipecat-ai/pipecat-examples/tree/main/websocket
    (`bot.py`, main HEAD `93c0bd5f31e18d5d182a49f4a5eeead60b640db6`, commit date 2026-09-23)
  - npm registry: https://registry.npmjs.org/@pipecat-ai/client-js and
    .../@pipecat-ai/websocket-transport; crates.io API for prost.
- Licenses: Pipecat (Python), the JS client and the WebSocket transport are BSD-2-Clause
  (SPDX headers in the files; npm registry says `BSD-2-Clause` for both `client-js` and
  `websocket-transport`). If the `.proto` text is copied into this repo, keep the Daily
  copyright and license header. A Rust re-implementation of the same wire layout via prost
  structs needs no copied text (design option, not legal advice).
- Auth: none in the protocol. The JS WebSocket transport accepts an optional `token` connect
  param and appends it as a query parameter: `?token=<urlencoded>` (or `&token=` if the URL
  already has `?`). Authentication is the server operator's business (UNVERIFIED how Pipecat's
  own FastAPI example consumes it; the example `bot.py` contains no token handling).
- Endpoint(s) / regions: any WebSocket URL the client is given: `connect({ wsUrl })` (`ws_url` is
  a deprecated alias) or `startBotAndConnect({ endpoint })`, where the HTTP endpoint returns
  connection params (`{ wsUrl }`; shape UNVERIFIED in this pass). The example README says the client calls
  `POST /start` on `http://localhost:7860` before opening the WebSocket; the WebSocket path is not
  given there (UNVERIFIED; an earlier draft said `/ws` and `/api/start`, not confirmed). No path
  convention is required by the client.

## Versions (current on 2026-10-08)

| Component | Version | Date | Notes |
|---|---|---|---|
| RTVI protocol constant | `"2.1.0"` | v1.12.0 | `PROTOCOL_VERSION` in `models.py`, `RTVI_PROTOCOL_VERSION` in client-js `rtvi/messages.ts`. Both identical. |
| Pipecat (Python) | v1.12.0 | 2026-09-26 | GitHub latest release. `LEGACY_SUPPORTED_MAJOR = 1`: 1.x clients are accepted. |
| `@pipecat-ai/client-js` | 1.13.1 | 2026-09-04 | npm `latest`. BSD-2-Clause. Deps: uuid, bowser, events, clone-deep, typed-emitter, @types/events. |
| `@pipecat-ai/websocket-transport` | 1.7.2 | 2026-09-15 | peer dep `@pipecat-ai/client-js ~1.13.0`. Deps include `@daily-co/daily-js ^0.90.0`, `@protobuf-ts/runtime ^2.11.0`, `@protobuf-ts/plugin ^2.11.0`, `x-law ^0.3.1`. |

The docs page title is "The RTVI Standard" and its body says "This page documents version 1.0 of
the RTVI standard, released in June 2025"; it also says five messages (`user-mute-started`,
`user-mute-stopped`, `llm-function-call-started`, `llm-function-call-in-progress`,
`llm-function-call-stopped`) were "Introduced in RTVI version 1.2.0 (Pipecat 0.0.102, client-js
1.6.0)". These numbers are the page's own release labels and do NOT match the wire version
`2.1.0` that both implementations carry (the server docs page also states "The server protocol
version is 2.1.0"). Treat the source constants as authoritative; the doc page is internally
inconsistent. Per the docs page: `bot-transcription` deprecated in Pipecat 0.0.95 / client-js
1.5.0; `llm-function-call` deprecated in Pipecat 0.0.102 / client-js 1.6.0.
Protocol version history: UNVERIFIED beyond what the code says (client warns if bot major < 2;
the multi-key DTMF form needs bot protocol 2.1.0+, older bots expect a legacy single `button`
message per key; `DTMFInputData` in `models.py` has only `buttons`).

## Transports the Pipecat client SDKs use

- WebSocket: `@pipecat-ai/websocket-transport` (this doc's focus). Docs call it "best suited for
  server-server applications and prototyping client/server apps"; a WebRTC-based transport is
  recommended for client/server production apps.
- Daily (WebRTC): `daily-transport` (npm latest 1.6.9, 2026-09-15). RTVI messages travel over the
  Daily data channel app messages (UNVERIFIED details; not fetched).
- SmallWebRTC: `small-webrtc-transport` (npm latest 1.10.8, 2026-09-15). RTVI over a WebRTC data
  channel (UNVERIFIED).
- LiveKit: `livekit-transport` (npm latest 1.0.0, 2026-09-04). Other transports named in the
  earlier draft (openai-realtime-webrtc, gemini-live-websocket, moq): UNVERIFIED, not re-checked.
- The RTVI JSON envelope is identical on all transports; only the carrier differs. On WebSocket
  the carrier is a binary protobuf `MessageFrame` (below), NOT a bare JSON text frame.

## Envelope (identical on every transport)

```json
{ "label": "rtvi-ai", "type": "<string>", "id": "<string>", "data": { } }
```

- `label`: always `"rtvi-ai"`. The JS WebSocket transport silently drops any message whose
  label differs (`if (parsed.message.label === "rtvi-ai")`). Pipecat's processor drops (warning
  log `Ignoring not RTVI message`, no reply) incoming messages with another label.
- `type`: kebab-case string (tables below).
- `id`: the JS client generates `uuidv4().slice(0, 8)` (8 hex chars) for each message. The
  Pipecat base `Message` model that validates ALL incoming client messages declares `id: str`
  with no default, so a client message without `id` fails validation (`error` "Invalid RTVI
  transport message"). Replies (`bot-ready`, `server-response`, `error-response`) reuse the
  request id. Server-initiated events (`user-started-speaking`, `bot-llm-text`, `error`,
  `server-message`, ...) are separate pydantic models in `models.py` with NO `id` field, so
  Pipecat v1.12.0 emits no `id` on them. The JS client never reads `id` on server events.
- `data`: for payload-less server events (e.g. `user-started-speaking`) the Pipecat model has no
  `data` field and `push_transport_message` uses `exclude_none=True`, so `data` is omitted.
  Incoming `disconnect-bot` from the JS client sends `data: {}`; Pipecat does not read it.

## Protocol (messages in/out, with field names)

Source of truth for the server side is `models.py` (v1.12.0); client side is
`client-js/rtvi/messages.ts` (v1.13.1). "Seen by JS client" = a `switch` case exists in
`client.ts handleMessage`; unknown types are logged at debug level (`Unrecognized message type`)
and ignored.

### Client to server

| `type` | `data` | Pipecat v1.12.0 server behavior |
|---|---|---|
| `client-ready` | `{ "version": "2.1.0", "about": { "library": "<pkg name>", "library_version": "<ver>", "platform"?, "platform_version"?, "platform_details"? } }` | Parses `version` (must split into exactly 3 integers). Same major as server (2) or legacy major 1: accepted. Missing/invalid/other major: sends `error-response` ("Client version unknown." / "Invalid client version format (...)." / "RTVI version ... is not compatible with server protocol 2.1.0.", each with " Compatibility issues may occur." appended) but the connection continues. Then pushes `InputTransportStartAudioStreamingFrame`, remembers the request id, fires `on_client_ready`. |
| `disconnect-bot` | `{}` (JS sends empty object; docs: data `undefined`) | Pushes `EndWorkerFrame` downstream. Payload never read. No reply. |
| `client-message` | `{ "t": "<name>", "d": <any, optional> }` | Pushed as `RTVIClientMessageFrame` (with `msg_id`), fires `on_client_message`. App answers with `send_server_response`. |
| `send-text` | `{ "content": "<text>", "options"?: { "run_immediately"?: true, "audio_response"?: true } }` (both options default `true`) | If `run_immediately`: interrupts the bot and flushes the pipeline. If `audio_response=false`: TTS skipped for this turn (`LLMConfigureOutputFrame(skip_tts)`), restored afterwards. Then appends `{"role":"user","content":...}` via `LLMMessagesAppendFrame(run_llm=run_immediately)`. |
| `append-to-context` | deprecated; JS `appendToContext()` logs a deprecation warning on every call | NO case in the `processor.py` v1.12.0 dispatch, so it gets an `error-response` `Unsupported type append-to-context`. The JS enum still has `append-to-context` and `append-to-context-result`. |
| `llm-function-call-result` | `{ "function_name", "tool_call_id", "arguments": {..}, "result": {..} or "<string>" }` | Result goes to the LLM as a `FunctionCallResultFrame`. |
| `dtmf` | `{ "buttons": ["1","2","#"] }` (min 1; keys `0-9 * #`) | One `InputDTMFFrame` per key, in order. Multi-key `buttons` form needs protocol 2.1.0+ (JS comment). Not on the docs standard page. |
| `ui-snapshot` / `ui-event` / `ui-cancel-job-group` | UI worker protocol (see `models.py`) | Optional; ignore for voice-only. |
| `raw-audio`, `raw-audio-batch` | `{ "base64Audio": "<b64>", "sampleRate", "numChannels" }`; batch uses `"base64AudioBatch": [<b64>, ...]` (else falls back to `base64Audio`) | Handled by the processor (one `InputAudioRawFrame` per decoded chunk; missing `sampleRate`/`numChannels` is caught and logged). There is no pydantic model for these in `models.py`. The Python `RTVIClientSerializer` emits `raw-audio` (`id` from a counter, base64 ASCII). Not used by the JS client, which sends audio as protobuf frames. |

### Server to client

| `type` | `data` | Seen by JS client (callback) |
|---|---|---|
| `bot-ready` | `{ "version": "2.1.0", "about"?: { "library": "pipecat-ai", "library_version": "<pipecat ver>" } }` | yes (`onBotReady`; resolves the pending `connect()` promise; warns if major < 2; missing version treated as 0.0.0) |
| `error` | `{ "error": "<string>", "fatal": <bool> }` | yes (`onError`; `fatal: true` makes the client disconnect) |
| `error-response` | `{ "error": "<string>" }`, id = request id | yes (rejects the pending request, `onMessageError`) |
| `server-response` | `{ "t": "<client-message t>", "d": <any> }`, id = request id | yes (resolves the pending request) |
| `server-message` | any JSON value | yes (`onServerMessage`) |
| `user-started-speaking`, `user-stopped-speaking` | none | yes |
| `vad-user-started-speaking`, `vad-user-stopped-speaking` | none (default off in the observer) | no case in `handleMessage` (debug log) |
| `bot-started-speaking`, `bot-stopped-speaking` | none | yes |
| `bot-interrupted` | none (fires on every `InterruptionFrame`, gated by observer `bot_speaking_enabled`) | NO case in client-js 1.13.1 (debug log only) |
| `user-mute-started`, `user-mute-stopped` | none | yes |
| `user-transcription` | `{ "text", "user_id", "timestamp", "final": <bool> }` (interim = `final:false`) | yes (`onUserTranscript`) |
| `user-llm-text` | `{ "text" }` | yes |
| `bot-output` | `{ "text", "aggregated_by": "sentence"/"word"/"code"/"url"/..., "segment_id"?, "spoken"? (v1 clients), "will_be_spoken"?, "spoken_status"?: "new"/"in-progress"/"completed", "spoken_progress"?: { "accumulated_text", "remaining_text" } (2.0+ clients) }` | yes (`onBotOutput`) |
| `bot-transcription` | `{ "text" }` DEPRECATED (use `bot-output`); Pipecat still emits it when a sentence ends | yes (`onBotTranscript`) |
| `bot-llm-started`, `bot-llm-stopped` | none | yes |
| `bot-llm-text` | `{ "text" }` (streamed token/chunk) | yes |
| `bot-llm-marker` | `{ "text", "kind"?, "raw", "markers": [] }` (off by default, eval only) | no |
| `bot-tts-started`, `bot-tts-stopped` | none | yes |
| `bot-tts-text` | `{ "text" }` | yes |
| `metrics` | `{ "processing"?: [{"processor","value","model"?}], "ttfb"?: [...], "characters"?: [...] }` per the docs page; the observer also builds keys `ttfa`, `ttfat`, `tokens`, `stt_usage` | yes |
| `llm-function-call-started` | `{ "function_name"? }` | yes |
| `llm-function-call-in-progress` | `{ "tool_call_id", "function_name"?, "arguments"? }` | yes (JS invokes a registered handler by `function_name`) |
| `llm-function-call-stopped` | `{ "tool_call_id", "cancelled": <bool>, "function_name"?, "result"? }` | yes |
| `llm-function-call` | `{ "function_name", "tool_call_id", "args" }` DEPRECATED since Pipecat 0.0.102 | yes |
| `bot-llm-search-response` | `{ "search_result"?, "rendered_content"?, "origins"?: [{ "site_uri"?, "site_title"?, "results": [{ "text", "confidence": [0.0] }] }] }` (per the docs page; no class in `models.py` v1.12.0, so whether the v1.12.0 server emits it is UNVERIFIED) | yes |
| `bot-tts-audio` | `{ "audio": "<b64>", "sample_rate", "num_channels" }` | model exists; v1.12.0 observer does NOT emit it (its TTS audio only feeds `bot-audio-level`) |
| `user-audio-level`, `bot-audio-level` | `{ "value": <float> }` (off by default; throttled by `audio_level_period_secs`, default 0.15) | no |
| `system-log` | `{ "text" }` (off by default) | no |
| `ui-command`, `ui-job-group` | UI worker protocol | yes (not needed) |

Function-call fields (`function_name`, `arguments`, `result`) are only populated at the
observer's `function_call_report_level` NAME/ARGUMENTS/FULL; default is `{"*": NONE}`.
Observer defaults (v1.12.0): `bot_output`, `bot_llm`, `bot_tts`, `bot_speaking`, `user_llm`,
`user_speaking`, `user_mute`, `user_transcription`, `metrics` enabled; `bot_llm_marker`,
`vad_user_speaking`, `user_audio_level`, `bot_audio_level`, `system_logs` disabled.

Not present in the v2 protocol sources: the old v0/1.0 `config` / `action` /
`describe-config` / `describe-actions` message family. The server doc page still says the
`bot-ready` response contains the protocol version, "Current service configuration" and available
actions, which does not match the `BotReadyData` model (`version`, `about` only) nor the
standard page. Treat actions/config as removed. (UNVERIFIED for 1.x legacy clients; they are
accepted with only the `bot-output` legacy behavior mentioned in the docs.)

### JSON examples (envelope per `models.py`; values illustrative)

```json
{"label":"rtvi-ai","type":"client-ready","id":"a1b2c3d4","data":{"version":"2.1.0","about":{"library":"@pipecat-ai/client-js","library_version":"1.13.1"}}}
{"label":"rtvi-ai","type":"bot-ready","id":"a1b2c3d4","data":{"version":"2.1.0","about":{"library":"pipecat-ai","library_version":"1.12.0"}}}
{"label":"rtvi-ai","type":"user-started-speaking"}
{"label":"rtvi-ai","type":"user-transcription","data":{"text":"merhaba","user_id":"","timestamp":"2026-10-08T10:00:00.000+00:00","final":true}}
{"label":"rtvi-ai","type":"bot-llm-started"}
{"label":"rtvi-ai","type":"bot-llm-text","data":{"text":"Merhaba"}}
{"label":"rtvi-ai","type":"bot-llm-stopped"}
{"label":"rtvi-ai","type":"bot-tts-started"}
{"label":"rtvi-ai","type":"bot-tts-text","data":{"text":"Merhaba, nasilsiniz?"}}
{"label":"rtvi-ai","type":"bot-started-speaking"}
{"label":"rtvi-ai","type":"bot-output","data":{"text":"Merhaba, nasilsiniz?","aggregated_by":"sentence","spoken_status":"completed","will_be_spoken":true}}
{"label":"rtvi-ai","type":"bot-stopped-speaking"}
{"label":"rtvi-ai","type":"error","data":{"error":"stt connection lost","fatal":false}}
{"label":"rtvi-ai","type":"client-message","id":"e5f6a7b8","data":{"t":"get-weather","d":{"city":"Ankara"}}}
{"label":"rtvi-ai","type":"server-response","id":"e5f6a7b8","data":{"t":"get-weather","d":{"temp":21}}}
{"label":"rtvi-ai","type":"error-response","id":"e5f6a7b8","data":{"error":"Unsupported type foo"}}
{"label":"rtvi-ai","type":"send-text","id":"c9d0e1f2","data":{"content":"hello","options":{"run_immediately":true,"audio_response":true}}}
{"label":"rtvi-ai","type":"server-message","data":{"any":"json"}}
{"label":"rtvi-ai","type":"metrics","data":{"ttfb":[{"processor":"CartesiaTTSService#0","value":0.21,"model":"sonic"}]}}
```
The `client-ready` JSON is built from `RTVIMessage.clientReady()` (`version`, `about.library`,
`about.library_version`, constructed by the JS code; the id is random). The `bot-ready` JSON
mirrors `BotReadyData` and the processor's default `about`. All other examples are constructed
from the model field names above, not captured from a live run (UNVERIFIED as byte-exact).
The `timestamp` is typed as a plain string in both `models.py` and the docs page; the format is
unspecified (UNVERIFIED; ISO-8601 in practice).

## Handshake and ordering

1. Client opens the WebSocket (`ReconnectingWebSocket`; `ws.on("open")` only logs).
2. `PipecatClient.connect()`: init devices (mic) if needed, `transport.connect()` (WebSocket + media
   manager, transport state `connected`), then `transport.sendReadyMessage()`. The
   WebSocket transport's `sendReadyMessage()` does `this.state = "ready"` and sends
   `client-ready` immediately (the transport state becomes `ready` at that moment, NOT when
   `bot-ready` arrives).
3. Mic audio captured before state `ready` is queued (unbounded queue) and flushed afterwards.
   Audio therefore starts flowing right after `client-ready`, before `bot-ready` may have
   arrived. The server must tolerate audio frames before it replies.
4. Server replies `bot-ready` with the same `id` as `client-ready`. In Pipecat the app must call
   `await rtvi.set_bot_ready()` (typically in `on_client_ready`); the processor does not send it
   by itself (source `processor.py`; the docs: "The server must mark the bot as ready before it
   can process client messages"). Its `version` is the server protocol version, except that a 1.x
   client gets its own declared version echoed back.
5. The JS `connect()` promise resolves on the first `bot-ready` and has NO timeout in
   `client.ts` (it waits indefinitely). The JS client does not check the id of `bot-ready`
   (confirmed in `client.ts`).
6. Ordering observed in the Pipecat observer (v1.12.0): aggregated bot text is queued until
   `BotStartedSpeakingFrame`; then `bot-started-speaking` is sent first, then the queued
   `bot-output` messages; while the bot is speaking, aggregated text is sent immediately.
   `bot-llm-text` is sent per `LLMTextFrame`; the deprecated `bot-transcription` buffer is built
   from `LLMTextFrame` text separately (relative send order between the two: UNVERIFIED).
   Broadcast frames are handled downstream only.
7. Shutdown: the JS `disconnect()` awaits `transport.disconnect()` (state `disconnecting` then
   `disconnected`; closes the WebSocket; no RTVI message sent). `disconnect-bot` is an explicit
   "end the bot" request with no reply. A `fatal: true` `error` makes the JS client call
   `disconnect()` itself. In the WebSocket transport, `close`, `error`, `connection-timeout` and
   `reconnect-failed` socket events each surface as a fatal `error` to the client.
8. Client-request correlation: `sendClientRequest()` (`client-message`) waits for `server-response`
   or `error-response` with the same id; `client.ts` applies no default timeout (an optional
   `timeout` is passed through); any default lives in `MessageDispatcher` (not fetched, UNVERIFIED).

## Pipecat WebSocket serializer (protobuf) — what a JS WebSocket client needs

The JS WebSocket transport's default serializer is `ProtobufFrameSerializer`; the only other
shipped one is `TwilioSerializer` (and `websocketSerializer.ts`, base). The Python server pairs
it with `FastAPIWebsocketTransport` + `ProtobufFrameSerializer()`, `add_wav_header=False`
(confirmed in `pipecat-examples/websocket/bot.py`; `add_wav_header` defaults to `False` anyway).

### Schema: Pipecat server, tag v1.12.0, `src/pipecat/frames/frames.proto` (BSD-2-Clause)

The file also has a leading comment block explaining how to regenerate `frames_pb2.py` with
grpcio-tools 1.67.x; it is omitted below. Message definitions are verbatim.

```proto
//
// Copyright (c) 2024–2026, Daily
//
// SPDX-License-Identifier: BSD 2-Clause License
//

syntax = "proto3";

package pipecat;

message TextFrame {
  uint64 id = 1;
  string name = 2;
  string text = 3;
}

message AudioRawFrame {
  uint64 id = 1;
  string name = 2;
  bytes audio = 3;
  uint32 sample_rate = 4;
  uint32 num_channels = 5;
  optional uint64 pts = 6;
}

message TranscriptionFrame {
  uint64 id = 1;
  string name = 2;
  string text = 3;
  string user_id = 4;
  string timestamp = 5;
}

message MessageFrame {
    string data = 1;
}

message InterruptionFrame {
  uint64 id = 1;
  string name = 2;
}

message Frame {
  oneof frame {
    TextFrame text = 1;
    AudioRawFrame audio = 2;
    TranscriptionFrame transcription = 3;
    MessageFrame message = 4;
    InterruptionFrame interruption = 5;
  }
}
```

### Schema: JS client, tag websocket-transport-v1.7.2, `proto/frames.proto` (differences only)

Identical messages 1 to 4 and the same `Frame` field numbers 1 to 4, but there is NO
`InterruptionFrame` and no `interruption = 5` in the client's `oneof` (header says 2024–2025;
the client file also carries explanatory comments). Consequence: when Pipecat's
`ProtobufFrameSerializer` sends an `interruption` frame (`SERIALIZABLE_TYPES` includes
`InterruptionFrame`), the JS client cannot decode that variant.
What the client then does is UNVERIFIED (the serializer throws "Unknown frame kind" for any
`oneofKind` other than `audio`/`message`; that `ws.on("message")` catches and logs it is NOT
confirmed; whether protobuf-ts keeps or rejects the unknown field 5 is not confirmed). A
Brisvox server should NOT send field 5.

### Wire rules (from `protobuf.py` v1.12.0 and the TS serializer)

- Each WebSocket message is ONE binary frame holding one serialized `Frame`. The JS
  `deserialize` requires `data instanceof Blob`, otherwise throws "Unknown data type". So
  WebSocket TEXT frames from the server are rejected by the default JS serializer: send RTVI
  messages ONLY as protobuf `MessageFrame` inside binary frames.
- RTVI message from either side: `Frame{ message: MessageFrame{ data: "<JSON string of the RTVI
  envelope>" } }`. JS sends `data: JSON.stringify(msg)`. Python wraps outgoing transport
  messages (`OutputTransportMessageFrame` / `OutputTransportMessageUrgentFrame`) with
  `MessageFrame(data=json.dumps(frame.message))` and parses incoming with `json.loads`
  (a parse failure drops the frame).
- Audio, client to server: `Frame{ audio: AudioRawFrame{ id: 0, name: "audio", audio:
  <PCM16 LE bytes>, sample_rate: <recorderSampleRate, default 16000>, num_channels: 1 } }`.
  `serializeAudio(data, sampleRate, numChannels)` is called with channels hardcoded to 1.
  proto3 omits zero/empty defaults on the wire (`id` 0 is not encoded).
- Audio, server to client: `Frame{ audio: AudioRawFrame{ audio: <PCM16 LE mono bytes>,
  sample_rate, num_channels } }`. The JS client IGNORES `sample_rate` and `num_channels` on
  receipt (the transport passes the parsed audio to `bufferBotAudio`; it plays at its configured
  `playerSampleRate`, default 24000, configurable only in the `WebSocketTransport`
  constructor). The server must therefore produce audio at the rate the client was configured
  with (24 kHz for the defaults). `add_wav_header=False`: no WAV header, raw PCM16.
  (That the bytes are read as `Int16Array`: confirmed in the serializer.)
- The JS client discards decoded frames that are neither `audio` nor `message`. `TextFrame` and
  `TranscriptionFrame` are in the schema but the TS `deserialize` does not return them.
- Python `deserialize` of client frames: only `text`, `audio`, `transcription`, `message`,
  `interruption` are accepted; `audio` becomes `InputAudioRawFrame`; `message` becomes
  `InputTransportMessageFrame`; the `id`/`name`/`pts` fields are copied when truthy. Python
  `serialize` sends `OutputAudioRawFrame` as `audio` and copies a field only if truthy.
- Max message size: the JS WebSocket transport sets `_maxMessageSize = 1024 * 1024` ("python
  websockets default to 1MB"), but the file never reads the value again, so its effect
  (if any) is UNVERIFIED. An earlier draft stated a 64 KiB default in the base `Transport` class;
  not confirmed in this pass (UNVERIFIED). Keep each outgoing frame well below 1 MiB (a 20 to
  40 ms audio frame is far below).
- Keepalive: the JS transport sets `ws.keepAliveInterval = 0` (no client-initiated pings;
  comment: "disabling the keep alive, there is no API for it inside Pipecat"). Browser WebSocket
  stacks answer server pings automatically. On drop the transport tries to reconnect twice
  (docs), then disconnects (retry count/backoff live in `ReconnectingWebSocket`, not fetched).
- Connection params accepted by `connect()`: only `wsUrl`/`ws_url` and `token` (snake_case keys
  are converted to camelCase; each must be a string); any other key throws
  ``Unrecognized connection parameter: <key>.`` (the docs say an `RTVIError`).

## Minimum a server must implement for `@pipecat-ai/client-js` + websocket transport to converse

Verified from the fetched sources (items marked UNVERIFIED are not confirmed by running a client):

1. Accept a WebSocket upgrade at the URL given in `wsUrl` (any path; optional `?token=`).
   No subprotocol requirement was found (UNVERIFIED that none is requested).
2. Speak ONLY binary frames, each a protobuf `pipecat.Frame` using fields 2 (audio) and 4
   (message). Decode `audio` (PCM16 mono at the client's recorder rate, 16 kHz by default,
   `sample_rate` in the frame is authoritative) and `message` (JSON string, envelope WITH an `id`).
3. On `client-ready`, reply with `bot-ready` echoing the request `id`
   (`data.version = "2.1.0"` (a major >= 2 avoids the client's warning), optional `data.about`).
   Without `bot-ready`, `connect()` never resolves (no timeout) and all `@transportReady`-gated
   calls (`sendText`, `sendClientMessage`, ...) stay unusable.
4. Stream bot speech as `audio` frames: PCM16 LE mono at the client's `playerSampleRate`
   (default 24 kHz). Pace at roughly real time: the transport has no way to clear audio already
   handed to the browser's `WavStreamPlayer` (see interruption below), so over-buffering makes
   barge-in sound late.
5. That is enough to talk. All RTVI events in the server table are optional for the client to
   function (each maps to an optional callback). Recommended minimum for a usable UI:
   `user-started-speaking`, `user-stopped-speaking`, `user-transcription`,
   `bot-started-speaking`, `bot-stopped-speaking`, `bot-llm-text` or `bot-output`, `error`.
6. Answer `client-message` with `server-response` (same id, `data: {t, d}`) or `error-response`,
   otherwise `sendClientRequest()` never resolves unless a timeout is set (default UNVERIFIED).
7. `disconnect-bot` and the WebSocket close both mean "end the call". No reply is defined.
8. Barge-in: `bot-interrupted` is not handled by client-js 1.13.1, and the websocket transport
   source has no interrupt/clear logic (confirmed in `webSocketTransport.ts`; it does not
   reference `WavStreamPlayer`; bot audio goes to `mediaManager.bufferBotAudio`).
   `DailyMediaManager.userStartedSpeaking()` calling `WavStreamPlayer.interrupt()` was not
   re-fetched in the fact-check (UNVERIFIED who triggers it and whether `interrupt()` drops
   buffered audio). Do not rely on a client-side flush; keep server-side pacing tight.

## Brisvox implications (design notes, not source facts)

- The existing generic protocol (`docs/WEBSOCKET_PROTOCOL.md`) carries bare PCM in binary frames
  and JSON in text frames. RTVI over WebSocket needs a different framing (protobuf `Frame` in
  every binary frame, no text frames), so it must be a separate adapter/mode behind the
  transport trait, not an extension of the generic framing.
- Rust implementation option: hand-written `prost` message structs (`#[derive(prost::Message)]`)
  matching the field numbers above need no `protoc`. `prost` 0.14.4 (updated 2026-06-07,
  Apache-2.0, 624,317,919 total downloads, repo tokio-rs/prost); `prost-build` 0.14.4 (Apache-2.0,
  2026-06-07). The `protobuf` crate (BSD-3-Clause per crates.io API for the newest version) is an
  alternative; its newest listed version was `4.36.2-release`, max stable `3.7.2` (updated
  2026-09-17; license of 3.7.2 not checked). Final pick: a design decision. No Rust RTVI
  crate exists: crates.io search for "rtvi" returned only an unrelated crate (`portview`).
- Rate mismatch: the JS client plays at 24 kHz by default and ignores the frame's
  `sample_rate`. Brisvox's generic server currently drops audio at a rate other than the call's
  (known limitation in WEBSOCKET_PROTOCOL.md), so the RTVI mode should treat the client's
  `playerSampleRate` (24 kHz default) as the call rate for output and 16 kHz for input unless
  the example configures otherwise.
- Test target: the Python `RTVIClientSerializer` (JSON text, not protobuf; used by Pipecat's
  own eval/websocket client transport) is a different carrier from the JS path. A conformance
  test should use the protobuf path (JS client) as the primary.

## Brisvox engine bridge (design notes, not source facts)

`brisvox_engine::rtvi::RtviBridge` drives a call's RTVI messages from the engine. What it sends
and when (checked end to end by `crates/brisvox-engine/tests/rtvi_events.rs`, a scripted client
speaking the protobuf wire above over a real socket):

- Events are observed on the inputs of five pipeline stages by the stages' own tasks
  (`brisvox_engine::events`; no extra stage or hop) and published to a bounded drop-oldest ring
  (tokio `broadcast`), so a slow client loses the oldest events and never stalls the pipeline.
  One bridge task maps them to RTVI messages and sends them through the call's `RtviSender`, in
  order, on one queue. The bridge switches the adapter's own `auto_events` off for the call
  (`RtviSender::set_auto_events`) while it sends the bot speaking events itself.
- `user-started-speaking` / `user-stopped-speaking`: the user's turn as the turn driver decides it
  (as Pipecat 1.x's turn start/stop strategies do), not the raw VAD; the VAD variants
  (`vad-user-*`, off by default in Pipecat's observer) are not sent. The turn driver publishes
  them itself, so every start gets its stop, also for a turn without words (noise, an empty
  final), a held end that a new turn supersedes, and the hang-up. A typed turn raises neither
  (Pipecat's `send-text` neither).
- `user-transcription`: every STT transcript (interim `final: false`, final segments
  `final: true`), `user_id` empty, `timestamp` ISO-8601 UTC from the injected clock.
- `user-llm-text`: the whole text of the turn the LLM answers (sent before
  `user-stopped-speaking`; the driver publishes both as it emits the transcript and the turn
  end).
- `bot-llm-started` / `bot-llm-text` (each streamed delta) / `bot-llm-stopped`: per LLM round.
- `bot-tts-started` (before the first chunk of a response), `bot-tts-text` (each chunk sent to the
  TTS, verbatim), `bot-tts-stopped` (the TTS produced the last audio, or a barge-in).
- `bot-output` per TTS chunk (`aggregated_by: "sentence"`, `will_be_spoken: true`), held until
  `bot-started-speaking` as Pipecat's observer does (step 6 of the handshake section); held ones are
  dropped on a barge-in (never spoken).
- `bot-started-speaking`: the first audio of the response handed to the transport;
  `bot-stopped-speaking`: the transport reports the response's end mark played (for RTVI the
  adapter's playout estimate).
- `bot-interrupted`, then `bot-stopped-speaking`, `bot-tts-stopped`, `bot-llm-stopped` for what was
  running; events of the ended epoch that arrive later are dropped.
- `metrics` once per turn, right after `bot-started-speaking`: `ttfb` of `llm` (time to first
  token) and `tts` (time to first byte), `processing` of `stt` (VAD stop to final), `turn`
  (end-of-turn delay) and `response` (server-side voice-to-voice), in seconds, `model` omitted.
- `send-text`: a user turn of its own (an interruption first while the bot thinks or speaks), the
  text appended to the context as a user message and answered, with no user speaking events;
  while the user speaks it joins the open turn; `run_immediately: false` holds it for the next
  turn (joined to that turn's user message) instead of appending a separate message at once
  without running the LLM (Pipecat's behavior; a documented deviation); `audio_response: false`
  is not supported (answered with audio, counted).
- `client-message`: answered by the application's handler (`server-response` echoing `t` with the
  handler's `d`), or `error-response` on its error, after `reply_timeout` (default 5 s) or without
  a handler. Up to 8 replies run at once in their own tasks, beside the inbox, so a slow handler
  never delays a later `send-text`; a request beyond 8 in flight gets an `error-response` at
  once.
- Not sent: `llm-function-call-*`, `bot-transcription` (deprecated), `bot-llm-search-response`,
  `bot-tts-audio`, audio levels, `system-log`, `user-mute-*`.

## Keepalive / timeouts

- JS client: no client pings (`keepAliveInterval = 0`); no timeout waiting for `bot-ready`.
- Server (Pipecat `FastAPIWebsocketParams`, v1.12.0 `transports/websocket/fastapi.py`):
  `session_timeout` (default None), `ws_close_timeout` (default 0.5 s), `allowed_origins`
  (default from env `PIPECAT_ALLOWED_ORIGINS`, comma-separated; empty list = allow all; when set,
  a missing or disallowed Origin header is rejected; parsing code not fetched).
- Brisvox should still enforce its own `start_timeout` equivalent (e.g. time to receive
  `client-ready`), since the client never will on its own if the server is silent.

## Finalize / flush / cancel semantics

- No explicit flush. Interruption: Pipecat emits `bot-interrupted` (any interruption) and sends
  the protobuf `interruption` frame (field 5, unknown to the JS client schema). `send-text` with
  `run_immediately=true` interrupts and flushes the pipeline before appending.
- `disconnect-bot`: ends the pipeline worker; no response.

## Timestamps / interim results / tool-call deltas

- `user-transcription.timestamp` is a string; `final=false` for interim results
  (`InterimTranscriptionFrame`).
- `bot-output` word-level progress: `spoken_progress{accumulated_text, remaining_text}`
  ("Word-level TTS progress within a spoken segment"); no per-word timestamp fields appear in
  `models.py`.
- Tool calls: `llm-function-call-started`/`in-progress`/`stopped` (not deltas of arguments);
  client results return via `llm-function-call-result`.

## Concurrency / rate limits

None defined by the protocol. `_maxMessageSize` 1 MiB is set in the JS WebSocket transport
(effect UNVERIFIED, see above).

## Turkish support

The protocol is language neutral (UTF-8 JSON inside a protobuf string). Turkish is the
providers' concern, not RTVI's.

## Gotchas found during research

- Only binary frames: a text frame from the server makes the default JS serializer throw.
- `sample_rate` on incoming bot audio is ignored by the JS client; 24 kHz assumed by default.
- Transport state becomes `ready` when the client SENDS `client-ready`, not when `bot-ready`
  arrives; user audio flows before `bot-ready`.
- JS client proto lacks `InterruptionFrame` (field 5) that the server proto has.
- Docs page body ("version 1.0 ... June 2025", "RTVI 1.2.0") contradicts the wire version 2.1.0;
  follow the source constants.
- `bot-transcription`, `llm-function-call`, `append-to-context` are deprecated; `bot-output`
  and `llm-function-call-in-progress` replace them.
- Server doc claims config/actions in `bot-ready`; the model has only `version` and `about`.
- Client messages MUST carry an `id` (Pipecat's `Message` model requires it).

## UNVERIFIED (collected)

1. Whether any WebSocket subprotocol or specific `binaryType` is required by the client.
2. Behavior of the JS client on receiving protobuf field 5 (`interruption`) and who calls
   `DailyMediaManager.userStartedSpeaking()` / what `WavStreamPlayer.interrupt()` does.
3. The default timeout of `sendClientRequest()` (MessageDispatcher not fetched).
4. Daily, SmallWebRTC, openai-realtime-webrtc, gemini-live-websocket, moq RTVI carrier details
   and existence (not fetched).
5. Byte-exactness of the constructed JSON examples above and the `timestamp` string format.
6. How Pipecat's example consumes the `token` query parameter (the example bot has no token code).
7. An end-to-end run with a real JS client (not possible in this session); the implementation must do it.
8. The example server's WebSocket path and the `startBotAndConnect` response shape; the
   effect of `_maxMessageSize`; base `Transport` default max size; whether the JS `ws.on("message")`
   handler catches deserialize errors; relative send order of `bot-llm-text` vs `bot-transcription`;
   whether the v1.12.0 server ever emits `bot-llm-search-response`; license of `protobuf` 3.7.2;
   commit hash `1559a684...` of tag v1.12.0.

## Fact-check log

Fact-checked 2026-10-08: 98 claims checked, 15 corrected. Corrections:

1. Docs-page title/labels: page is titled "The RTVI Standard"; "version 1.0 ... June 2025" is body text;
   the "1.2.0" label covers five specific messages (mute and function-call started/in-progress/stopped).
2. `raw-audio` data fields: single `base64Audio`, batch uses `base64AudioBatch` (list); no model in
   `models.py`; "Field semantics UNVERIFIED" replaced with the processor code facts.
3. `append-to-context`: now CONFIRMED absent from the processor dispatch (was inferred from a summary).
4. Server-initiated events: CONFIRMED to have no `id` field in `models.py` (was UNVERIFIED);
   payload-less events omit `data` (was UNVERIFIED); client messages require `id` (new fact).
5. Reference example endpoints: README says `POST /start`; `/ws` and `/api/start` could not be
   confirmed (moved to UNVERIFIED).
6. `frames.proto` (server) was labeled verbatim but omitted the regeneration comment block;
   now stated.
7. "No per-word timing fields exist" was quoted as a model docstring; that phrase is not in
   `models.py` (closest text: "Word-level TTS progress within a spoken segment"); reworded.
8. JS `bot-ready`: "no id check" CONFIRMED (was UNVERIFIED).
9. `sendClientRequest` / `connect()`: no default timeout in `client.ts`; the note that
   requests "reject only by timeout" corrected (they never resolve without a timeout).
10. `dtmf`: "needs protocol 2.x" corrected to 2.1.0+ for the `buttons` form; the earlier
    "button (2.0)" history is only a JS code comment about legacy bots; DTMF is not on the docs page.
11. Connection param error: confirmed text is `Unrecognized connection parameter: <key>.`; the
    `RTVIError` class comes from the docs, not from the transport source.
12. Max message size: base `Transport` 64 KiB claim not confirmed (UNVERIFIED); the 1 MiB value
    is never read after being set in the transport file.
13. `client-ready` error texts and the exactly-three-integers version format added; the label-drop
    warning text added.
14. Dependency lists: added `@types/events` (client-js) and `@protobuf-ts/plugin`, `x-law ^0.3.1`
    (transport); `ws.on("message")` catches deserialize errors downgraded to UNVERIFIED; the
    FastAPI transport defaults (`session_timeout`, `ws_close_timeout`, `allowed_origins`) now
    cite `transports/websocket/fastapi.py`, which was previously uncited.
15. Ordering of `bot-llm-text` relative to `bot-transcription` buffer update was not confirmed
    by the observer summary; downgraded to UNVERIFIED. `bot-llm-search-response` is not in
    `models.py`; shape is from the docs page only.

Confirmed unchanged (examples): versions/dates for Pipecat v1.12.0, client-js 1.13.1,
websocket-transport 1.7.2, daily-transport 1.6.9, small-webrtc-transport 1.10.8, livekit-transport
1.0.0; commit hashes of main, examples, client-js, websocket-transport tags and the v1.12.0 tag
object; `PROTOCOL_VERSION` 2.1.0 in both implementations; `LEGACY_SUPPORTED_MAJOR` 1; licenses
(BSD-2-Clause for npm packages); prost/prost-build 0.14.4 Apache-2.0, 624,317,919 downloads,
2026-06-07; protobuf crate versions and license; "rtvi" crate search result; proto schemas
(server and client); keepalive 0; 24 kHz / 16 kHz defaults; token param format; Blob requirement;
audio queueing before ready; fatal error disconnect; observer defaults and metrics keys;
bot-tts-audio not emitted; bot-interrupted/vad/audio-level/system-log/marker not handled by JS.
