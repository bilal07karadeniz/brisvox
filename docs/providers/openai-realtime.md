# OpenAI Realtime — speech-to-speech (S2S)

- Docs (all fetched 2026-10-13; the old `platform.openai.com/docs/...` URLs redirect with a 301 to
  `developers.openai.com/api/...`):
  - Overview and migration: https://developers.openai.com/api/docs/guides/realtime
  - WebSocket transport: https://developers.openai.com/api/docs/guides/voice-websockets?api=realtime
  - Conversations (function calling, interruption, formats, limits): https://developers.openai.com/api/docs/guides/realtime-conversations
  - Voice activity detection: https://developers.openai.com/api/docs/guides/realtime-vad
  - Tools: https://developers.openai.com/api/docs/guides/realtime-mcp
  - Client events: https://developers.openai.com/api/reference/resources/realtime/client-events
  - Server events: https://developers.openai.com/api/reference/resources/realtime/server-events
  - The `platform.openai.com` API reference pages answer 403 to an unauthenticated fetch; the
    `developers.openai.com` reference pages above are the readable copy.
- Auth: `Authorization: Bearer <OPENAI_API_KEY>` on the WebSocket upgrade request, from a server
  (never a browser). Browsers use an ephemeral key (`POST /v1/realtime/client_secrets`) as a
  WebSocket subprotocol `openai-insecure-api-key.<key>`; Brisvox is a server, so it uses the
  header. Optional `OpenAI-Safety-Identifier` header (a stable, hashed end-user id) on the
  connection request. The GA interface must NOT send `OpenAI-Beta: realtime=v1` (that header
  selected the removed beta interface).
- Endpoint(s) / regions: `wss://api.openai.com/v1/realtime?model=<model>`. Region/data-residency
  hosts: UNVERIFIED (not on the pages read). WebRTC (`/v1/realtime/calls`) and SIP exist; Brisvox
  uses WebSocket only, so the transport stays our own.
- Models: `gpt-realtime-2.1` is the model of every current doc example; `gpt-realtime-2` and
  `gpt-realtime` are named (image input section); `gpt-realtime-2025-08-28` appears in a
  `session.created` example. `gpt-realtime-whisper` / `gpt-live-transcribe` are transcription-only
  models (turn detection must be omitted or `null`). Exact pricing/latency: UNVERIFIED (not read).
  The model is chosen in the URL query and cannot change in `session.update`.
- Voices: `alloy ash ballad coral echo sage shimmer verse marin cedar` (`marin`/`cedar`
  recommended). `voice` cannot change after the model has produced audio in the session.
- Max session duration: 60 minutes ("The maximum duration of a Realtime session is 60 minutes",
  conversations guide, fetched 2026-10-13). What happens at the limit (error code, close) is NOT
  documented: UNVERIFIED. Handled by the engine: `session_limit` reports 60 minutes, the
  stage opens a replacement session seeded from its local mirror two minutes before and switches
  over while the bot is idle; if the session dies anyway the normal reconnect also seeds.
- History seeding: `conversation.item.create` (fetched 2026-10-13, client-events
  reference): `message` items with `role` `system` / `user` (content `input_text`) / `assistant`
  (content `output_text` or `output_audio`; "the type depends on the session's output_modalities";
  the event "cannot populate assistant audio messages"), `function_call` (`name`, `arguments` JSON
  string, optional `call_id`) and `function_call_output` (`call_id`, `output`); `status` has no
  effect; the item is appended unless `previous_item_id` is given. The adapter sends one event per
  history item after `session.updated`, before any audio. UNVERIFIED: whether an `assistant`
  message with `output_text` is accepted in a session whose `output_modalities` is `["audio"]`
  (the guide has no example); `seed_assistant_as_system` switches such turns to `system` messages
  ("The assistant said earlier: ...") if the server rejects them. Rejections arrive as `error`
  events (delivered as warnings); the session still works.
- Protocol: JSON text messages both ways, every event has `type` and an optional client-chosen
  `event_id`. Audio is base64 inside JSON (`input_audio_buffer.append`, up to 15 MB per chunk;
  no server acknowledgement). Server sends `session.created` right after the upgrade.

## Client events (we send)

| event | fields (verified) |
|---|---|
| `session.update` | `session: {type:"realtime", instructions, tools:[{type:"function",name,description,parameters}], tool_choice:"auto", output_modalities, audio:{input:{format, turn_detection, transcription, noise_reduction}, output:{format, voice, speed}}}`. Only included fields change. `session.type` is required on the GA interface. |
| `input_audio_buffer.append` | `audio` (base64) |
| `input_audio_buffer.commit` | none. Creates the user message item from the buffer; with server VAD it commits automatically; fails on an empty buffer. |
| `input_audio_buffer.clear` | none; server answers `input_audio_buffer.cleared` |
| `conversation.item.create` | `item`, optional `previous_item_id` (`"root"` = front). Message: `{type:"message",role:"user",content:[{type:"input_text",text}]}`. Tool result: `{type:"function_call_output",call_id,output}` (`output` is a string). |
| `conversation.item.truncate` | `item_id`, `content_index` (use 0), `audio_end_ms`. Truncates the assistant audio to what the user heard and deletes the server-side transcript of the item. |
| `conversation.item.delete` | `item_id` |
| `response.create` | optional `response` overrides (`instructions`, `tools`, `conversation:"none"` for out of band, `output_modalities`, `input`, `metadata`) |
| `response.cancel` | optional `response_id` (omitted: the in-progress response of the default conversation) |
| `output_audio_buffer.clear` | WebRTC/SIP only. Not used. |

## Server events (we handle)

| event | fields (verified) |
|---|---|
| `session.created` / `session.updated` | `session` (`id`, `model`, `audio.input.format`, `audio.output.{format,voice}`, `tools`, ...) |
| `error` | `error:{type, code, message, param, event_id}` (`event_id` is the client event that failed) |
| `input_audio_buffer.speech_started` | `audio_start_ms`, `item_id` |
| `input_audio_buffer.speech_stopped` | `audio_end_ms`, `item_id` |
| `input_audio_buffer.committed` | `item_id`, `previous_item_id` |
| `response.created` | `response:{id, status:"in_progress", output:[], ...}` |
| `response.output_audio.delta` | `response_id`, `item_id`, `output_index`, `content_index`, `delta` (base64 PCM) |
| `response.output_audio.done` | `response_id`, `item_id`, `output_index`, `content_index` |
| `response.output_audio_transcript.delta` / `.done` | same ids; `delta` / `transcript` |
| `response.function_call_arguments.done` | `response_id`, `item_id`, `output_index`, `call_id`, `name`, `arguments` (JSON string). The conversations guide does not mention this event (it reads the call from `response.done`); the reference lists it. The adapter accepts BOTH and deduplicates by `call_id`. |
| `response.output_item.done` | `response_id`, `output_index`, `item` (a `function_call` item has `call_id`, `name`, `arguments`) |
| `response.done` | `response:{id, status: completed\|cancelled\|failed\|incomplete, status_details:{type,reason,error}, output:[items], usage}` |
| `conversation.item.truncated` | `item_id`, `content_index`, `audio_end_ms` |
| `conversation.item.added` / `.done` | `item` |
| `rate_limits.updated` | `rate_limits:[{name,limit,remaining,reset_seconds}]` |
| `conversation.item.input_audio_transcription.completed` | UNVERIFIED (not in the pages read); the adapter reads `item_id` and `transcript` if present and ignores it otherwise. |

## Audio formats

- Session audio format is an object `{type:"audio/pcm", rate:24000}`; the `session.created`
  example shows `audio/pcm` at 24000 for input and output. PCM16 little-endian mono, no
  container header, even byte count per chunk.
- G.711: the docs show `"audio/pcmu"` (output example); `audio/pcma` and the 8 kHz rate for both
  G.711 variants are documented for the separate GPT-Live API (`wss://api.openai.com/v1/live/sessions`,
  first message `session.start`); for the Realtime API this is UNVERIFIED. The adapter requests
  `audio/pcm` at 24 kHz only; the engine resamples. G.711 passthrough is a follow-up.
- The format can be set per session (`audio.input.format`, `audio.output.format`) or per response.
- Input transcription: UNVERIFIED. The conversations guide only says "input transcription (if
  enabled)". The adapter sends `audio.input.transcription: {model, language}` (the layout is from
  memory of the session object, not from the pages read) only when the session asks for
  transcription AND `OpenAiRealtimeConfig::transcription_model` is set; it is off by default.
  The completion event `conversation.item.input_audio_transcription.completed` is UNVERIFIED too.

## Turn detection (`session.audio.input.turn_detection`)

- `server_vad` (default for S2S models): `threshold` (0..1, default 0.5), `prefix_padding_ms`
  (300), `silence_duration_ms` (200 in the `session.created` example, 500 in the VAD guide
  example), `create_response` (bool), `interrupt_response` (bool). `idle_timeout_ms`: UNVERIFIED
  (not on the guide page).
- `semantic_vad`: `eagerness` `low|medium|high|auto` (auto = medium), same two booleans.
- `null` (manual): the client commits (`input_audio_buffer.commit`) and asks for a response
  (`response.create`). Required for the transcription-only models.

## Interruption and truncation

- With VAD enabled the server detects user speech, cancels the in-progress response
  (`response.done` with `status:"cancelled"`; the guide also names a `response.cancelled` event that
  the server-events reference does not list: the adapter treats it as a synonym of a cancelled
  `response.done`).
- WebRTC/SIP: the server truncates unplayed audio itself. **WebSocket: the client must stop
  playback, note how much audio was played and send `conversation.item.truncate`** with
  `audio_end_ms` = played milliseconds. The server cannot know how much the caller heard, because
  it generates faster than real time and the audio sits in our and the carrier's buffers. The
  transcript of the truncated item is deleted server-side; the truncated transcript is NOT given
  back ("not precisely aligned").
- `response.cancel` on a response that already finished answers an `error` event (code
  UNVERIFIED, probably `response_cancel_not_active`); the adapter treats any error that carries
  the `event_id` of one of our `response.cancel`/`conversation.item.truncate` events as benign.

## Function calling

1. Tools in `session.update` (or per `response.create`): `{type:"function", name, description,
   parameters:<JSON Schema>}`, `tool_choice:"auto"`.
2. The model's call arrives as a `function_call` item (`type, name, arguments` JSON string,
   `call_id`) in `response.output_item.done` / `response.done.response.output[]`, and as
   `response.function_call_arguments.done` (+ `.delta` stream).
3. We run the tool, then send `conversation.item.create` with `{type:"function_call_output",
   call_id, output:"<string>"}`, then **`response.create`** so the model speaks the result. The
   model does not continue by itself after a function call.

## Keepalive / timeouts

- No application-level keepalive is documented. The adapter relies on the audio stream (caller
  audio flows continuously) and WebSocket ping replies. UNVERIFIED: server idle close behaviour.
- Rate limits arrive as `rate_limits.updated`. Concurrency limits: UNVERIFIED.

## Turkish support

UNVERIFIED for the speech-to-speech models in the pages read (the models are multilingual; no
language list was found). Input transcription (separate Whisper-class model) language hint:
UNVERIFIED. A live run must test Turkish before it is claimed.

## Gotchas found during implementation

- Audio deltas arrive faster than real time, so a barge-in at wall-clock `t` happens when the
  server has already sent seconds of audio: the heard duration must come from the transport's
  playout position (PlayoutBook), never from the count of deltas received.
- `audio_end_ms` larger than the audio the server holds for the item is an error; the engine
  clamps to the audio it received for the item.
- Do not send `response.create` for a tool result of a superseded response (the user is already
  speaking again); only add the output item.
- Every `function_call` should get a `function_call_output`, also `interrupted` ones, so the
  server-side history stays well formed.
